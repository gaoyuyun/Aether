//! P5：TLS 指纹仿真的**纯数据规格**。
//!
//! 本模块不依赖任何 TLS 库；它只描述"原生客户端的 ClientHello 长什么样"，由网关的
//! `execution_runtime/transport.rs` 把规格映射成 wreq/BoringSSL 的 `Emulation`。
//!
//! ## 参数来源与可信度
//!
//! 三个 profile 的参数以 CLIProxyAPI（commit `c93978c`）复刻的 **Claude Code 2.1.220**
//! 抓包为起点：
//!
//! - 推理面：`internal/runtime/executor/helps/utls_client.go::claudeCodeTLSClientHelloSpec`
//! - OAuth 控制面：`internal/auth/claude/utls_transport.go::claudeOAuthTLSClientHelloSpec`
//!
//! 本地身份 profile 是 2.1.161（`profile.rs`），两者之间 Node 内嵌的 OpenSSL 可能已变化，
//! **所有参数都标记为"待真实抓包核对"**（见 `docs/operations/tls-fingerprint-capture.md`）。
//!
//! ## 与计划文档的偏离
//!
//! 计划 5.2 写的推理面 ALPN 是 `h2, http/1.1`；参考抓包里推理面 ALPN **只有 `http/1.1`**
//! （Node fetch/undici 不走 h2），控制面**不发 ALPN 扩展**。这里以抓包为准，因此两个
//! Claude Code profile 都是 `http1_only`，不需要 HTTP/2 帧层指纹。

use aether_contracts::{TRANSPORT_HTTP_MODE_AUTO, TRANSPORT_HTTP_MODE_HTTP1_ONLY};

/// 供应商 / Key `fingerprint.transport_profile` 的取值：Claude Code 推理面（Node/OpenSSL）。
pub const CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL: &str = "claude_code_node_openssl";
/// OAuth 控制面（platform.claude.com / api.anthropic.com 的 Axios 请求）。
pub const CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE: &str = "claude_code_oauth_control_plane";
/// chatgpt.com 控制面：直接复用 wreq-util 内置的 Chrome 136 emulation。
pub const CHATGPT_COM_CHROME_TLS_PROFILE: &str = "chatgpt_com_chrome";

/// wreq-util 内置 emulation 名称，`chatgpt_com_chrome` 映射到它（与 OAuth cookie 登录用的
/// `claude_oauth_chrome136` 保持同一版本）。
pub const CHATGPT_COM_CHROME_BROWSER_PROFILE: &str = "chrome136";

/// 所有内置 TLS 仿真 profile 的 id。
pub const CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS: &[&str] = &[
    CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL,
    CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE,
    CHATGPT_COM_CHROME_TLS_PROFILE,
];

/// TLS 版本（只用于规格描述，不绑定任何库的枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsProtocolVersion {
    Tls12,
    Tls13,
}

impl TlsProtocolVersion {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tls12 => "TLS1.2",
            Self::Tls13 => "TLS1.3",
        }
    }
}

/// 一类请求的 HTTP/1.1 头顺序（原生客户端实际发出的顺序，大小写照抄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeCodeHeaderOrder {
    /// `messages` / `count_tokens` / `oauth_refresh` / `oauth_inspect`
    pub request_kind: &'static str,
    pub headers: &'static [&'static str],
}

/// 一份 ClientHello 仿真规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeCodeTlsEmulationSpec {
    pub id: &'static str,
    pub min_tls_version: TlsProtocolVersion,
    pub max_tls_version: TlsProtocolVersion,
    /// BoringSSL 标准名称（`TLS_AES_128_GCM_SHA256:...`），冒号分隔，顺序即偏好顺序。
    pub cipher_list: &'static str,
    /// `X25519:P-256:P-384`
    pub curves_list: &'static str,
    /// `ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:...`
    pub sigalgs_list: &'static str,
    /// `None` = 不发 ALPN 扩展；`Some(&[])` 不合法，构造时不会出现。
    pub alpn: Option<&'static [&'static str]>,
    pub session_ticket: bool,
    pub renegotiation: bool,
    pub psk_dhe_ke: bool,
    pub ocsp_stapling: bool,
    pub signed_cert_timestamps: bool,
    /// OpenSSL 不做 GREASE，也不乱序扩展。
    pub grease: bool,
    pub permute_extensions: bool,
    /// key_share 只带一个（X25519）。
    pub key_shares_limit: u8,
    /// ClientHello 扩展的发送顺序（IANA 扩展编号）。`padding`(21) 与 `pre_shared_key`(41)
    /// 由 BoringSSL 固定放在末尾，这里保留以便与抓包逐项对照。
    pub extension_order: &'static [u16],
    /// `http1_only` / `auto`
    pub http_mode: &'static str,
    pub header_orders: &'static [ClaudeCodeHeaderOrder],
    /// 与 BoringSSL 现有能力的已知偏差（映射时无法逐字节复现的点），文档里列成"待核对"。
    pub known_gaps: &'static [&'static str],
}

