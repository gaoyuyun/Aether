//! 推理回放账本。
//!
//! 跨格式多轮 tool-use 时，客户端拿到的是转换后的历史，上游要求随工具调用一起
//! 回传的推理签名（Codex 的 `reasoning.encrypted_content`、Gemini / Antigravity 的
//! `thoughtSignature`）会在往返里丢失，后续轮次被上游 400 拒绝。这里按
//! `(provider_key_id, session_scope, model)` 记住最近一轮成功响应里的签名，
//! 下一轮请求按 `call_id` 锚定插回。
//!
//! 设计约束：
//! - 只存签名与锚点，不存文本；单条目上限见 [`REASONING_REPLAY_MAX_ENTRIES`]，
//!   TTL 见 [`REASONING_REPLAY_TTL`]。
//! - 回放是幂等的：请求里已经带签名的位置不动，只补缺。
//! - Codex 的 `encrypted_content` 先过 Fernet 外形校验，形状不对的不入账。
//! - 上游 400 且错误文本涉及签名时，调用方用 [`ReasoningReplayLedger::clear_if_generation`]
//!   做 CAS 清理，避免误删并发请求刚写入的新一轮。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::ExpiringMap;

/// 账本条目存活时间。
pub const REASONING_REPLAY_TTL: Duration = Duration::from_secs(60 * 60);
/// 账本容量上限（条目数）。
pub const REASONING_REPLAY_MAX_ENTRIES: usize = 10_240;
/// 单个条目累积的最多签名数；再多就丢最旧的轮次。
pub const REASONING_REPLAY_MAX_ITEMS_PER_ENTRY: usize = 256;
/// Codex 推理签名允许的最大长度（字节）。
pub const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
/// Gemini 在首个工具调用上允许的占位签名；回放时视为「缺签名」。
pub const GEMINI_SKIP_THOUGHT_SIGNATURE_PLACEHOLDER: &str = "skip_thought_signature_validator";

const STORAGE_KEY_PREFIX: &str = "reasoning_replay";

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// 每个进程一个随机种子：代次只在 CAS 清理时比较相等，多实例共享 Redis 账本时
/// 各自从 1 计数会碰撞（A 实例第 7 代 == B 实例第 7 代），把别的实例刚写的新一轮
/// 当成自己那一代清掉。
fn generation_seed() -> u64 {
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SEED.get_or_init(|| {
        getrandom::u64().unwrap_or_else(|_| {
            // CSPRNG 不可用属于环境故障；账本代次不是安全边界，用时间退化即可。
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0x9e37_79b9_7f4a_7c15)
        }) | 1
    })
}

fn next_generation() -> u64 {
    let counter = GENERATION.fetch_add(1, Ordering::Relaxed);
    // 种子与计数做乘法混合而不是简单异或：异或只会让低位随计数变化，
    // 两个实例种子相近时仍可能在小计数上撞车。
    counter
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(generation_seed())
        .rotate_left(17)
        ^ generation_seed()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayProvider {
    /// OpenAI Responses 形状（Codex）：`reasoning.encrypted_content` + `function_call.call_id`。
    Codex,
    /// Gemini 形状（Gemini CLI / Antigravity / Vertex）：`functionCall` 上的 `thoughtSignature`。
    Gemini,
}

impl ReasoningReplayProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

/// 账本键：同一把上游凭据、同一个客户端会话、同一个模型。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReasoningReplayKey {
    pub provider: ReasoningReplayProvider,
    pub provider_key_id: String,
    pub session_scope: String,
    pub model: String,
}

impl ReasoningReplayKey {
    pub fn new(
        provider: ReasoningReplayProvider,
        provider_key_id: impl Into<String>,
        session_scope: impl Into<String>,
        model: impl Into<String>,
    ) -> Option<Self> {
        let provider_key_id = provider_key_id.into().trim().to_string();
        let session_scope = session_scope.into().trim().to_string();
        let model = model.into().trim().to_string();
        if provider_key_id.is_empty() || session_scope.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider,
            provider_key_id,
            session_scope,
            model,
        })
    }

    /// 存储键。会话 scope 与模型名经过哈希，避免把客户端会话标识原样写进 Redis 键名。
    pub fn storage_key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.session_scope.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.model.as_bytes());
        let digest = hasher.finalize();
        let digest_hex = digest
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!(
            "{STORAGE_KEY_PREFIX}:{}:{}:{digest_hex}",
            self.provider.as_str(),
            self.provider_key_id
        )
    }

    /// 某把凭据下所有条目的存储键前缀（管理端按 Key 清缓存用）。
    pub fn storage_key_prefix_for_provider_key(provider_key_id: &str) -> Vec<String> {
        let provider_key_id = provider_key_id.trim();
        [
            ReasoningReplayProvider::Codex,
            ReasoningReplayProvider::Gemini,
        ]
        .iter()
        .map(|provider| {
            format!(
                "{STORAGE_KEY_PREFIX}:{}:{provider_key_id}:",
                provider.as_str()
            )
        })
        .collect()
    }
}

/// 一条可回放的签名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReasoningReplayItem {
    /// Codex 推理项，锚定在 `call_id` 对应的工具调用之前。
    CodexReasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        encrypted_content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<Value>,
        call_id: String,
    },
    /// Codex 工具调用项的原生 `id`，用于把跨格式往返后丢掉的 item id 补回去。
    CodexFunctionCall {
        item_type: String,
        id: String,
        call_id: String,
    },
    /// Gemini 工具调用上的 `thoughtSignature`。Gemini 响应里的 `functionCall` 通常没有
    /// `id`，这时锚点是 `name` + `args` 规范化 JSON 的摘要（[`gemini_function_call_args_digest`]）；
    /// 两者都缺时只在账本里该 `name` 唯一时才按名回放，绝不按名 FIFO 跨调用猜。
    GeminiThoughtSignature {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        signature: String,
        /// `sha256(canonical_json(functionCall.args))` 前 16 位 hex；旧条目没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_digest: Option<String>,
    },
}

fn replay_item_identity(item: &ReasoningReplayItem) -> (u8, String, String) {
    match item {
        ReasoningReplayItem::CodexReasoning { call_id, .. } => (0, call_id.clone(), String::new()),
        ReasoningReplayItem::CodexFunctionCall { call_id, .. } => {
            (1, call_id.clone(), String::new())
        }
        ReasoningReplayItem::GeminiThoughtSignature {
            call_id: Some(call_id),
            ..
        } => (2, call_id.clone(), String::new()),
        ReasoningReplayItem::GeminiThoughtSignature {
            call_id: None,
            name,
            signature,
            ..
        } => (3, name.clone(), signature.clone()),
    }
}

/// 摘要长度（hex 字符数）。
const GEMINI_ARGS_DIGEST_HEX_LEN: usize = 16;

fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            // 跨格式往返可能把 1.0 写成 1：整数值的浮点按整数写，摘要才稳定。
            if let Some(integer) = number.as_i64() {
                out.push_str(&integer.to_string());
            } else if let Some(integer) = number.as_u64() {
                out.push_str(&integer.to_string());
            } else if let Some(float) = number.as_f64() {
                if float.is_finite()
                    && float.fract() == 0.0
                    && float.abs() < 9_007_199_254_740_992.0
                {
                    out.push_str(&(float as i64).to_string());
                } else {
                    out.push_str(&number.to_string());
                }
            } else {
                out.push_str(&number.to_string());
            }
        }
        Value::String(text) => {
            out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string()))
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()));
                out.push(':');
                write_canonical_json(&object[key], out);
            }
            out.push('}');
        }
    }
}

