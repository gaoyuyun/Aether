//! 预设模型目录：给上游不提供模型列表接口的渠道类型（claude_code、grok、gemini_cli，
//! 以及 kiro 无端点时的兜底）提供模型清单。
//!
//! 目录有两层来源：编译期内嵌的 `preset_models.json`，以及网关后台从远程仓库
//! 拉取的同格式文件。远程拉取成功且通过校验后覆盖内嵌副本；任何一次校验失败都
//! 保留当前生效的目录，不会让渠道模型列表变空。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};

const EMBEDDED_PRESET_MODELS_JSON: &str = include_str!("preset_models.json");
const PRESET_CATALOG_SCHEMA_VERSION: u64 = 1;
const PRESET_CATALOG_MAX_MODELS_PER_PROVIDER: usize = 512;

/// 允许通过目录配置的渠道类型。其它键会被忽略并在解析结果里报告。
pub const PRESET_CATALOG_PROVIDER_TYPES: &[&str] = &["claude_code", "gemini_cli", "grok", "kiro"];

const PRESET_MODEL_CATALOG_URLS_DEFAULT: &[&str] = &[
    "https://raw.githubusercontent.com/gaoyuyun/aether-models/main/models.json",
    "https://cdn.jsdelivr.net/gh/gaoyuyun/aether-models@main/models.json",
];
const PRESET_MODEL_CATALOG_REFRESH_MINUTES_DEFAULT: u64 = 180;
const PRESET_MODEL_CATALOG_REFRESH_MINUTES_MIN: u64 = 30;
const PRESET_MODEL_CATALOG_REFRESH_MINUTES_MAX: u64 = 10080;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetModelCatalog {
    pub updated_at: Option<String>,
    providers: BTreeMap<String, Vec<Value>>,
}

impl PresetModelCatalog {
    pub fn models_for_provider(&self, provider_type: &str) -> Option<&[Value]> {
        self.providers
            .get(&normalize_provider_type(provider_type))
            .map(Vec::as_slice)
    }

    pub fn provider_types(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}

/// 一次目录解析的结果。`ignored_provider_types` 是文件里出现但网关不认识的键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPresetModelCatalog {
    pub catalog: PresetModelCatalog,
    pub ignored_provider_types: Vec<String>,
}

/// 应用远程目录后的变化摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetModelCatalogUpdate {
    pub changed_provider_types: Vec<String>,
    pub ignored_provider_types: Vec<String>,
    pub updated_at: Option<String>,
}

fn normalize_provider_type(provider_type: &str) -> String {
    provider_type.trim().to_ascii_lowercase()
}

fn catalog_store() -> &'static RwLock<Arc<PresetModelCatalog>> {
    static STORE: OnceLock<RwLock<Arc<PresetModelCatalog>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(Arc::new(embedded_preset_model_catalog())))
}

/// 编译进二进制的目录副本。内嵌文件在测试里被校验，这里解析失败属于构建错误。
pub fn embedded_preset_model_catalog() -> PresetModelCatalog {
    parse_preset_model_catalog(EMBEDDED_PRESET_MODELS_JSON)
        .expect("embedded preset_models.json must be valid")
        .catalog
}

/// 当前生效的目录快照。
pub fn current_preset_model_catalog() -> Arc<PresetModelCatalog> {
    catalog_store()
        .read()
        .map(|guard| Arc::clone(&guard))
        .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
}

/// 解析并校验远程目录文本，通过后替换当前目录，返回发生变化的渠道类型。
pub fn apply_remote_preset_model_catalog(text: &str) -> Result<PresetModelCatalogUpdate, String> {
    let parsed = parse_preset_model_catalog(text)?;
    let store = catalog_store();
    let mut guard = store
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = Arc::clone(&guard);
    let changed_provider_types = PRESET_CATALOG_PROVIDER_TYPES
        .iter()
        .filter(|provider_type| {
            previous.models_for_provider(provider_type)
                != parsed.catalog.models_for_provider(provider_type)
        })
        .map(|provider_type| (*provider_type).to_string())
        .collect::<Vec<_>>();
    let updated_at = parsed.catalog.updated_at.clone();
    *guard = Arc::new(parsed.catalog);
    Ok(PresetModelCatalogUpdate {
        changed_provider_types,
        ignored_provider_types: parsed.ignored_provider_types,
        updated_at,
    })
}

/// 仅测试用：恢复为内嵌目录，避免用例之间互相污染。
pub fn reset_preset_model_catalog_to_embedded() {
    let store = catalog_store();
    let mut guard = store
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Arc::new(embedded_preset_model_catalog());
}

