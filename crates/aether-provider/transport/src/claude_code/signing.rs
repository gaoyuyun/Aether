//! Claude Code `cch=` 请求签名。
//!
//! 原生 Claude Code 在 `x-anthropic-billing-header` 系统块里携带 5 位小写十六进制的
//! `cch=xxxxx;`，值是对**最终请求体字节**做归一化后的 xxHash64 低 20 位。归一化规则
//! （与 Claude Code 2.1.220 一致，见 CLIProxyAPI `claude_signing.go`）：
//!
//! - 所有 `"model"` 键的字符串值清空（保留引号），任意层级；
//! - `"max_tokens"`、`"fallbacks"`、`"fallback_credit_token"` 成员整体删除，任意层级；
//!   一个对象末尾连续多个被删成员时保留前一个逗号（原生实现的怪癖，必须复刻）；
//! - 其它字节原样参与哈希，字段顺序有意义；
//! - 哈希前把 `cch=` 后的 5 位先置为 `00000`。
//!
//! 签名只改动那 5 个字节，签名之后请求体不得再变。

use serde_json::Value;
use xxhash_rust::xxh64::xxh64;

/// Claude Code 2.1.220 使用的 xxHash64 seed。
pub const CLAUDE_CODE_CCH_SEED: u64 = 0x4D65_9218_E32A_3268;
const CCH_LENGTH: usize = 5;
const CCH_PLACEHOLDER: &[u8; 5] = b"00000";
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeSigningError {
    message: String,
}

impl ClaudeCodeSigningError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ClaudeCodeSigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClaudeCodeSigningError {}

/// 签名结果：改写后的字节与写入的 cch 值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeSignedBody {
    pub bytes: Vec<u8>,
    pub cch: Option<String>,
}

/// 对序列化后的请求体签名。找不到 `cch=xxxxx;` 占位时原样返回（`cch = None`）。
pub fn sign_claude_code_request_bytes(
    body: &[u8],
) -> Result<ClaudeCodeSignedBody, ClaudeCodeSigningError> {
    let Some(offset) = claude_code_billing_cch_digits_offset(body) else {
        return Ok(ClaudeCodeSignedBody {
            bytes: body.to_vec(),
            cch: None,
        });
    };
    let mut unsigned = body.to_vec();
    unsigned[offset..offset + CCH_LENGTH].copy_from_slice(CCH_PLACEHOLDER);
    let normalized = normalize_claude_code_cch_input(&unsigned)?;
    let digest = xxh64(&normalized, CLAUDE_CODE_CCH_SEED);
    let cch = format!("{:05x}", digest & 0xF_FFFF);
    unsigned[offset..offset + CCH_LENGTH].copy_from_slice(cch.as_bytes());
    Ok(ClaudeCodeSignedBody {
        bytes: unsigned,
        cch: Some(cch),
    })
}

/// 序列化 JSON 请求体、签名，再解析回 `Value`。
///
/// 网关的执行运行时用同一套 `serde_json`（`preserve_order` + `float_roundtrip`）
/// 重新序列化 `Value`，因此签名字节与最终发出的字节一致；测试里用往返断言固化。
pub fn sign_claude_code_request_body(
    body: &mut Value,
) -> Result<Option<String>, ClaudeCodeSigningError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|err| ClaudeCodeSigningError::new(format!("serialize body: {err}")))?;
    let signed = sign_claude_code_request_bytes(&bytes)?;
    let Some(cch) = signed.cch else {
        return Ok(None);
    };
    let reparsed: Value = serde_json::from_slice(&signed.bytes)
        .map_err(|err| ClaudeCodeSigningError::new(format!("reparse signed body: {err}")))?;
    let roundtrip = serde_json::to_vec(&reparsed)
        .map_err(|err| ClaudeCodeSigningError::new(format!("reserialize body: {err}")))?;
    if roundtrip != signed.bytes {
        return Err(ClaudeCodeSigningError::new(
            "signed body does not survive a JSON round-trip; refusing to send a body whose bytes differ from the signed bytes",
        ));
    }
    *body = reparsed;
    Ok(Some(cch))
}