/// Gemini `functionCall.args` 的规范化摘要：键排序、整数值浮点按整数写，
/// 取 sha256 前 16 位 hex。`args` 缺失时返回 `None`。
pub fn gemini_function_call_args_digest(function_call: &Map<String, Value>) -> Option<String> {
    let args = function_call.get("args")?;
    let mut canonical = String::new();
    write_canonical_json(args, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    Some(
        digest
            .iter()
            .take(GEMINI_ARGS_DIGEST_HEX_LEN / 2)
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

impl ReasoningReplayItem {
    fn anchor_call_id(&self) -> Option<&str> {
        match self {
            Self::CodexReasoning { call_id, .. } | Self::CodexFunctionCall { call_id, .. } => {
                Some(call_id.as_str())
            }
            Self::GeminiThoughtSignature { call_id, .. } => call_id.as_deref(),
        }
    }
}

/// 一轮响应捕获到的全部签名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningReplayEntry {
    pub provider: ReasoningReplayProvider,
    pub items: Vec<ReasoningReplayItem>,
    /// 写入代次，CAS 清理用。进程随机种子与计数混合而成，只比较相等；
    /// 多实例共享 Redis 时不会因为各自从 1 计数而撞车。
    pub generation: u64,
    pub captured_at_unix_secs: u64,
}

impl ReasoningReplayEntry {
    pub fn new(
        provider: ReasoningReplayProvider,
        items: Vec<ReasoningReplayItem>,
        captured_at_unix_secs: u64,
    ) -> Self {
        Self {
            provider,
            items,
            generation: next_generation(),
            captured_at_unix_secs,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 把新一轮捕获的条目并入旧条目：旧轮次在前，新轮次在后；同一锚点以新为准；
    /// 超过 [`REASONING_REPLAY_MAX_ITEMS_PER_ENTRY`] 时丢最旧的。多轮 tool-use 里
    /// 第三轮请求会同时带第一、二轮的工具调用，只存最近一轮会让更早的锚点失配。
    pub fn merged_with(previous: Option<&ReasoningReplayEntry>, mut latest: Self) -> Self {
        let Some(previous) = previous.filter(|previous| previous.provider == latest.provider)
        else {
            return latest;
        };
        let latest_anchors = latest
            .items
            .iter()
            .map(replay_item_identity)
            .collect::<std::collections::BTreeSet<_>>();
        let mut merged = previous
            .items
            .iter()
            .filter(|item| !latest_anchors.contains(&replay_item_identity(item)))
            .cloned()
            .collect::<Vec<_>>();
        merged.append(&mut latest.items);
        if merged.len() > REASONING_REPLAY_MAX_ITEMS_PER_ENTRY {
            let overflow = merged.len() - REASONING_REPLAY_MAX_ITEMS_PER_ENTRY;
            merged.drain(..overflow);
        }
        latest.items = merged;
        latest
    }

    pub fn call_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for item in &self.items {
            if let Some(call_id) = item.anchor_call_id() {
                if !ids.iter().any(|existing| existing == call_id) {
                    ids.push(call_id.to_string());
                }
            }
        }
        ids
    }
}

/// 回放结果摘要，落到 `report_context` 供请求详情展示。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningReplayApplied {
    /// 插回的推理项 / 签名数。
    pub inserted: usize,
    /// 补回的工具调用 item id 数。
    pub restored_ids: usize,
    /// 命中的锚点 `call_id`。
    pub anchors: Vec<String>,
    /// 请求里已带签名、被跳过的锚点。
    pub skipped: Vec<String>,
    /// 缓存里有但请求里找不到锚点的 `call_id`。
    pub unmatched: Vec<String>,
}

impl ReasoningReplayApplied {
    pub fn changed(&self) -> bool {
        self.inserted > 0 || self.restored_ids > 0
    }
}

/// 进程内账本。Redis 部署下调用方把 [`ReasoningReplayEntry`] 序列化后走运行时 KV，
/// 本地账本只作一级缓存。
#[derive(Debug, Default)]
pub struct ReasoningReplayLedger {
    entries: ExpiringMap<String, ReasoningReplayEntry>,
}

impl ReasoningReplayLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &ReasoningReplayKey) -> Option<ReasoningReplayEntry> {
        self.get_by_storage_key(&key.storage_key())
    }

    pub fn get_by_storage_key(&self, storage_key: &str) -> Option<ReasoningReplayEntry> {
        self.entries
            .get_fresh(storage_key, REASONING_REPLAY_TTL)
            .filter(|entry| !entry.is_empty())
    }

    pub fn put(&self, key: &ReasoningReplayKey, entry: ReasoningReplayEntry) {
        self.put_by_storage_key(key.storage_key(), entry);
    }

    pub fn put_by_storage_key(&self, storage_key: String, entry: ReasoningReplayEntry) {
        if entry.is_empty() {
            self.entries.remove(&storage_key);
            return;
        }
        self.entries.insert(
            storage_key,
            entry,
            REASONING_REPLAY_TTL,
            REASONING_REPLAY_MAX_ENTRIES,
        );
    }

    pub fn clear(&self, key: &ReasoningReplayKey) -> bool {
        self.entries.remove(&key.storage_key()).is_some()
    }

    /// 只在当前条目仍是 `generation` 那一代时删除；并发请求刚写入的新一轮不受影响。
    pub fn clear_if_generation(&self, key: &ReasoningReplayKey, generation: u64) -> bool {
        let storage_key = key.storage_key();
        match self.entries.get_fresh(&storage_key, REASONING_REPLAY_TTL) {
            Some(current) if current.generation == generation => {
                self.entries.remove(&storage_key).is_some()
            }
            _ => false,
        }
    }

    /// 清掉某把凭据下的全部条目，返回删除数。
    pub fn clear_provider_key(&self, provider_key_id: &str) -> usize {
        let prefixes = ReasoningReplayKey::storage_key_prefix_for_provider_key(provider_key_id);
        let keys = self
            .entries
            .snapshot_fresh(REASONING_REPLAY_TTL)
            .into_iter()
            .map(|entry| entry.key)
            .filter(|key| prefixes.iter().any(|prefix| key.starts_with(prefix)))
            .collect::<Vec<_>>();
        keys.iter()
            .filter(|key| self.entries.remove(key.as_str()).is_some())
            .count()
    }

    /// 清空整个账本（测试与管理端全局重置用）。
    pub fn clear_all(&self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Fernet 外形校验
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GptReasoningSignatureInfo {
    pub decoded_len: usize,
    pub ciphertext_len: usize,
}

pub fn is_valid_gpt_reasoning_signature(raw: &str) -> bool {
    inspect_gpt_reasoning_signature(raw).is_ok()
}

/// 校验 Codex `encrypted_content` 的 Fernet 外层形状：`gAAAA` 前缀、base64url 字符集、
/// 版本字节 0x80、`1 + 8 + 16 + N*16 + 32` 的长度结构。只证明形状，不证明可解密。
pub fn inspect_gpt_reasoning_signature(raw: &str) -> Result<GptReasoningSignatureInfo, String> {
    let signature = raw.trim();
    if signature.is_empty() {
        return Err("empty GPT reasoning signature".to_string());
    }
    if signature.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return Err(format!(
            "GPT reasoning signature exceeds maximum length ({MAX_GPT_REASONING_SIGNATURE_LEN} bytes)"
        ));
    }
    if !signature.starts_with("gAAAA") {
        return Err("invalid GPT reasoning signature: expected gAAAA prefix".to_string());
    }
    if let Some((index, byte)) = signature
        .bytes()
        .enumerate()
        .find(|(_, byte)| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'=')))
    {
        return Err(format!(
            "invalid GPT reasoning signature: contains non-base64url byte 0x{byte:02x} at {index}"
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(signature.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(signature))
        .map_err(|_| "invalid GPT reasoning signature: base64url decode failed".to_string())?;
    if decoded.len() < 73 {
        return Err("invalid GPT reasoning signature: decoded payload too short".to_string());
    }
    if decoded[0] != 0x80 {
        return Err(format!(
            "invalid GPT reasoning signature: expected version 0x80, got 0x{:02x}",
            decoded[0]
        ));
    }
    let ciphertext_len = decoded.len() - 1 - 8 - 16 - 32;
    if ciphertext_len == 0 || ciphertext_len % 16 != 0 {
        return Err(format!(
            "invalid GPT reasoning signature: ciphertext length {ciphertext_len} is not a positive AES block multiple"
        ));
    }
    Ok(GptReasoningSignatureInfo {
        decoded_len: decoded.len(),
        ciphertext_len,
    })
}

/// 上游 400 错误文本是否指向推理签名失效。
pub fn error_text_indicates_invalid_reasoning_signature(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("signature")
        || lower.contains("encrypted_content")
        || lower.contains("encrypted content")
}

// ---------------------------------------------------------------------------
// 捕获
// ---------------------------------------------------------------------------

fn non_empty_str<'a>(value: Option<&'a Value>) -> Option<&'a str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_codex_tool_call_type(item_type: &str) -> bool {
    matches!(item_type, "function_call" | "custom_tool_call")
}

