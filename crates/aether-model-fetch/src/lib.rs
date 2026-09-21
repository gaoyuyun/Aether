mod association_sync;
mod config;
mod logic;
mod onboarding;
mod preset_catalog;
mod strategy;
mod transport;

pub use association_sync::{
    sync_provider_model_whitelist_associations, ModelFetchAssociationStore,
};
pub use config::{
    model_fetch_interval_minutes, model_fetch_startup_delay_seconds, model_fetch_startup_enabled,
};
pub use logic::{
    aggregate_models_for_cache, apply_model_filters, build_models_fetch_url,
    build_models_fetch_url_for_client_version, deepseek_anthropic_models_fetch_uses_openai_auth,
    endpoint_supports_rust_models_fetch, extract_error_message, json_string_list,
    merge_upstream_metadata, model_catalog_upstream_metadata, parse_models_response,
    parse_models_response_page, parse_windsurf_model_configs_response, preset_models_for_provider,
    project_codex_models_for_legacy_cache, provider_type_uses_preset_models,
    select_models_fetch_endpoint, selected_models_fetch_endpoints,
    upstream_metadata_namespace_updates, ModelFetchRunSummary, ModelsFetchPage, ModelsFetchSuccess,
};
pub use onboarding::{
    classify_cloud_code_onboard_response, cloud_code_onboard_tier_id,
    extract_cloud_code_project_id, onboard_cloud_code_user, CloudCodeOnboardPoll,
    CloudCodeOnboardingClient, CloudCodeOnboardingOutcome, CLOUD_CODE_ONBOARD_MAX_ATTEMPTS,
    CLOUD_CODE_ONBOARD_POLL_INTERVAL,
};
pub use preset_catalog::{
    apply_remote_preset_model_catalog, current_preset_model_catalog, embedded_preset_model_catalog,
    parse_preset_model_catalog, preset_model_catalog_refresh_enabled,
    preset_model_catalog_refresh_minutes, preset_model_catalog_urls,
    reset_preset_model_catalog_to_embedded, ParsedPresetModelCatalog, PresetModelCatalog,
    PresetModelCatalogUpdate, PRESET_CATALOG_PROVIDER_TYPES,
};
pub use strategy::{
    antigravity_model_id_is_routable, fetch_models_from_transports,
    fetch_models_from_transports_for_client_version, fetch_models_from_transports_for_management,
    hydrate_antigravity_project, hydrate_gemini_cli_project, AntigravityProjectHydration,
    GeminiCliProjectHydration, ModelFetchStrategy, ModelFetchStrategyKind, ModelsFetchOutcome,
    SelectedModelFetchStrategy,
};
pub use transport::{
    build_antigravity_fetch_available_models_plan, build_antigravity_load_code_assist_plan,
    build_antigravity_onboard_user_plan, build_gemini_cli_load_code_assist_plan,
    build_gemini_cli_onboard_user_plan, build_kiro_list_available_models_plan,
    build_models_fetch_execution_plan, build_models_fetch_execution_plan_for_client_version,
    build_standard_models_fetch_execution_plan,
    build_standard_models_fetch_execution_plan_for_client_version,
    build_vertex_models_fetch_execution_plan, build_windsurf_model_configs_execution_plan,
    ModelFetchTransportRuntime,
};