/// 在 `system[0].text` 的计费头里补 ` cch=00000;` 占位；没有计费头时用 `fallback_billing`
/// 前置一个系统文本块（`None` 则不动）。返回是否补了计费块。
pub fn ensure_claude_code_billing_cch_placeholder(
    body: &mut Value,
    fallback_billing: Option<&str>,
) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let mut prepended = false;
    if !system_first_block_is_billing_header(object.get("system")) {
        let Some(fallback) = fallback_billing
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        prepend_billing_system_block(object, fallback);
        prepended = true;
    }
    let Some(text) = object
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|system| system.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|block| block.get_mut("text"))
    else {
        return prepended;
    };
    let Some(current) = text.as_str() else {
        return prepended;
    };
    if billing_text_has_cch(current) {
        return prepended;
    }
    let Some(entrypoint) = current.find("cc_entrypoint=") else {
        return prepended;
    };
    let Some(entrypoint_end) = current[entrypoint..].find(';') else {
        return prepended;
    };
    let insert_at = entrypoint + entrypoint_end + 1;
    let updated = format!(
        "{} cch=00000;{}",
        &current[..insert_at],
        &current[insert_at..]
    );
    *text = Value::String(updated);
    prepended
}

/// 生成第三方请求缺少计费头时的兜底计费块文本。
///
/// `cc_version` 的三位构建哈希与原生一致：`sha256(salt + msg[4] + msg[7] + msg[20] + version)[:3]`。
pub fn build_claude_code_fallback_billing_header(
    cli_version: &str,
    first_user_message_text: &str,
    entrypoint: &str,
    is_subagent: bool,
) -> String {
    let entrypoint = if entrypoint.trim().is_empty() {
        "cli"
    } else {
        entrypoint.trim()
    };
    let build_hash = claude_code_build_fingerprint(first_user_message_text, cli_version);
    let mut out = format!(
        "{BILLING_HEADER_PREFIX} cc_version={cli_version}.{build_hash}; cc_entrypoint={entrypoint}; cch=00000;"
    );
    if is_subagent {
        out.push_str(" cc_is_subagent=true;");
    }
    out
}

const BUILD_FINGERPRINT_SALT: &str = "59cf53e54c78";

fn claude_code_build_fingerprint(message_text: &str, version: &str) -> String {
    use sha2::{Digest, Sha256};
    let chars = message_text.chars().collect::<Vec<_>>();
    let mut picked = String::new();
    for index in [4usize, 7, 20] {
        picked.push(chars.get(index).copied().unwrap_or('0'));
    }
    let input = format!("{BUILD_FINGERPRINT_SALT}{picked}{version}");
    let digest = Sha256::digest(input.as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    hex[..3].to_string()
}

/// 读取当前 body 里的 cch 值（用于报告与测试）。
pub fn claude_code_cch_from_body(body: &Value) -> Option<String> {
    let text = body
        .get("system")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    if !text.starts_with(BILLING_HEADER_PREFIX) {
        return None;
    }
    let mut search_from = 0;
    while let Some(relative) = text[search_from..].find("cch=") {
        let digits = search_from + relative + 4;
        let end = digits + CCH_LENGTH;
        if end < text.len()
            && text.as_bytes()[end] == b';'
            && is_lower_hex(&text.as_bytes()[digits..end])
        {
            return Some(text[digits..end].to_string());
        }
        search_from = digits;
    }
    None
}

fn billing_text_has_cch(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = text[search_from..].find("cch=") {
        let digits = search_from + relative + 4;
        let end = digits + CCH_LENGTH;
        if end < bytes.len() && bytes[end] == b';' && is_lower_hex(&bytes[digits..end]) {
            return true;
        }
        search_from = digits;
    }
    false
}

fn system_first_block_is_billing_header(system: Option<&Value>) -> bool {
    system
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| text.starts_with(BILLING_HEADER_PREFIX))
}

