pub(crate) use self::batch::parse_admin_provider_key_batch_update_patch;
pub(crate) use self::create::build_admin_create_provider_key_record;
pub(crate) use self::payload::build_admin_provider_keys_page_payload;
pub(crate) use self::payload::build_admin_provider_keys_payload;
pub(crate) use self::update::build_admin_update_provider_key_record;
pub(crate) use self::update::{
    admin_provider_key_update_requires_immediate_model_fetch,
    build_admin_update_provider_key_record_with_existing_keys,
    build_provider_catalog_key_admin_cas_update,
};

mod batch;
mod create;
mod payload;
mod update;

fn normalize_auth_config_sensitive_words(
    auth_config: &mut Option<serde_json::Value>,
    provider_type: &str,
) -> Result<(), String> {
    use super::provider::{
        normalize_cloak_sensitive_words, provider_type_supports_sensitive_words,
    };
    use aether_provider_transport::CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY;

    let Some(object) = auth_config
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };
    let Some(raw_words) = object.get(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY) else {
        return Ok(());
    };
    if raw_words.is_null() {
        object.remove(CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY);
        return Ok(());
    }
    if !provider_type_supports_sensitive_words(provider_type) {
        return Err(
            "cloak_sensitive_words 仅适用于 provider_type=claude_code / antigravity 的 Key"
                .to_string(),
        );
    }
    let invalid_shape = || "auth_config.cloak_sensitive_words 必须是字符串数组".to_string();
    let words = raw_words
        .as_array()
        .ok_or_else(invalid_shape)?
        .iter()
        .map(|word| {
            word.as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(invalid_shape)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let normalized = normalize_cloak_sensitive_words(&words)?;
    object.insert(
        CLOAK_SENSITIVE_WORDS_AUTH_CONFIG_KEY.to_string(),
        serde_json::json!(normalized),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn raw_auth_config_words_use_the_same_validation_as_the_typed_field() {
        let mut config =
            Some(json!({"refresh_token": "keep", "cloak_sensitive_words": ["Proxy", "proxy"]}));
        super::normalize_auth_config_sensitive_words(&mut config, "claude_code").unwrap();
        assert_eq!(
            config,
            Some(json!({"refresh_token": "keep", "cloak_sensitive_words": ["proxy"]}))
        );
        for invalid in [
            json!("proxy"),
            json!([123]),
            json!(["x"]),
            json!(["界".repeat(257)]),
        ] {
            let mut config = Some(json!({"cloak_sensitive_words": invalid}));
            assert!(
                super::normalize_auth_config_sensitive_words(&mut config, "claude_code").is_err()
            );
        }
        assert!(super::normalize_auth_config_sensitive_words(&mut config, "codex").is_err());
        let mut config = Some(json!({"refresh_token": "keep", "cloak_sensitive_words": null}));
        super::normalize_auth_config_sensitive_words(&mut config, "codex").unwrap();
        assert_eq!(config, Some(json!({"refresh_token": "keep"})));
    }
}
