use std::collections::BTreeSet;

use super::{
    AdminGlobalModelListQuery, DataLayerError, GatewayDataState, StoredAuthApiKeyExportRecord,
};
use crate::ai_serving::api::api_format_permission_covers;

#[derive(Default)]
struct AccessRestrictionCatalog {
    providers: Option<BTreeSet<String>>,
    api_formats: Option<BTreeSet<String>>,
    models: Option<BTreeSet<String>>,
}

impl AccessRestrictionCatalog {
    fn prune(&self, record: &mut StoredAuthApiKeyExportRecord) {
        if let Some(providers) = &self.providers {
            for list in [&mut record.allowed_providers, &mut record.denied_providers]
                .into_iter()
                .flatten()
            {
                list.retain(|value| providers.contains(&value.trim().to_ascii_lowercase()));
            }
        }
        if let Some(formats) = &self.api_formats {
            for list in [
                &mut record.allowed_api_formats,
                &mut record.denied_api_formats,
            ]
            .into_iter()
            .flatten()
            {
                list.retain(|value| {
                    formats.iter().any(|format| {
                        api_format_permission_covers(value, format)
                            || api_format_permission_covers(format, value)
                    })
                });
            }
        }
        if let Some(models) = &self.models {
            for list in [&mut record.allowed_models, &mut record.denied_models]
                .into_iter()
                .flatten()
            {
                list.retain(|value| models.contains(value));
            }
        }
        // Preserve Some([]): an emptied allowlist must continue to deny all access.
    }
}

impl GatewayDataState {
    async fn read_access_restriction_catalog(
        &self,
    ) -> Result<AccessRestrictionCatalog, DataLayerError> {
        let mut catalog = AccessRestrictionCatalog::default();
        if let Some(reader) = &self.provider_catalog_reader {
            // Include disabled entries: only deletion removes a stored restriction.
            let providers = reader.list_provider_identities(false).await?;
            let provider_ids = providers
                .iter()
                .map(|provider| provider.id.clone())
                .collect::<Vec<_>>();
            catalog.providers = Some(
                providers
                    .into_iter()
                    .flat_map(|provider| [provider.id, provider.name, provider.provider_type])
                    .map(|value| value.trim().to_ascii_lowercase())
                    .collect(),
            );
            catalog.api_formats = Some(
                reader
                    .list_endpoint_identities_by_provider_ids(&provider_ids)
                    .await?
                    .into_iter()
                    .map(|endpoint| endpoint.api_format)
                    .collect(),
            );
        }
        if let Some(reader) = &self.global_model_reader {
            let mut models = BTreeSet::new();
            let mut offset = 0;
            loop {
                let page = reader
                    .list_admin_global_models(&AdminGlobalModelListQuery {
                        offset,
                        limit: 1000,
                        is_active: None,
                        search: None,
                    })
                    .await?;
                let count = page.items.len();
                models.extend(page.items.into_iter().map(|model| model.name));
                offset += count;
                if count == 0 || offset >= page.total {
                    break;
                }
            }
            catalog.models = Some(models);
        }
        Ok(catalog)
    }

    pub(crate) async fn clean_user_api_key_access_records(
        &self,
        mut records: Vec<StoredAuthApiKeyExportRecord>,
    ) -> Result<Vec<StoredAuthApiKeyExportRecord>, DataLayerError> {
        let Some(writer) = &self.auth_api_key_writer else {
            return Ok(records);
        };
        if !records.iter().any(|record| {
            !record.is_standalone
                && (record.allowed_providers.is_some()
                    || record.allowed_api_formats.is_some()
                    || record.allowed_models.is_some()
                    || record.denied_providers.is_some()
                    || record.denied_api_formats.is_some()
                    || record.denied_models.is_some())
        }) {
            return Ok(records);
        }
        // Read keys before the catalog so newly created resources cannot be mistaken for deletions.
        let catalog = self.read_access_restriction_catalog().await?;
        let mut changed = false;
        for record in &mut records {
            if record.is_standalone {
                continue;
            }
            let mut replacement = record.clone();
            catalog.prune(&mut replacement);
            if replacement == *record {
                continue;
            }
            if writer
                .compare_and_swap_user_api_key_access_lists(record, &replacement)
                .await?
            {
                *record = replacement;
                changed = true;
            }
        }
        if changed {
            self.clear_auth_api_key_read_cache();
        }
        Ok(records)
    }

