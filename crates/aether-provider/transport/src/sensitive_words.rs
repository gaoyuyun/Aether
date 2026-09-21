//! 敏感词零宽混淆。
//!
//! 第三方客户端的系统提示词里常带「proxy」「API」一类代理特征词。这里在匹配词的第一个
//! Unicode 标量之后插入 U+200B（零宽空格），对模型语义无影响，但让上游做的字面匹配
//! 失效。规则见 docs/operations/provider-quality-improvement-plan.md §6.1：
//!
//! - 词表：供应商 `config.cloak.sensitive_words: string[]`，Key `auth_config.cloak_sensitive_words`
//!   覆盖；每个词至少 2 个字符，不区分大小写，按长度降序编译成一个正则。
//! - 只处理 claude_code 的 `system[].text`（跳过 `x-anthropic-billing-header:` 开头的块）与
//!   `messages[].content[].text`；antigravity 的 `request.systemInstruction.parts[].text`。
//! - 绝不处理 `tool_use.input`、`tool_result.content`、thinking 文本、`name` 字段、JSON schema。
//! - 幂等：已经含零宽字符的词不再插入。
//!
//! 执行顺序由调用方保证：混淆必须在身份改写与 CCH 签名之前。

use std::{borrow::Cow, collections::BTreeSet};

use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};
use serde_json::{json, Map, Value};

/// 插入的零宽字符。
pub const SENSITIVE_WORD_ZERO_WIDTH: char = '\u{200B}';
/// 供应商 `config.cloak` 命名空间。
pub const CLOAK_CONFIG_NAMESPACE: &str = "cloak";
/// 供应商 `config.cloak.sensitive_words`。
pub const CLOAK_SENSITIVE_WORDS_CONFIG_KEY: &str = "sensitive_words";
/// Key 级 `auth_config.cloak_sensitive_words`。
pub const CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY: &str = "cloak_sensitive_words";
/// `report_context` 里的字段名。
pub const SENSITIVE_WORDS_OBFUSCATION_REPORT_FIELD: &str = "sensitive_words_obfuscation";
/// 词表条目最短长度（Unicode 标量数）。
pub const SENSITIVE_WORD_MIN_CHARS: usize = 2;
/// 单个词的最长长度（Unicode 标量数），与管理端校验一致。
pub const SENSITIVE_WORD_MAX_CHARS: usize = 256;
/// 词表条目上限，防止一个超长词表把正则编译拖垮。
pub const SENSITIVE_WORD_MAX_ENTRIES: usize = 256;
/// 为最多 256 × 256 个 Unicode 标量的字面量及其简单大小写折叠留出编译空间。
const SENSITIVE_WORD_REGEX_SIZE_LIMIT: usize = 64 * 1024 * 1024;
/// claude_code `system[]` 里以此前缀开头的块是计费头透传，不能改写。
pub const ANTHROPIC_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// 一次混淆的结果，原样写入 `report_context.sensitive_words_obfuscation`。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SensitiveWordObfuscationReport {
    /// 是否至少改写了一处。
    pub applied: bool,
    /// 插入零宽字符的次数（按匹配计，同一字段多个匹配各算一次）。
    pub replaced: usize,
    /// 被改写的字段路径，例如 `system[0]`、`messages[3].content[0]`。
    pub fields: Vec<String>,
}

impl SensitiveWordObfuscationReport {
    pub fn to_json(&self) -> Value {
        json!({
            "applied": self.applied,
            "replaced": self.replaced,
            "fields": self.fields,
        })
    }
}

/// 归一化后的词表：去重、去空、长度过滤、小写、按长度降序。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SensitiveWordList {
    words: Vec<String>,
}