impl ClaudeCodeTlsEmulationSpec {
    /// 出站 TLS 记录用的 `alpn_offered`。
    pub fn alpn_offered(&self) -> &'static [&'static str] {
        self.alpn.unwrap_or(&[])
    }

    pub fn tls_versions_offered(&self) -> [&'static str; 2] {
        [self.max_tls_version.as_str(), self.min_tls_version.as_str()]
    }

    /// 某类请求的头顺序；没有专门条目时退回第一条。
    pub fn header_order_for(&self, request_kind: &str) -> Option<&'static [&'static str]> {
        self.header_orders
            .iter()
            .find(|order| order.request_kind.eq_ignore_ascii_case(request_kind))
            .or_else(|| self.header_orders.first())
            .map(|order| order.headers)
    }
}

// ---- IANA TLS 扩展编号（RFC 8446 §4.2 / IANA registry） ----
pub const TLS_EXT_SERVER_NAME: u16 = 0;
pub const TLS_EXT_STATUS_REQUEST: u16 = 5;
pub const TLS_EXT_SUPPORTED_GROUPS: u16 = 10;
pub const TLS_EXT_EC_POINT_FORMATS: u16 = 11;
pub const TLS_EXT_SIGNATURE_ALGORITHMS: u16 = 13;
pub const TLS_EXT_ALPN: u16 = 16;
pub const TLS_EXT_SIGNED_CERTIFICATE_TIMESTAMP: u16 = 18;
pub const TLS_EXT_PADDING: u16 = 21;
pub const TLS_EXT_EXTENDED_MASTER_SECRET: u16 = 23;
pub const TLS_EXT_SESSION_TICKET: u16 = 35;
pub const TLS_EXT_PRE_SHARED_KEY: u16 = 41;
pub const TLS_EXT_SUPPORTED_VERSIONS: u16 = 43;
pub const TLS_EXT_PSK_KEY_EXCHANGE_MODES: u16 = 45;
pub const TLS_EXT_KEY_SHARE: u16 = 51;
pub const TLS_EXT_RENEGOTIATION_INFO: u16 = 0xff01;

/// Node/OpenSSL 默认密码套件顺序（17 个，含 TLS 1.3 三个）。来源：CLIProxyAPI 2.1.220 抓包。
const NODE_OPENSSL_CIPHER_LIST: &str = concat!(
    "TLS_AES_128_GCM_SHA256:",
    "TLS_AES_256_GCM_SHA384:",
    "TLS_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA:",
    "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA:",
    "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA:",
    "TLS_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_RSA_WITH_AES_128_CBC_SHA:",
    "TLS_RSA_WITH_AES_256_CBC_SHA"
);

const NODE_OPENSSL_CURVES_LIST: &str = "X25519:P-256:P-384";

/// 9 个签名算法，含 `rsa_pkcs1_sha1`（OpenSSL 默认仍广告 SHA-1）。
const NODE_OPENSSL_SIGALGS_LIST: &str = concat!(
    "ecdsa_secp256r1_sha256:",
    "rsa_pss_rsae_sha256:",
    "rsa_pkcs1_sha256:",
    "ecdsa_secp384r1_sha384:",
    "rsa_pss_rsae_sha384:",
    "rsa_pkcs1_sha384:",
    "rsa_pss_rsae_sha512:",
    "rsa_pkcs1_sha512:",
    "rsa_pkcs1_sha1"
);

const NODE_OPENSSL_ALPN: &[&str] = &["http/1.1"];

