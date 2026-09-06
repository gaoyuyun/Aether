//! Set AETHER_QUOTA_REPAIR_DATABASE_URL; defaults to a bounded dry run.
use aether_data_contracts::repository::quota::ProviderQuotaWriteRepository;
use aether_data_mysql::MysqlProviderQuotaRepository;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|v| v == flag)
            .and_then(|i| args.get(i + 1))
    };
    let provider = value("--provider").ok_or("--provider is required")?;
    let limit: usize = value("--limit")
        .map(String::as_str)
        .unwrap_or("100")
        .parse()?;
    if !(1..=1000).contains(&limit) {
        return Err("--limit must be between 1 and 1000".into());
    }
    let dry_run = !args.iter().any(|v| v == "--apply");
    let url = std::env::var("AETHER_QUOTA_REPAIR_DATABASE_URL")?;
    let pool = sqlx::MySqlPool::connect(&url).await?;
    let repository = MysqlProviderQuotaRepository::new(pool);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let result = repository
        .recover_attempts(Some(provider), limit, now, dry_run)
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({ "dry_run": dry_run, "records": result })
        )?
    );
    Ok(())
}