impl SensitiveWordList {
    pub fn from_words<I, S>(words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut seen = BTreeSet::new();
        let mut normalized = Vec::new();
        for word in words {
            let word = word.as_ref().trim();
            let chars = word.chars().take(SENSITIVE_WORD_MAX_CHARS + 1).count();
            if !(SENSITIVE_WORD_MIN_CHARS..=SENSITIVE_WORD_MAX_CHARS).contains(&chars) {
                continue;
            }
            if word.chars().any(|ch| ch == SENSITIVE_WORD_ZERO_WIDTH) {
                // 词表本身不该带零宽；带了就当作没写。
                continue;
            }
            let lower = normalize_sensitive_word(word);
            if seen.insert(lower.clone()) {
                normalized.push(lower);
            }
            if normalized.len() >= SENSITIVE_WORD_MAX_ENTRIES {
                break;
            }
        }
        // 长的在前，避免短词先命中把长词拆开。
        normalized.sort_by(|left, right| {
            right
                .chars()
                .count()
                .cmp(&left.chars().count())
                .then_with(|| left.cmp(right))
        });
        Self { words: normalized }
    }

    pub fn from_json_array(value: Option<&Value>) -> Self {
        let words = value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        Self::from_words(words)
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// 编译成一个不区分大小写的正则；词表为空时返回 `None`。
    pub fn compile(&self) -> Option<Regex> {
        if self.words.is_empty() {
            return None;
        }
        let alternation = self
            .words
            .iter()
            .map(|word| regex::escape(word))
            .collect::<Vec<_>>()
            .join("|");
        match RegexBuilder::new(&format!("(?i)(?:{alternation})"))
            .size_limit(SENSITIVE_WORD_REGEX_SIZE_LIMIT)
            .build()
        {
            Ok(regex) => Some(regex),
            Err(_) => {
                // 字面量已转义且输入有界；若将来编译约束变化，不能静默关闭整份词表。
                // 不把正则错误原文写日志，避免其中包含配置词表。
                tracing::warn!(
                    event_name = "sensitive_word_regex_compile_failed",
                    word_count = self.words.len(),
                    "bounded sensitive word regex failed to compile"
                );
                None
            }
        }
    }
}

/// 与 regex 的 Unicode 简单大小写折叠一致的去重键；不会把一个标量扩为多个标量。
/// 例如 Σ/σ/ς 相等，而 İ 与 i + U+0307 在正则中并不等价，必须保留为不同词。
pub fn normalize_sensitive_word(word: &str) -> String {
    word.trim()
        .chars()
        .map(|scalar| {
            if scalar.is_ascii() {
                return scalar.to_ascii_lowercase();
            }
            let mut equivalents = ClassUnicode::new([ClassUnicodeRange::new(scalar, scalar)]);
            equivalents.case_fold_simple();
            let canonical = equivalents.ranges()[0].start();
            let mut lowercase = canonical.to_lowercase();
            let first = lowercase.next().unwrap_or(canonical);
            if lowercase.next().is_none() {
                first
            } else {
                canonical
            }
        })
        .collect()
}

/// 敏感词混淆只对有作用范围定义的供应商类型生效（见 [`apply_sensitive_word_obfuscation`]）。
pub fn provider_type_supports_sensitive_words(provider_type: &str) -> bool {
    matches!(
        provider_type.trim().to_ascii_lowercase().as_str(),
        "claude_code" | "antigravity"
    )
}

/// 供应商级词表：`config.cloak.sensitive_words`。
pub fn provider_sensitive_word_list(provider_config: Option<&Value>) -> SensitiveWordList {
    SensitiveWordList::from_json_array(
        provider_config
            .and_then(Value::as_object)
            .and_then(|config| config.get(CLOAK_CONFIG_NAMESPACE))
            .and_then(Value::as_object)
            .and_then(|cloak| cloak.get(CLOAK_SENSITIVE_WORDS_CONFIG_KEY)),
    )
}

/// Key 级覆盖：`auth_config.cloak_sensitive_words` 存在（哪怕是空数组）就整体覆盖供应商词表。
pub fn key_sensitive_word_list_override(
    auth_config: Option<&Map<String, Value>>,
) -> Option<SensitiveWordList> {
    let value = auth_config?.get(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY)?;
    if value.is_null() {
        return None;
    }
    Some(SensitiveWordList::from_json_array(Some(value)))
}

/// 解密后的 auth_config 原文里的 Key 级覆盖。
pub fn key_sensitive_word_list_override_from_raw(
    raw_auth_config: Option<&str>,
) -> Option<SensitiveWordList> {
    let raw = raw_auth_config
        .map(str::trim)
        .filter(|raw| !raw.is_empty())?;
    let object = serde_json::from_str::<Value>(raw)
        .ok()?
        .as_object()
        .cloned()?;
    key_sensitive_word_list_override(Some(&object))
}

/// 供应商词表与 Key 级覆盖合并后的有效词表。
pub fn resolve_sensitive_word_list(
    provider_config: Option<&Value>,
    raw_auth_config: Option<&str>,
) -> SensitiveWordList {
    key_sensitive_word_list_override_from_raw(raw_auth_config)
        .unwrap_or_else(|| provider_sensitive_word_list(provider_config))
}

/// 对一段文本做混淆；返回插入次数。匹配时忽略已有的 U+200B，再映射回原文字节范围。
/// 已含零宽的完整匹配原样保留，避免重放时继续命中已混淆长词内部的子词。
pub fn obfuscate_text(text: &str, regex: &Regex) -> (String, usize) {
    let has_zero_width = text.contains(SENSITIVE_WORD_ZERO_WIDTH);
    let matching_text = if has_zero_width {
        Cow::Owned(strip_zero_width(text))
    } else {
        Cow::Borrowed(text)
    };
    let mut source_chars = text
        .char_indices()
        .filter(|(_, scalar)| *scalar != SENSITIVE_WORD_ZERO_WIDTH);
    let mut matching_offset = 0;
    let mut output = String::with_capacity(text.len() + 8);
    let mut last = 0;
    let mut replaced = 0;
    for matched in regex.find_iter(&matching_text) {
        if matched.is_empty() {
            continue;
        }
        let (mut start, mut end) = (matched.start(), matched.end());
        if has_zero_width {
            // 匹配按顺序不重叠；原文只扫描一遍，避免建立与正文大小成正比的偏移表。
            for (source_offset, scalar) in source_chars.by_ref() {
                if matching_offset == matched.start() {
                    start = source_offset;
                }
                matching_offset += scalar.len_utf8();
                if matching_offset == matched.end() {
                    end = source_offset + scalar.len_utf8();
                    break;
                }
            }
        }
        output.push_str(&text[last..start]);
        let word = &text[start..end];
        if word.contains(SENSITIVE_WORD_ZERO_WIDTH) {
            output.push_str(word);
        } else if let Some(first) = word.chars().next() {
            output.push(first);
            output.push(SENSITIVE_WORD_ZERO_WIDTH);
            output.push_str(&word[first.len_utf8()..]);
            replaced += 1;
        }
        last = end;
    }
    output.push_str(&text[last..]);
    (output, replaced)
}

/// 去掉文本里的零宽字符（前端「去除零宽字符后复制」与服务端搜索都用它）。
pub fn strip_zero_width(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch != SENSITIVE_WORD_ZERO_WIDTH)
        .collect()
}