/// 推理面扩展顺序（照抄 `claudeCodeTLSClientHelloSpec`）。
const NODE_OPENSSL_EXTENSION_ORDER: &[u16] = &[
    TLS_EXT_SERVER_NAME,
    TLS_EXT_EXTENDED_MASTER_SECRET,
    TLS_EXT_RENEGOTIATION_INFO,
    TLS_EXT_SUPPORTED_GROUPS,
    TLS_EXT_EC_POINT_FORMATS,
    TLS_EXT_SESSION_TICKET,
    TLS_EXT_ALPN,
    TLS_EXT_STATUS_REQUEST,
    TLS_EXT_SIGNATURE_ALGORITHMS,
    TLS_EXT_SIGNED_CERTIFICATE_TIMESTAMP,
    TLS_EXT_KEY_SHARE,
    TLS_EXT_PSK_KEY_EXCHANGE_MODES,
    TLS_EXT_SUPPORTED_VERSIONS,
    TLS_EXT_PADDING,
    TLS_EXT_PRE_SHARED_KEY,
];

/// 控制面扩展顺序（照抄 `claudeOAuthTLSClientHelloSpec`：无 ALPN、无 status_request、无 SCT、无 padding）。
const OAUTH_CONTROL_PLANE_EXTENSION_ORDER: &[u16] = &[
    TLS_EXT_SERVER_NAME,
    TLS_EXT_EXTENDED_MASTER_SECRET,
    TLS_EXT_RENEGOTIATION_INFO,
    TLS_EXT_SUPPORTED_GROUPS,
    TLS_EXT_EC_POINT_FORMATS,
    TLS_EXT_SESSION_TICKET,
    TLS_EXT_SIGNATURE_ALGORITHMS,
    TLS_EXT_KEY_SHARE,
    TLS_EXT_PSK_KEY_EXCHANGE_MODES,
    TLS_EXT_SUPPORTED_VERSIONS,
    TLS_EXT_PRE_SHARED_KEY,
];

/// `/v1/messages` 的头顺序（`claudeCodeMessagesHeaderOrder`）。
const CLAUDE_CODE_MESSAGES_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Authorization",
    "Content-Type",
    "User-Agent",
    "X-Claude-Code-Session-Id",
    "X-Stainless-Arch",
    "X-Stainless-Lang",
    "X-Stainless-OS",
    "X-Stainless-Package-Version",
    "X-Stainless-Retry-Count",
    "X-Stainless-Runtime",
    "X-Stainless-Runtime-Version",
    "X-Stainless-Timeout",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
    "x-client-request-id",
    "Connection",
    "Host",
    "Accept-Encoding",
    "Content-Length",
];

/// `/v1/messages/count_tokens` 的头顺序（比 messages 少 `X-Stainless-Timeout`）。
const CLAUDE_CODE_COUNT_TOKENS_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Authorization",
    "Content-Type",
    "User-Agent",
    "X-Claude-Code-Session-Id",
    "X-Stainless-Arch",
    "X-Stainless-Lang",
    "X-Stainless-OS",
    "X-Stainless-Package-Version",
    "X-Stainless-Retry-Count",
    "X-Stainless-Runtime",
    "X-Stainless-Runtime-Version",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
    "x-client-request-id",
    "Connection",
    "Host",
    "Accept-Encoding",
    "Content-Length",
];

/// Axios 的 token 交换 / 刷新 POST（`claudeOAuthRefreshHeaderOrder`）。
const CLAUDE_CODE_OAUTH_REFRESH_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Content-Type",
    "User-Agent",
    "Content-Length",
    "Accept-Encoding",
    "Host",
    "Connection",
];

/// Axios 的 profile / claude_cli roles GET（`claudeOAuthInspectHeaderOrder`）。
const CLAUDE_CODE_OAUTH_INSPECT_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Content-Type",
    "Authorization",
    "Cache-Control",
    "User-Agent",
    "Accept-Encoding",
    "Host",
    "Connection",
];

pub const CLAUDE_CODE_REQUEST_KIND_MESSAGES: &str = "messages";
pub const CLAUDE_CODE_REQUEST_KIND_COUNT_TOKENS: &str = "count_tokens";
pub const CLAUDE_CODE_REQUEST_KIND_OAUTH_REFRESH: &str = "oauth_refresh";
pub const CLAUDE_CODE_REQUEST_KIND_OAUTH_INSPECT: &str = "oauth_inspect";

const NODE_OPENSSL_KNOWN_GAPS: &[&str] = &[
    "session_ticket 扩展在 BoringSSL 里只有在会话缓存启用时才发出非空 payload；首个 ClientHello 与抓包一致（空 ticket），后续连接可能带 ticket",
    "padding(21) 由 BoringSSL 按长度自动决定是否附加，无法强制与抓包等长",
    "本地身份 profile 是 2.1.161，参数取自 CLIProxyAPI 复刻的 2.1.220，待真实抓包核对",
];

