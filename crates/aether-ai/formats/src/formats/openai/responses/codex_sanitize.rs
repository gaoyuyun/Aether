//! Codex 后端的工具 schema 与 input item id 清洗。
//!
//! Codex 的 Responses 后端比公开 OpenAI API 严格：`$schema`、`\p{}` 正则、
//! 不满足 strict 约束的 `strict: true`、超过 64 字符的工具名、legacy 的
//! `web_search_preview` 工具类型、没有前缀或超长的 input item id 都会换来 400。
//! MCP 工具定义最常触发这些问题。这里在请求发往 Codex 之前做最小改写：
//!
//! - 只处理已知的 JSON Schema 关键字位置，不碰 `description` / `default` /
//!   `enum` 里恰好叫 `pattern` 的用户数据。
//! - `oneOf` / `anyOf` 里全是纯 `const` 分支时折叠成 `enum`（语义等价）。
//! - 工具名超长时截断并接 sha256 前 16 hex，长度固定 64；响应侧用
//!   [`restore_codex_shortened_tool_name`] 按原始工具表还原。
//! - input item id：`message` → `msg`、`function_call` → `fc`、`custom_tool_call` → `ctc`、
//!   `custom_tool_call_output` → `ctco`；超过 64 字符的 id 截断加哈希；超长 id 的加密
//!   推理项整条丢弃（上游无法回放）。推理项 id 不改前缀，交给既有的严格回放过滤。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Codex 工具名与 input item id 的长度上限。
pub const CODEX_TOOL_NAME_MAX_LEN: usize = 64;
pub const CODEX_INPUT_ITEM_ID_MAX_LEN: usize = 64;
const HASH_SUFFIX_HEX_LEN: usize = 16;

const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];
const SCHEMA_VALUE_KEYWORDS: &[&str] = &[
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];
/// OpenAI strict 模式不接受的校验关键字；出现即降级 `strict: false`。
const STRICT_UNSUPPORTED_KEYWORDS: &[&str] = &[
    "pattern",
    "format",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "patternProperties",
    "unevaluatedProperties",
    "propertyNames",
    "dependentSchemas",
    "dependencies",
    "if",
    "then",
    "else",
    "not",
    "oneOf",
    "contains",
];

/// 一次清洗的改动计数，供测试与请求详情观测。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexToolSanitizeReport {
    pub schema_keywords_removed: usize,
    pub patterns_removed: usize,
    pub strict_downgraded: usize,
    pub names_shortened: usize,
    pub tool_types_normalized: usize,
    pub const_unions_folded: usize,
}

impl CodexToolSanitizeReport {
    pub fn changed(&self) -> bool {
        self.schema_keywords_removed > 0
            || self.patterns_removed > 0
            || self.strict_downgraded > 0
            || self.names_shortened > 0
            || self.tool_types_normalized > 0
            || self.const_unions_folded > 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexInputIdSanitizeReport {
    pub ids_prefixed: usize,
    pub ids_shortened: usize,
    pub reasoning_items_dropped: usize,
}

impl CodexInputIdSanitizeReport {
    pub fn changed(&self) -> bool {
        self.ids_prefixed > 0 || self.ids_shortened > 0 || self.reasoning_items_dropped > 0
    }
}

fn sha256_hex_prefix(input: &str, hex_len: usize) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest
        .iter()
        .take(hex_len.div_ceil(2))
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(hex_len)
        .collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

/// 把超长标识符压到 `max_len`：前缀截断 + `_` + sha256 前 16 hex。`attempt > 0`
/// 用于碰撞时换一个哈希。
fn shorten_identifier(value: &str, max_len: usize, attempt: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    let hash_input = if attempt == 0 {
        value.to_string()
    } else {
        format!("{value}\0{attempt}")
    };
    let suffix = format!("_{}", sha256_hex_prefix(&hash_input, HASH_SUFFIX_HEX_LEN));
    let prefix_len = max_len.saturating_sub(suffix.len());
    format!("{}{suffix}", truncate_chars(value, prefix_len))
}

/// Codex 工具名的确定性缩短规则。`mcp__server__tool` 形状先尝试只保留
/// `mcp__tool`，仍超长再走哈希。这是无碰撞时的结果；同一请求里多个工具缩短后
/// 重名时由 [`codex_tool_name_map`] 递增 `attempt` 换哈希，两侧按同一规则推导。
pub fn shorten_codex_tool_name(name: &str) -> String {
    shorten_codex_tool_name_attempt(name, 0)
}

fn shorten_codex_tool_name_attempt(name: &str, attempt: usize) -> String {
    if name.chars().count() <= CODEX_TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    if attempt == 0 {
        if let Some(rest) = name.strip_prefix("mcp__") {
            if let Some((_, leaf)) = rest.rsplit_once("__") {
                let candidate = format!("mcp__{leaf}");
                if !leaf.is_empty() && candidate.chars().count() <= CODEX_TOOL_NAME_MAX_LEN {
                    return candidate;
                }
            }
        }
    }
    shorten_identifier(name, CODEX_TOOL_NAME_MAX_LEN, attempt)
}

/// 碰撞换哈希的最多尝试次数；16 hex 的哈希后缀实际上第 1 次就能分开。
const CODEX_TOOL_NAME_MAX_ATTEMPTS: usize = 64;

/// 按一组原始工具名推导 `原名 → 缩短名` 映射（只含需要缩短的名字）。
///
/// 规则与顺序无关：不需要缩短的名字先占位；需要缩短的名字按字典序处理，缩短结果
/// 与已占位的名字重名时递增 `attempt` 换哈希后缀。请求侧（转换后的 Responses 工具表）
/// 与响应侧（`original_request_body.tools`）各自从自己的工具表推导，只要两边的
/// 名字集合相同，映射就一致——不依赖数组顺序，也不依赖 Chat / Responses 形状。
pub fn codex_tool_name_map<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, String> {
    let names = names.into_iter().collect::<BTreeSet<&str>>();
    let mut used = names
        .iter()
        .filter(|name| name.chars().count() <= CODEX_TOOL_NAME_MAX_LEN)
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<String>>();
    let mut map = BTreeMap::new();
    for name in names
        .iter()
        .filter(|name| name.chars().count() > CODEX_TOOL_NAME_MAX_LEN)
    {
        let mut attempt = 0usize;
        let shortened = loop {
            let candidate = shorten_codex_tool_name_attempt(name, attempt);
            if !used.contains(&candidate) || attempt >= CODEX_TOOL_NAME_MAX_ATTEMPTS {
                break candidate;
            }
            attempt += 1;
        };
        used.insert(shortened.clone());
        map.insert((*name).to_string(), shortened);
    }
    map
}

fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        if index + 2 < bytes.len()
            && matches!(bytes[index + 1], b'p' | b'P')
            && bytes[index + 2] == b'{'
        {
            return true;
        }
        index += 2;
    }
    false
}