/// 对最终请求体做混淆。`provider_type` 决定作用范围；不认识的供应商类型什么都不做。
///
/// 调用方必须在身份改写与 CCH 签名之前调用，并把返回值写进
/// `report_context.sensitive_words_obfuscation`。
pub fn apply_sensitive_word_obfuscation(
    body: &mut Value,
    provider_type: &str,
    word_list: &SensitiveWordList,
) -> SensitiveWordObfuscationReport {
    let mut report = SensitiveWordObfuscationReport::default();
    let Some(regex) = word_list.compile() else {
        return report;
    };
    match provider_type.trim().to_ascii_lowercase().as_str() {
        "claude_code" => obfuscate_claude_code_body(body, &regex, &mut report),
        "antigravity" => obfuscate_antigravity_body(body, &regex, &mut report),
        _ => {}
    }
    report.applied = report.replaced > 0;
    report
}

fn obfuscate_string_field(
    container: &mut Map<String, Value>,
    key: &str,
    path: &str,
    regex: &Regex,
    report: &mut SensitiveWordObfuscationReport,
) {
    let Some(Value::String(text)) = container.get(key) else {
        return;
    };
    let (next, replaced) = obfuscate_text(text, regex);
    if replaced == 0 {
        return;
    }
    container.insert(key.to_string(), Value::String(next));
    report.replaced += replaced;
    report.fields.push(path.to_string());
}