fn prepend_billing_system_block(object: &mut serde_json::Map<String, Value>, billing: &str) {
    let billing_block = serde_json::json!({"type": "text", "text": billing});
    let mut blocks = vec![billing_block];
    match object.remove("system") {
        Some(Value::String(text)) => {
            blocks.push(serde_json::json!({"type": "text", "text": text}));
        }
        Some(Value::Array(existing)) => blocks.extend(existing),
        _ => {}
    }
    // `system` 位于对象最前面并不是原生形状；原生是 model, messages, system... 的顺序，
    // 但这里只能保证存在性——签名对顺序敏感，因此不重排其它字段。
    object.insert("system".to_string(), Value::Array(blocks));
}

/// 在序列化字节里定位 `system[0].text` 中 `cch=` 后 5 位数字的偏移。
fn claude_code_billing_cch_digits_offset(body: &[u8]) -> Option<usize> {
    let mut scanner = CchScanner::new(body);
    scanner
        .locate_billing_text()
        .ok()
        .flatten()
        .and_then(|(start, end)| {
            // start/end 是 JSON 字符串字面量（含引号）的范围；原生按 raw 字节找 `cch=`。
            let raw = &body[start..end];
            let text_view = std::str::from_utf8(raw).ok()?;
            if !text_view
                .trim_start_matches('"')
                .starts_with(BILLING_HEADER_PREFIX)
            {
                return None;
            }
            let mut search_from = 0usize;
            while let Some(relative) = text_view[search_from..].find("cch=") {
                let prefix = search_from + relative;
                let digits = prefix + 4;
                let end_digits = digits + CCH_LENGTH;
                if end_digits < raw.len()
                    && raw[end_digits] == b';'
                    && is_lower_hex(&raw[digits..end_digits])
                {
                    return Some(start + digits);
                }
                search_from = digits;
            }
            None
        })
}