/// 从 OpenAI Responses 的 `output` 数组捕获推理签名。推理项只在紧随其后的工具调用
/// 存在时才入账，锚点是那个工具调用的 `call_id`。
pub fn capture_reasoning_replay_from_openai_responses_output(
    output: &[Value],
) -> Vec<ReasoningReplayItem> {
    let mut items = Vec::new();
    let mut pending: Vec<(Option<String>, String, Vec<Value>)> = Vec::new();
    for item in output {
        let Some(object) = item.as_object() else {
            continue;
        };
        let item_type = non_empty_str(object.get("type")).unwrap_or_default();
        match item_type {
            "reasoning" => {
                let Some(encrypted_content) = non_empty_str(object.get("encrypted_content")) else {
                    continue;
                };
                if !is_valid_gpt_reasoning_signature(encrypted_content) {
                    continue;
                }
                let summary = object
                    .get("summary")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                pending.push((
                    non_empty_str(object.get("id")).map(ToOwned::to_owned),
                    encrypted_content.to_string(),
                    summary,
                ));
            }
            other if is_codex_tool_call_type(other) => {
                let Some(call_id) = non_empty_str(object.get("call_id")) else {
                    pending.clear();
                    continue;
                };
                for (id, encrypted_content, summary) in pending.drain(..) {
                    items.push(ReasoningReplayItem::CodexReasoning {
                        id,
                        encrypted_content,
                        summary,
                        call_id: call_id.to_string(),
                    });
                }
                if let Some(id) = non_empty_str(object.get("id")) {
                    items.push(ReasoningReplayItem::CodexFunctionCall {
                        item_type: other.to_string(),
                        id: id.to_string(),
                        call_id: call_id.to_string(),
                    });
                }
            }
            _ => {
                // 推理项后面跟的是消息而不是工具调用：这一段推理不需要随工具回传。
                pending.clear();
            }
        }
    }
    items
}

fn gemini_candidates(value: &Value) -> Option<&Vec<Value>> {
    let object = value.as_object()?;
    let response = object
        .get("response")
        .and_then(Value::as_object)
        .filter(|response| response.contains_key("candidates"))
        .unwrap_or(object);
    response.get("candidates").and_then(Value::as_array)
}

fn gemini_native_signature(part: &Map<String, Value>) -> Option<&str> {
    non_empty_str(
        part.get("thoughtSignature")
            .or_else(|| part.get("thought_signature")),
    )
    .filter(|signature| *signature != GEMINI_SKIP_THOUGHT_SIGNATURE_PLACEHOLDER)
}

/// 从一段 Gemini 响应（同步 JSON 或单个流式 chunk，可带 Antigravity / Gemini CLI 的
/// `response` 外壳）捕获 `functionCall` 上的 `thoughtSignature`。
pub fn capture_reasoning_replay_from_gemini_response(value: &Value) -> Vec<ReasoningReplayItem> {
    let mut items = Vec::new();
    let Some(candidates) = gemini_candidates(value) else {
        return items;
    };
    for candidate in candidates {
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let mut pending_signature: Option<String> = None;
        for part in parts {
            let Some(part_object) = part.as_object() else {
                continue;
            };
            let signature = gemini_native_signature(part_object);
            if let Some(function_call) = part_object.get("functionCall").and_then(Value::as_object)
            {
                let Some(name) = non_empty_str(function_call.get("name")) else {
                    pending_signature = None;
                    continue;
                };
                let signature = signature
                    .map(ToOwned::to_owned)
                    .or_else(|| pending_signature.take());
                let Some(signature) = signature else {
                    continue;
                };
                items.push(ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: non_empty_str(function_call.get("id")).map(ToOwned::to_owned),
                    name: name.to_string(),
                    signature,
                    args_digest: gemini_function_call_args_digest(function_call),
                });
                continue;
            }
            let is_thought = part_object
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(signature) = signature {
                if is_thought {
                    // 思考块上的签名归属于紧随其后的工具调用。
                    pending_signature = Some(signature.to_string());
                    continue;
                }
            }
            pending_signature = None;
        }
    }
    items
}

fn sse_data_payloads(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines().filter_map(|line| {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let payload = trimmed.strip_prefix("data:")?.trim_start();
        if payload.is_empty() || payload == "[DONE]" {
            return None;
        }
        serde_json::from_str::<Value>(payload).ok()
    })
}

/// 从捕获的上游 SSE 文本提取签名：Codex 取 `response.completed` / `response.done` 的
/// `response.output`；Gemini 逐 chunk 扫描 `candidates`。
pub fn capture_reasoning_replay_from_sse_text(
    provider: ReasoningReplayProvider,
    text: &str,
) -> Vec<ReasoningReplayItem> {
    match provider {
        ReasoningReplayProvider::Codex => {
            let mut latest = Vec::new();
            for payload in sse_data_payloads(text) {
                let event_type = non_empty_str(payload.get("type")).unwrap_or_default();
                if !matches!(event_type, "response.completed" | "response.done") {
                    continue;
                }
                if let Some(output) = payload
                    .get("response")
                    .and_then(|response| response.get("output"))
                    .and_then(Value::as_array)
                {
                    latest = capture_reasoning_replay_from_openai_responses_output(output);
                }
            }
            if latest.is_empty() {
                // 没有终态事件（例如上游只发了 output_item.done）：退回逐项收集。
                let mut output = Vec::new();
                for payload in sse_data_payloads(text) {
                    if non_empty_str(payload.get("type")) == Some("response.output_item.done") {
                        if let Some(item) = payload.get("item") {
                            output.push(item.clone());
                        }
                    }
                }
                latest = capture_reasoning_replay_from_openai_responses_output(&output);
            }
            latest
        }
        ReasoningReplayProvider::Gemini => {
            let mut items = Vec::new();
            for payload in sse_data_payloads(text) {
                items.extend(capture_reasoning_replay_from_gemini_response(&payload));
            }
            dedup_gemini_items(items)
        }
    }
}

fn dedup_gemini_items(items: Vec<ReasoningReplayItem>) -> Vec<ReasoningReplayItem> {
    let mut deduped: Vec<ReasoningReplayItem> = Vec::with_capacity(items.len());
    for item in items {
        if !deduped.contains(&item) {
            deduped.push(item);
        }
    }
    deduped
}

// ---------------------------------------------------------------------------
// 回放
// ---------------------------------------------------------------------------

