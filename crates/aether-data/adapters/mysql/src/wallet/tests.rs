use super::{
    admin_payment_callback_list_builder, admin_payment_order_list_builder,
    admin_redeem_batch_list_builder, admin_redeem_code_list_builder,
    admin_wallet_ledger_list_builder, admin_wallet_list_builder,
    admin_wallet_refund_request_list_builder, wallets_by_owner_ids_builder,
    MysqlWalletReadRepository,
};
use crate::run_migrations;
use aether_data_contracts::repository::wallet::{
    AdminPaymentOrderListQuery, AdminRedeemCodeBatchListQuery, AdminRedeemCodeListQuery,
    AdminWalletLedgerQuery, AdminWalletListQuery, AdminWalletRefundRequestListQuery,
    WalletLookupKey, WalletReadRepository,
};

#[test]
fn mysql_wallet_builders_bind_filters_and_page_bounds() {
    let injected_status = "active' OR 1 = 1 --".to_string();
    let query = AdminWalletListQuery {
        status: Some(injected_status.clone()),
        owner_type: Some("user".to_string()),
        limit: 7,
        offset: 3,
    };
    let builder = admin_wallet_list_builder(&query, 7, 3);
    let sql = compact_sql(builder.sql());

    assert!(sql.contains("AND w.status = ?"));
    assert!(sql.contains("AND w.user_id IS NOT NULL"));
    assert!(sql.contains("ORDER BY w.updated_at DESC, w.id DESC LIMIT ? OFFSET ?"));
    assert!(!sql.contains(&injected_status));
    assert_eq!(sql.matches('?').count(), 3);

    let ids = vec!["user-1".to_string(), "user-2' OR 1 = 1 --".to_string()];
    let builder = wallets_by_owner_ids_builder("user_id", &ids);
    let sql = compact_sql(builder.sql());
    assert!(sql.contains("WHERE user_id IN (?, ?) ORDER BY id ASC"));
    assert!(!sql.contains(&ids[0]));
    assert!(!sql.contains(&ids[1]));
}

#[test]
fn mysql_wallet_admin_builders_cover_filters_ordering_and_mapping_columns() {
    let ledger_query = AdminWalletLedgerQuery {
        category: Some("credit".to_string()),
        reason_code: Some("manual_adjustment".to_string()),
        owner_type: Some("api_key".to_string()),
        limit: 5,
        offset: 2,
    };
    let ledger_sql = compact_sql(admin_wallet_ledger_list_builder(&ledger_query, 5, 2).sql());
    assert!(ledger_sql.contains("tx.category = ?"));
    assert!(ledger_sql.contains("tx.reason_code = ?"));
    assert!(ledger_sql.contains("w.api_key_id IS NOT NULL"));
    assert!(ledger_sql.contains("wallet_users.username AS wallet_user_name"));
    assert!(ledger_sql.contains("ORDER BY tx.created_at DESC, tx.id DESC LIMIT ? OFFSET ?"));

    let refund_query = AdminWalletRefundRequestListQuery {
        status: Some("pending_approval".to_string()),
        limit: 4,
        offset: 1,
    };
    let refund_sql =
        compact_sql(admin_wallet_refund_request_list_builder(&refund_query, 4, 1).sql());
    assert!(refund_sql.contains("WHERE w.user_id IS NOT NULL"));
    assert!(refund_sql.contains("rr.status = ?"));
    assert!(refund_sql.contains("rr.payout_proof"));
    assert!(refund_sql.contains("ORDER BY rr.created_at DESC, rr.id DESC LIMIT ? OFFSET ?"));

    let order_query = AdminPaymentOrderListQuery {
        status: Some("expired".to_string()),
        payment_method: Some("card".to_string()),
        limit: 8,
        offset: 6,
    };
    let order_sql = compact_sql(admin_payment_order_list_builder(&order_query, 100, 8, 6).sql());
    assert!(order_sql.contains("payment_method = ?"));
    assert!(order_sql.contains(
        "CASE WHEN status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ? THEN 'expired'"
    ));
    assert!(order_sql.contains("ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"));

    let callback_sql =
        compact_sql(admin_payment_callback_list_builder(Some("redeem_code"), 9, 4).sql());
    assert!(callback_sql.contains("payment_method = ?"));
    assert!(callback_sql.contains("ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"));

    let batch_query = AdminRedeemCodeBatchListQuery {
        status: Some("active".to_string()),
        limit: 2,
        offset: 1,
    };
    let batch_sql = compact_sql(admin_redeem_batch_list_builder(&batch_query, 2, 1).sql());
    assert!(batch_sql.contains("batches.status = ?"));
    assert!(batch_sql.contains("AS redeemed_count"));
    assert!(
        batch_sql.contains("ORDER BY batches.created_at DESC, batches.id DESC LIMIT ? OFFSET ?")
    );

    let code_query = AdminRedeemCodeListQuery {
        batch_id: "batch-1".to_string(),
        status: Some("redeemed".to_string()),
        limit: 3,
        offset: 2,
    };
    let code_sql = compact_sql(admin_redeem_code_list_builder(&code_query, 3, 2).sql());
    assert!(code_sql.contains("codes.batch_id = ?"));
    assert!(code_sql.contains("codes.status = ?"));
    assert!(code_sql.contains("orders.order_no AS redeemed_order_no"));
    assert!(code_sql.contains("ORDER BY codes.created_at DESC, codes.id DESC LIMIT ? OFFSET ?"));
    assert!(!code_sql.contains("batch-1"));
}

