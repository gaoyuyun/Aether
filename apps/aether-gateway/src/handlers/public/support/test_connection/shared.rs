use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;

pub(super) fn select_test_connection_provider(
    providers: Vec<StoredProviderCatalogProvider>,
    provider_query: Option<&str>,
) -> Option<StoredProviderCatalogProvider> {
    let provider_query = provider_query
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(provider_query) = provider_query {
        if let Some(provider) = providers
            .iter()
            .find(|provider| provider.id.eq_ignore_ascii_case(provider_query))
        {
            return Some(provider.clone());
        }
        return None;
    }
    providers.into_iter().next()
}