/// 把缓存的签名插回请求体。Codex 作用于 `input`；Gemini 作用于 `contents`（支持
/// Antigravity / Gemini CLI 的 `request` 外壳）。已经带签名的位置不动。
pub fn apply_reasoning_replay(
    body: &mut Value,
    entry: &ReasoningReplayEntry,
) -> ReasoningReplayApplied {
    match entry.provider {
        ReasoningReplayProvider::Codex => apply_codex_reasoning_replay(body, &entry.items),
        ReasoningReplayProvider::Gemini => apply_gemini_reasoning_replay(body, &entry.items),
    }
}

fn apply_codex_reasoning_replay(
    body: &mut Value,
    items: &[ReasoningReplayItem],
) -> ReasoningReplayApplied {
    let mut applied = ReasoningReplayApplied::default();
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        applied.unmatched = items
            .iter()
            .filter_map(|item| item.anchor_call_id().map(ToOwned::to_owned))
            .collect();
        return applied;
    };

    // call_id → 工具调用项下标（同一 call_id 出现多次时取第一个）。
    let mut call_index: BTreeMap<String, usize> = BTreeMap::new();
    for (index, item) in input.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        if !non_empty_str(object.get("type")).is_some_and(is_codex_tool_call_type) {
            continue;
        }
        if let Some(call_id) = non_empty_str(object.get("call_id")) {
            call_index.entry(call_id.to_string()).or_insert(index);
        }
    }

    // 先补 item id（不改变下标）。
    for item in items {
        let ReasoningReplayItem::CodexFunctionCall {
            item_type,
            id,
            call_id,
        } = item
        else {
            continue;
        };
        let Some(index) = call_index.get(call_id).copied() else {
            continue;
        };
        let Some(object) = input[index].as_object_mut() else {
            continue;
        };
        if non_empty_str(object.get("type")) != Some(item_type.as_str()) {
            continue;
        }
        if non_empty_str(object.get("id")).is_none() {
            object.insert("id".to_string(), Value::String(id.clone()));
            applied.restored_ids += 1;
        }
    }

    // 再按锚点收集要插入的推理项：下标 → 推理项列表（保持缓存顺序）。
    let mut insertions: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
    let mut seen_anchor = std::collections::BTreeSet::new();
    for item in items {
        let ReasoningReplayItem::CodexReasoning {
            id,
            encrypted_content,
            summary,
            call_id,
        } = item
        else {
            continue;
        };
        if !is_valid_gpt_reasoning_signature(encrypted_content) {
            continue;
        }
        let Some(index) = call_index.get(call_id).copied() else {
            if !applied.unmatched.iter().any(|existing| existing == call_id) {
                applied.unmatched.push(call_id.clone());
            }
            continue;
        };
        let already_signed = index > 0
            && input[index - 1].as_object().is_some_and(|previous| {
                non_empty_str(previous.get("type")) == Some("reasoning")
                    && non_empty_str(previous.get("encrypted_content")).is_some()
            });
        if already_signed {
            if seen_anchor.insert(call_id.clone()) {
                applied.skipped.push(call_id.clone());
            }
            continue;
        }
        let mut reasoning = Map::new();
        reasoning.insert("type".to_string(), Value::String("reasoning".to_string()));
        if let Some(id) = id {
            reasoning.insert("id".to_string(), Value::String(id.clone()));
        }
        reasoning.insert("summary".to_string(), Value::Array(summary.clone()));
        reasoning.insert(
            "encrypted_content".to_string(),
            Value::String(encrypted_content.clone()),
        );
        insertions
            .entry(index)
            .or_default()
            .push(Value::Object(reasoning));
        if seen_anchor.insert(call_id.clone()) {
            applied.anchors.push(call_id.clone());
        }
    }

    if insertions.is_empty() {
        return applied;
    }
    let original = std::mem::take(input);
    let mut rebuilt = Vec::with_capacity(original.len() + insertions.len());
    for (index, item) in original.into_iter().enumerate() {
        if let Some(reasoning_items) = insertions.remove(&index) {
            applied.inserted += reasoning_items.len();
            rebuilt.extend(reasoning_items);
        }
        rebuilt.push(item);
    }
    *input = rebuilt;
    applied
}

fn gemini_contents_mut(body: &mut Value) -> Option<&mut Vec<Value>> {
    let object = body.as_object_mut()?;
    if object.contains_key("contents") {
        return object.get_mut("contents").and_then(Value::as_array_mut);
    }
    object
        .get_mut("request")
        .and_then(Value::as_object_mut)
        .and_then(|request| request.get_mut("contents"))
        .and_then(Value::as_array_mut)
}

fn gemini_name_digest_key(name: &str, digest: &str) -> String {
    format!("{name}\0{digest}")
}