pub fn parse_preset_model_catalog(text: &str) -> Result<ParsedPresetModelCatalog, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|error| format!("目录不是合法 JSON：{error}"))?;
    let root = root
        .as_object()
        .ok_or_else(|| "目录顶层必须是对象".to_string())?;

    match root.get("schema_version").and_then(Value::as_u64) {
        Some(PRESET_CATALOG_SCHEMA_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "不支持的 schema_version {other}，当前网关只接受 {PRESET_CATALOG_SCHEMA_VERSION}"
            ))
        }
        None => return Err("缺少 schema_version".to_string()),
    }

    let updated_at = root
        .get("updated_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let providers = root
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| "providers 必须是对象".to_string())?;

    let mut parsed_providers = BTreeMap::new();
    let mut ignored_provider_types = Vec::new();
    for (raw_provider_type, raw_models) in providers {
        let provider_type = normalize_provider_type(raw_provider_type);
        if !PRESET_CATALOG_PROVIDER_TYPES.contains(&provider_type.as_str()) {
            ignored_provider_types.push(raw_provider_type.clone());
            continue;
        }
        let models = parse_provider_models(&provider_type, raw_models)?;
        parsed_providers.insert(provider_type, models);
    }

    for required in PRESET_CATALOG_PROVIDER_TYPES {
        if !parsed_providers.contains_key(*required) {
            return Err(format!("providers 缺少渠道类型 {required}"));
        }
    }

    Ok(ParsedPresetModelCatalog {
        catalog: PresetModelCatalog {
            updated_at,
            providers: parsed_providers,
        },
        ignored_provider_types,
    })
}

fn parse_provider_models(provider_type: &str, raw_models: &Value) -> Result<Vec<Value>, String> {
    let items = raw_models
        .as_array()
        .ok_or_else(|| format!("providers.{provider_type} 必须是数组"))?;
    if items.is_empty() {
        return Err(format!("providers.{provider_type} 不能为空"));
    }
    if items.len() > PRESET_CATALOG_MAX_MODELS_PER_PROVIDER {
        return Err(format!(
            "providers.{provider_type} 超过 {PRESET_CATALOG_MAX_MODELS_PER_PROVIDER} 个模型"
        ));
    }

    let mut seen = BTreeSet::new();
    let mut models = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let path = format!("providers.{provider_type}[{index}]");
        let object = item
            .as_object()
            .ok_or_else(|| format!("{path} 必须是对象"))?;
        let model_id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{path}.id 必填且非空"))?;
        if !seen.insert(model_id.to_string()) {
            return Err(format!("{path}.id 重复：{model_id}"));
        }
        let api_formats = object
            .get("api_formats")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{path}.api_formats 必须是数组"))?;
        if api_formats.is_empty()
            || api_formats
                .iter()
                .any(|format| format.as_str().map(str::trim).is_none_or(str::is_empty))
        {
            return Err(format!("{path}.api_formats 必须是非空字符串数组"));
        }
        let owned_by = object
            .get("owned_by")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown");
        let display_name = object
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(model_id);

        let mut model = object.clone();
        model.insert("id".to_string(), json!(model_id));
        model.insert("object".to_string(), json!("model"));
        model.insert("owned_by".to_string(), json!(owned_by));
        model.insert("display_name".to_string(), json!(display_name));
        model.insert(
            "api_formats".to_string(),
            Value::Array(
                api_formats
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|format| json!(format.trim()))
                    .collect(),
            ),
        );
        models.push(Value::Object(model));
    }
    Ok(models)
}

pub fn preset_model_catalog_urls() -> Vec<String> {
    let configured = std::env::var("PRESET_MODEL_CATALOG_URLS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        PRESET_MODEL_CATALOG_URLS_DEFAULT
            .iter()
            .map(|url| (*url).to_string())
            .collect()
    } else {
        configured
    }
}

pub fn preset_model_catalog_refresh_minutes() -> u64 {
    std::env::var("PRESET_MODEL_CATALOG_REFRESH_MINUTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| {
            value.clamp(
                PRESET_MODEL_CATALOG_REFRESH_MINUTES_MIN,
                PRESET_MODEL_CATALOG_REFRESH_MINUTES_MAX,
            )
        })
        .unwrap_or(PRESET_MODEL_CATALOG_REFRESH_MINUTES_DEFAULT)
}