fn compact_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn mysql_gateway_order_uniqueness_preflights_before_persistent_changes() {
    const UNIQUENESS_MIGRATION: &str = include_str!(
        "../../migrations/20260821120000_enforce_payment_gateway_order_uniqueness.sql"
    );

    let executable_migration = UNIQUENESS_MIGRATION
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let migration = compact_sql(&executable_migration);
    let initial_cleanup = migration
        .find("DROP TEMPORARY TABLE IF EXISTS aether_payment_gateway_order_uniqueness_preflight")
        .expect("migration should clean up a same-session failed preflight");
    let create_preflight = migration
        .find("CREATE TEMPORARY TABLE aether_payment_gateway_order_uniqueness_preflight")
        .expect("migration should create a non-persistent conflict guard");
    let seed_preflight = migration
        .find("INSERT INTO aether_payment_gateway_order_uniqueness_preflight (conflict_marker) VALUES (1)")
        .expect("migration should seed the duplicate-key conflict guard");
    let conflict_probe = migration
        .find("INSERT INTO aether_payment_gateway_order_uniqueness_preflight (conflict_marker) SELECT 1 FROM payment_orders")
        .expect("migration should reject normalized historical conflicts");
    let final_cleanup = migration
        .find("DROP TEMPORARY TABLE aether_payment_gateway_order_uniqueness_preflight;")
        .expect("migration should remove the successful preflight guard");
    let first_persistent_update = migration
        .find("UPDATE payment_orders SET payment_method")
        .expect("migration should normalize payment order methods");
    let alter = migration
        .find(
            "MODIFY COLUMN gateway_order_id VARCHAR(128) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NULL",
        )
        .expect("migration should enforce binary gateway identifiers");
    let unique = migration
        .find("ADD UNIQUE INDEX uq_payment_orders_payment_method_gateway_order_id")
        .expect("migration should create composite uniqueness");

    assert!(migration.contains("GROUP BY LOWER(TRIM(payment_method)), CONVERT(gateway_order_id USING utf8mb4) COLLATE utf8mb4_0900_bin HAVING COUNT(*) > 1 LIMIT 1"));
    assert!(migration.contains("WHERE BINARY payment_method <> BINARY LOWER(TRIM(payment_method))"));
    let preflight = &migration[..final_cleanup];
    assert!(!preflight.contains("UPDATE payment_orders"));
    assert!(!preflight.contains("UPDATE payment_callbacks"));
    assert!(!preflight.contains("ALTER TABLE"));
    assert!(
        initial_cleanup < create_preflight
            && create_preflight < seed_preflight
            && seed_preflight < conflict_probe
            && conflict_probe < final_cleanup
            && final_cleanup < first_persistent_update
            && first_persistent_update < alter,
        "the conflict probe must finish before any persistent UPDATE or ALTER"
    );
    assert!(
        alter < unique && !migration.contains("CREATE UNIQUE INDEX"),
        "the collation and unique index must be one atomic ALTER TABLE"
    );
}

