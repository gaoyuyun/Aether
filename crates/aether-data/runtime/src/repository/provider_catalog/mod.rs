mod memory;

#[allow(unused_imports)]
pub(crate) use aether_data_contracts::repository::provider_catalog::{
    ProviderCatalogKeyListOrder, ProviderCatalogKeyListQuery, ProviderCatalogReadRepository,
    ProviderCatalogSnapshot, ProviderCatalogUpstreamMetadataNamespaceUpdate,
    ProviderCatalogWriteRepository, StoredProviderCatalogEndpoint,
    StoredProviderCatalogEndpointIdentity, StoredProviderCatalogKey,
    StoredProviderCatalogKeyMaintenanceSummary, StoredProviderCatalogKeyPage,
    StoredProviderCatalogKeyStats, StoredProviderCatalogProvider,
    StoredProviderCatalogProviderIdentity,
};
#[cfg(feature = "mysql")]
pub use aether_data_mysql::MysqlProviderCatalogReadRepository;
#[cfg(feature = "postgres")]
pub use aether_data_postgres::SqlxProviderCatalogReadRepository;
#[cfg(feature = "sqlite")]
pub use aether_data_sqlite::SqliteProviderCatalogReadRepository;
pub use memory::InMemoryProviderCatalogReadRepository;
