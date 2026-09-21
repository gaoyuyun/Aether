mod auth;
pub mod beta;
pub mod cache_control;
pub mod client_detection;
pub mod cloak;
mod fingerprint;
pub mod identity;
mod policy;
mod profile;
mod request;
pub mod signing;
pub mod thinking;
pub mod tls_profile;
mod url;

pub use auth::supports_local_claude_code_auth;
pub use fingerprint::{
    generate_fingerprint, generate_random_fingerprint, header_fingerprint_from_fingerprint,
    sanitize_fingerprint,
};
pub use policy::{
    local_claude_code_transport_unsupported_reason_with_network,
    supports_local_claude_code_transport_with_network,
};
pub use profile::{
    current_claude_code_transport_identity_profile, ClaudeCodeBodyCapabilityGate,
    ClaudeCodeTransportIdentityProfile, ClaudeCodeTransportIdentityProfileVersion,
    CLAUDE_CODE_CONTEXT_MANAGEMENT_BETA, CLAUDE_CODE_TRANSPORT_IDENTITY_2026_04,
};
pub use request::{
    build_claude_code_passthrough_headers, sanitize_claude_code_request_body,
    sanitize_claude_code_request_body_for_beta_header,
};
pub use url::build_claude_code_messages_url;

pub use beta::{
    assemble_claude_code_beta_header, claude_code_request_is_subagent, ClaudeCodeBetaContext,
};
pub use cache_control::{
    apply_claude_code_cache_control_policy, ClaudeCodeCacheControlPolicy,
    ClaudeCodeCacheControlSummary, CLAUDE_CODE_MAX_CACHE_CONTROL_BREAKPOINTS,
};
pub use client_detection::{
    claude_code_cloak_applies, claude_code_metadata_user_id_is_native, detect_claude_code_client,
    resolve_claude_code_cloak_mode, ClaudeCodeClientDetection, ClaudeCodeClientKind,
    ClaudeCodeCloakMode, CLAUDE_CODE_CLOAK_CONFIG_KEY, CLAUDE_CODE_CLOAK_MODE_CONFIG_KEY,
};
pub use cloak::{
    apply_claude_code_cloak_pipeline, ClaudeCodeCloakPipeline, ClaudeCodeCloakReport,
    SensitiveWordsHook,
};
pub use identity::{
    apply_claude_code_metadata_user_id, derive_claude_code_account_uuid,
    derive_claude_code_device_id, derive_claude_code_session_id,
    resolve_claude_code_device_profile, ClaudeCodeDeviceBaseline, ClaudeCodeDeviceCandidate,
    ClaudeCodeDeviceProfile, ClaudeCodeDeviceResolution, ClaudeCodeIdentityInput,
    CLAUDE_CODE_DEVICE_PROFILE_KEY, CLAUDE_CODE_DEVICE_PROFILE_TTL_SECS,
};
pub use signing::{
    build_claude_code_fallback_billing_header, claude_code_cch_from_body,
    ensure_claude_code_billing_cch_placeholder, sign_claude_code_request_body,
    sign_claude_code_request_bytes, ClaudeCodeSignedBody, ClaudeCodeSigningError,
    CLAUDE_CODE_CCH_SEED,
};
pub use thinking::{normalize_claude_code_thinking, ClaudeCodeThinkingSummary};
pub use tls_profile::{
    is_claude_code_tls_emulation_profile, normalize_claude_code_tls_profile_id,
    oauth_control_plane_tls_profile_for_provider_type, resolve_claude_code_tls_emulation_spec,
    selectable_tls_emulation_profiles_for_provider_type, ClaudeCodeHeaderOrder,
    ClaudeCodeTlsEmulationSpec, TlsProtocolVersion, CHATGPT_COM_CHROME_BROWSER_PROFILE,
    CHATGPT_COM_CHROME_SPEC, CHATGPT_COM_CHROME_TLS_PROFILE, CLAUDE_CODE_NODE_OPENSSL_SPEC,
    CLAUDE_CODE_OAUTH_CONTROL_PLANE_SPEC, CLAUDE_CODE_REQUEST_KIND_COUNT_TOKENS,
    CLAUDE_CODE_REQUEST_KIND_MESSAGES, CLAUDE_CODE_REQUEST_KIND_OAUTH_INSPECT,
    CLAUDE_CODE_REQUEST_KIND_OAUTH_REFRESH, CLAUDE_CODE_TLS_EMULATION_PROFILE_IDS,
    CLAUDE_CODE_TLS_PROFILE_NODE_OPENSSL, CLAUDE_CODE_TLS_PROFILE_OAUTH_CONTROL_PLANE,
};