const OAUTH_CONTROL_PLANE_KNOWN_GAPS: &[&str] = &[
    "无 ALPN 扩展依赖 wreq `alpn_protocols = None`；wreq 的 http1_only 会改回 `http/1.1`，因此该 profile 用 http_mode=auto 并靠不发 ALPN 落到 HTTP/1.1",
    "本地身份 profile 是 2.1.161，参数取自 CLIProxyAPI 复刻的 2.1.220，待真实抓包核对",
];

const CHATGPT_COM_CHROME_KNOWN_GAPS: &[&str] = &[
    "直接复用 wreq-util 内置 Chrome 136（含 GREASE / 扩展乱序 / ECH GREASE / ALPS），不逐项核对",
];

/// Claude Code 推理面（api.anthropic.com `/v1/messages`）。
pub const CLAUDE_CODE_NODE_OPENSSL_SPEC: ClaudeCodeTlsEmulationSpec = ClaudeCodeTlsEmulationSpec {
    id: CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL,
    min_tls_version: TlsProtocolVersion::Tls12,
    max_tls_version: TlsProtocolVersion::Tls13,
    cipher_list: NODE_OPENSSL_CIPHER_LIST,
    curves_list: NODE_OPENSSL_CURVES_LIST,
    sigalgs_list: NODE_OPENSSL_SIGALGS_LIST,
    alpn: Some(NODE_OPENSSL_ALPN),
    session_ticket: true,
    renegotiation: true,
    psk_dhe_ke: true,
    ocsp_stapling: true,
    signed_cert_timestamps: true,
    grease: false,
    permute_extensions: false,
    key_shares_limit: 1,
    extension_order: NODE_OPENSSL_EXTENSION_ORDER,
    http_mode: TRANSPORT_HTTP_MODE_HTTP1_ONLY,
    header_orders: &[
        ClaudeCodeHeaderOrder {
            request_kind: CLAUDE_CODE_REQUEST_KIND_MESSAGES,
            headers: CLAUDE_CODE_MESSAGES_HEADER_ORDER,
        },
        ClaudeCodeHeaderOrder {
            request_kind: CLAUDE_CODE_REQUEST_KIND_COUNT_TOKENS,
            headers: CLAUDE_CODE_COUNT_TOKENS_HEADER_ORDER,
        },
    ],
    known_gaps: NODE_OPENSSL_KNOWN_GAPS,
};

/// Claude Code OAuth 控制面（platform.claude.com token、api.anthropic.com profile/roles）。
pub const CLAUDE_CODE_OAUTH_CONTROL_PLANE_SPEC: ClaudeCodeTlsEmulationSpec =
    ClaudeCodeTlsEmulationSpec {
        id: CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE,
        min_tls_version: TlsProtocolVersion::Tls12,
        max_tls_version: TlsProtocolVersion::Tls13,
        cipher_list: NODE_OPENSSL_CIPHER_LIST,
        curves_list: NODE_OPENSSL_CURVES_LIST,
        sigalgs_list: NODE_OPENSSL_SIGALGS_LIST,
        alpn: None,
        session_ticket: true,
        renegotiation: true,
        psk_dhe_ke: true,
        ocsp_stapling: false,
        signed_cert_timestamps: false,
        grease: false,
        permute_extensions: false,
        key_shares_limit: 1,
        extension_order: OAUTH_CONTROL_PLANE_EXTENSION_ORDER,
        http_mode: TRANSPORT_HTTP_MODE_AUTO,
        header_orders: &[
            ClaudeCodeHeaderOrder {
                request_kind: CLAUDE_CODE_REQUEST_KIND_OAUTH_REFRESH,
                headers: CLAUDE_CODE_OAUTH_REFRESH_HEADER_ORDER,
            },
            ClaudeCodeHeaderOrder {
                request_kind: CLAUDE_CODE_REQUEST_KIND_OAUTH_INSPECT,
                headers: CLAUDE_CODE_OAUTH_INSPECT_HEADER_ORDER,
            },
        ],
        known_gaps: OAUTH_CONTROL_PLANE_KNOWN_GAPS,
    };