fn is_lower_hex(value: &[u8]) -> bool {
    value.len() == CCH_LENGTH
        && value
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// 构造哈希视图，不重新序列化 JSON。
pub fn normalize_claude_code_cch_input(body: &[u8]) -> Result<Vec<u8>, ClaudeCodeSigningError> {
    if serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err() {
        return Err(ClaudeCodeSigningError::new("invalid JSON body"));
    }
    let mut scanner = CchScanner::new(body);
    scanner.parse_value(true)?;
    scanner.skip_whitespace();
    if scanner.pos != body.len() {
        return Err(ClaudeCodeSigningError::new(format!(
            "unexpected JSON data at byte {}",
            scanner.pos
        )));
    }
    scanner.edits.sort_by_key(|edit| edit.start);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0usize;
    for edit in &scanner.edits {
        if edit.start < last || edit.end > body.len() {
            return Err(ClaudeCodeSigningError::new(format!(
                "overlapping CCH normalization edit at byte {}",
                edit.start
            )));
        }
        normalized.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

#[derive(Debug, Clone, Copy)]
struct Edit {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct CchScanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<Edit>,
    /// 解析时顺便记录 `system[0].text` 字符串字面量的位置（含引号）。
    billing_text_span: Option<(usize, usize)>,
    /// 当前对象路径深度标记：top-level → "system" → array index 0 → "text"。
    path: Vec<PathStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStep {
    Key(&'static str),
    OtherKey,
    Index(usize),
}

impl<'a> CchScanner<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            edits: Vec::new(),
            billing_text_span: None,
            path: Vec::new(),
        }
    }

    fn locate_billing_text(&mut self) -> Result<Option<(usize, usize)>, ClaudeCodeSigningError> {
        self.parse_value(false)?;
        Ok(self.billing_text_span)
    }

    fn parse_value(&mut self, collect: bool) -> Result<(), ClaudeCodeSigningError> {
        self.skip_whitespace();
        let Some(&byte) = self.body.get(self.pos) else {
            return Err(ClaudeCodeSigningError::new(format!(
                "missing JSON value at byte {}",
                self.pos
            )));
        };
        match byte {
            b'{' => self.parse_object(collect),
            b'[' => self.parse_array(collect),
            b'"' => {
                let (start, end) = self.parse_string()?;
                if self.path.as_slice()
                    == [
                        PathStep::Key("system"),
                        PathStep::Index(0),
                        PathStep::Key("text"),
                    ]
                {
                    self.billing_text_span = Some((start, end));
                }
                Ok(())
            }
            _ => {
                let start = self.pos;
                while let Some(&current) = self.body.get(self.pos) {
                    if matches!(current, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(ClaudeCodeSigningError::new(format!(
                        "missing JSON value at byte {start}"
                    )));
                }
                Ok(())
            }
        }
    }

    fn parse_object(&mut self, collect: bool) -> Result<(), ClaudeCodeSigningError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members: Vec<Member> = Vec::new();
        let mut comma_before: Option<usize> = None;
        loop {
            self.skip_whitespace();
            let member_start = self.pos;
            let (key_start, key_end) = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(ClaudeCodeSigningError::new(format!(
                    "missing object colon at byte {}",
                    self.pos
                )));
            }
            self.skip_whitespace();
            let key = &self.body[key_start..key_end];
            let excluded = collect && is_excluded_key(key);
            let step = match key {
                b"\"system\"" => PathStep::Key("system"),
                b"\"text\"" => PathStep::Key("text"),
                _ => PathStep::OtherKey,
            };
            self.path.push(step);
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (value_start, value_end) = self.parse_string()?;
                self.add_edit(value_start + 1, value_end - 1);
            } else {
                self.parse_value(collect && !excluded)?;
            }
            self.path.pop();
            let member_end = self.pos;
            self.skip_whitespace();
            let comma_after = if self.consume(b',') {
                Some(self.pos - 1)
            } else {
                None
            };
            members.push(Member {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });
            if let Some(comma) = comma_after {
                comma_before = Some(comma);
                continue;
            }
            if !self.consume(b'}') {
                return Err(ClaudeCodeSigningError::new(format!(
                    "missing object end at byte {}",
                    self.pos
                )));
            }
            break;
        }
        if collect {
            self.add_excluded_member_edits(&members);
        }
        Ok(())
    }

    fn parse_array(&mut self, collect: bool) -> Result<(), ClaudeCodeSigningError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        let mut index = 0usize;
        loop {
            self.path.push(PathStep::Index(index));
            let result = self.parse_value(collect);
            self.path.pop();
            result?;
            self.skip_whitespace();
            if self.consume(b',') {
                index += 1;
                continue;
            }
            if !self.consume(b']') {
                return Err(ClaudeCodeSigningError::new(format!(
                    "missing array end at byte {}",
                    self.pos
                )));
            }
            return Ok(());
        }
    }

    fn parse_string(&mut self) -> Result<(usize, usize), ClaudeCodeSigningError> {
        if self.body.get(self.pos) != Some(&b'"') {
            return Err(ClaudeCodeSigningError::new(format!(
                "missing JSON string at byte {}",
                self.pos
            )));
        }
        let start = self.pos;
        self.pos += 1;
        while let Some(&byte) = self.body.get(self.pos) {
            match byte {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(ClaudeCodeSigningError::new(format!(
            "unterminated JSON string at byte {start}"
        )))
    }

    fn add_excluded_member_edits(&mut self, members: &[Member]) {
        let mut start = 0usize;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            if end + 1 < members.len() {
                if let Some(comma_after) = members[end].comma_after {
                    self.add_edit(members[start].start, comma_after + 1);
                }
            } else if start > 0 && end > start {
                // Claude Code 2.1.220 在对象末尾连续多个被剔除成员时保留前一个逗号。
                self.add_edit(members[start].start, members[end].end);
            } else if start > 0 {
                if let Some(comma_before) = members[start].comma_before {
                    self.add_edit(comma_before, members[end].end);
                }
            } else {
                self.add_edit(members[start].start, members[end].end);
            }
            start = end + 1;
        }
    }

    fn add_edit(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.edits.push(Edit { start, end });
    }

    fn skip_whitespace(&mut self) {
        while let Some(&byte) = self.body.get(self.pos) {
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                return;
            }
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.body.get(self.pos) == Some(&expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn is_excluded_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE_BODY: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

    fn cch_of(body: &str) -> String {
        let signed = sign_claude_code_request_bytes(body.as_bytes()).expect("sign");
        let value: Value = serde_json::from_slice(&signed.bytes).expect("signed json");
        let cch = claude_code_cch_from_body(&value).expect("cch present");
        assert_eq!(signed.cch.as_deref(), Some(cch.as_str()));
        cch
    }

    /// CLIProxyAPI `claude_signing_test.go` 里 Claude Code 2.1.220 的已知向量。
    #[test]
    fn signing_matches_claude_code_2_1_220_known_vectors() {
        let cases: &[(&str, String, &str)] = &[
            ("base", BASE_BODY.to_string(), "7ee87"),
            (
                "model value ignored",
                BASE_BODY.replacen(r#""model":"model-a""#, r#""model":"model-b""#, 1),
                "7ee87",
            ),
            (
                "max tokens ignored",
                BASE_BODY.replacen(r#""max_tokens":1"#, r#""max_tokens":2"#, 1),
                "7ee87",
            ),
            (
                "message changes hash",
                BASE_BODY.replacen(r#""text":"x""#, r#""text":"y""#, 1),
                "b9cc8",
            ),
            (
                "system changes hash",
                BASE_BODY.replacen(r#""system-x""#, r#""system-y""#, 1),
                "a30d3",
            ),
            (
                "metadata changes hash",
                BASE_BODY.replacen(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#, 1),
                "7a89d",
            ),
            (
                "thinking changes hash",
                BASE_BODY.replacen(
                    r#""thinking":{"type":"adaptive","display":"omitted"}"#,
                    r#""thinking":{"type":"disabled"}"#,
                    1,
                ),
                "7205c",
            ),
            (
                "context changes hash",
                BASE_BODY.replacen(
                    r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#,
                    r#""context_management":{"edits":[]}"#,
                    1,
                ),
                "05073",
            ),
            (
                "effort changes hash",
                BASE_BODY.replacen(r#""effort":"high""#, r#""effort":"low""#, 1),
                "12366",
            ),
            (
                "stream changes hash",
                BASE_BODY.replacen(r#""stream":true"#, r#""stream":false"#, 1),
                "60400",
            ),
            (
                "tool changes hash",
                BASE_BODY.replacen(
                    r#""tools":[]"#,
                    r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#,
                    1,
                ),
                "3d78d",
            ),
            (
                "extra field changes hash",
                BASE_BODY.replacen(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#, 1),
                "2d622",
            ),
            (
                "field order remains significant",
                r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#.to_string(),
                "e5b6c",
            ),
            (
                "nested model value ignored",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","model":"a"}"#,
                    1,
                ),
                "0601b",
            ),
            (
                "nested max tokens member omitted",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":2}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                "top level fallbacks member omitted",
                BASE_BODY.replacen(
                    r#""stream":true}"#,
                    r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                "nested fallbacks member omitted",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","fallbacks":[{"model":"nested-a"}]}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                "top level fallback credit token omitted",
                BASE_BODY.replacen(
                    r#""stream":true}"#,
                    r#""stream":true,"fallback_credit_token":"a"}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                "nested fallback credit token omitted",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","fallback_credit_token":"a"}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                "trailing dispatch run keeps native comma",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#,
                    1,
                ),
                "4589b",
            ),
            (
                "model before trailing dispatch run",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#,
                    1,
                ),
                "2d312",
            ),
            (
                "model splits dispatch runs",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#,
                    1,
                ),
                "0601b",
            ),
            (
                "ordinary nested member remains",
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","plain":"a"}"#,
                    1,
                ),
                "8d74c",
            ),
            (
                "billing block only",
                r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_string(),
                "f2edb",
            ),
        ];
        for (name, body, want) in cases {
            assert_eq!(&cch_of(body), want, "case {name}");
        }
    }

    #[test]
    fn signing_changes_only_the_five_cch_bytes() {
        let literal = "keep literal cch=00000; in the message";
        let body = BASE_BODY.replacen(r#""text":"x""#, &format!(r#""text":"{literal}""#), 1);
        let signed = sign_claude_code_request_bytes(body.as_bytes()).expect("sign");
        let value: Value = serde_json::from_slice(&signed.bytes).expect("json");
        assert_eq!(value["messages"][0]["content"][0]["text"], literal);
        let offset = claude_code_billing_cch_digits_offset(&signed.bytes).expect("offset");
        let mut unsigned = signed.bytes.clone();
        unsigned[offset..offset + 5].copy_from_slice(b"00000");
        assert_eq!(unsigned, body.as_bytes());
    }

    #[test]
    fn placeholder_is_inserted_after_entrypoint_and_fallback_block_is_prepended() {
        let mut without_placeholder: Value =
            serde_json::from_str(&BASE_BODY.replacen(" cch=00000;", "", 1)).expect("json");
        assert!(!ensure_claude_code_billing_cch_placeholder(
            &mut without_placeholder,
            None
        ));
        assert_eq!(
            without_placeholder["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"
        );
        assert_eq!(
            sign_claude_code_request_body(&mut without_placeholder).expect("sign"),
            Some("7ee87".to_string())
        );

        let mut plain_system = json!({
            "model":"claude-opus-4-6",
            "system":"keep this system text",
            "messages":[{"role":"user","content":"hello"}],
            "max_tokens":128
        });
        let fallback = build_claude_code_fallback_billing_header("2.1.161", "hello", "cli", false);
        assert!(ensure_claude_code_billing_cch_placeholder(
            &mut plain_system,
            Some(&fallback)
        ));
        assert!(plain_system["system"][0]["text"]
            .as_str()
            .expect("billing")
            .starts_with("x-anthropic-billing-header: cc_version=2.1.161."));
        assert_eq!(plain_system["system"][1]["text"], "keep this system text");
        let cch = sign_claude_code_request_body(&mut plain_system)
            .expect("sign")
            .expect("cch");
        assert_eq!(cch.len(), 5);
        assert_eq!(
            claude_code_cch_from_body(&plain_system).as_deref(),
            Some(cch.as_str())
        );

        let mut no_system = json!({"model":"m","messages":[]});
        assert!(!ensure_claude_code_billing_cch_placeholder(
            &mut no_system,
            None
        ));
        assert_eq!(
            sign_claude_code_request_body(&mut no_system).expect("sign"),
            None
        );
    }

    #[test]
    fn value_signing_round_trips_serde_json_bytes() {
        let mut body: Value = serde_json::from_str(BASE_BODY).expect("json");
        body["temperature"] = json!(0.1);
        body["big"] = json!(12345678901234567890u64);
        let cch = sign_claude_code_request_body(&mut body)
            .expect("sign")
            .expect("cch");
        let bytes = serde_json::to_vec(&body).expect("serialize");
        let resigned = sign_claude_code_request_bytes(&bytes).expect("resign");
        assert_eq!(resigned.cch.as_deref(), Some(cch.as_str()));
        assert_eq!(
            resigned.bytes, bytes,
            "signature must be stable across serialization"
        );
    }

    #[test]
    fn build_fingerprint_matches_native_algorithm_shape() {
        let header =
            build_claude_code_fallback_billing_header("2.1.161", "hello world", "cli", true);
        assert!(header.starts_with("x-anthropic-billing-header: cc_version=2.1.161."));
        assert!(header.contains("; cc_entrypoint=cli; cch=00000; cc_is_subagent=true;"));
        let hash = header
            .trim_start_matches("x-anthropic-billing-header: cc_version=2.1.161.")
            .split(';')
            .next()
            .expect("hash");
        assert_eq!(hash.len(), 3);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