fn strip_incompatible_schema_keywords(schema: &mut Value, report: &mut CodexToolSanitizeReport) {
    match schema {
        Value::Object(object) => {
            if object.remove("$schema").is_some() {
                report.schema_keywords_removed += 1;
            }
            if object
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(has_unsupported_unicode_property_escape)
            {
                object.remove("pattern");
                report.patterns_removed += 1;
            }
            if let Some(Value::Object(pattern_properties)) = object.get_mut("patternProperties") {
                let bad_keys = pattern_properties
                    .keys()
                    .filter(|key| has_unsupported_unicode_property_escape(key))
                    .cloned()
                    .collect::<Vec<_>>();
                for key in bad_keys {
                    pattern_properties.remove(&key);
                    report.patterns_removed += 1;
                }
                for sub_schema in pattern_properties.values_mut() {
                    strip_incompatible_schema_keywords(sub_schema, report);
                }
            }
            for keyword in SCHEMA_MAP_KEYWORDS {
                if *keyword == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = object.get_mut(*keyword) {
                    for sub_schema in sub_map.values_mut() {
                        strip_incompatible_schema_keywords(sub_schema, report);
                    }
                }
            }
            for keyword in SCHEMA_VALUE_KEYWORDS {
                match object.get_mut(*keyword) {
                    Some(sub_schema @ Value::Object(_)) => {
                        strip_incompatible_schema_keywords(sub_schema, report);
                    }
                    Some(Value::Array(items)) => {
                        for item in items {
                            strip_incompatible_schema_keywords(item, report);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_incompatible_schema_keywords(item, report);
            }
        }
        _ => {}
    }
}

fn canonical_const_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(format!("s:{text}")),
        Value::Number(number) => Some(format!("n:{number}")),
        Value::Bool(flag) => Some(format!("b:{flag}")),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn pure_const_branch(branch: &Value) -> Option<(String, Value)> {
    let object = branch.as_object()?;
    let constant = object.get("const")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "const" | "description" | "title"))
    {
        return None;
    }
    Some((canonical_const_key(constant)?, constant.clone()))
}

/// 把 `{"oneOf": [{"const": ...}, ...]}` 折叠成 `{"enum": [...]}`。只在每个分支都是
/// 纯常量且互不重复时改写；已有 `enum` 且集合相同就只删掉冗余的 union。
fn fold_const_union(object: &mut Map<String, Value>, report: &mut CodexToolSanitizeReport) {
    let has_one_of = object.contains_key("oneOf");
    let has_any_of = object.contains_key("anyOf");
    if has_one_of == has_any_of {
        return;
    }
    let union_name = if has_one_of { "oneOf" } else { "anyOf" };
    let Some(branches) = object.get(union_name).and_then(Value::as_array) else {
        return;
    };
    if branches.len() < 2 {
        return;
    }
    let mut keys = BTreeSet::new();
    let mut values = Vec::with_capacity(branches.len());
    for branch in branches {
        let Some((key, value)) = pure_const_branch(branch) else {
            return;
        };
        if !keys.insert(key) {
            return;
        }
        values.push(value);
    }
    if let Some(existing) = object.get("enum").and_then(Value::as_array) {
        let existing_keys = existing
            .iter()
            .map(canonical_const_key)
            .collect::<Option<BTreeSet<_>>>();
        if existing_keys.is_some_and(|existing_keys| existing_keys == keys) {
            object.remove(union_name);
            report.const_unions_folded += 1;
        }
        return;
    }
    object.remove(union_name);
    object.insert("enum".to_string(), Value::Array(values));
    report.const_unions_folded += 1;
}

fn fold_const_unions_recursively(schema: &mut Value, report: &mut CodexToolSanitizeReport) {
    match schema {
        Value::Object(object) => {
            fold_const_union(object, report);
            for keyword in SCHEMA_MAP_KEYWORDS {
                if let Some(Value::Object(sub_map)) = object.get_mut(*keyword) {
                    for sub_schema in sub_map.values_mut() {
                        fold_const_unions_recursively(sub_schema, report);
                    }
                }
            }
            for keyword in SCHEMA_VALUE_KEYWORDS {
                match object.get_mut(*keyword) {
                    Some(sub_schema @ Value::Object(_)) => {
                        fold_const_unions_recursively(sub_schema, report);
                    }
                    Some(Value::Array(items)) => {
                        for item in items {
                            fold_const_unions_recursively(item, report);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                fold_const_unions_recursively(item, report);
            }
        }
        _ => {}
    }
}

fn schema_is_object_like(object: &Map<String, Value>) -> bool {
    match object.get("type") {
        Some(Value::String(type_name)) => type_name == "object",
        Some(Value::Array(types)) => types.iter().any(|value| value.as_str() == Some("object")),
        _ => object.contains_key("properties"),
    }
}

/// OpenAI strict 模式要求：每个对象 `additionalProperties: false`、`required` 覆盖全部
/// 属性、不含 strict 不支持的关键字。任何一处不满足就返回 false。
pub fn schema_satisfies_codex_strict_mode(schema: &Value) -> bool {
    match schema {
        Value::Object(object) => {
            if object
                .keys()
                .any(|key| STRICT_UNSUPPORTED_KEYWORDS.contains(&key.as_str()))
            {
                return false;
            }
            if schema_is_object_like(object) {
                if object.get("additionalProperties") != Some(&Value::Bool(false)) {
                    return false;
                }
                let properties = object
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().cloned().collect::<BTreeSet<_>>())
                    .unwrap_or_default();
                let required = object
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|required| {
                        required
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                if properties != required {
                    return false;
                }
            }
            for keyword in SCHEMA_MAP_KEYWORDS {
                if let Some(Value::Object(sub_map)) = object.get(*keyword) {
                    if !sub_map.values().all(schema_satisfies_codex_strict_mode) {
                        return false;
                    }
                }
            }
            for keyword in SCHEMA_VALUE_KEYWORDS {
                match object.get(*keyword) {
                    Some(sub_schema @ Value::Object(_)) => {
                        if !schema_satisfies_codex_strict_mode(sub_schema) {
                            return false;
                        }
                    }
                    Some(Value::Array(items)) => {
                        if !items.iter().all(schema_satisfies_codex_strict_mode) {
                            return false;
                        }
                    }
                    _ => {}
                }
            }
            true
        }
        Value::Bool(_) => true,
        _ => false,
    }
}

fn normalize_codex_builtin_tool_type(tool_type: &str) -> Option<&'static str> {
    match tool_type {
        "web_search_preview" | "web_search_preview_2025_03_11" => Some("web_search"),
        _ => None,
    }
}

fn sanitize_codex_tool(
    tool: &mut Map<String, Value>,
    name_map: &BTreeMap<String, String>,
    report: &mut CodexToolSanitizeReport,
) {
    if let Some(normalized) = tool
        .get("type")
        .and_then(Value::as_str)
        .and_then(normalize_codex_builtin_tool_type)
    {
        tool.insert("type".to_string(), Value::String(normalized.to_string()));
        report.tool_types_normalized += 1;
    }
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function")
        .to_string();
    if tool_type == "namespace" {
        if let Some(Value::Array(nested)) = tool.get_mut("tools") {
            for nested_tool in nested.iter_mut().filter_map(Value::as_object_mut) {
                sanitize_codex_tool(nested_tool, name_map, report);
            }
        }
        return;
    }
    if !matches!(tool_type.as_str(), "function" | "custom") {
        return;
    }
    if let Some(shortened) = tool
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name_map.get(name))
        .cloned()
    {
        tool.insert("name".to_string(), Value::String(shortened));
        report.names_shortened += 1;
    }
    if let Some(parameters) = tool.get_mut("parameters") {
        strip_incompatible_schema_keywords(parameters, report);
        fold_const_unions_recursively(parameters, report);
    }
    if tool.get("strict") == Some(&Value::Bool(true)) {
        let satisfied = tool
            .get("parameters")
            .is_none_or(schema_satisfies_codex_strict_mode);
        if !satisfied {
            tool.insert("strict".to_string(), Value::Bool(false));
            report.strict_downgraded += 1;
        }
    }
}

fn rewrite_tool_reference_name(
    target: &mut Map<String, Value>,
    name_map: &BTreeMap<String, String>,
) {
    if let Some(shortened) = target
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name_map.get(name))
    {
        target.insert("name".to_string(), Value::String(shortened.clone()));
    }
}

/// 清洗 `tools`、`tool_choice` 与 `input` 历史里的工具引用。返回改动计数。
pub fn sanitize_codex_tools_for_backend(
    body_object: &mut Map<String, Value>,
) -> CodexToolSanitizeReport {
    let mut report = CodexToolSanitizeReport::default();
    let mut original_names = Vec::new();
    if let Some(tools) = body_object.get("tools") {
        collect_original_tool_names(tools, &mut original_names);
    }
    let name_map = codex_tool_name_map(original_names.iter().map(String::as_str));
    if let Some(Value::Array(tools)) = body_object.get_mut("tools") {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            sanitize_codex_tool(tool, &name_map, &mut report);
        }
    }
    if let Some(Value::Object(tool_choice)) = body_object.get_mut("tool_choice") {
        if let Some(normalized) = tool_choice
            .get("type")
            .and_then(Value::as_str)
            .and_then(normalize_codex_builtin_tool_type)
        {
            tool_choice.insert("type".to_string(), Value::String(normalized.to_string()));
            report.tool_types_normalized += 1;
        }
        rewrite_tool_reference_name(tool_choice, &name_map);
    }
    if name_map.is_empty() {
        return report;
    }
    if let Some(Value::Array(input)) = body_object.get_mut("input") {
        for item in input.iter_mut().filter_map(Value::as_object_mut) {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|item_type| matches!(item_type, "function_call" | "custom_tool_call"))
            {
                rewrite_tool_reference_name(item, &name_map);
            }
        }
    }
    report
}