/// chatgpt.com 控制面：参数由 wreq-util Chrome 136 提供，这里只记录元信息。
pub const CHATGPT_COM_CHROME_SPEC: ClaudeCodeTlsEmulationSpec = ClaudeCodeTlsEmulationSpec {
    id: CHATGPT_COM_CHROME_TLS_PROFILE,
    min_tls_version: TlsProtocolVersion::Tls12,
    max_tls_version: TlsProtocolVersion::Tls13,
    cipher_list: "",
    curves_list: "",
    sigalgs_list: "",
    alpn: Some(&["h2", "http/1.1"]),
    session_ticket: true,
    renegotiation: true,
    psk_dhe_ke: true,
    ocsp_stapling: true,
    signed_cert_timestamps: true,
    grease: true,
    permute_extensions: true,
    key_shares_limit: 0,
    extension_order: &[],
    http_mode: TRANSPORT_HTTP_MODE_AUTO,
    header_orders: &[],
    known_gaps: CHATGPT_COM_CHROME_KNOWN_GAPS,
};

/// 归一化 profile id：不区分大小写，`-` / 空格 视同 `_`。
pub fn normalize_claude_code_tls_profile_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// 按 id 找内置规格；未知 id 返回 `None`（调用方保持原有 `reqwest_rustls` 语义）。
pub fn resolve_claude_code_tls_emulation_spec(
    profile_id: &str,
) -> Option<&'static ClaudeCodeTlsEmulationSpec> {
    match normalize_claude_code_tls_profile_id(profile_id).as_str() {
        CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL => Some(&CLAUDE_CODE_NODE_OPENSSL_SPEC),
        CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE => Some(&CLAUDE_CODE_OAUTH_CONTROL_PLANE_SPEC),
        CHATGPT_COM_CHROME_TLS_PROFILE => Some(&CHATGPT_COM_CHROME_SPEC),
        _ => None,
    }
}

/// 是否是内置 TLS 仿真 profile（供 network.rs / 管理端校验用）。
pub fn is_claude_code_tls_emulation_profile(profile_id: &str) -> bool {
    resolve_claude_code_tls_emulation_spec(profile_id).is_some()
}

/// 供应商类型对应的 OAuth 控制面 profile：claude_code → Node/OpenSSL 控制面，codex → Chrome。
pub fn oauth_control_plane_tls_profile_for_provider_type(
    provider_type: &str,
) -> Option<&'static str> {
    match provider_type.trim().to_ascii_lowercase().as_str() {
        "claude_code" => Some(CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE),
        "codex" => Some(CHATGPT_COM_CHROME_TLS_PROFILE),
        _ => None,
    }
}