fn obfuscate_claude_code_body(
    body: &mut Value,
    regex: &Regex,
    report: &mut SensitiveWordObfuscationReport,
) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    match root.get_mut("system") {
        Some(Value::String(text)) => {
            if !text.starts_with(ANTHROPIC_BILLING_HEADER_PREFIX) {
                let (next, replaced) = obfuscate_text(text, regex);
                if replaced > 0 {
                    *text = next;
                    report.replaced += replaced;
                    report.fields.push("system".to_string());
                }
            }
        }
        Some(Value::Array(blocks)) => {
            for (index, block) in blocks.iter_mut().enumerate() {
                let Some(block) = block.as_object_mut() else {
                    continue;
                };
                if !block_is_text(block) {
                    continue;
                }
                if block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(ANTHROPIC_BILLING_HEADER_PREFIX))
                {
                    continue;
                }
                obfuscate_string_field(block, "text", &format!("system[{index}]"), regex, report);
            }
        }
        _ => {}
    }
    let Some(Value::Array(messages)) = root.get_mut("messages") else {
        return;
    };
    for (message_index, message) in messages.iter_mut().enumerate() {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        match message.get_mut("content") {
            Some(Value::String(text)) => {
                let (next, replaced) = obfuscate_text(text, regex);
                if replaced > 0 {
                    *text = next;
                    report.replaced += replaced;
                    report
                        .fields
                        .push(format!("messages[{message_index}].content"));
                }
            }
            Some(Value::Array(blocks)) => {
                for (block_index, block) in blocks.iter_mut().enumerate() {
                    let Some(block) = block.as_object_mut() else {
                        continue;
                    };
                    // 只碰 text 块：tool_use.input / tool_result.content / thinking 都跳过。
                    if !block_is_text(block) {
                        continue;
                    }
                    obfuscate_string_field(
                        block,
                        "text",
                        &format!("messages[{message_index}].content[{block_index}]"),
                        regex,
                        report,
                    );
                }
            }
            _ => {}
        }
    }
}

fn block_is_text(block: &Map<String, Value>) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind.eq_ignore_ascii_case("text"))
        && block.get("text").is_some_and(Value::is_string)
}