pub fn preset_model_catalog_refresh_enabled() -> bool {
    std::env::var("PRESET_MODEL_CATALOG_REFRESH_ENABLED")
        .ok()
        .map(|value| !value.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        apply_remote_preset_model_catalog, current_preset_model_catalog,
        embedded_preset_model_catalog, parse_preset_model_catalog,
        reset_preset_model_catalog_to_embedded, PRESET_CATALOG_PROVIDER_TYPES,
    };

    fn full_catalog_with(provider_type: &str, models: serde_json::Value) -> String {
        let embedded = embedded_preset_model_catalog();
        let mut providers = serde_json::Map::new();
        for required in PRESET_CATALOG_PROVIDER_TYPES {
            providers.insert(
                (*required).to_string(),
                serde_json::Value::Array(
                    embedded
                        .models_for_provider(required)
                        .unwrap_or_default()
                        .to_vec(),
                ),
            );
        }
        providers.insert(provider_type.to_string(), models);
        json!({"schema_version": 1, "updated_at": "2026-09-19", "providers": providers}).to_string()
    }

    #[test]
    fn embedded_catalog_covers_every_supported_provider_type() {
        let catalog = embedded_preset_model_catalog();
        for provider_type in PRESET_CATALOG_PROVIDER_TYPES {
            let models = catalog
                .models_for_provider(provider_type)
                .unwrap_or_else(|| panic!("{provider_type} missing"));
            assert!(!models.is_empty(), "{provider_type} must not be empty");
            for model in models {
                assert_eq!(model["object"], json!("model"));
                assert!(model["id"].as_str().is_some_and(|id| !id.is_empty()));
                assert!(model["api_formats"]
                    .as_array()
                    .is_some_and(|formats| !formats.is_empty()));
            }
        }
    }

    #[test]
    fn parse_rejects_missing_schema_version() {
        let error = parse_preset_model_catalog(r#"{"providers":{}}"#).unwrap_err();
        assert!(error.contains("schema_version"), "{error}");
    }

    #[test]
    fn parse_rejects_unknown_schema_version() {
        let error =
            parse_preset_model_catalog(r#"{"schema_version":2,"providers":{}}"#).unwrap_err();
        assert!(error.contains("schema_version 2"), "{error}");
    }

    #[test]
    fn parse_rejects_missing_required_provider() {
        let error = parse_preset_model_catalog(
            r#"{"schema_version":1,"providers":{"grok":[{"id":"g","api_formats":["openai:chat"]}]}}"#,
        )
        .unwrap_err();
        assert!(error.contains("缺少渠道类型"), "{error}");
    }

    #[test]
    fn parse_rejects_duplicate_model_ids() {
        let text = full_catalog_with(
            "grok",
            json!([
                {"id": "grok-1", "api_formats": ["openai:chat"]},
                {"id": "grok-1", "api_formats": ["openai:chat"]}
            ]),
        );
        let error = parse_preset_model_catalog(&text).unwrap_err();
        assert!(error.contains("重复"), "{error}");
    }

    #[test]
    fn parse_rejects_empty_api_formats() {
        let text = full_catalog_with("grok", json!([{"id": "grok-1", "api_formats": []}]));
        let error = parse_preset_model_catalog(&text).unwrap_err();
        assert!(error.contains("api_formats"), "{error}");
    }

    #[test]
    fn parse_rejects_empty_provider_list() {
        let text = full_catalog_with("grok", json!([]));
        let error = parse_preset_model_catalog(&text).unwrap_err();
        assert!(error.contains("不能为空"), "{error}");
    }

    #[test]
    fn parse_reports_ignored_provider_types_and_normalizes_models() {
        let mut text: serde_json::Value = serde_json::from_str(&full_catalog_with(
            "grok",
            json!([{"id": "  grok-1 ", "api_formats": [" openai:chat "], "extra": true}]),
        ))
        .unwrap();
        text["providers"]["codex"] = json!([{"id": "x", "api_formats": ["openai:chat"]}]);
        let parsed = parse_preset_model_catalog(&text.to_string()).unwrap();
        assert_eq!(parsed.ignored_provider_types, vec!["codex".to_string()]);
        let grok = parsed.catalog.models_for_provider("GROK").unwrap();
        assert_eq!(
            grok[0],
            json!({
                "id": "grok-1",
                "object": "model",
                "owned_by": "unknown",
                "display_name": "grok-1",
                "api_formats": ["openai:chat"],
                "extra": true
            })
        );
        assert_eq!(parsed.catalog.updated_at.as_deref(), Some("2026-09-19"));
    }

    #[test]
    fn apply_remote_catalog_reports_changed_providers_and_keeps_current_on_error() {
        reset_preset_model_catalog_to_embedded();
        let before = current_preset_model_catalog();

        let text = full_catalog_with(
            "grok",
            json!([{"id": "grok-next", "owned_by": "xai", "display_name": "Grok Next", "api_formats": ["openai:chat"]}]),
        );
        let update = apply_remote_preset_model_catalog(&text).unwrap();
        assert_eq!(update.changed_provider_types, vec!["grok".to_string()]);
        let after = current_preset_model_catalog();
        assert_eq!(after.models_for_provider("grok").unwrap().len(), 1);
        assert_eq!(
            after.models_for_provider("kiro"),
            before.models_for_provider("kiro")
        );

        let error = apply_remote_preset_model_catalog("{").unwrap_err();
        assert!(error.contains("JSON"), "{error}");
        assert_eq!(
            current_preset_model_catalog()
                .models_for_provider("grok")
                .unwrap()
                .len(),
            1,
            "invalid remote catalog must not replace the current one"
        );

        let unchanged = apply_remote_preset_model_catalog(&text).unwrap();
        assert!(unchanged.changed_provider_types.is_empty());

        reset_preset_model_catalog_to_embedded();
        assert_eq!(*current_preset_model_catalog(), *before);
    }
}