fn collect_original_tool_names(tools: &Value, names: &mut Vec<String>) {
    let Some(tools) = tools.as_array() else {
        return;
    };
    for tool in tools.iter().filter_map(Value::as_object) {
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            names.push(name.to_string());
        }
        if let Some(name) = tool
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
        {
            names.push(name.to_string());
        }
        if let Some(nested) = tool.get("tools") {
            collect_original_tool_names(nested, names);
        }
    }
}

/// 响应侧还原：上游回传的工具名若等于某个原始超长工具名的缩短形式，换回原名。
/// `report_context.original_request_body.tools` 兼容 Responses（`name`）与 Chat
/// （`function.name`）两种形状。映射用 [`codex_tool_name_map`] 从原始工具表重新推导，
/// 与请求侧同一规则，碰撞时的哈希后缀也能对上。原始工具表里没有超长名字时返回 `None`。
pub fn restore_codex_shortened_tool_name(report_context: &Value, name: &str) -> Option<String> {
    let tools = report_context
        .get("original_request_body")
        .and_then(|request| request.get("tools"))?;
    let mut names = Vec::new();
    collect_original_tool_names(tools, &mut names);
    if names.iter().any(|original| original == name) {
        return None;
    }
    codex_tool_name_map(names.iter().map(String::as_str))
        .into_iter()
        .find_map(|(original, shortened)| (shortened == name).then_some(original))
}