fn obfuscate_antigravity_body(
    body: &mut Value,
    regex: &Regex,
    report: &mut SensitiveWordObfuscationReport,
) {
    let Some(parts) = body
        .as_object_mut()
        .and_then(|root| root.get_mut("request"))
        .and_then(Value::as_object_mut)
        .and_then(|request| request.get_mut("systemInstruction"))
        .and_then(Value::as_object_mut)
        .and_then(|instruction| instruction.get_mut("parts"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for (index, part) in parts.iter_mut().enumerate() {
        let Some(part) = part.as_object_mut() else {
            continue;
        };
        obfuscate_string_field(
            part,
            "text",
            &format!("request.systemInstruction.parts[{index}]"),
            regex,
            report,
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        apply_sensitive_word_obfuscation, key_sensitive_word_list_override_from_raw,
        obfuscate_text, provider_sensitive_word_list, resolve_sensitive_word_list,
        strip_zero_width, SensitiveWordList, SENSITIVE_WORD_MAX_CHARS, SENSITIVE_WORD_MAX_ENTRIES,
        SENSITIVE_WORD_ZERO_WIDTH,
    };

    fn words(list: &[&str]) -> SensitiveWordList {
        SensitiveWordList::from_words(list.iter().copied())
    }

    #[test]
    fn word_list_normalizes_dedupes_and_sorts_longest_first() {
        let list = words(&["API", " proxy ", "a", "Proxy", "openai-proxy", ""]);
        assert_eq!(list.words(), ["openai-proxy", "proxy", "api"]);
        assert!(words(&["x", ""]).is_empty());
        assert!(words(&["p\u{200B}roxy"]).is_empty());
    }

    #[test]
    fn obfuscation_inserts_zero_width_after_first_scalar_case_insensitively() {
        let regex = words(&["proxy", "API"]).compile().expect("regex");
        let (text, replaced) = obfuscate_text("This proxy calls the api and the PROXY.", &regex);
        assert_eq!(replaced, 3);
        assert_eq!(
            text,
            "This p\u{200B}roxy calls the a\u{200B}pi and the P\u{200B}ROXY."
        );

        let (text, replaced) =
            obfuscate_text("代理服务器 proxy", &words(&["代理"]).compile().unwrap());
        assert_eq!(replaced, 1);
        assert_eq!(text, "代\u{200B}理服务器 proxy");
    }

    #[test]
    fn obfuscation_is_idempotent() {
        let regex = words(&["proxy", "API"]).compile().expect("regex");
        let (once, first) = obfuscate_text("proxy API", &regex);
        let (twice, second) = obfuscate_text(&once, &regex);
        assert_eq!(first, 2);
        assert_eq!(second, 0);
        assert_eq!(once, twice);
        assert_eq!(strip_zero_width(&once), "proxy API");
    }

    #[test]
    fn unicode_deduplication_preserves_the_regex_case_folding_semantics() {
        let original = [
            "İx",
            "i\u{0307}x",
            "Σx",
            "ςx",
            "σX",
            "ſx",
            "SX",
            "Kx",
            "kx",
            "ẞx",
            "ßX",
            "ıx",
            "Ix",
        ];
        let list = words(&original);
        assert_eq!(list.len(), 8);
        let regex = list.compile().expect("Unicode literals compile");
        for input in original {
            let first = input.chars().next().unwrap();
            let expected = format!("{first}\u{200B}{}", &input[first.len_utf8()..]);
            assert_eq!(obfuscate_text(input, &regex), (expected, 1), "{input}");
        }
        let dotted_i = words(&["İx"]).compile().unwrap();
        assert_eq!(obfuscate_text("İx", &dotted_i).1, 1);
        assert_eq!(obfuscate_text("i\u{0307}x ix ıx", &dotted_i).1, 0);
    }

    #[test]
    fn regex_metacharacters_are_matched_literally() {
        let literals = [
            "a.b", "a+b", "[api]", "(api)", "a|b", "a\\b", "a$b", "a^b", "api?", "a{2}",
        ];
        let regex = words(&literals).compile().unwrap();
        let input = literals.join("; ");
        let (output, count) = obfuscate_text(&input, &regex);
        assert_eq!(count, literals.len());
        assert_eq!(strip_zero_width(&output), input);
        assert_eq!(obfuscate_text(&output, &regex), (output, 0));
        assert_eq!(obfuscate_text("aXb aab api", &regex).1, 0);
    }

    #[test]
    fn replay_does_not_obfuscate_subwords_inside_an_already_obfuscated_match() {
        let regex = words(&["proxy server", "proxy", "server", "roxy", "xy"])
            .compile()
            .unwrap();
        for (input, expected_count) in [
            ("proxy server; proxy; server", 3),
            ("p\u{200B}roxy server; server", 1),
            ("pro\u{200B}\u{200B}xy server; proxy", 1),
            (
                "\u{200B}proxy server\u{200B}; p\u{200B}roxy; server\u{200B}",
                2,
            ),
        ] {
            let (once, first) = obfuscate_text(input, &regex);
            assert_eq!(first, expected_count, "{input}");
            assert_eq!(strip_zero_width(&once), strip_zero_width(input));
            assert_eq!(obfuscate_text(&once, &regex), (once, 0), "{input}");
        }

        let regex = words(&["代理服务器", "服务器", "🙂代理", "代理"])
            .compile()
            .unwrap();
        let (once, first) = obfuscate_text("代\u{200B}理服务器 🙂代\u{200B}理 代理", &regex);
        assert_eq!(first, 1);
        assert_eq!(once, "代\u{200B}理服务器 🙂代\u{200B}理 代\u{200B}理");
        assert_eq!(obfuscate_text(&once, &regex), (once, 0));
        assert_eq!(
            obfuscate_text("🙂代理", &regex),
            ("🙂\u{200B}代理".to_string(), 1)
        );
    }

    #[test]
    fn word_lengths_are_bounded_without_disabling_valid_words() {
        let longest = "🙂".repeat(SENSITIVE_WORD_MAX_CHARS);
        let too_long = "🙂".repeat(SENSITIVE_WORD_MAX_CHARS + 1);
        let list = words(&[&longest, &too_long, "proxy"]);
        assert_eq!(list.len(), 2);
        let regex = list.compile().expect("bounded literals compile");
        assert_eq!(obfuscate_text("proxy", &regex).1, 1);
        assert_eq!(obfuscate_text(&longest, &regex).1, 1);
    }

    #[test]
    fn largest_accepted_unicode_word_list_compiles() {
        let suffix: String = ['Σ', 'ſ', 'K', 'ß', 'ΐ', 'İ', '🙂']
            .into_iter()
            .cycle()
            .take(SENSITIVE_WORD_MAX_CHARS - 3)
            .collect();
        let entries = (0..SENSITIVE_WORD_MAX_ENTRIES)
            .map(|index| format!("{index:03}{suffix}"))
            .collect::<Vec<_>>();
        let list = SensitiveWordList::from_words(&entries);
        assert_eq!(list.len(), SENSITIVE_WORD_MAX_ENTRIES);
        let regex = list
            .compile()
            .expect("the full accepted word list must compile");
        assert_eq!(
            obfuscate_text(&entries[SENSITIVE_WORD_MAX_ENTRIES - 1], &regex).1,
            1
        );
    }

    #[test]
    fn longest_word_wins_over_its_prefix() {
        let regex = words(&["proxy", "proxy server"]).compile().expect("regex");
        let (text, replaced) = obfuscate_text("proxy server", &regex);
        assert_eq!(replaced, 1);
        assert_eq!(text, "p\u{200B}roxy server");
    }

    #[test]
    fn claude_code_body_only_touches_system_and_text_blocks() {
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: proxy=1"},
                {"type": "text", "text": "You are a proxy for the API."}
            ],
            "messages": [
                {"role": "user", "content": "use the proxy"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "proxy thinking", "signature": "sig"},
                    {"type": "text", "text": "calling API"},
                    {"type": "tool_use", "id": "toolu_1", "name": "proxy_tool", "input": {"url": "proxy"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "proxy result"}
                ]}
            ],
            "tools": [{"name": "proxy_tool", "description": "proxy", "input_schema": {"type": "object", "properties": {"proxy": {"type": "string"}}}}]
        });
        let report =
            apply_sensitive_word_obfuscation(&mut body, "claude_code", &words(&["proxy", "API"]));
        assert!(report.applied);
        assert_eq!(report.replaced, 4);
        assert_eq!(
            report.fields,
            vec!["system[1]", "messages[0].content", "messages[1].content[1]",]
        );
        assert_eq!(
            body["system"][0]["text"],
            "x-anthropic-billing-header: proxy=1"
        );
        assert_eq!(
            body["system"][1]["text"],
            "You are a p\u{200B}roxy for the A\u{200B}PI."
        );
        assert_eq!(body["messages"][0]["content"], "use the p\u{200B}roxy");
        assert_eq!(
            body["messages"][1]["content"][0]["thinking"],
            "proxy thinking"
        );
        assert_eq!(
            body["messages"][1]["content"][1]["text"],
            "calling A\u{200B}PI"
        );
        assert_eq!(body["messages"][1]["content"][2]["name"], "proxy_tool");
        assert_eq!(body["messages"][1]["content"][2]["input"]["url"], "proxy");
        assert_eq!(body["messages"][2]["content"][0]["content"], "proxy result");
        assert_eq!(body["tools"][0]["description"], "proxy");
        assert_eq!(body["model"], "claude-sonnet-4-5");

        let snapshot = body.clone();
        let again =
            apply_sensitive_word_obfuscation(&mut body, "claude_code", &words(&["proxy", "API"]));
        assert!(!again.applied);
        assert_eq!(again.replaced, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn antigravity_body_only_touches_system_instruction_parts() {
        let mut body = json!({
            "project": "p",
            "request": {
                "systemInstruction": {"parts": [{"text": "proxy instructions"}, {"text": "clean"}]},
                "contents": [{"role": "user", "parts": [{"text": "proxy question"}]}],
                "tools": [{"functionDeclarations": [{"name": "proxy", "description": "proxy"}]}]
            }
        });
        let report = apply_sensitive_word_obfuscation(&mut body, "antigravity", &words(&["proxy"]));
        assert_eq!(report.replaced, 1);
        assert_eq!(report.fields, vec!["request.systemInstruction.parts[0]"]);
        assert_eq!(
            body["request"]["systemInstruction"]["parts"][0]["text"],
            "p\u{200B}roxy instructions"
        );
        assert_eq!(
            body["request"]["contents"][0]["parts"][0]["text"],
            "proxy question"
        );
        assert_eq!(
            body["request"]["tools"][0]["functionDeclarations"][0]["name"],
            "proxy"
        );
    }

    #[test]
    fn unknown_provider_types_and_empty_word_lists_are_no_ops() {
        let mut body = json!({"messages": [{"role": "user", "content": "proxy"}]});
        let snapshot = body.clone();
        assert!(!apply_sensitive_word_obfuscation(&mut body, "codex", &words(&["proxy"])).applied);
        assert!(!apply_sensitive_word_obfuscation(&mut body, "claude_code", &words(&[])).applied);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn word_lists_come_from_provider_config_and_key_override() {
        let provider = json!({"cloak": {"sensitive_words": ["proxy", "API"]}});
        assert_eq!(
            provider_sensitive_word_list(Some(&provider)).words(),
            ["proxy", "api"]
        );
        assert!(provider_sensitive_word_list(Some(&json!({}))).is_empty());

        let override_list = key_sensitive_word_list_override_from_raw(Some(
            r#"{"refresh_token":"rt","cloak_sensitive_words":["gateway"]}"#,
        ))
        .expect("override");
        assert_eq!(override_list.words(), ["gateway"]);
        assert!(
            key_sensitive_word_list_override_from_raw(Some(r#"{"refresh_token":"rt"}"#)).is_none()
        );
        assert!(key_sensitive_word_list_override_from_raw(Some(
            r#"{"cloak_sensitive_words":null}"#
        ))
        .is_none());

        // 空数组也是覆盖：Key 显式关闭混淆。
        let cleared =
            resolve_sensitive_word_list(Some(&provider), Some(r#"{"cloak_sensitive_words":[]}"#));
        assert!(cleared.is_empty());
        let inherited =
            resolve_sensitive_word_list(Some(&provider), Some(r#"{"refresh_token":"rt"}"#));
        assert_eq!(inherited.words(), ["proxy", "api"]);
    }

    #[test]
    fn report_json_shape_is_stable() {
        let mut body = json!({"system": "proxy"});
        let report = apply_sensitive_word_obfuscation(&mut body, "claude_code", &words(&["proxy"]));
        assert_eq!(
            report.to_json(),
            json!({"applied": true, "replaced": 1, "fields": ["system"]})
        );
        assert_eq!(body["system"], format!("p{SENSITIVE_WORD_ZERO_WIDTH}roxy"));
    }
}