/// Provider/key configuration exposes inference profiles only. The OAuth profile
/// is selected internally after resolving a supported configured profile.
pub fn selectable_tls_emulation_profiles_for_provider_type(
    provider_type: &str,
) -> &'static [&'static str] {
    match provider_type.trim().to_ascii_lowercase().as_str() {
        "claude_code" => &[
            CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL,
            CHATGPT_COM_CHROME_TLS_PROFILE,
        ],
        "codex" => &[CHATGPT_COM_CHROME_TLS_PROFILE],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtin_profiles_case_and_separator_insensitively() {
        assert_eq!(
            resolve_claude_code_tls_emulation_spec("Claude-Code-Node-OpenSSL").map(|spec| spec.id),
            Some(CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL)
        );
        assert_eq!(
            resolve_claude_code_tls_emulation_spec(" claude_code_oauth_control_plane ")
                .map(|spec| spec.id),
            Some(CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE)
        );
        assert_eq!(
            resolve_claude_code_tls_emulation_spec("CHATGPT_COM_CHROME").map(|spec| spec.id),
            Some(CHATGPT_COM_CHROME_TLS_PROFILE)
        );
        assert!(resolve_claude_code_tls_emulation_spec("chrome_136").is_none());
        assert!(resolve_claude_code_tls_emulation_spec("claude_code_nodejs").is_none());
        assert!(!is_claude_code_tls_emulation_profile(""));
    }

    #[test]
    fn node_openssl_spec_matches_reference_capture_shape() {
        let spec = &CLAUDE_CODE_NODE_OPENSSL_SPEC;
        assert_eq!(spec.cipher_list.split(':').count(), 17);
        assert!(spec.cipher_list.starts_with("TLS_AES_128_GCM_SHA256:"));
        assert!(spec.cipher_list.ends_with(":TLS_RSA_WITH_AES_256_CBC_SHA"));
        assert_eq!(spec.curves_list, "X25519:P-256:P-384");
        assert_eq!(spec.sigalgs_list.split(':').count(), 9);
        assert!(spec.sigalgs_list.ends_with(":rsa_pkcs1_sha1"));
        // 参考抓包：推理面 ALPN 只有 http/1.1，因此走 http1_only。
        assert_eq!(spec.alpn, Some(&["http/1.1"][..]));
        assert_eq!(spec.http_mode, TRANSPORT_HTTP_MODE_HTTP1_ONLY);
        assert_eq!(spec.alpn_offered(), &["http/1.1"]);
        assert_eq!(spec.tls_versions_offered(), ["TLS1.3", "TLS1.2"]);
        assert!(!spec.grease);
        assert!(!spec.permute_extensions);
        assert_eq!(spec.key_shares_limit, 1);
        assert_eq!(spec.extension_order.first(), Some(&TLS_EXT_SERVER_NAME));
        assert_eq!(spec.extension_order.last(), Some(&TLS_EXT_PRE_SHARED_KEY));
        assert!(spec.extension_order.contains(&TLS_EXT_ALPN));
        assert!(spec.extension_order.contains(&TLS_EXT_STATUS_REQUEST));
        assert!(spec
            .extension_order
            .contains(&TLS_EXT_SIGNED_CERTIFICATE_TIMESTAMP));
    }

    #[test]
    fn oauth_control_plane_spec_sends_no_alpn_and_no_ocsp() {
        let spec = &CLAUDE_CODE_OAUTH_CONTROL_PLANE_SPEC;
        assert_eq!(spec.alpn, None);
        assert!(spec.alpn_offered().is_empty());
        assert!(!spec.ocsp_stapling);
        assert!(!spec.signed_cert_timestamps);
        assert!(!spec.extension_order.contains(&TLS_EXT_ALPN));
        assert!(!spec.extension_order.contains(&TLS_EXT_STATUS_REQUEST));
        assert!(!spec.extension_order.contains(&TLS_EXT_PADDING));
        assert_eq!(spec.http_mode, TRANSPORT_HTTP_MODE_AUTO);
    }

    #[test]
    fn header_orders_follow_native_client_shapes() {
        let spec = &CLAUDE_CODE_NODE_OPENSSL_SPEC;
        let messages = spec
            .header_order_for(CLAUDE_CODE_REQUEST_KIND_MESSAGES)
            .expect("messages order");
        assert_eq!(messages.first(), Some(&"Accept"));
        assert!(messages.contains(&"X-Stainless-Timeout"));
        assert_eq!(messages.last(), Some(&"Content-Length"));
        let count_tokens = spec
            .header_order_for(CLAUDE_CODE_REQUEST_KIND_COUNT_TOKENS)
            .expect("count_tokens order");
        assert!(!count_tokens.contains(&"X-Stainless-Timeout"));
        // 未知类型退回第一条（messages）。
        assert_eq!(spec.header_order_for("unknown"), Some(messages));

        let control = &CLAUDE_CODE_OAUTH_CONTROL_PLANE_SPEC;
        let refresh = control
            .header_order_for(CLAUDE_CODE_REQUEST_KIND_OAUTH_REFRESH)
            .expect("refresh order");
        assert_eq!(refresh, CLAUDE_CODE_OAUTH_REFRESH_HEADER_ORDER);
        let inspect = control
            .header_order_for(CLAUDE_CODE_REQUEST_KIND_OAUTH_INSPECT)
            .expect("inspect order");
        assert_eq!(inspect[2], "Authorization");
        assert_eq!(inspect[3], "Cache-Control");

        assert_eq!(CHATGPT_COM_CHROME_SPEC.header_order_for("messages"), None);
    }

    #[test]
    fn control_plane_profile_follows_provider_type() {
        assert_eq!(
            oauth_control_plane_tls_profile_for_provider_type("claude_code"),
            Some(CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE)
        );
        assert_eq!(
            oauth_control_plane_tls_profile_for_provider_type(" Codex "),
            Some(CHATGPT_COM_CHROME_TLS_PROFILE)
        );
        assert_eq!(
            oauth_control_plane_tls_profile_for_provider_type("gemini_cli"),
            None
        );
    }

    #[test]
    fn every_builtin_profile_lists_its_known_gaps() {
        for id in CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS {
            let spec = resolve_claude_code_tls_emulation_spec(id).expect("builtin profile");
            assert_eq!(spec.id, *id);
            assert!(
                !spec.known_gaps.is_empty(),
                "{id} must document its unverified parameters"
            );
        }
    }
}