/// 响应侧：把 Responses `output` 数组（或单个 output item）里被缩短的工具名还原。
pub fn restore_codex_shortened_tool_names_in_output(
    report_context: &Value,
    output: &mut Value,
) -> usize {
    let mut restored = 0;
    match output {
        Value::Array(items) => {
            for item in items {
                restored += restore_codex_shortened_tool_names_in_output(report_context, item);
            }
        }
        Value::Object(item) => {
            let is_tool_call = item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|item_type| matches!(item_type, "function_call" | "custom_tool_call"));
            if is_tool_call {
                if let Some(original) = item
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|name| restore_codex_shortened_tool_name(report_context, name))
                {
                    item.insert("name".to_string(), Value::String(original));
                    restored += 1;
                }
            }
        }
        _ => {}
    }
    restored
}

/// 响应侧：Responses SSE 事件里的工具名还原（`response.output_item.*` 的 `item`、
/// `response.completed` / `response.done` 的 `response.output`）。
pub fn restore_codex_shortened_tool_names_in_stream_event(
    report_context: &Value,
    event: &mut Value,
) -> usize {
    let Some(object) = event.as_object_mut() else {
        return 0;
    };
    let mut restored = 0;
    if let Some(item) = object.get_mut("item") {
        restored += restore_codex_shortened_tool_names_in_output(report_context, item);
    }
    if let Some(output) = object
        .get_mut("response")
        .and_then(Value::as_object_mut)
        .and_then(|response| response.get_mut("output"))
    {
        restored += restore_codex_shortened_tool_names_in_output(report_context, output);
    }
    restored
}

/// 响应侧：canonical 响应里工具调用块的名字还原。
pub(crate) fn restore_codex_shortened_tool_names_in_canonical_response(
    report_context: &Value,
    response: &mut crate::protocol::canonical::CanonicalResponse,
) -> usize {
    use crate::protocol::canonical::CanonicalContentBlock;
    let mut restored = 0;
    let mut restore_blocks = |blocks: &mut Vec<CanonicalContentBlock>| {
        for block in blocks.iter_mut() {
            if let CanonicalContentBlock::ToolUse { name, .. } = block {
                if let Some(original) = restore_codex_shortened_tool_name(report_context, name) {
                    *name = original;
                    restored += 1;
                }
            }
        }
    };
    restore_blocks(&mut response.content);
    for output in response.outputs.iter_mut() {
        restore_blocks(&mut output.content);
    }
    restored
}

/// 推理项的 id 是上游的不透明引用，Aether 不改它的前缀：外来 id（如 `item_…`）由
/// 后续的严格回放过滤剔除，改前缀会让它们冒充可回放项。这里只给其余类型补前缀。
fn codex_input_item_id_prefix(item_type: &str) -> Option<&'static str> {
    match item_type {
        "message" => Some("msg"),
        "function_call" => Some("fc"),
        "custom_tool_call" => Some("ctc"),
        "custom_tool_call_output" => Some("ctco"),
        _ => None,
    }
}

fn normalize_codex_input_item_id(item_type: &str, id: &str) -> String {
    let Some(prefix) = codex_input_item_id_prefix(item_type) else {
        return id.to_string();
    };
    if id.is_empty() || id.starts_with(prefix) {
        return id.to_string();
    }
    format!("{prefix}_{id}")
}

fn should_drop_codex_encrypted_reasoning_item(item: &Map<String, Value>) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning")
        && item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.chars().count() > CODEX_INPUT_ITEM_ID_MAX_LEN)
        && item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|content| !content.is_empty())
}