#[tokio::test]
async fn mysql_wallet_read_repository_reads_wallet_contract_views() {
    let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        eprintln!("skipping mysql wallet read smoke test because AETHER_TEST_MYSQL_URL is unset");
        return;
    };

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("mysql pool should connect");
    run_migrations(&pool)
        .await
        .expect("mysql migrations should run");
    cleanup_rows(&pool).await;
    seed_rows(&pool).await;

    let repository = MysqlWalletReadRepository::new(pool);
    let wallet = repository
        .find(WalletLookupKey::UserId("user-1"))
        .await
        .expect("wallet find should query")
        .expect("wallet should exist");
    assert_eq!(wallet.total_adjusted, 3.0);

    let user_wallets = repository
        .list_wallets_by_user_ids(&[
            "user-2".to_string(),
            "missing-user".to_string(),
            "user-1".to_string(),
        ])
        .await
        .expect("user wallets should list");
    assert_eq!(
        user_wallets
            .iter()
            .map(|wallet| wallet.id.as_str())
            .collect::<Vec<_>>(),
        vec!["wallet-1", "wallet-2"]
    );
    let api_key_wallets = repository
        .list_wallets_by_api_key_ids(&["api-key-1".to_string()])
        .await
        .expect("api key wallets should list");
    assert_eq!(api_key_wallets[0].id, "wallet-api-1");
    assert!(repository
        .list_wallets_by_user_ids(&[])
        .await
        .expect("empty user ids should short circuit")
        .is_empty());

    let page = repository
        .list_admin_wallets(&AdminWalletListQuery {
            status: Some("wallet-read-smoke".to_string()),
            owner_type: Some("user".to_string()),
            limit: 1,
            offset: 1,
        })
        .await
        .expect("admin wallets should list");
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 1);
    let wallet_item = &page.items[0];
    assert_eq!(wallet_item.id, "wallet-1");
    assert_eq!(wallet_item.total_adjusted, 3.0);

    let unknown_owner = repository
        .list_admin_wallets(&AdminWalletListQuery {
            status: Some("wallet-read-smoke".to_string()),
            owner_type: Some("unknown".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("unknown owner type should preserve snapshot semantics");
    assert_eq!(unknown_owner.total, 3);

    let ledger = repository
        .list_admin_wallet_ledger(&AdminWalletLedgerQuery {
            category: Some("credit".to_string()),
            reason_code: Some("manual_adjustment".to_string()),
            owner_type: Some("user".to_string()),
            limit: 1,
            offset: 1,
        })
        .await
        .expect("admin ledger should list");
    assert_eq!(ledger.total, 2);
    assert_eq!(ledger.items[0].id, "tx-1");
    assert_eq!(ledger.items[0].wallet_user_name.as_deref(), Some("Alice"));

    let refund_requests = repository
        .list_admin_wallet_refund_requests(&AdminWalletRefundRequestListQuery {
            status: Some("pending_approval".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("admin refund requests should list");
    assert_eq!(refund_requests.total, 1);
    assert_eq!(refund_requests.items[0].id, "refund-2");
    assert_eq!(
        refund_requests.items[0].wallet_user_name.as_deref(),
        Some("Alice")
    );

    let transactions = repository
        .list_admin_wallet_transactions("wallet-1", 1, 1)
        .await
        .expect("wallet transactions should page");
    assert_eq!(transactions.total, 2);
    assert_eq!(transactions.items[0].id, "tx-1");

    let orders = repository
        .list_admin_payment_orders(&AdminPaymentOrderListQuery {
            status: Some("credited".to_string()),
            payment_method: Some("redeem_code".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("payment orders should list");
    assert_eq!(orders.total, 1);
    assert_eq!(
        orders.items[0].gateway_response.as_ref().unwrap()["ok"],
        true
    );

    let expired_orders = repository
        .list_admin_payment_orders(&AdminPaymentOrderListQuery {
            status: Some("expired".to_string()),
            payment_method: Some("card".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("expired payment orders should filter by effective status");
    assert_eq!(expired_orders.total, 1);
    assert_eq!(expired_orders.items[0].id, "order-2");

    let user_orders = repository
        .list_wallet_payment_orders_by_user_id("user-1", 2, 0)
        .await
        .expect("user payment orders should page");
    assert_eq!(user_orders.total, 3);
    assert_eq!(user_orders.items.len(), 2);
    assert_eq!(user_orders.items[0].id, "order-3");
    assert_eq!(user_orders.items[1].id, "order-2");
    assert_eq!(user_orders.items[1].status, "expired");
    assert_eq!(
        repository
            .count_pending_payment_orders_by_user_id("user-1")
            .await
            .expect("pending payment orders should count"),
        2
    );
    assert_eq!(
        repository
            .find_admin_payment_order("order-1")
            .await
            .expect("admin payment order should query")
            .expect("admin payment order should exist")
            .id,
        "order-1"
    );
    assert!(repository
        .find_wallet_payment_order_by_user_id("user-2", "order-1")
        .await
        .expect("cross-user payment order lookup should query")
        .is_none());

    let refunds = repository
        .list_admin_wallet_refunds("wallet-1", 1, 1)
        .await
        .expect("refunds should list");
    assert_eq!(refunds.total, 2);
    assert_eq!(refunds.items[0].id, "refund-1");
    assert_eq!(
        refunds.items[0].payout_proof.as_ref().unwrap()["proof"],
        "ok"
    );
    assert_eq!(
        repository
            .count_pending_refunds_by_user_id("user-1")
            .await
            .expect("pending refunds should count"),
        1
    );
    assert!(repository
        .find_wallet_refund("wallet-2", "refund-1")
        .await
        .expect("cross-wallet refund lookup should query")
        .is_none());

    let callbacks = repository
        .list_admin_payment_callbacks(Some("redeem_code"), 1, 1)
        .await
        .expect("callbacks should list");
    assert_eq!(callbacks.total, 2);
    assert_eq!(callbacks.items[0].id, "callback-1");
    assert!(callbacks.items[0].signature_valid);

    let batches = repository
        .list_admin_redeem_code_batches(&AdminRedeemCodeBatchListQuery {
            status: Some("active".to_string()),
            limit: 1,
            offset: 0,
        })
        .await
        .expect("redeem batches should filter");
    assert_eq!(batches.total, 1);
    assert_eq!(batches.items[0].id, "batch-1");
    assert_eq!(batches.items[0].redeemed_count, 1);
    assert_eq!(batches.items[0].active_count, 1);
    let batch = repository
        .find_admin_redeem_code_batch("batch-2")
        .await
        .expect("redeem batch should query")
        .expect("redeem batch should exist");
    assert_eq!(batch.active_count, 1);

    let codes = repository
        .list_admin_redeem_codes(&AdminRedeemCodeListQuery {
            batch_id: "batch-1".to_string(),
            status: Some("redeemed".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("redeem codes should list");
    assert_eq!(codes.total, 1);
    assert_eq!(codes.items[0].masked_code, "ABCD****WXYZ");
    let paged_codes = repository
        .list_admin_redeem_codes(&AdminRedeemCodeListQuery {
            batch_id: "batch-1".to_string(),
            status: None,
            limit: 1,
            offset: 1,
        })
        .await
        .expect("redeem codes should page");
    assert_eq!(paged_codes.total, 2);
    assert_eq!(paged_codes.items[0].id, "code-1");

    let today = super::current_billing_date("UTC").expect("UTC should parse");
    sqlx::query("UPDATE wallet_daily_usage_ledgers SET billing_date = ? WHERE id = 'daily-1'")
        .bind(today)
        .execute(repository.pool())
        .await
        .expect("daily row should update");
    let daily = repository
        .find_wallet_today_usage("wallet-1", "UTC")
        .await
        .expect("daily usage should query")
        .expect("daily usage should exist");
    assert_eq!(daily.total_requests, 2);

    cleanup_rows(repository.pool()).await;
}

impl MysqlWalletReadRepository {
    fn pool(&self) -> &sqlx::MySqlPool {
        &self.pool
    }
}

async fn cleanup_rows(pool: &sqlx::MySqlPool) {
    for sql in [
        "DELETE FROM wallet_daily_usage_ledgers WHERE id = 'daily-1'",
        "DELETE FROM redeem_codes WHERE id IN ('code-1', 'code-2', 'code-3')",
        "DELETE FROM redeem_code_batches WHERE id IN ('batch-1', 'batch-2')",
        "DELETE FROM wallet_transactions WHERE id IN ('tx-1', 'tx-2', 'tx-api-1')",
        "DELETE FROM refund_requests WHERE id IN ('refund-1', 'refund-2', 'refund-api-1')",
        "DELETE FROM payment_callbacks WHERE id IN ('callback-1', 'callback-2', 'callback-3')",
        "DELETE FROM payment_orders WHERE id IN ('order-1', 'order-2', 'order-3', 'order-4')",
        "DELETE FROM wallets WHERE id IN ('wallet-1', 'wallet-2', 'wallet-api-1')",
        "DELETE FROM api_keys WHERE id = 'api-key-1'",
        "DELETE FROM users WHERE id IN ('user-1', 'user-2')",
    ] {
        sqlx::query(sql)
            .execute(pool)
            .await
            .expect("cleanup should succeed");
    }
}

async fn seed_rows(pool: &sqlx::MySqlPool) {
    sqlx::query(
        r#"
INSERT INTO users (id, username, email, auth_source, created_at, updated_at)
VALUES
  ('user-1', 'Alice', 'alice@example.com', 'local', 1, 1),
  ('user-2', 'Bob', 'bob@example.com', 'local', 1, 1)
"#,
    )
    .execute(pool)
    .await
    .expect("users should seed");

    sqlx::query(
        r#"
INSERT INTO api_keys (id, user_id, key_hash, name, created_at, updated_at)
VALUES ('api-key-1', 'user-1', 'wallet-test-api-key-hash-1', 'Standalone Key', 1, 1)
"#,
    )
    .execute(pool)
    .await
    .expect("api key should seed");

    sqlx::query(
        r#"
INSERT INTO wallets (
    id, user_id, api_key_id, balance, gift_balance, status, total_recharged,
    total_consumed, total_refunded, total_adjusted, created_at, updated_at
) VALUES
    ('wallet-1', 'user-1', NULL, 10.0, 2.0, 'wallet-read-smoke', 20.0, 4.0, 1.0, 3.0, 1, 2),
    ('wallet-2', 'user-2', NULL, 4.0, 1.0, 'wallet-read-smoke', 5.0, 2.0, 0.0, 0.0, 1, 3),
    ('wallet-api-1', NULL, 'api-key-1', 7.0, 0.0, 'wallet-read-smoke', 7.0, 0.0, 0.0, 0.0, 1, 4)
"#,
    )
    .execute(pool)
    .await
    .expect("wallets should seed");

    sqlx::query(
        r#"
INSERT INTO payment_orders (
  id, order_no, wallet_id, user_id, amount_usd, refunded_amount_usd,
  refundable_amount_usd, payment_method, gateway_response, status, created_at,
  expires_at
) VALUES
  ('order-1', 'order-no-1', 'wallet-1', 'user-1', 5.0, 1.0, 4.0,
   'redeem_code', '{"ok":true}', 'credited', 3, NULL),
  ('order-2', 'order-no-2', 'wallet-1', 'user-1', 2.0, 0.0, 2.0,
   'card', NULL, 'pending', 12, 1),
  ('order-3', 'order-no-3', 'wallet-1', 'user-1', 3.0, 0.0, 3.0,
   'card', NULL, 'pending', 13, 4102444800),
  ('order-4', 'order-no-4', 'wallet-2', 'user-2', 4.0, 0.0, 4.0,
   'bank', NULL, 'paid', 14, 4102444800)
"#,
    )
    .execute(pool)
    .await
    .expect("payment orders should seed");

    sqlx::query(
        r#"
INSERT INTO payment_callbacks (
  id, payment_order_id, payment_method, callback_key, order_no,
  signature_valid, payload, created_at
) VALUES
  ('callback-1', 'order-1', 'redeem_code', 'callback-key-1',
   'order-no-1', 1, '{"event":"paid"}', 4),
  ('callback-2', 'order-1', 'redeem_code', 'callback-key-2',
   'order-no-1', 1, '{"event":"duplicate"}', 5),
  ('callback-3', 'order-2', 'card', 'callback-key-3',
   'order-no-2', 0, '{"event":"created"}', 6)
"#,
    )
    .execute(pool)
    .await
    .expect("callbacks should seed");

    sqlx::query(
        r#"
INSERT INTO refund_requests (
  id, refund_no, wallet_id, user_id, payment_order_id, source_type,
  refund_mode, amount_usd, status, payout_proof, created_at, updated_at
) VALUES
  ('refund-1', 'refund-no-1', 'wallet-1', 'user-1', 'order-1',
   'payment_order', 'offline_payout', 1.0, 'completed',
   '{"proof":"ok"}', 5, 6),
  ('refund-2', 'refund-no-2', 'wallet-1', 'user-1', 'order-2',
   'payment_order', 'original_channel', 0.5, 'pending_approval',
   NULL, 6, 7),
  ('refund-api-1', 'refund-no-api-1', 'wallet-api-1', NULL, NULL,
   'manual', 'offline_payout', 0.5, 'pending_approval',
   NULL, 7, 8)
"#,
    )
    .execute(pool)
    .await
    .expect("refunds should seed");

    sqlx::query(
        r#"
INSERT INTO wallet_transactions (
  id, wallet_id, category, reason_code, amount, balance_before,
  balance_after, recharge_balance_before, recharge_balance_after,
  gift_balance_before, gift_balance_after, created_at
) VALUES
  ('tx-1', 'wallet-1', 'credit', 'manual_adjustment', 3.0, 7.0, 10.0,
   5.0, 8.0, 2.0, 2.0, 7),
  ('tx-2', 'wallet-1', 'credit', 'manual_adjustment', 1.0, 11.0, 12.0,
   9.0, 10.0, 2.0, 2.0, 8),
  ('tx-api-1', 'wallet-api-1', 'credit', 'manual_adjustment', 1.0, 6.0, 7.0,
   6.0, 7.0, 0.0, 0.0, 9)
"#,
    )
    .execute(pool)
    .await
    .expect("transactions should seed");

    sqlx::query(
        r#"
INSERT INTO redeem_code_batches (
  id, name, amount_usd, total_count, status, created_at, updated_at
) VALUES
  ('batch-1', 'Batch One', 5.0, 2, 'active', 8, 9),
  ('batch-2', 'Batch Two', 8.0, 1, 'disabled', 9, 10)
"#,
    )
    .execute(pool)
    .await
    .expect("redeem batches should seed");

    sqlx::query(
        r#"
INSERT INTO redeem_codes (
  id, batch_id, code_hash, code_prefix, code_suffix, status,
  redeemed_by_user_id, redeemed_wallet_id, redeemed_payment_order_id,
  redeemed_at, created_at, updated_at
) VALUES
  ('code-1', 'batch-1', 'hash-1', 'ABCD', 'WXYZ', 'redeemed',
   'user-1', 'wallet-1', 'order-1', 10, 8, 10),
  ('code-2', 'batch-1', 'hash-2', 'EFGH', 'QRST', 'active',
   NULL, NULL, NULL, NULL, 9, 10),
  ('code-3', 'batch-2', 'hash-3', 'IJKL', 'MNOP', 'active',
   NULL, NULL, NULL, NULL, 10, 11)
"#,
    )
    .execute(pool)
    .await
    .expect("redeem codes should seed");

    sqlx::query(
        r#"
INSERT INTO wallet_daily_usage_ledgers (
  id, wallet_id, billing_date, billing_timezone, total_cost_usd,
  total_requests, input_tokens, output_tokens, cache_creation_tokens,
  cache_read_tokens, aggregated_at, created_at, updated_at
) VALUES (
  'daily-1', 'wallet-1', '2000-01-01', 'UTC', 1.25, 2, 10, 20, 3, 4, 11, 11, 11
)
"#,
    )
    .execute(pool)
    .await
    .expect("daily usage should seed");
}