    pub(crate) async fn prune_user_api_key_access_restrictions(
        &self,
    ) -> Result<(), DataLayerError> {
        if let Some(reader) = &self.auth_api_key_reader {
            let records = reader.list_user_api_keys_with_access_restrictions().await?;
            self.clean_user_api_key_access_records(records).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::repository::auth::{
        AuthApiKeyReadRepository, InMemoryAuthApiKeySnapshotRepository, StoredAuthApiKeySnapshot,
    };
    use aether_data::repository::global_models::InMemoryGlobalModelReadRepository;
    use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
    use aether_data_contracts::repository::global_models::StoredAdminGlobalModel;
    use aether_data_contracts::repository::provider_catalog::{
        StoredProviderCatalogEndpoint, StoredProviderCatalogProvider,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn key_snapshot() -> StoredAuthApiKeySnapshot {
        StoredAuthApiKeySnapshot::new(
            "user".into(),
            "user".into(),
            None,
            "user".into(),
            "local".into(),
            true,
            false,
            None,
            None,
            None,
            "key".into(),
            Some("restricted".into()),
            true,
            false,
            false,
            None,
            None,
            None,
            Some(json!(["removed", "disabled"])),
            Some(json!(["openai:responses", "claude:messages"])),
            Some(json!(["old-model", "disabled-model"])),
        )
        .unwrap()
        .with_denied_lists(
            Some(json!(["removed", "disabled"])),
            Some(json!(["openai:responses", "claude:messages"])),
            Some(json!(["old-model", "disabled-model"])),
        )
        .unwrap()
    }

    fn global_model(name: &str, active: bool) -> StoredAdminGlobalModel {
        StoredAdminGlobalModel::new(
            name.into(),
            name.into(),
            name.into(),
            active,
            None,
            None,
            None,
            None,
            0,
            0,
            0,
            None,
            None,
        )
        .unwrap()
    }

    async fn stored_key(
        repository: &InMemoryAuthApiKeySnapshotRepository,
    ) -> StoredAuthApiKeyExportRecord {
        repository
            .list_export_api_keys_by_ids(&["key".into()])
            .await
            .unwrap()
            .remove(0)
    }

    #[tokio::test]
    async fn catalog_deletions_prune_both_lists_and_keep_disabled_and_shared_entries() {
        let keys = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
            Some("hash".into()),
            key_snapshot(),
        )]));
        let mut disabled = StoredProviderCatalogProvider::new(
            "disabled".into(),
            "Disabled".into(),
            None,
            "anthropic".into(),
        )
        .unwrap();
        disabled.is_active = false;
        let providers = Arc::new(InMemoryProviderCatalogReadRepository::seed(
            vec![
                StoredProviderCatalogProvider::new(
                    "removed".into(),
                    "Removed".into(),
                    None,
                    "openai".into(),
                )
                .unwrap(),
                StoredProviderCatalogProvider::new(
                    "kept".into(),
                    "Kept".into(),
                    None,
                    "openai".into(),
                )
                .unwrap(),
                disabled,
            ],
            vec![
                StoredProviderCatalogEndpoint::new(
                    "removed-endpoint".into(),
                    "removed".into(),
                    "openai:responses".into(),
                    None,
                    None,
                    true,
                )
                .unwrap(),
                StoredProviderCatalogEndpoint::new(
                    "shared-endpoint".into(),
                    "kept".into(),
                    "openai:responses".into(),
                    None,
                    None,
                    true,
                )
                .unwrap(),
                StoredProviderCatalogEndpoint::new(
                    "disabled-endpoint".into(),
                    "disabled".into(),
                    "claude:messages".into(),
                    None,
                    None,
                    false,
                )
                .unwrap(),
            ],
            Vec::new(),
        ));
        let models = Arc::new(
            InMemoryGlobalModelReadRepository::seed(Vec::new()).with_admin_global_models([
                global_model("old-model", true),
                global_model("disabled-model", false),
            ]),
        );
        let mut state = GatewayDataState::with_auth_api_key_repository_for_tests(keys.clone());
        state.provider_catalog_reader = Some(providers.clone());
        state.provider_catalog_writer = Some(providers);
        state.global_model_reader = Some(models.clone());
        state.global_model_writer = Some(models);

        assert!(state
            .delete_provider_catalog_provider("removed")
            .await
            .unwrap());
        let record = stored_key(&keys).await;
        assert_eq!(record.allowed_providers, Some(vec!["disabled".into()]));
        assert_eq!(record.denied_providers, record.allowed_providers);
        assert_eq!(
            record.allowed_api_formats,
            Some(vec!["openai:responses".into(), "claude:messages".into()])
        );
        assert_eq!(
            record.allowed_models,
            Some(vec!["old-model".into(), "disabled-model".into()])
        );

        assert!(state
            .delete_provider_catalog_endpoint("shared-endpoint")
            .await
            .unwrap());
        let record = stored_key(&keys).await;
        assert_eq!(
            record.allowed_api_formats,
            Some(vec!["claude:messages".into()])
        );
        assert_eq!(record.denied_api_formats, record.allowed_api_formats);
        assert!(state.delete_admin_global_model("old-model").await.unwrap());
        let record = stored_key(&keys).await;
        assert_eq!(record.allowed_models, Some(vec!["disabled-model".into()]));
        assert_eq!(record.denied_models, record.allowed_models);
        assert!(state
            .delete_admin_global_model("disabled-model")
            .await
            .unwrap());
        let record = stored_key(&keys).await;
        assert_eq!(record.allowed_models, Some(Vec::new()));
        assert_eq!(record.denied_models, Some(Vec::new()));
    }

    #[tokio::test]
    async fn reading_key_management_repairs_old_references_without_requiring_another_delete() {
        let keys = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
            Some("hash".into()),
            key_snapshot(),
        )]));
        let mut state = GatewayDataState::with_auth_api_key_repository_for_tests(keys.clone());
        state.provider_catalog_reader = Some(Arc::new(
            InMemoryProviderCatalogReadRepository::seed(Vec::new(), Vec::new(), Vec::new()),
        ));
        // An unavailable model catalog must not erase model restrictions.
        let records = state
            .clean_user_api_key_access_records(vec![stored_key(&keys).await])
            .await
            .unwrap();
        assert_eq!(records[0].allowed_providers, Some(Vec::new()));
        assert_eq!(records[0].denied_providers, Some(Vec::new()));
        assert_eq!(records[0].allowed_api_formats, Some(Vec::new()));
        assert_eq!(
            records[0].allowed_models,
            Some(vec!["old-model".into(), "disabled-model".into()])
        );
        assert_eq!(stored_key(&keys).await, records[0]);
    }
}