/// Gemini 回放的锚点规则，按优先级：
///
/// 1. 请求里的 `functionCall.id` 与账本条目的 `call_id` 相等；
/// 2. 账本条目与请求都能算出 `args` 摘要时，按 `(name, args_digest)` 匹配；
///    同一会话里同名同参的多次调用按历史顺序对应（账本按轮次先旧后新合并）；
/// 3. 任一侧算不出摘要（旧条目或请求没有 `args`）时，只在账本里该 `name`
///    只有一条无 `call_id` 的签名时按名回放。
///
/// 绝不按名 FIFO 跨调用猜：两个并发会话共用同一把 Key、都调用 `search` 时，
/// 老规则会把 A 的签名插进 B 的历史，上游 400 后两边都被清空。
fn apply_gemini_reasoning_replay(
    body: &mut Value,
    items: &[ReasoningReplayItem],
) -> ReasoningReplayApplied {
    let mut applied = ReasoningReplayApplied::default();
    struct GeminiSignature<'a> {
        call_id: Option<&'a str>,
        name: &'a str,
        signature: &'a str,
        args_digest: Option<&'a str>,
    }
    let signatures = items
        .iter()
        .filter_map(|item| match item {
            ReasoningReplayItem::GeminiThoughtSignature {
                call_id,
                name,
                signature,
                args_digest,
            } => Some(GeminiSignature {
                call_id: call_id.as_deref(),
                name: name.as_str(),
                signature: signature.as_str(),
                args_digest: args_digest.as_deref(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if signatures.is_empty() {
        return applied;
    }
    let Some(contents) = gemini_contents_mut(body) else {
        applied.unmatched = signatures
            .iter()
            .filter_map(|signature| signature.call_id.map(ToOwned::to_owned))
            .collect();
        return applied;
    };

    let mut by_call_id: BTreeMap<&str, &str> = BTreeMap::new();
    // `name\0args_digest` → 签名队列，按账本顺序（先旧后新）。
    let mut by_name_digest: BTreeMap<String, VecDeque<&str>> = BTreeMap::new();
    // name → 无 call_id 的全部签名（含没有摘要的旧条目），用于「唯一才按名回放」。
    let mut by_name: BTreeMap<&str, Vec<(Option<&str>, &str)>> = BTreeMap::new();
    for signature in &signatures {
        match signature.call_id {
            Some(call_id) => {
                by_call_id.entry(call_id).or_insert(signature.signature);
            }
            None => {
                if let Some(digest) = signature.args_digest {
                    by_name_digest
                        .entry(gemini_name_digest_key(signature.name, digest))
                        .or_default()
                        .push_back(signature.signature);
                }
                by_name
                    .entry(signature.name)
                    .or_default()
                    .push((signature.args_digest, signature.signature));
            }
        }
    }
    let mut matched_call_ids = std::collections::BTreeSet::new();
    let mut consumed_by_name: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for content in contents.iter_mut() {
        let Some(content_object) = content.as_object_mut() else {
            continue;
        };
        if non_empty_str(content_object.get("role")).is_some_and(|role| role != "model") {
            continue;
        }
        let Some(parts) = content_object
            .get_mut("parts")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(part_object) = part.as_object_mut() else {
                continue;
            };
            let Some((call_id, name, request_digest)) = part_object
                .get("functionCall")
                .and_then(Value::as_object)
                .map(|function_call| {
                    (
                        non_empty_str(function_call.get("id")).map(ToOwned::to_owned),
                        non_empty_str(function_call.get("name"))
                            .unwrap_or_default()
                            .to_string(),
                        gemini_function_call_args_digest(function_call),
                    )
                })
            else {
                continue;
            };
            let resolved: Option<(&str, String)> = match call_id.as_deref() {
                Some(call_id) if by_call_id.contains_key(call_id) => {
                    matched_call_ids.insert(call_id.to_string());
                    Some((by_call_id[call_id], call_id.to_string()))
                }
                _ => {
                    let by_digest = request_digest.as_deref().and_then(|digest| {
                        by_name_digest
                            .get_mut(&gemini_name_digest_key(name.as_str(), digest))
                            .and_then(VecDeque::pop_front)
                            .map(|signature| (signature, format!("name:{name}#{digest}")))
                    });
                    match by_digest {
                        Some(resolved) => Some(resolved),
                        None => {
                            // 任一侧没有摘要：只在该名字唯一时按名回放。
                            let candidates = by_name.get(name.as_str());
                            let unique = candidates
                                .filter(|candidates| candidates.len() == 1)
                                .and_then(|candidates| candidates.first());
                            match unique {
                                Some((digest, signature))
                                    if (digest.is_none() || request_digest.is_none())
                                        && !consumed_by_name.contains(name.as_str()) =>
                                {
                                    consumed_by_name.insert(name.clone());
                                    Some((*signature, format!("name:{name}")))
                                }
                                _ => None,
                            }
                        }
                    }
                }
            };
            let Some((signature, anchor_label)) = resolved else {
                continue;
            };
            if gemini_native_signature(part_object).is_some() {
                applied.skipped.push(anchor_label);
                continue;
            }
            part_object.remove("thought_signature");
            part_object.insert(
                "thoughtSignature".to_string(),
                Value::String(signature.to_string()),
            );
            applied.inserted += 1;
            applied.anchors.push(anchor_label);
        }
    }
    for call_id in by_call_id.keys() {
        if !matched_call_ids.contains(*call_id) {
            applied.unmatched.push((*call_id).to_string());
        }
    }
    for (key, remaining) in &by_name_digest {
        if !remaining.is_empty() {
            let (name, digest) = key.split_once('\0').unwrap_or((key.as_str(), ""));
            applied.unmatched.push(format!("name:{name}#{digest}"));
        }
    }
    for (name, candidates) in &by_name {
        let digest_less = candidates.iter().any(|(digest, _)| digest.is_none());
        if digest_less && !consumed_by_name.contains(*name) {
            applied.unmatched.push(format!("name:{name}"));
        }
    }
    applied
}

/// 回放结果的 JSON 形状（写入 `report_context.reasoning_replay`）。
pub fn reasoning_replay_report_value(
    key: &ReasoningReplayKey,
    generation: u64,
    applied: &ReasoningReplayApplied,
) -> Value {
    json!({
        "provider": key.provider.as_str(),
        "storage_key": key.storage_key(),
        "generation": generation,
        "applied": applied.changed(),
        "inserted": applied.inserted,
        "restored_ids": applied.restored_ids,
        "anchors": applied.anchors,
        "skipped": applied.skipped,
        "unmatched": applied.unmatched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成形状合法的 Fernet 令牌：0x80 + 8 字节时间戳 + 16 字节 IV + N*16 密文 + 32 字节 HMAC。
    pub(super) fn fernet_like(seed: u8, blocks: usize) -> String {
        let mut bytes = vec![0x80u8];
        // 真实 Fernet 的时间戳是大端 64 位秒数，高位字节为 0，所以令牌总以 gAAAA 开头。
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, seed]);
        bytes.extend_from_slice(&[seed.wrapping_add(1); 16]);
        bytes.extend(std::iter::repeat_n(seed.wrapping_add(2), 16 * blocks));
        bytes.extend_from_slice(&[seed.wrapping_add(3); 32]);
        URL_SAFE.encode(bytes)
    }

    #[test]
    fn fernet_shape_validation_accepts_well_formed_and_rejects_malformed() {
        let valid = fernet_like(7, 3);
        assert!(valid.starts_with("gAAAA"));
        let info = inspect_gpt_reasoning_signature(&valid).expect("well-formed token");
        assert_eq!(info.ciphertext_len, 48);
        assert!(is_valid_gpt_reasoning_signature(
            valid.trim_end_matches('=')
        ));

        assert!(!is_valid_gpt_reasoning_signature(""));
        assert!(!is_valid_gpt_reasoning_signature("hello world"));
        assert!(!is_valid_gpt_reasoning_signature("gAAAA not base64!"));
        // 长度不是 16 的倍数
        let mut bytes = vec![0x80u8];
        bytes.extend_from_slice(&[0u8; 8 + 16 + 17 + 32]);
        assert!(!is_valid_gpt_reasoning_signature(&URL_SAFE.encode(bytes)));
        // 版本字节不对
        let mut bytes = vec![0x81u8];
        bytes.extend_from_slice(&[0u8; 8 + 16 + 16 + 32]);
        let token = URL_SAFE.encode(bytes);
        assert!(!is_valid_gpt_reasoning_signature(&token));
    }

    #[test]
    fn storage_key_hashes_session_scope_and_supports_provider_key_prefix() {
        let key = ReasoningReplayKey::new(
            ReasoningReplayProvider::Codex,
            "key-1",
            "session:abc",
            "gpt-5.6-sol",
        )
        .expect("key");
        let storage_key = key.storage_key();
        assert!(storage_key.starts_with("reasoning_replay:codex:key-1:"));
        assert!(!storage_key.contains("session:abc"));
        assert!(
            ReasoningReplayKey::storage_key_prefix_for_provider_key("key-1")
                .iter()
                .any(|prefix| storage_key.starts_with(prefix))
        );
        assert!(
            ReasoningReplayKey::new(ReasoningReplayProvider::Codex, " ", "session", "model")
                .is_none()
        );
    }

    #[test]
    fn captures_codex_reasoning_anchored_to_following_tool_call() {
        let signature = fernet_like(1, 2);
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "think"}], "encrypted_content": signature}),
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{}"}),
            json!({"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "not-fernet"}),
            json!({"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "lookup", "arguments": "{}"}),
            json!({"type": "reasoning", "id": "rs_3", "summary": [], "encrypted_content": fernet_like(2, 1)}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}),
        ];
        let items = capture_reasoning_replay_from_openai_responses_output(&output);
        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            ReasoningReplayItem::CodexReasoning { id: Some(id), call_id, .. } if id == "rs_1" && call_id == "call_1"
        ));
        assert!(matches!(
            &items[1],
            ReasoningReplayItem::CodexFunctionCall { id, call_id, .. } if id == "fc_1" && call_id == "call_1"
        ));
        assert!(matches!(
            &items[2],
            ReasoningReplayItem::CodexFunctionCall { id, call_id, .. } if id == "fc_2" && call_id == "call_2"
        ));
    }

    #[test]
    fn captures_codex_from_sse_completed_event_and_falls_back_to_item_done() {
        let signature = fernet_like(3, 1);
        let sse = format!(
            "event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "encrypted_content": signature, "summary": []}}),
            json!({"type": "response.completed", "response": {"output": [
                {"type": "reasoning", "id": "rs_a", "encrypted_content": signature, "summary": []},
                {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "f", "arguments": "{}"}
            ]}}),
        );
        let items = capture_reasoning_replay_from_sse_text(ReasoningReplayProvider::Codex, &sse);
        assert_eq!(items.len(), 2);

        let sse_without_terminal = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_b", "encrypted_content": signature, "summary": []}}),
            json!({"type": "response.output_item.done", "item": {"type": "custom_tool_call", "id": "ctc_b", "call_id": "call_b", "name": "shell", "input": "ls"}}),
        );
        let items = capture_reasoning_replay_from_sse_text(
            ReasoningReplayProvider::Codex,
            &sse_without_terminal,
        );
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[1],
            ReasoningReplayItem::CodexFunctionCall { item_type, call_id, .. } if item_type == "custom_tool_call" && call_id == "call_b"
        ));
    }

    #[test]
    fn replays_codex_reasoning_before_matching_call_and_restores_item_ids() {
        let signature = fernet_like(4, 1);
        let entry = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Codex,
            vec![
                ReasoningReplayItem::CodexReasoning {
                    id: Some("rs_1".to_string()),
                    encrypted_content: signature.clone(),
                    summary: vec![],
                    call_id: "call_1".to_string(),
                },
                ReasoningReplayItem::CodexFunctionCall {
                    item_type: "function_call".to_string(),
                    id: "fc_1".to_string(),
                    call_id: "call_1".to_string(),
                },
                ReasoningReplayItem::CodexReasoning {
                    id: None,
                    encrypted_content: signature.clone(),
                    summary: vec![],
                    call_id: "call_missing".to_string(),
                },
            ],
            0,
        );
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ]
        });
        let applied = apply_reasoning_replay(&mut body, &entry);
        assert_eq!(applied.inserted, 1);
        assert_eq!(applied.restored_ids, 1);
        assert_eq!(applied.anchors, vec!["call_1"]);
        assert_eq!(applied.unmatched, vec!["call_missing"]);
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_1");
        assert_eq!(input[1]["encrypted_content"], signature);
        assert_eq!(input[2]["id"], "fc_1");
        assert_eq!(input[2]["call_id"], "call_1");

        // 幂等：再回放一次不再插入。
        let again = apply_reasoning_replay(&mut body, &entry);
        assert_eq!(again.inserted, 0);
        assert_eq!(again.restored_ids, 0);
        assert_eq!(again.skipped, vec!["call_1"]);
        assert_eq!(body["input"].as_array().expect("input").len(), 5);
    }

    #[test]
    fn captures_gemini_signatures_from_function_call_and_thought_parts() {
        let response = json!({
            "response": {
                "candidates": [{
                    "content": {"role": "model", "parts": [
                        {"text": "thinking", "thought": true, "thoughtSignature": "sig-thought"},
                        {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}},
                        {"functionCall": {"id": "call_2", "name": "lookup", "args": {}}, "thoughtSignature": "sig-2"},
                        {"functionCall": {"name": "noid", "args": {}}, "thoughtSignature": "skip_thought_signature_validator"},
                        {"functionCall": {"name": "byname", "args": {}}, "thoughtSignature": "sig-3"}
                    ]}
                }]
            }
        });
        let items = capture_reasoning_replay_from_gemini_response(&response);
        let empty_args_digest =
            gemini_function_call_args_digest(json!({"args": {}}).as_object().expect("object"))
                .expect("digest");
        assert_eq!(
            items,
            vec![
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: Some("call_1".to_string()),
                    name: "lookup".to_string(),
                    signature: "sig-thought".to_string(),
                    args_digest: Some(empty_args_digest.clone()),
                },
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: Some("call_2".to_string()),
                    name: "lookup".to_string(),
                    signature: "sig-2".to_string(),
                    args_digest: Some(empty_args_digest.clone()),
                },
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: None,
                    name: "byname".to_string(),
                    signature: "sig-3".to_string(),
                    args_digest: Some(empty_args_digest.clone()),
                },
            ]
        );

        let sse = format!(
            "data: {}\r\n\r\ndata: {}\r\n\r\n",
            response,
            json!({"candidates": [{"content": {"parts": [{"functionCall": {"id": "call_9", "name": "x", "args": {}}, "thought_signature": "sig-9"}]}}]})
        );
        let streamed =
            capture_reasoning_replay_from_sse_text(ReasoningReplayProvider::Gemini, &sse);
        assert_eq!(streamed.len(), 4);
    }

    #[test]
    fn replays_gemini_signatures_into_envelope_and_direct_bodies() {
        let entry = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Gemini,
            vec![
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: Some("call_1".to_string()),
                    name: "lookup".to_string(),
                    signature: "sig-1".to_string(),
                    args_digest: None,
                },
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: None,
                    name: "byname".to_string(),
                    signature: "sig-n".to_string(),
                    args_digest: None,
                },
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: Some("call_gone".to_string()),
                    name: "lookup".to_string(),
                    signature: "sig-gone".to_string(),
                    args_digest: None,
                },
            ],
            0,
        );
        let mut envelope = json!({
            "model": "gemini-3-pro",
            "request": {"contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [
                    {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}, "thoughtSignature": "skip_thought_signature_validator"},
                    {"functionCall": {"name": "byname", "args": {}}},
                    {"functionCall": {"id": "call_signed", "name": "lookup", "args": {}}, "thoughtSignature": "native"}
                ]},
                {"role": "user", "parts": [{"functionResponse": {"id": "call_1", "name": "lookup", "response": {}}}]}
            ]}
        });
        let applied = apply_reasoning_replay(&mut envelope, &entry);
        assert_eq!(applied.inserted, 2);
        assert_eq!(applied.anchors, vec!["call_1", "name:byname"]);
        assert_eq!(applied.unmatched, vec!["call_gone"]);
        let parts = envelope["request"]["contents"][1]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts[0]["thoughtSignature"], "sig-1");
        assert_eq!(parts[1]["thoughtSignature"], "sig-n");
        assert_eq!(parts[2]["thoughtSignature"], "native");

        let mut direct = json!({"contents": [{"role": "model", "parts": [{"functionCall": {"id": "call_1", "name": "lookup", "args": {}}}]}]});
        let applied = apply_reasoning_replay(&mut direct, &entry);
        assert_eq!(applied.inserted, 1);
        assert_eq!(
            direct["contents"][0]["parts"][0]["thoughtSignature"],
            "sig-1"
        );
        let again = apply_reasoning_replay(&mut direct, &entry);
        assert_eq!(again.inserted, 0);
        assert_eq!(again.skipped, vec!["call_1"]);
    }

    #[test]
    fn ledger_round_trips_cas_clear_and_provider_key_clear() {
        let ledger = ReasoningReplayLedger::new();
        let key = ReasoningReplayKey::new(
            ReasoningReplayProvider::Codex,
            "key-1",
            "session-1",
            "gpt-5.6-sol",
        )
        .expect("key");
        let other = ReasoningReplayKey::new(
            ReasoningReplayProvider::Gemini,
            "key-1",
            "session-2",
            "gemini-3-pro",
        )
        .expect("key");
        let unrelated = ReasoningReplayKey::new(
            ReasoningReplayProvider::Codex,
            "key-2",
            "session-1",
            "gpt-5.6-sol",
        )
        .expect("key");
        let item = ReasoningReplayItem::CodexFunctionCall {
            item_type: "function_call".to_string(),
            id: "fc".to_string(),
            call_id: "call".to_string(),
        };
        let first =
            ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![item.clone()], 1);
        let first_generation = first.generation;
        ledger.put(&key, first);
        ledger.put(
            &other,
            ReasoningReplayEntry::new(ReasoningReplayProvider::Gemini, vec![item.clone()], 1),
        );
        ledger.put(
            &unrelated,
            ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![item.clone()], 1),
        );
        assert_eq!(ledger.len(), 3);
        assert_eq!(
            ledger.get(&key).expect("entry").generation,
            first_generation
        );

        // 新一轮覆盖后，旧代次的 CAS 清理不生效。
        let second =
            ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![item.clone()], 2);
        let second_generation = second.generation;
        ledger.put(&key, second);
        assert!(!ledger.clear_if_generation(&key, first_generation));
        assert!(ledger.get(&key).is_some());
        assert!(ledger.clear_if_generation(&key, second_generation));
        assert!(ledger.get(&key).is_none());

        // 空条目等同删除。
        ledger.put(
            &key,
            ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![], 3),
        );
        assert!(ledger.get(&key).is_none());

        assert_eq!(ledger.clear_provider_key("key-1"), 1);
        assert!(ledger.get(&other).is_none());
        assert!(ledger.get(&unrelated).is_some());
    }

    #[test]
    fn merging_entries_keeps_older_turns_and_prefers_latest_anchor() {
        let signature = fernet_like(9, 1);
        let previous = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Codex,
            vec![
                ReasoningReplayItem::CodexReasoning {
                    id: Some("rs_old".to_string()),
                    encrypted_content: signature.clone(),
                    summary: vec![],
                    call_id: "call_1".to_string(),
                },
                ReasoningReplayItem::CodexFunctionCall {
                    item_type: "function_call".to_string(),
                    id: "fc_1".to_string(),
                    call_id: "call_1".to_string(),
                },
            ],
            1,
        );
        let latest = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Codex,
            vec![
                ReasoningReplayItem::CodexReasoning {
                    id: Some("rs_new".to_string()),
                    encrypted_content: signature.clone(),
                    summary: vec![],
                    call_id: "call_1".to_string(),
                },
                ReasoningReplayItem::CodexReasoning {
                    id: Some("rs_2".to_string()),
                    encrypted_content: signature.clone(),
                    summary: vec![],
                    call_id: "call_2".to_string(),
                },
            ],
            2,
        );
        let merged = ReasoningReplayEntry::merged_with(Some(&previous), latest);
        assert_eq!(merged.call_ids(), vec!["call_1", "call_2"]);
        assert_eq!(merged.items.len(), 3);
        assert!(
            matches!(&merged.items[0], ReasoningReplayItem::CodexFunctionCall { call_id, .. } if call_id == "call_1")
        );
        assert!(
            matches!(&merged.items[1], ReasoningReplayItem::CodexReasoning { id: Some(id), .. } if id == "rs_new")
        );

        let other_provider = ReasoningReplayEntry::new(ReasoningReplayProvider::Gemini, vec![], 3);
        let replaced = ReasoningReplayEntry::merged_with(Some(&previous), other_provider.clone());
        assert_eq!(replaced, other_provider);

        let mut many = Vec::new();
        for index in 0..(REASONING_REPLAY_MAX_ITEMS_PER_ENTRY + 10) {
            many.push(ReasoningReplayItem::CodexFunctionCall {
                item_type: "function_call".to_string(),
                id: format!("fc_{index}"),
                call_id: format!("call_{index}"),
            });
        }
        let capped = ReasoningReplayEntry::merged_with(
            Some(&previous),
            ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, many, 4),
        );
        assert_eq!(capped.items.len(), REASONING_REPLAY_MAX_ITEMS_PER_ENTRY);
        assert!(
            matches!(capped.items.last(), Some(ReasoningReplayItem::CodexFunctionCall { call_id, .. }) if call_id == &format!("call_{}", REASONING_REPLAY_MAX_ITEMS_PER_ENTRY + 9))
        );
    }

    #[test]
    fn entry_serializes_for_external_kv_round_trip() {
        let entry = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Gemini,
            vec![ReasoningReplayItem::GeminiThoughtSignature {
                call_id: None,
                name: "f".to_string(),
                signature: "s".to_string(),
                args_digest: None,
            }],
            42,
        );
        let text = serde_json::to_string(&entry).expect("serialize");
        assert!(!text.contains("args_digest"));
        let decoded: ReasoningReplayEntry = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(decoded, entry);
        assert_eq!(entry.call_ids(), Vec::<String>::new());

        // 升级前写入的条目没有 args_digest 字段，也要能读回。
        let legacy = serde_json::json!({
            "provider": "gemini",
            "items": [{"kind": "gemini_thought_signature", "name": "f", "signature": "s"}],
            "generation": 7,
            "captured_at_unix_secs": 1
        });
        let decoded: ReasoningReplayEntry =
            serde_json::from_value(legacy).expect("legacy entry should deserialize");
        assert!(matches!(
            &decoded.items[0],
            ReasoningReplayItem::GeminiThoughtSignature {
                args_digest: None,
                ..
            }
        ));
    }

    #[test]
    fn generation_mixes_a_random_process_seed_into_the_counter() {
        let first = ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![], 0);
        let second = ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, vec![], 0);
        assert_ne!(first.generation, second.generation);
        // 同一进程内的代次由种子与计数混合而成，不再是从 1 起的小整数：
        // 另一个实例同样从 1 计数时不会撞上。
        assert!(first.generation > u64::from(u16::MAX) || second.generation > u64::from(u16::MAX));
        assert_ne!(generation_seed(), 0);
        assert_eq!(generation_seed(), generation_seed());
    }

    #[test]
    fn gemini_args_digest_is_canonical_and_absent_without_args() {
        let a = json!({"name": "search", "args": {"query": "rust", "limit": 5}});
        let b = json!({"name": "search", "args": {"limit": 5.0, "query": "rust"}});
        let c = json!({"name": "search", "args": {"query": "go", "limit": 5}});
        let digest =
            |value: &Value| gemini_function_call_args_digest(value.as_object().expect("object"));
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&c));
        assert_eq!(
            digest(&a).expect("digest").len(),
            GEMINI_ARGS_DIGEST_HEX_LEN
        );
        assert!(digest(&json!({"name": "search"})).is_none());
    }

    #[test]
    fn gemini_replay_anchors_same_name_calls_by_args_digest_not_fifo() {
        // 同一会话两次调用 search，参数不同；客户端回传的历史顺序与账本顺序相反。
        let response = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "search", "args": {"query": "first"}}, "thoughtSignature": "sig-first"},
            {"functionCall": {"name": "search", "args": {"query": "second"}}, "thoughtSignature": "sig-second"}
        ]}}]});
        let items = capture_reasoning_replay_from_gemini_response(&response);
        assert_eq!(items.len(), 2);
        let entry = ReasoningReplayEntry::new(ReasoningReplayProvider::Gemini, items, 0);
        let mut body = json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "search", "args": {"query": "second"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "search", "response": {}}}]},
            {"role": "model", "parts": [{"functionCall": {"name": "search", "args": {"query": "first"}}}]}
        ]});
        let applied = apply_reasoning_replay(&mut body, &entry);
        assert_eq!(applied.inserted, 2);
        assert!(applied.unmatched.is_empty());
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "sig-second"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["thoughtSignature"],
            "sig-first"
        );

        // 另一个会话的历史：同名工具但参数不同 → 不回放（不猜），记为 unmatched。
        let mut other = json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "search", "args": {"query": "third"}}}]}
        ]});
        let applied = apply_reasoning_replay(&mut other, &entry);
        assert_eq!(applied.inserted, 0);
        assert!(other["contents"][0]["parts"][0]
            .get("thoughtSignature")
            .is_none());
        assert_eq!(applied.unmatched.len(), 2);

        // 同名同参出现两次（同一会话重复调用）：按历史顺序各取一条，不会把第二次的
        // 签名套到第一次上后再无签名可用。
        let repeated = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "ls", "args": {"path": "/"}}, "thoughtSignature": "sig-a"},
            {"functionCall": {"name": "ls", "args": {"path": "/"}}, "thoughtSignature": "sig-b"}
        ]}}]});
        let entry = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Gemini,
            capture_reasoning_replay_from_gemini_response(&repeated),
            0,
        );
        let mut body = json!({"contents": [{"role": "model", "parts": [
            {"functionCall": {"name": "ls", "args": {"path": "/"}}},
            {"functionCall": {"name": "ls", "args": {"path": "/"}}}
        ]}]});
        let applied = apply_reasoning_replay(&mut body, &entry);
        assert_eq!(applied.inserted, 2);
        assert_eq!(body["contents"][0]["parts"][0]["thoughtSignature"], "sig-a");
        assert_eq!(body["contents"][0]["parts"][1]["thoughtSignature"], "sig-b");
    }

    #[test]
    fn gemini_replay_without_digest_only_matches_a_unique_name() {
        // 升级前写入的旧条目没有摘要：同名只有一条时按名回放，多条时一律不猜。
        let unique = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Gemini,
            vec![ReasoningReplayItem::GeminiThoughtSignature {
                call_id: None,
                name: "search".to_string(),
                signature: "sig-only".to_string(),
                args_digest: None,
            }],
            0,
        );
        let mut body = json!({"contents": [{"role": "model", "parts": [
            {"functionCall": {"name": "search", "args": {"query": "a"}}},
            {"functionCall": {"name": "search", "args": {"query": "b"}}}
        ]}]});
        let applied = apply_reasoning_replay(&mut body, &unique);
        assert_eq!(applied.inserted, 1);
        assert_eq!(applied.anchors, vec!["name:search"]);
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "sig-only"
        );
        assert!(body["contents"][0]["parts"][1]
            .get("thoughtSignature")
            .is_none());

        let ambiguous = ReasoningReplayEntry::new(
            ReasoningReplayProvider::Gemini,
            vec![
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: None,
                    name: "search".to_string(),
                    signature: "sig-1".to_string(),
                    args_digest: None,
                },
                ReasoningReplayItem::GeminiThoughtSignature {
                    call_id: None,
                    name: "search".to_string(),
                    signature: "sig-2".to_string(),
                    args_digest: None,
                },
            ],
            0,
        );
        let mut body = json!({"contents": [{"role": "model", "parts": [
            {"functionCall": {"name": "search", "args": {"query": "a"}}}
        ]}]});
        let applied = apply_reasoning_replay(&mut body, &ambiguous);
        assert_eq!(applied.inserted, 0);
        assert_eq!(applied.unmatched, vec!["name:search"]);
        assert!(body["contents"][0]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn invalid_signature_error_text_detection() {
        assert!(error_text_indicates_invalid_reasoning_signature(
            r#"{"error":{"message":"Invalid signature in thinking block"}}"#
        ));
        assert!(error_text_indicates_invalid_reasoning_signature(
            "invalid_encrypted_content"
        ));
        assert!(!error_text_indicates_invalid_reasoning_signature(
            "context length exceeded"
        ));
    }

    /// 简单可复现的伪随机数（LCG），差分测试不引入 rand 依赖。
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

    #[test]
    fn random_histories_replay_signatures_one_to_one_for_codex_and_gemini() {
        let mut rng = Lcg(0x5eed_1234);
        for round in 0..1000u64 {
            let calls = 1 + rng.below(6) as usize;
            let mut output = Vec::new();
            let mut expected = Vec::new();
            for index in 0..calls {
                let signed = rng.below(4) != 0;
                let call_id = format!("call_{round}_{index}");
                if signed {
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("rs_{round}_{index}"),
                        "summary": [],
                        "encrypted_content": fernet_like((round + index as u64) as u8, 1 + rng.below(3) as usize),
                    }));
                    expected.push(call_id.clone());
                }
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{round}_{index}"),
                    "call_id": call_id,
                    "name": format!("tool_{}", rng.below(3)),
                    "arguments": "{}",
                }));
            }
            let items = capture_reasoning_replay_from_openai_responses_output(&output);
            let entry = ReasoningReplayEntry::new(ReasoningReplayProvider::Codex, items, round);

            // 客户端跨格式回传：丢掉 reasoning 项与 function_call 的 id，只剩 call_id。
            let mut input = Vec::new();
            input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]}));
            for item in &output {
                if item["type"] == "function_call" {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": item["call_id"],
                        "name": item["name"],
                        "arguments": "{}",
                    }));
                    input.push(json!({"type": "function_call_output", "call_id": item["call_id"], "output": "ok"}));
                }
            }
            let mut body = json!({"model": "gpt-5.6-sol", "input": input});
            let applied = apply_reasoning_replay(&mut body, &entry);
            assert_eq!(applied.inserted, expected.len(), "round {round}");
            assert_eq!(applied.restored_ids, calls, "round {round}");
            assert_eq!(applied.anchors, expected, "round {round}");
            assert!(applied.unmatched.is_empty(), "round {round}");
            let rebuilt = body["input"].as_array().expect("input");
            for window in rebuilt.windows(2) {
                if window[0]["type"] == "reasoning" {
                    assert_eq!(window[1]["type"], "function_call", "round {round}");
                    let call_id = window[1]["call_id"].as_str().expect("call id");
                    assert!(expected.iter().any(|id| id == call_id), "round {round}");
                    assert!(is_valid_gpt_reasoning_signature(
                        window[0]["encrypted_content"].as_str().expect("signature")
                    ));
                }
            }
            let again = apply_reasoning_replay(&mut body, &entry);
            assert_eq!(again.inserted, 0, "round {round}");

            // Gemini 侧：同一批调用改成 functionCall + thoughtSignature。
            let mut parts = Vec::new();
            let mut gemini_expected = 0usize;
            for index in 0..calls {
                let has_id = rng.below(3) != 0;
                let signed = rng.below(4) != 0;
                let mut part =
                    json!({"functionCall": {"name": format!("tool_{index}"), "args": {}}});
                if has_id {
                    part["functionCall"]["id"] = json!(format!("call_{round}_{index}"));
                }
                if signed {
                    part["thoughtSignature"] = json!(format!("sig_{round}_{index}"));
                    gemini_expected += 1;
                }
                parts.push(part);
            }
            let response =
                json!({"candidates": [{"content": {"role": "model", "parts": parts.clone()}}]});
            let items = capture_reasoning_replay_from_gemini_response(&response);
            assert_eq!(items.len(), gemini_expected, "round {round}");
            let entry = ReasoningReplayEntry::new(ReasoningReplayProvider::Gemini, items, round);
            let stripped = parts
                .iter()
                .map(|part| {
                    let mut part = part.clone();
                    part.as_object_mut()
                        .expect("part")
                        .remove("thoughtSignature");
                    part
                })
                .collect::<Vec<_>>();
            let mut body = json!({"request": {"contents": [{"role": "model", "parts": stripped}]}});
            let applied = apply_reasoning_replay(&mut body, &entry);
            assert_eq!(applied.inserted, gemini_expected, "round {round}");
            assert!(applied.unmatched.is_empty(), "round {round}");
            let rebuilt = body["request"]["contents"][0]["parts"]
                .as_array()
                .expect("parts");
            for (index, part) in rebuilt.iter().enumerate() {
                let original = &parts[index];
                assert_eq!(
                    part.get("thoughtSignature"),
                    original.get("thoughtSignature"),
                    "round {round} part {index}"
                );
            }
        }
    }
}
