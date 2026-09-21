mod auth;
mod policy;
mod request;
mod url;
mod version;

pub use auth::{
    antigravity_request_user_agent, build_antigravity_static_client_headers,
    build_antigravity_static_identity_headers, resolve_local_antigravity_request_auth,
    AntigravityRequestAuth, AntigravityRequestAuthSupport, AntigravityRequestAuthUnsupportedReason,
    ANTIGRAVITY_CLIENT_VERSION, ANTIGRAVITY_PROVIDER_TYPE, ANTIGRAVITY_REQUEST_USER_AGENT,
};
pub use policy::{
    classify_local_antigravity_request_support, is_antigravity_provider_transport,
    AntigravityRequestSideSpec, AntigravityRequestSideSupport,
    AntigravityRequestSideUnsupportedReason,
};
pub use request::{
    antigravity_model_is_claude, antigravity_model_max_output_tokens,
    build_antigravity_safe_v1internal_request,
    build_antigravity_safe_v1internal_request_with_policy, classify_antigravity_safe_request_body,
    AntigravityEnvelopeRequestType, AntigravityRequestEnvelopeSupport,
    AntigravityRequestEnvelopeUnsupportedReason, AntigravityRequestPolicy,
};
pub use url::{
    build_antigravity_v1internal_url, AntigravityRequestUrlAction,
    ANTIGRAVITY_V1INTERNAL_PATH_TEMPLATE,
};
pub use version::{
    antigravity_client_version, antigravity_client_version_state,
    antigravity_request_user_agent_for_version, parse_antigravity_client_version,
    parse_antigravity_hub_manifest_version, resolve_antigravity_client_version,
    validate_antigravity_client_version, AntigravityClientVersionError,
    AntigravityClientVersionState, ANTIGRAVITY_CLIENT_VERSION_SYSTEM_CONFIG_KEY,
    ANTIGRAVITY_HUB_LATEST_MANIFEST_URL, ANTIGRAVITY_MIN_CLIENT_VERSION,
};