/// 归一化 `input[].id`：补类型前缀、超长哈希、丢弃无法回放的超长加密推理项。
/// 同一个 id 被多个项引用时保持一致映射；与原生保留的 id 冲突时换哈希。
pub fn sanitize_codex_input_item_ids(
    body_object: &mut Map<String, Value>,
) -> CodexInputIdSanitizeReport {
    let mut report = CodexInputIdSanitizeReport::default();
    let Some(Value::Array(input)) = body_object.get_mut("input") else {
        return report;
    };

    // 第一遍：记录会原样保留的 id，避免改写后的 id 撞上它们。
    let mut preserved: BTreeSet<String> = BTreeSet::new();
    for item in input.iter().filter_map(Value::as_object) {
        if should_drop_codex_encrypted_reasoning_item(item) {
            continue;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        let normalized = normalize_codex_input_item_id(item_type, id);
        if normalized == id && id.chars().count() <= CODEX_INPUT_ITEM_ID_MAX_LEN {
            preserved.insert(id.to_string());
        }
    }

    let mut mapped: BTreeMap<String, String> = BTreeMap::new();
    let mut occupied = preserved.clone();
    let original = std::mem::take(input);
    let mut rebuilt = Vec::with_capacity(original.len());
    for mut item in original {
        let Some(object) = item.as_object_mut() else {
            rebuilt.push(item);
            continue;
        };
        if should_drop_codex_encrypted_reasoning_item(object) {
            report.reasoning_items_dropped += 1;
            continue;
        }
        let Some(original_id) = object
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            rebuilt.push(item);
            continue;
        };
        let item_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_string();
        let mut next_id = normalize_codex_input_item_id(&item_type, &original_id);
        let prefixed = next_id != original_id;
        if next_id != original_id || next_id.chars().count() > CODEX_INPUT_ITEM_ID_MAX_LEN {
            let map_key = format!("{item_type}\0{original_id}");
            next_id = match mapped.get(&map_key) {
                Some(existing) => existing.clone(),
                None => {
                    let mut candidate = next_id.clone();
                    let mut attempt = 0;
                    loop {
                        let collides = candidate != original_id && preserved.contains(&candidate);
                        let too_long = candidate.chars().count() > CODEX_INPUT_ITEM_ID_MAX_LEN;
                        if !collides && !too_long {
                            break;
                        }
                        attempt += 1;
                        candidate = if too_long {
                            shorten_identifier(&next_id, CODEX_INPUT_ITEM_ID_MAX_LEN, attempt - 1)
                        } else {
                            shorten_identifier(
                                &format!("{next_id}{}", "_".repeat(CODEX_INPUT_ITEM_ID_MAX_LEN)),
                                CODEX_INPUT_ITEM_ID_MAX_LEN,
                                attempt - 1,
                            )
                        };
                        if occupied.contains(&candidate) && attempt < 64 {
                            continue;
                        }
                    }
                    occupied.insert(candidate.clone());
                    mapped.insert(map_key, candidate.clone());
                    candidate
                }
            };
        }
        if next_id != original_id {
            if prefixed {
                report.ids_prefixed += 1;
            }
            if next_id.chars().count() < original_id.chars().count()
                || original_id.chars().count() > CODEX_INPUT_ITEM_ID_MAX_LEN
            {
                report.ids_shortened += 1;
            }
            object.insert("id".to_string(), Value::String(next_id));
        }
        rebuilt.push(item);
    }
    *input = rebuilt;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_schema_and_unicode_property_patterns_only_in_schema_positions() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "pattern": "^\\p{L}+$", "description": "pattern: \\p{L}"},
                        "plain": {"type": "string", "pattern": "^[a-z]+$"},
                        "nested": {
                            "type": "object",
                            "$schema": "x",
                            "patternProperties": {"^\\p{N}$": {"type": "string"}, "^ok$": {"type": "string"}}
                        },
                        "list": {"type": "array", "items": {"type": "string", "pattern": "\\P{Lu}"}}
                    },
                    "default": {"pattern": "\\p{L}"}
                }
            }]
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.schema_keywords_removed, 2);
        assert_eq!(report.patterns_removed, 3);
        let parameters = &body["tools"][0]["parameters"];
        assert!(parameters.get("$schema").is_none());
        assert!(parameters["properties"]["name"].get("pattern").is_none());
        assert_eq!(
            parameters["properties"]["name"]["description"],
            "pattern: \\p{L}"
        );
        assert_eq!(parameters["properties"]["plain"]["pattern"], "^[a-z]+$");
        assert!(parameters["properties"]["nested"].get("$schema").is_none());
        assert!(parameters["properties"]["nested"]["patternProperties"]
            .get("^\\p{N}$")
            .is_none());
        assert!(parameters["properties"]["nested"]["patternProperties"]
            .get("^ok$")
            .is_some());
        assert!(parameters["properties"]["list"]["items"]
            .get("pattern")
            .is_none());
        assert_eq!(parameters["default"]["pattern"], "\\p{L}");
    }

    #[test]
    fn folds_pure_const_unions_into_enum_and_leaves_mixed_unions_alone() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "name": "pick",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {"oneOf": [
                            {"const": "fast", "description": "quick"},
                            {"const": "slow", "title": "Slow"},
                            {"const": 3}
                        ]},
                        "mixed": {"anyOf": [{"const": "a"}, {"type": "string", "minLength": 1}]},
                        "dup": {"oneOf": [{"const": "a"}, {"const": "a"}]},
                        "already": {"enum": ["x", "y"], "anyOf": [{"const": "y"}, {"const": "x"}]}
                    }
                }
            }]
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.const_unions_folded, 2);
        let properties = &body["tools"][0]["parameters"]["properties"];
        assert_eq!(properties["mode"]["enum"], json!(["fast", "slow", 3]));
        assert!(properties["mode"].get("oneOf").is_none());
        assert!(properties["mixed"].get("anyOf").is_some());
        assert!(properties["dup"].get("oneOf").is_some());
        assert_eq!(properties["already"]["enum"], json!(["x", "y"]));
        assert!(properties["already"].get("anyOf").is_none());
    }

    #[test]
    fn downgrades_strict_only_when_schema_cannot_satisfy_strict_mode() {
        let mut body = json!({
            "tools": [
                {
                    "type": "function",
                    "name": "strict_ok",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {"a": {"type": "string"}, "b": {"type": "object", "properties": {"c": {"type": "number"}}, "required": ["c"], "additionalProperties": false}},
                        "required": ["a", "b"],
                        "additionalProperties": false
                    }
                },
                {
                    "type": "function",
                    "name": "missing_required",
                    "strict": true,
                    "parameters": {"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": false}
                },
                {
                    "type": "function",
                    "name": "unsupported_keyword",
                    "strict": true,
                    "parameters": {"type": "object", "properties": {"a": {"type": "string", "minLength": 1}}, "required": ["a"], "additionalProperties": false}
                },
                {
                    "type": "function",
                    "name": "no_params",
                    "strict": true
                }
            ]
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.strict_downgraded, 2);
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tools"][1]["strict"], false);
        assert_eq!(body["tools"][2]["strict"], false);
        assert_eq!(body["tools"][3]["strict"], true);
    }

    #[test]
    fn shortens_long_tool_names_consistently_and_restores_them_from_report_context() {
        let long_name = format!("mcp__server__{}", "x".repeat(80));
        let plain_long = "y".repeat(70);
        let mut body = json!({
            "tools": [
                {"type": "function", "name": long_name},
                {"type": "function", "name": plain_long},
                {"type": "function", "name": "short"},
                {"type": "namespace", "name": "ns", "tools": [{"type": "function", "name": plain_long}]}
            ],
            "tool_choice": {"type": "function", "name": plain_long},
            "input": [
                {"type": "function_call", "call_id": "c1", "name": plain_long, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.names_shortened, 3);
        let mcp_short = body["tools"][0]["name"].as_str().expect("name");
        assert!(mcp_short.starts_with("mcp__"));
        assert_eq!(mcp_short.chars().count(), CODEX_TOOL_NAME_MAX_LEN);
        let plain_short = body["tools"][1]["name"].as_str().expect("name").to_string();
        assert_eq!(plain_short.chars().count(), CODEX_TOOL_NAME_MAX_LEN);
        assert!(plain_short.starts_with(&"y".repeat(47)));
        assert_eq!(body["tools"][2]["name"], "short");
        assert_eq!(body["tools"][3]["tools"][0]["name"], plain_short);
        assert_eq!(body["tool_choice"]["name"], plain_short);
        assert_eq!(body["input"][0]["name"], plain_short);
        assert_eq!(shorten_codex_tool_name(&plain_long), plain_short);

        let responses_context =
            json!({"original_request_body": {"tools": [{"type": "function", "name": plain_long}]}});
        assert_eq!(
            restore_codex_shortened_tool_name(&responses_context, &plain_short).as_deref(),
            Some(plain_long.as_str())
        );
        let chat_context = json!({"original_request_body": {"tools": [{"type": "function", "function": {"name": plain_long}}]}});
        assert_eq!(
            restore_codex_shortened_tool_name(&chat_context, &plain_short).as_deref(),
            Some(plain_long.as_str())
        );
        assert!(restore_codex_shortened_tool_name(&chat_context, "short").is_none());
        assert!(restore_codex_shortened_tool_name(&chat_context, &plain_long).is_none());

        let leaf = format!("mcp__srv__{}", "z".repeat(20));
        assert_eq!(shorten_codex_tool_name(&leaf), leaf);
        let long_leaf = format!("mcp__{}__leaf", "s".repeat(80));
        assert_eq!(shorten_codex_tool_name(&long_leaf), "mcp__leaf");
    }

    #[test]
    fn colliding_shortened_tool_names_get_distinct_suffixes_and_round_trip() {
        // 两个 MCP 服务器都暴露 search，全名都超过 64：默认规则都缩成 mcp__search。
        let github = "mcp__github_enterprise_server_with_a_very_very_long_name_for_tests__search";
        let wiki = "mcp__internal_wiki_server_with_another_very_long_name_for_tests__search";
        assert!(github.chars().count() > CODEX_TOOL_NAME_MAX_LEN);
        assert!(wiki.chars().count() > CODEX_TOOL_NAME_MAX_LEN);
        assert_eq!(shorten_codex_tool_name(github), "mcp__search");
        assert_eq!(shorten_codex_tool_name(wiki), "mcp__search");

        let mut body = json!({
            "tools": [
                {"type": "function", "name": github},
                {"type": "function", "name": wiki},
                // 一个本来就叫 mcp__search 的短名也在场：缩短结果不得覆盖它。
                {"type": "function", "name": "mcp__search"}
            ],
            "tool_choice": {"type": "function", "name": wiki},
            "input": [
                {"type": "function_call", "call_id": "c1", "name": github, "arguments": "{}"},
                {"type": "function_call", "call_id": "c2", "name": wiki, "arguments": "{}"}
            ]
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.names_shortened, 2);
        let github_short = body["tools"][0]["name"].as_str().expect("name").to_string();
        let wiki_short = body["tools"][1]["name"].as_str().expect("name").to_string();
        assert_eq!(body["tools"][2]["name"], "mcp__search");
        assert_ne!(github_short, wiki_short);
        assert_ne!(github_short, "mcp__search");
        assert_ne!(wiki_short, "mcp__search");
        assert!(github_short.chars().count() <= CODEX_TOOL_NAME_MAX_LEN);
        assert!(wiki_short.chars().count() <= CODEX_TOOL_NAME_MAX_LEN);
        assert_eq!(body["tool_choice"]["name"], wiki_short);
        assert_eq!(body["input"][0]["name"], github_short);
        assert_eq!(body["input"][1]["name"], wiki_short);

        // 响应侧从原始工具表重新推导，顺序打乱、Chat 形状也能对上。
        let context = json!({"original_request_body": {"tools": [
            {"type": "function", "function": {"name": "mcp__search"}},
            {"type": "function", "function": {"name": wiki}},
            {"type": "function", "function": {"name": github}}
        ]}});
        assert_eq!(
            restore_codex_shortened_tool_name(&context, &github_short).as_deref(),
            Some(github)
        );
        assert_eq!(
            restore_codex_shortened_tool_name(&context, &wiki_short).as_deref(),
            Some(wiki)
        );
        assert!(restore_codex_shortened_tool_name(&context, "mcp__search").is_none());

        // 没有碰撞时仍是可读的 mcp__leaf 形式。
        let map = codex_tool_name_map([github, "other_tool"]);
        assert_eq!(map.get(github).map(String::as_str), Some("mcp__search"));
        assert!(!map.contains_key("other_tool"));
    }

    #[test]
    fn normalizes_legacy_web_search_tool_types() {
        let mut body = json!({
            "tools": [{"type": "web_search_preview"}, {"type": "web_search_preview_2025_03_11"}, {"type": "web_search"}],
            "tool_choice": {"type": "web_search_preview"}
        });
        let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
        assert_eq!(report.tool_types_normalized, 3);
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tools"][1]["type"], "web_search");
        assert_eq!(body["tool_choice"]["type"], "web_search");
    }

    #[test]
    fn input_ids_get_prefixes_hashes_and_drop_unreplayable_reasoning() {
        let long_id = "a".repeat(80);
        let mut body = json!({
            "input": [
                {"type": "message", "id": "msg-1", "role": "user", "content": []},
                {"type": "message", "id": "client-1", "role": "user", "content": []},
                {"type": "reasoning", "id": "abc", "summary": []},
                {"type": "reasoning", "id": long_id, "encrypted_content": "gAAAA", "summary": []},
                {"type": "function_call", "id": "call_1", "call_id": "call_1", "name": "f", "arguments": "{}"},
                {"type": "function_call", "id": long_id, "call_id": "call_2", "name": "f", "arguments": "{}"},
                {"type": "custom_tool_call", "id": "x", "call_id": "c", "name": "shell", "input": ""},
                {"type": "custom_tool_call_output", "id": "x", "call_id": "c", "output": ""},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"},
                {"type": "message", "id": "", "role": "user", "content": []}
            ]
        });
        let report = sanitize_codex_input_item_ids(body.as_object_mut().expect("object"));
        assert_eq!(report.reasoning_items_dropped, 1);
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 10);
        assert_eq!(input[0]["id"], "msg-1");
        assert_eq!(input[1]["id"], "msg_client-1");
        // 推理项 id 不改前缀：外来 id 留给严格回放过滤处理。
        assert_eq!(input[2]["id"], "abc");
        assert_eq!(input[3]["id"], "fc_call_1");
        let hashed = input[4]["id"].as_str().expect("id");
        assert!(hashed.starts_with("fc_aaaa"));
        assert_eq!(hashed.chars().count(), CODEX_INPUT_ITEM_ID_MAX_LEN);
        assert_eq!(input[5]["id"], "ctc_x");
        assert_eq!(input[6]["id"], "ctco_x");
        assert!(input[7].get("id").is_none());
        assert_eq!(input[8]["id"], "ws_1");
        assert_eq!(input[9]["id"], "");
        assert!(report.ids_prefixed >= 4);
        assert_eq!(report.ids_shortened, 1);

        // 幂等
        let again = sanitize_codex_input_item_ids(body.as_object_mut().expect("object"));
        assert!(!again.changed());
    }

    #[test]
    fn input_id_rewrite_avoids_colliding_with_preserved_ids() {
        let mut body = json!({
            "input": [
                {"type": "message", "id": "msg_dup", "role": "user", "content": []},
                {"type": "message", "id": "dup", "role": "user", "content": []}
            ]
        });
        sanitize_codex_input_item_ids(body.as_object_mut().expect("object"));
        let first = body["input"][0]["id"].as_str().expect("id");
        let second = body["input"][1]["id"].as_str().expect("id");
        assert_eq!(first, "msg_dup");
        assert_ne!(second, first);
        assert!(second.starts_with("msg_dup"));
        assert!(second.chars().count() <= CODEX_INPUT_ITEM_ID_MAX_LEN);
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound.max(1)
        }
    }

    fn random_schema(rng: &mut Lcg, depth: usize) -> Value {
        let kind = rng.below(if depth == 0 { 3 } else { 6 });
        match kind {
            0 => {
                json!({"type": "string", "pattern": if rng.below(2) == 0 { "^\\p{L}+$" } else { "^[a-z]+$" }})
            }
            1 => json!({"type": "integer", "minimum": 0}),
            2 => {
                let branches = (0..(2 + rng.below(6)))
                    .map(|index| json!({"const": format!("v{index}"), "description": "d"}))
                    .collect::<Vec<_>>();
                json!({"oneOf": branches})
            }
            3 => json!({"type": "array", "items": random_schema(rng, depth - 1)}),
            4 => json!({"anyOf": [random_schema(rng, depth - 1), {"type": "null"}]}),
            _ => {
                let count = 1 + rng.below(4);
                let mut properties = Map::new();
                for index in 0..count {
                    properties.insert(format!("p{index}"), random_schema(rng, depth - 1));
                }
                let required = properties
                    .keys()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>();
                let mut object =
                    json!({"type": "object", "properties": properties, "$schema": "draft"});
                if rng.below(2) == 0 {
                    object["required"] = Value::Array(required);
                    object["additionalProperties"] = Value::Bool(false);
                }
                object
            }
        }
    }

    fn assert_schema_is_codex_clean(schema: &Value) {
        match schema {
            Value::Object(object) => {
                assert!(object.get("$schema").is_none());
                if let Some(pattern) = object.get("pattern").and_then(Value::as_str) {
                    assert!(!has_unsupported_unicode_property_escape(pattern));
                }
                if let Some(branches) = object.get("oneOf").and_then(Value::as_array) {
                    assert!(
                        branches
                            .iter()
                            .any(|branch| pure_const_branch(branch).is_none())
                            || branches.len() < 2
                    );
                }
                for value in object.values() {
                    assert_schema_is_codex_clean(value);
                }
            }
            Value::Array(items) => items.iter().for_each(assert_schema_is_codex_clean),
            _ => {}
        }
    }

    #[test]
    fn random_tool_definitions_are_sanitized_and_names_round_trip() {
        let mut rng = Lcg(0x0dd_ba11);
        for round in 0..1000u64 {
            let tool_count = 1 + rng.below(5) as usize;
            let mut tools = Vec::new();
            let mut originals = Vec::new();
            for index in 0..tool_count {
                let name = match rng.below(5) {
                    0 => format!(
                        "mcp__server_{round}__{}",
                        "t".repeat(40 + rng.below(60) as usize)
                    ),
                    // 碰撞构造：不同服务器、同一个 leaf，默认规则都缩成 mcp__<leaf>。
                    1 => format!(
                        "mcp__server_{round}_{index}_{}__shared_leaf",
                        "s".repeat(50 + rng.below(30) as usize)
                    ),
                    // 碰撞构造：前 47 个字符相同的超长名，只靠哈希后缀区分。
                    2 => format!(
                        "{}{}",
                        "p".repeat(60),
                        "q".repeat(1 + rng.below(30) as usize)
                    ),
                    3 => format!(
                        "tool_{round}_{index}_{}",
                        "n".repeat(rng.below(90) as usize)
                    ),
                    _ => format!("tool_{round}_{index}"),
                };
                if originals.contains(&name) {
                    // 同名工具是客户端自己的问题，不在差分范围内。
                    continue;
                }
                originals.push(name.clone());
                tools.push(json!({
                    "type": "function",
                    "name": name,
                    "strict": rng.below(2) == 0,
                    "parameters": random_schema(&mut rng, 3),
                }));
            }
            let mut body = json!({"tools": tools.clone(), "input": []});
            let report = sanitize_codex_tools_for_backend(body.as_object_mut().expect("object"));
            let sanitized = body["tools"].as_array().expect("tools");
            assert_eq!(sanitized.len(), originals.len());
            let mut seen_names = BTreeSet::new();
            for (index, tool) in sanitized.iter().enumerate() {
                let name = tool["name"].as_str().expect("name");
                assert!(
                    name.chars().count() <= CODEX_TOOL_NAME_MAX_LEN,
                    "round {round}"
                );
                // 缩短后的名字在同一请求里必须唯一：碰撞由哈希后缀递增解开。
                assert!(
                    seen_names.insert(name.to_string()),
                    "round {round}: duplicate shortened name {name}"
                );
                assert_schema_is_codex_clean(&tool["parameters"]);
                if tool["strict"] == true {
                    assert!(
                        schema_satisfies_codex_strict_mode(&tool["parameters"]),
                        "round {round}"
                    );
                }
                let context = json!({"original_request_body": {"tools": tools.clone()}});
                let restored = restore_codex_shortened_tool_name(&context, name)
                    .unwrap_or_else(|| name.to_string());
                assert_eq!(restored, originals[index], "round {round}");
            }
            let _ = report;

            // 随机历史：input item id 归一化后长度合规、前缀正确、函数调用 call_id 不动。
            let item_count = 1 + rng.below(8) as usize;
            let mut input = Vec::new();
            let mut expected_call_ids = Vec::new();
            for index in 0..item_count {
                let id_len = rng.below(120) as usize;
                let id = format!(
                    "{}{}",
                    ["", "msg", "rs", "fc", "custom"][rng.below(5) as usize],
                    "i".repeat(id_len)
                );
                match rng.below(4) {
                    0 => input.push(json!({"type": "message", "id": id, "role": "user", "content": []})),
                    1 => input.push(json!({"type": "reasoning", "id": id, "summary": [], "encrypted_content": if rng.below(2) == 0 { "gAAAA" } else { "" }})),
                    2 => {
                        let call_id = format!("call_{round}_{index}");
                        expected_call_ids.push(call_id.clone());
                        input.push(json!({"type": "function_call", "id": id, "call_id": call_id, "name": "f", "arguments": "{}"}));
                    }
                    _ => input.push(json!({"type": "function_call_output", "call_id": format!("call_{round}_{index}"), "output": "ok"})),
                }
            }
            let mut body = json!({"input": input});
            sanitize_codex_input_item_ids(body.as_object_mut().expect("object"));
            let mut call_ids = Vec::new();
            for item in body["input"].as_array().expect("input") {
                let item_type = item["type"].as_str().expect("type");
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    assert!(
                        id.chars().count() <= CODEX_INPUT_ITEM_ID_MAX_LEN,
                        "round {round}"
                    );
                    if let Some(prefix) = codex_input_item_id_prefix(item_type) {
                        assert!(
                            id.is_empty() || id.starts_with(prefix),
                            "round {round}: {id}"
                        );
                    }
                }
                if item_type == "function_call" {
                    call_ids.push(item["call_id"].as_str().expect("call id").to_string());
                }
            }
            assert_eq!(call_ids, expected_call_ids, "round {round}");
            let again = sanitize_codex_input_item_ids(body.as_object_mut().expect("object"));
            assert!(!again.changed(), "round {round}");
        }
    }
}
