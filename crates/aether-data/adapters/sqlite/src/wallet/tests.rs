use super::{replace_matching_plan_entitlements_sqlite, SqliteWalletReadRepository};
use crate::run_migrations;
use aether_data_contracts::repository::wallet::{
    AdjustWalletBalanceInput, AdminPaymentOrderListQuery, AdminRedeemCodeListQuery,
    AdminWalletListQuery, CompareAndSwapPaymentOrderStripeClientSecretInput,
    CompleteAdminWalletRefundInput, CreateAdminRedeemCodeBatchInput,
    CreateManualWalletRechargeInput, CreatePlanPurchaseOrderInput, CreatePlanPurchaseOrderOutcome,
    CreateWalletRechargeOrderInput, CreateWalletRechargeOrderOutcome,
    CreateWalletRefundRequestInput, CreateWalletRefundRequestOutcome, CreditAdminPaymentOrderInput,
    DeleteAdminRedeemCodeBatchInput, DisableAdminRedeemCodeBatchInput, DisableAdminRedeemCodeInput,
    FailAdminWalletRefundInput, FailWalletRechargeCheckoutInput, ProcessAdminWalletRefundInput,
    ProcessPaymentCallbackInput, ProcessPaymentCallbackOutcome, RedeemWalletCodeInput,
    RedeemWalletCodeOutcome, UpdateAdminWalletRefundGatewayInput,
    UpdateWalletRechargeCheckoutInput, WalletLookupKey, WalletMutationOutcome,
    WalletReadRepository, WalletWriteRepository,
};
use aether_data_contracts::DataLayerError;
use serde_json::json;
use std::{sync::Arc, time::Duration};

async fn ensure_test_user(pool: &sqlx::SqlitePool, user_id: &str) {
    let username = format!("wallet-test-{user_id}");
    let email = format!("{user_id}@wallet-test.example");
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, 'local', 1, 1)",
    )
    .bind(user_id)
    .bind(username)
    .bind(email)
    .execute(pool)
    .await
        .expect("test user should seed");
}

async fn ensure_test_users(pool: &sqlx::SqlitePool, user_ids: &[&str]) {
    for user_id in user_ids {
        ensure_test_user(pool, user_id).await;
    }
}

#[tokio::test]
async fn sqlite_stripe_secret_cas_requires_the_exact_locked_row() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    ensure_test_user(&pool, "stripe-cas-user").await;
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let wallet = repository
        .initialize_auth_user_wallet("stripe-cas-user", 0.0, false)
        .await
        .expect("wallet initialization should run")
        .expect("wallet should exist");
    let legacy = "gAAAAABsqlite-legacy";
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some(wallet.id),
            user_id: "stripe-cas-user".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(10.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            gateway_order_id: "pi-sqlite-cas".to_string(),
            gateway_response: json!({
                "gateway": "stripe",
                "publishable_key": "pk_test_public",
                "_stripe_client_secret_encrypted": legacy,
            }),
            order_no: "po-sqlite-cas".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("order creation should run")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        other => panic!("unexpected order creation outcome: {other:?}"),
    };
    let observed = order
        .gateway_response
        .clone()
        .expect("created order should contain a response");
    let replacement = concat!(
        "aether-payment-order-stripe-client-secret-v2:",
        "aether-runtime-secret-v1:gAAAAABsqlite-replacement"
    );
    let input = CompareAndSwapPaymentOrderStripeClientSecretInput {
        order_id: order.id.clone(),
        order_no: order.order_no.clone(),
        wallet_id: order.wallet_id.clone(),
        user_id: order.user_id.clone(),
        payment_method: order.payment_method.clone(),
        payment_provider: order.payment_provider.clone(),
        order_kind: order.order_kind.clone(),
        gateway_order_id: order.gateway_order_id.clone(),
        expected_status: order.status.clone(),
        expected_expires_at_unix_secs: order.expires_at_unix_secs,
        expected_gateway_response: observed,
        expected_client_secret_encrypted: legacy.to_string(),
        replacement_client_secret_encrypted: replacement.to_string(),
    };

    let mut foreign = input.clone();
    foreign.user_id = Some("stripe-cas-foreign-user".to_string());
    assert!(!repository
        .compare_and_swap_payment_order_stripe_client_secret(foreign)
        .await
        .expect("identity mismatch should be a normal CAS miss"));
    assert!(repository
        .compare_and_swap_payment_order_stripe_client_secret(input.clone())
        .await
        .expect("exact CAS should succeed"));
    assert!(!repository
        .compare_and_swap_payment_order_stripe_client_secret(input)
        .await
        .expect("stale CAS must not replace the new value"));

    let stored: String =
        sqlx::query_scalar("SELECT gateway_response FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(&pool)
            .await
            .expect("stored gateway response should query");
    let stored: serde_json::Value =
        serde_json::from_str(&stored).expect("stored response should be valid JSON");
    assert_eq!(
        stored["_stripe_client_secret_encrypted"].as_str(),
        Some(replacement)
    );
    assert_eq!(stored["publishable_key"], "pk_test_public");
}

#[tokio::test]
async fn sqlite_admin_balance_adjustment_rejects_invalid_numbers_without_writes() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "invalid-adjustment-user").await;
    let wallet = repository
        .initialize_auth_user_wallet("invalid-adjustment-user", 0.0, false)
        .await
        .expect("wallet initialization should run")
        .expect("wallet should exist");

    for amount_usd in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = repository
            .adjust_wallet_balance(AdjustWalletBalanceInput {
                wallet_id: wallet.id.clone(),
                amount_usd,
                balance_type: "recharge".to_string(),
                operator_id: Some("admin-1".to_string()),
                description: None,
            })
            .await
            .expect_err("invalid adjustment should fail before writing");
        assert!(matches!(error, DataLayerError::InvalidInput(_)));
    }

    sqlx::query("UPDATE wallets SET balance = ? WHERE id = ?")
        .bind(f64::MAX)
        .bind(&wallet.id)
        .execute(&pool)
        .await
        .expect("overflow fixture should update");
    let error = repository
        .adjust_wallet_balance(AdjustWalletBalanceInput {
            wallet_id: wallet.id.clone(),
            amount_usd: f64::MAX,
            balance_type: "recharge".to_string(),
            operator_id: Some("admin-1".to_string()),
            description: None,
        })
        .await
        .expect_err("overflowing adjustment should fail before writing");
    assert!(matches!(error, DataLayerError::UnexpectedValue(_)));

    let stored_balance = sqlx::query_scalar::<_, f64>("SELECT balance FROM wallets WHERE id = ?")
        .bind(&wallet.id)
        .fetch_one(&pool)
        .await
        .expect("wallet balance should query");
    assert_eq!(stored_balance, f64::MAX);
    let adjustment_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM wallet_transactions WHERE wallet_id = ? AND category = 'adjust'",
    )
    .bind(&wallet.id)
    .fetch_one(&pool)
    .await
    .expect("adjustment ledger count should query");
    assert_eq!(adjustment_count, 0);
}

#[tokio::test]
async fn sqlite_manual_wallet_recharge_rejects_invalid_numbers_without_writes() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    ensure_test_user(&pool, "invalid-manual-recharge-user").await;
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let wallet = repository
        .initialize_auth_user_wallet("invalid-manual-recharge-user", 0.0, false)
        .await
        .expect("wallet initialization should run")
        .expect("wallet should exist");
    let initial_order_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payment_orders WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("payment order count should query");
    let initial_transaction_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wallet_transactions WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("wallet transaction count should query");

    for (index, amount_usd) in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]
        .into_iter()
        .enumerate()
    {
        let error = repository
            .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
                wallet_id: wallet.id.clone(),
                amount_usd,
                payment_method: "admin_manual".to_string(),
                operator_id: Some("admin-invalid-recharge".to_string()),
                description: None,
                order_no: format!("invalid-manual-recharge-{index}"),
            })
            .await
            .expect_err("invalid manual recharge should fail before writing");
        assert!(matches!(error, DataLayerError::InvalidInput(_)));
    }

    let unchanged = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet should query")
        .expect("wallet should remain present");
    assert_eq!(unchanged, wallet);
    let order_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payment_orders WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("payment order count should query");
    let transaction_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wallet_transactions WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("wallet transaction count should query");
    assert_eq!(order_count, initial_order_count);
    assert_eq!(transaction_count, initial_transaction_count);

    sqlx::query("UPDATE wallets SET balance = ?, total_recharged = ? WHERE id = ?")
        .bind(f64::MAX)
        .bind(f64::MAX)
        .bind(&wallet.id)
        .execute(&pool)
        .await
        .expect("overflow fixture should update");
    let overflow_fixture = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("overflow fixture should query")
        .expect("wallet should remain present");
    let error = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: wallet.id.clone(),
            amount_usd: f64::MAX,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("admin-overflow-recharge".to_string()),
            description: None,
            order_no: "overflow-manual-recharge".to_string(),
        })
        .await
        .expect_err("overflowing manual recharge should fail before writing");
    assert!(matches!(error, DataLayerError::InvalidInput(_)));

    let unchanged = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet should query")
        .expect("wallet should remain present");
    assert_eq!(unchanged, overflow_fixture);
    let order_count_after_overflow: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payment_orders WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("payment order count should query");
    let transaction_count_after_overflow: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wallet_transactions WHERE wallet_id = ?")
            .bind(&wallet.id)
            .fetch_one(&pool)
            .await
            .expect("wallet transaction count should query");
    assert_eq!(order_count_after_overflow, initial_order_count);
    assert_eq!(transaction_count_after_overflow, initial_transaction_count);
}

#[tokio::test]
async fn sqlite_wallet_initialization_rejects_missing_owners_without_writes() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    assert!(repository
        .initialize_auth_user_wallet("missing-wallet-user", 5.0, false)
        .await
        .expect("missing user initialization should resolve")
        .is_none());
    assert!(repository
        .initialize_auth_api_key_wallet("missing-wallet-api-key", 5.0, false)
        .await
        .expect("missing api key initialization should resolve")
        .is_none());

    let wallet_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallets")
        .fetch_one(&pool)
        .await
        .expect("wallet count should query");
    assert_eq!(wallet_count, 0);
    let transaction_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallet_transactions")
        .fetch_one(&pool)
        .await
        .expect("transaction count should query");
    assert_eq!(transaction_count, 0);

    ensure_test_user(&pool, "wallet-owner-user").await;
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, key_hash, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind("wallet-owner-api-key")
    .bind("wallet-owner-user")
    .bind("wallet-owner-api-key-hash")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pool)
    .await
    .expect("api key should seed");
    let initialized = repository
        .initialize_auth_api_key_wallet("wallet-owner-api-key", 5.0, false)
        .await
        .expect("valid api key initialization should resolve")
        .expect("valid api key wallet should be created");
    assert_eq!(
        initialized.api_key_id.as_deref(),
        Some("wallet-owner-api-key")
    );
    assert_eq!(initialized.gift_balance, 5.0);
}

#[tokio::test]
async fn sqlite_wallet_read_repository_reads_wallet_contract_views() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    seed_rows(&pool).await;

    let repository = SqliteWalletReadRepository::new(pool);
    let wallet = repository
        .find(WalletLookupKey::UserId("user-1"))
        .await
        .expect("wallet find should query")
        .expect("wallet should exist");
    assert_eq!(wallet.total_adjusted, 3.0);

    let page = repository
        .list_admin_wallets(&AdminWalletListQuery {
            status: Some("active".to_string()),
            owner_type: Some("user".to_string()),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("admin wallets should list");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].total_adjusted, 3.0);

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

    let refunds = repository
        .list_admin_wallet_refunds("wallet-1", 10, 0)
        .await
        .expect("refunds should list");
    assert_eq!(refunds.total, 1);
    assert_eq!(
        refunds.items[0].payout_proof.as_ref().unwrap()["proof"],
        "ok"
    );

    let callbacks = repository
        .list_admin_payment_callbacks(Some("redeem_code"), 10, 0)
        .await
        .expect("callbacks should list");
    assert_eq!(callbacks.total, 1);
    assert!(callbacks.items[0].signature_valid);

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
}

#[tokio::test]
async fn sqlite_provisional_wallet_cleanup_is_activity_guarded() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    ensure_test_user(&pool, "provisional-user").await;
    let provisional_wallet = repository
        .initialize_auth_user_wallet("provisional-user", 10.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("provisional wallet should exist");
    assert!(repository
        .delete_provisional_auth_user_wallet(&provisional_wallet.id, "provisional-user")
        .await
        .expect("provisional cleanup should succeed"));
    assert!(repository
        .find(WalletLookupKey::UserId("provisional-user"))
        .await
        .expect("wallet lookup should succeed")
        .is_none());

    ensure_test_user(&pool, "active-user").await;
    repository
        .initialize_auth_user_wallet("active-user", 10.0, false)
        .await
        .expect("wallet initialization should succeed");
    let active_wallet = repository
        .find(WalletLookupKey::UserId("active-user"))
        .await
        .expect("wallet lookup should succeed")
        .expect("active wallet should exist");
    sqlx::query(
        r#"INSERT INTO wallet_daily_usage_ledgers (
            id, wallet_id, billing_date, billing_timezone, total_cost_usd,
            total_requests, input_tokens, output_tokens, cache_creation_tokens,
            cache_read_tokens, aggregated_at, created_at, updated_at
        ) VALUES (?, ?, '2000-01-01', 'UTC', 1.0, 1, 1, 1, 0, 0, 1, 1, 1)"#,
    )
    .bind("active-daily")
    .bind(&active_wallet.id)
    .execute(&pool)
    .await
    .expect("activity row should insert");
    assert!(!repository
        .delete_provisional_auth_user_wallet(&active_wallet.id, "active-user")
        .await
        .expect("provisional cleanup should succeed"));
    assert!(repository
        .find(WalletLookupKey::UserId("active-user"))
        .await
        .expect("wallet lookup should succeed")
        .is_some());
}

#[tokio::test]
async fn sqlite_wallet_compensation_delete_is_owner_and_reference_guarded() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    ensure_test_user(&pool, "funded-compensation-user").await;
    let funded_wallet = repository
        .initialize_auth_user_wallet("funded-compensation-user", 0.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("wallet should exist");
    sqlx::query(
        "UPDATE wallets SET balance = ?, total_recharged = ?, total_adjusted = ? WHERE id = ?",
    )
    .bind(1.0)
    .bind(1.0)
    .bind(1.0)
    .bind(&funded_wallet.id)
    .execute(&pool)
    .await
    .expect("funded wallet update should succeed");
    assert!(!repository
        .delete_wallet_if_unreferenced(
            &funded_wallet.id,
            WalletLookupKey::UserId("funded-compensation-user"),
        )
        .await
        .expect("funded wallet must not be deleted"));
    assert!(repository
        .find(WalletLookupKey::UserId("funded-compensation-user"))
        .await
        .expect("wallet lookup should succeed")
        .is_some());

    ensure_test_user(&pool, "compensation-user").await;
    let wallet = repository
        .initialize_auth_user_wallet("compensation-user", 0.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("wallet should exist");
    let wallet_id = wallet.id.clone();

    // A caller with a different owner must not be able to reclaim this wallet, even when the
    // wallet id is known.
    assert!(!repository
        .delete_wallet_if_unreferenced(&wallet_id, WalletLookupKey::ApiKeyId("different-api-key"),)
        .await
        .expect("owner mismatch should be handled cleanly"));

    sqlx::query(
        r#"
INSERT INTO wallet_daily_usage_ledgers (
  id, wallet_id, billing_date, billing_timezone, total_cost_usd,
  total_requests, input_tokens, output_tokens, cache_creation_tokens,
  cache_read_tokens, aggregated_at, created_at, updated_at
) VALUES (?, ?, '2000-01-01', 'UTC', 0, 1, 0, 0, 0, 0, 1, 1, 1)
"#,
    )
    .bind("compensation-daily")
    .bind(&wallet_id)
    .execute(&pool)
    .await
    .expect("usage reference should insert");

    assert!(!repository
        .delete_wallet_if_unreferenced(&wallet_id, WalletLookupKey::UserId("compensation-user"),)
        .await
        .expect("referenced wallet should be retained"));
    assert!(repository
        .find(WalletLookupKey::UserId("compensation-user"))
        .await
        .expect("wallet lookup should succeed")
        .is_some());

    sqlx::query("DELETE FROM wallet_daily_usage_ledgers WHERE id = ?")
        .bind("compensation-daily")
        .execute(&pool)
        .await
        .expect("usage reference should delete");
    assert!(repository
        .delete_wallet_if_unreferenced(&wallet_id, WalletLookupKey::UserId("compensation-user"),)
        .await
        .expect("unreferenced wallet should be deleted"));
    assert!(repository
        .find(WalletLookupKey::UserId("compensation-user"))
        .await
        .expect("wallet lookup should succeed")
        .is_none());
}

#[tokio::test]
async fn sqlite_wallet_snapshot_compensation_deletes_funded_match_only() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    ensure_test_user(&pool, "snapshot-compensation-user").await;
    let wallet = repository
        .initialize_auth_user_wallet("snapshot-compensation-user", 0.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("wallet should exist");
    sqlx::query(
        "UPDATE wallets SET balance = ?, total_recharged = ?, total_adjusted = ? WHERE id = ?",
    )
    .bind(12.5)
    .bind(12.5)
    .bind(0.0)
    .bind(&wallet.id)
    .execute(&pool)
    .await
    .expect("wallet funding fixture should update");
    let expected = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet lookup should succeed")
        .expect("funded wallet should exist");
    assert!(repository
        .delete_wallet_if_snapshot_matches_and_unreferenced(
            &expected,
            WalletLookupKey::UserId("snapshot-compensation-user"),
        )
        .await
        .expect("matching snapshot delete should succeed"));

    ensure_test_user(&pool, "snapshot-compensation-changed").await;
    let wallet = repository
        .initialize_auth_user_wallet("snapshot-compensation-changed", 0.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("wallet should exist");
    let expected = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet lookup should succeed")
        .expect("wallet should exist");
    sqlx::query("UPDATE wallets SET balance = 1.0 WHERE id = ?")
        .bind(&wallet.id)
        .execute(&pool)
        .await
        .expect("concurrent wallet change fixture should update");
    assert!(!repository
        .delete_wallet_if_snapshot_matches_and_unreferenced(
            &expected,
            WalletLookupKey::UserId("snapshot-compensation-changed"),
        )
        .await
        .expect("changed snapshot should be retained"));
    assert!(repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet lookup should succeed")
        .is_some());

    ensure_test_user(&pool, "snapshot-compensation-referenced").await;
    let wallet = repository
        .initialize_auth_user_wallet("snapshot-compensation-referenced", 0.0, false)
        .await
        .expect("wallet initialization should succeed")
        .expect("wallet should exist");
    let expected = repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet lookup should succeed")
        .expect("wallet should exist");
    sqlx::query(
        r#"
INSERT INTO wallet_daily_usage_ledgers (
  id, wallet_id, billing_date, billing_timezone, total_cost_usd,
  total_requests, input_tokens, output_tokens, cache_creation_tokens,
  cache_read_tokens, aggregated_at, created_at, updated_at
) VALUES (?, ?, '2000-01-01', 'UTC', 0, 1, 0, 0, 0, 0, 1, 1, 1)
"#,
    )
    .bind("snapshot-compensation-reference")
    .bind(&wallet.id)
    .execute(&pool)
    .await
    .expect("usage reference should insert");
    assert!(!repository
        .delete_wallet_if_snapshot_matches_and_unreferenced(
            &expected,
            WalletLookupKey::UserId("snapshot-compensation-referenced"),
        )
        .await
        .expect("referenced snapshot should be retained"));
    assert!(repository
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .expect("wallet lookup should succeed")
        .is_some());
}

#[tokio::test]
async fn sqlite_wallet_refund_rejects_invalid_amounts_and_foreign_wallets() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    ensure_test_user(&pool, "refund-security-user").await;
    let wallet = repository
        .initialize_auth_user_wallet("refund-security-user", 0.0, false)
        .await
        .expect("setup wallet initialization should run")
        .expect("setup wallet should exist");
    let (wallet, _) = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: wallet.id,
            amount_usd: 10.0,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("admin-1".to_string()),
            description: Some("refund security setup".to_string()),
            order_no: "refund-security-recharge".to_string(),
        })
        .await
        .expect("manual recharge should run")
        .expect("setup wallet should exist");

    for (index, amount_usd) in [0.0, -1.0, f64::NAN, f64::INFINITY].into_iter().enumerate() {
        let outcome = repository
            .create_wallet_refund_request(CreateWalletRefundRequestInput {
                wallet_id: wallet.id.clone(),
                user_id: wallet
                    .user_id
                    .clone()
                    .expect("setup wallet should have an owner"),
                amount_usd,
                payment_order_id: None,
                source_type: None,
                source_id: None,
                refund_mode: None,
                reason: None,
                idempotency_key: Some(format!("refund-security-invalid-{index}")),
                refund_no: format!("refund-security-invalid-{index}"),
            })
            .await
            .expect("invalid refund should be rejected cleanly");
        assert!(matches!(
            outcome,
            CreateWalletRefundRequestOutcome::InvalidInput(_)
        ));
    }

    let foreign_outcome = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id,
            user_id: "different-user".to_string(),
            amount_usd: 1.0,
            payment_order_id: None,
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: None,
            idempotency_key: Some("refund-security-foreign-wallet".to_string()),
            refund_no: "refund-security-foreign-wallet".to_string(),
        })
        .await
        .expect("foreign wallet refund should be rejected cleanly");
    assert!(matches!(
        foreign_outcome,
        CreateWalletRefundRequestOutcome::WalletMissing
    ));

    let refund_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refund_requests")
        .fetch_one(&pool)
        .await
        .expect("refund count should query");
    assert_eq!(refund_count, 0);
}

#[tokio::test]
async fn sqlite_wallet_refund_rejects_invalid_reserved_amounts() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    ensure_test_user(&pool, "refund-reservation-user").await;
    let wallet = repository
        .initialize_auth_user_wallet("refund-reservation-user", 0.0, false)
        .await
        .expect("reservation wallet initialization should run")
        .expect("reservation wallet should exist");
    let (wallet, order) = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: wallet.id,
            amount_usd: 10.0,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("reservation-admin".to_string()),
            description: Some("reservation setup".to_string()),
            order_no: "reservation-order".to_string(),
        })
        .await
        .expect("reservation recharge should run")
        .expect("reservation wallet should still exist");

    for (id, status, amount_usd) in [
        ("reservation-valid", "pending_approval", 2.0),
        ("reservation-negative", "approved", -100.0),
        ("reservation-infinite", "pending_approval", f64::INFINITY),
    ] {
        sqlx::query(
            r#"
INSERT INTO refund_requests (
  id, refund_no, wallet_id, user_id, payment_order_id, source_type,
  refund_mode, amount_usd, status, created_at, updated_at
)
VALUES (?, ?, ?, ?, ?, 'payment_order', 'offline_payout', ?, ?, 1, 1)
"#,
        )
        .bind(id)
        .bind(format!("{id}-no"))
        .bind(&wallet.id)
        .bind("refund-reservation-user")
        .bind(&order.id)
        .bind(amount_usd)
        .bind(status)
        .execute(&pool)
        .await
        .expect("corrupt reservation row should insert");
    }

    let outcome = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id,
            user_id: "refund-reservation-user".to_string(),
            amount_usd: 8.0,
            payment_order_id: Some(order.id),
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: Some("reservation regression".to_string()),
            idempotency_key: Some("reservation-regression-idempotency".to_string()),
            refund_no: "reservation-regression-refund".to_string(),
        })
        .await
        .expect("reservation request should run");
    assert!(matches!(
        outcome,
        CreateWalletRefundRequestOutcome::InvalidInput(_)
    ));
    let refund_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM refund_requests WHERE idempotency_key = ?")
            .bind("reservation-regression-idempotency")
            .fetch_one(&pool)
            .await
            .expect("refund count should query");
    assert_eq!(refund_count, 0);
}

#[derive(Debug, PartialEq)]
struct RefundMutationSnapshot {
    wallet_balance: f64,
    wallet_total_refunded: f64,
    order_refunded_amount: f64,
    order_refundable_amount: f64,
    refund_status: String,
    refund_failure_reason: Option<String>,
    refund_gateway_id: Option<String>,
    refund_payout_reference: Option<String>,
    refund_payout_proof: Option<String>,
    wallet_transaction_count: i64,
}

struct RefundMutationFixture {
    wallet_id: String,
    payment_order_id: String,
    refund_id: String,
}

async fn create_refund_mutation_fixture(
    repository: &SqliteWalletReadRepository,
    case_name: &str,
) -> RefundMutationFixture {
    let user_id = format!("refund-corruption-{case_name}");
    ensure_test_user(repository.pool(), &user_id).await;
    let wallet = repository
        .initialize_auth_user_wallet(&user_id, 0.0, false)
        .await
        .expect("refund corruption wallet initialization should run")
        .expect("refund corruption wallet should exist");
    let (wallet, order) = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: wallet.id,
            amount_usd: 10.0,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("admin-refund-corruption".to_string()),
            description: Some("refund corruption setup".to_string()),
            order_no: format!("refund-corruption-order-{case_name}"),
        })
        .await
        .expect("refund corruption recharge should run")
        .expect("refund corruption wallet should still exist");
    let refund = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id.clone(),
            user_id,
            amount_usd: 2.0,
            payment_order_id: Some(order.id.clone()),
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: Some("refund corruption setup".to_string()),
            idempotency_key: Some(format!("refund-corruption-idempotency-{case_name}")),
            refund_no: format!("refund-corruption-refund-{case_name}"),
        })
        .await
        .expect("refund corruption request should run");
    let CreateWalletRefundRequestOutcome::Created(refund) = refund else {
        panic!("refund corruption request should be created");
    };

    RefundMutationFixture {
        wallet_id: wallet.id,
        payment_order_id: order.id,
        refund_id: refund.id,
    }
}

async fn process_refund_mutation_fixture(
    repository: &SqliteWalletReadRepository,
    fixture: &RefundMutationFixture,
) {
    let outcome = repository
        .process_admin_wallet_refund(ProcessAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            operator_id: Some("admin-refund-corruption".to_string()),
        })
        .await
        .expect("refund corruption setup should process");
    assert!(matches!(outcome, WalletMutationOutcome::Applied(_)));
}

async fn corrupt_refund_amount(pool: &sqlx::SqlitePool, refund_id: &str, amount_usd: f64) {
    let result = sqlx::query("UPDATE refund_requests SET amount_usd = ? WHERE id = ?")
        .bind(amount_usd)
        .bind(refund_id)
        .execute(pool)
        .await
        .expect("persisted refund amount should be corruptible for the regression test");
    assert_eq!(result.rows_affected(), 1);

    let stored_amount: f64 =
        sqlx::query_scalar("SELECT amount_usd FROM refund_requests WHERE id = ?")
            .bind(refund_id)
            .fetch_one(pool)
            .await
            .expect("corrupted refund amount should query");
    if amount_usd.is_infinite() {
        assert!(stored_amount.is_infinite());
        assert_eq!(
            stored_amount.is_sign_positive(),
            amount_usd.is_sign_positive()
        );
    } else {
        assert_eq!(stored_amount, amount_usd);
    }
}

async fn refund_mutation_snapshot(
    pool: &sqlx::SqlitePool,
    fixture: &RefundMutationFixture,
) -> RefundMutationSnapshot {
    let (wallet_balance, wallet_total_refunded): (f64, f64) =
        sqlx::query_as("SELECT balance, total_refunded FROM wallets WHERE id = ?")
            .bind(&fixture.wallet_id)
            .fetch_one(pool)
            .await
            .expect("refund corruption wallet should query");
    let (order_refunded_amount, order_refundable_amount): (f64, f64) = sqlx::query_as(
        "SELECT refunded_amount_usd, refundable_amount_usd FROM payment_orders WHERE id = ?",
    )
    .bind(&fixture.payment_order_id)
    .fetch_one(pool)
    .await
    .expect("refund corruption payment order should query");
    let (
        refund_status,
        refund_failure_reason,
        refund_gateway_id,
        refund_payout_reference,
        refund_payout_proof,
    ): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        r#"
SELECT status, failure_reason, gateway_refund_id, payout_reference, payout_proof
FROM refund_requests
WHERE id = ?
"#,
    )
    .bind(&fixture.refund_id)
    .fetch_one(pool)
    .await
    .expect("refund corruption request should query");
    let wallet_transaction_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM wallet_transactions WHERE wallet_id = ?")
            .bind(&fixture.wallet_id)
            .fetch_one(pool)
            .await
            .expect("refund corruption transactions should count");

    RefundMutationSnapshot {
        wallet_balance,
        wallet_total_refunded,
        order_refunded_amount,
        order_refundable_amount,
        refund_status,
        refund_failure_reason,
        refund_gateway_id,
        refund_payout_reference,
        refund_payout_proof,
        wallet_transaction_count,
    }
}

#[tokio::test]
async fn sqlite_process_refund_rejects_corrupt_persisted_amount_without_side_effects() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    for (index, amount_usd) in [0.0, -1.0, f64::INFINITY].into_iter().enumerate() {
        let fixture =
            create_refund_mutation_fixture(&repository, &format!("process-{index}")).await;
        corrupt_refund_amount(&pool, &fixture.refund_id, amount_usd).await;
        let before = refund_mutation_snapshot(&pool, &fixture).await;

        let outcome = repository
            .process_admin_wallet_refund(ProcessAdminWalletRefundInput {
                wallet_id: fixture.wallet_id.clone(),
                refund_id: fixture.refund_id.clone(),
                operator_id: Some("admin-refund-corruption".to_string()),
            })
            .await
            .expect("corrupted refund process should be rejected cleanly");

        assert!(matches!(outcome, WalletMutationOutcome::Invalid(_)));
        assert_eq!(refund_mutation_snapshot(&pool, &fixture).await, before);
    }
}

#[tokio::test]
async fn sqlite_complete_refund_rejects_corrupt_persisted_amount_without_side_effects() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    for (index, amount_usd) in [0.0, -1.0, f64::INFINITY].into_iter().enumerate() {
        let fixture =
            create_refund_mutation_fixture(&repository, &format!("complete-{index}")).await;
        process_refund_mutation_fixture(&repository, &fixture).await;
        corrupt_refund_amount(&pool, &fixture.refund_id, amount_usd).await;
        let before = refund_mutation_snapshot(&pool, &fixture).await;

        let outcome = repository
            .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
                wallet_id: fixture.wallet_id.clone(),
                refund_id: fixture.refund_id.clone(),
                gateway_refund_id: Some("must-not-be-stored".to_string()),
                payout_reference: Some("must-not-be-stored".to_string()),
                payout_proof: Some(json!({ "proof": "must-not-be-stored" })),
            })
            .await
            .expect("corrupted refund completion should be rejected cleanly");

        assert!(matches!(outcome, WalletMutationOutcome::Invalid(_)));
        assert_eq!(refund_mutation_snapshot(&pool, &fixture).await, before);
    }
}

#[tokio::test]
async fn sqlite_fail_refund_rejects_corrupt_persisted_amount_without_side_effects() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    for (index, amount_usd) in [0.0, -1.0, f64::INFINITY].into_iter().enumerate() {
        let fixture = create_refund_mutation_fixture(&repository, &format!("fail-{index}")).await;
        process_refund_mutation_fixture(&repository, &fixture).await;
        corrupt_refund_amount(&pool, &fixture.refund_id, amount_usd).await;
        let before = refund_mutation_snapshot(&pool, &fixture).await;

        let outcome = repository
            .fail_admin_wallet_refund(FailAdminWalletRefundInput {
                wallet_id: fixture.wallet_id.clone(),
                refund_id: fixture.refund_id.clone(),
                reason: "must not be stored".to_string(),
                operator_id: Some("admin-refund-corruption".to_string()),
            })
            .await
            .expect("corrupted refund failure should be rejected cleanly");

        assert!(matches!(outcome, WalletMutationOutcome::Invalid(_)));
        assert_eq!(refund_mutation_snapshot(&pool, &fixture).await, before);
    }
}

#[tokio::test]
async fn sqlite_fail_refund_rejects_negative_recharge_balance_without_side_effects() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let fixture = create_refund_mutation_fixture(&repository, "fail-negative-balance").await;
    process_refund_mutation_fixture(&repository, &fixture).await;

    sqlx::query("UPDATE wallets SET balance = ? WHERE id = ?")
        .bind(-1.0_f64)
        .bind(&fixture.wallet_id)
        .execute(&pool)
        .await
        .expect("negative wallet balance should be seedable for the regression test");
    let before = refund_mutation_snapshot(&pool, &fixture).await;

    let outcome = repository
        .fail_admin_wallet_refund(FailAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            reason: "must not recover a corrupt wallet".to_string(),
            operator_id: Some("admin-refund-corruption".to_string()),
        })
        .await
        .expect("negative wallet balance failure should resolve");

    assert!(matches!(outcome, WalletMutationOutcome::Invalid(_)));
    assert_eq!(refund_mutation_snapshot(&pool, &fixture).await, before);
}

#[tokio::test]
async fn sqlite_pending_gateway_refund_evidence_is_durable_and_cannot_be_reverted() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let fixture = create_refund_mutation_fixture(&repository, "pending-gateway").await;
    process_refund_mutation_fixture(&repository, &fixture).await;

    let proof = json!({
        "gateway": "wxpay",
        "status": "processing",
        "refund_no": "provider-refund-1"
    });
    let recorded = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-1".to_string(),
            payout_proof: Some(proof.clone()),
        })
        .await
        .expect("gateway evidence update should run");
    let WalletMutationOutcome::Applied(recorded_refund) = recorded else {
        panic!("gateway evidence should be recorded");
    };
    assert_eq!(recorded_refund.status, "processing");
    assert_eq!(
        recorded_refund.gateway_refund_id.as_deref(),
        Some("provider-refund-1")
    );
    assert_eq!(recorded_refund.payout_proof, Some(proof.clone()));

    let replay = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-1".to_string(),
            payout_proof: Some(json!({ "status": "different" })),
        })
        .await
        .expect("same gateway evidence replay should run");
    let WalletMutationOutcome::Applied(replayed_refund) = replay else {
        panic!("same gateway evidence replay should be accepted");
    };
    assert_eq!(replayed_refund.payout_proof, Some(proof));

    let conflict = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-attacker".to_string(),
            payout_proof: None,
        })
        .await
        .expect("conflicting gateway evidence should resolve");
    assert!(matches!(conflict, WalletMutationOutcome::Invalid(_)));

    let before_fail = refund_mutation_snapshot(&pool, &fixture).await;
    let fail = repository
        .fail_admin_wallet_refund(FailAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            reason: "provider still processing".to_string(),
            operator_id: Some("admin-refund-pending".to_string()),
        })
        .await
        .expect("processing refund failure should resolve");
    assert!(matches!(fail, WalletMutationOutcome::Invalid(_)));
    assert_eq!(refund_mutation_snapshot(&pool, &fixture).await, before_fail);

    let completed = repository
        .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: None,
            payout_reference: None,
            payout_proof: None,
        })
        .await
        .expect("completion should preserve provider evidence");
    let WalletMutationOutcome::Applied(completed_refund) = completed else {
        panic!("processing refund should complete");
    };
    assert_eq!(completed_refund.status, "succeeded");
    assert_eq!(
        completed_refund.gateway_refund_id.as_deref(),
        Some("provider-refund-1")
    );
    assert_eq!(
        completed_refund.payout_proof,
        Some(json!({
            "gateway": "wxpay",
            "status": "processing",
            "refund_no": "provider-refund-1"
        }))
    );

    let terminal_update_conflict = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-attacker".to_string(),
            payout_proof: None,
        })
        .await
        .expect("terminal gateway evidence conflict should resolve");
    assert!(matches!(
        terminal_update_conflict,
        WalletMutationOutcome::Invalid(_)
    ));

    let terminal_complete_conflict = repository
        .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
            wallet_id: fixture.wallet_id,
            refund_id: fixture.refund_id,
            gateway_refund_id: Some("provider-refund-attacker".to_string()),
            payout_reference: None,
            payout_proof: None,
        })
        .await
        .expect("terminal completion conflict should resolve");
    assert!(matches!(
        terminal_complete_conflict,
        WalletMutationOutcome::Invalid(_)
    ));
}

#[tokio::test]
async fn sqlite_success_gateway_refund_proof_upgrades_processing_evidence() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let fixture = create_refund_mutation_fixture(&repository, "success-proof-upgrade").await;
    process_refund_mutation_fixture(&repository, &fixture).await;

    let processing_proof = json!({
        "gateway": "wxpay",
        "id": "provider-refund-upgrade",
        "status": "processing"
    });
    let recorded = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-upgrade".to_string(),
            payout_proof: Some(processing_proof.clone()),
        })
        .await
        .expect("processing evidence should persist");
    assert!(matches!(recorded, WalletMutationOutcome::Applied(_)));

    let replay = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-upgrade".to_string(),
            payout_proof: Some(json!({
                "gateway": "wxpay",
                "id": "provider-refund-upgrade",
                "status": "processing",
                "attempt": 2
            })),
        })
        .await
        .expect("processing replay should resolve");
    let WalletMutationOutcome::Applied(replayed) = replay else {
        panic!("processing replay should be accepted");
    };
    assert_eq!(replayed.payout_proof, Some(processing_proof));

    let success_proof = json!({
        "gateway": "wxpay",
        "id": "provider-refund-upgrade",
        "status": "success",
        "processed_at": "2026-08-29T00:00:00Z"
    });
    let upgraded = repository
        .update_admin_wallet_refund_gateway(UpdateAdminWalletRefundGatewayInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            gateway_refund_id: "provider-refund-upgrade".to_string(),
            payout_proof: Some(success_proof.clone()),
        })
        .await
        .expect("success evidence should upgrade processing proof");
    let WalletMutationOutcome::Applied(upgraded) = upgraded else {
        panic!("success evidence should be accepted");
    };
    assert_eq!(upgraded.payout_proof, Some(success_proof.clone()));

    let completed = repository
        .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id,
            gateway_refund_id: Some("provider-refund-upgrade".to_string()),
            payout_reference: None,
            payout_proof: None,
        })
        .await
        .expect("refund should complete");
    let WalletMutationOutcome::Applied(completed) = completed else {
        panic!("refund should complete");
    };
    assert_eq!(completed.status, "succeeded");
    assert_eq!(completed.payout_proof, Some(success_proof));
}

#[tokio::test]
async fn sqlite_offline_processing_refund_failure_releases_reservation() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let fixture = create_refund_mutation_fixture(&repository, "offline-failure").await;
    process_refund_mutation_fixture(&repository, &fixture).await;
    let before = refund_mutation_snapshot(&pool, &fixture).await;

    let outcome = repository
        .fail_admin_wallet_refund(FailAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            reason: "offline payout was not sent".to_string(),
            operator_id: Some("admin-offline-failure".to_string()),
        })
        .await
        .expect("offline processing refund failure should resolve");
    let WalletMutationOutcome::Applied((wallet, refund, transaction)) = outcome else {
        panic!("offline processing refund should be released");
    };
    let transaction = transaction.expect("refund recovery transaction should be recorded");
    assert_eq!(refund.status, "failed");
    assert_eq!(
        refund.failure_reason.as_deref(),
        Some("offline payout was not sent")
    );
    assert_eq!(transaction.reason_code, "refund_revert");
    assert_eq!(wallet.balance, 10.0);
    assert_eq!(wallet.total_refunded, 0.0);

    let after = refund_mutation_snapshot(&pool, &fixture).await;
    assert_eq!(after.wallet_balance, before.wallet_balance + 2.0);
    assert_eq!(after.wallet_total_refunded, 0.0);
    assert_eq!(after.order_refunded_amount, 0.0);
    assert_eq!(after.order_refundable_amount, 10.0);
    assert_eq!(after.refund_status, "failed");
    assert_eq!(
        after.wallet_transaction_count,
        before.wallet_transaction_count + 1
    );
}

#[tokio::test]
async fn sqlite_processing_refund_requires_offline_mode_without_gateway_evidence() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    let original_channel = create_refund_mutation_fixture(&repository, "original-channel").await;
    process_refund_mutation_fixture(&repository, &original_channel).await;
    sqlx::query("UPDATE refund_requests SET refund_mode = 'original_channel' WHERE id = ?")
        .bind(&original_channel.refund_id)
        .execute(&pool)
        .await
        .expect("refund mode should update for the regression test");

    let proof_only = create_refund_mutation_fixture(&repository, "proof-only").await;
    process_refund_mutation_fixture(&repository, &proof_only).await;
    sqlx::query("UPDATE refund_requests SET payout_proof = ? WHERE id = ?")
        .bind(r#"{"gateway":"manual-settlement"}"#)
        .bind(&proof_only.refund_id)
        .execute(&pool)
        .await
        .expect("proof-only evidence should update for the regression test");

    for fixture in [&original_channel, &proof_only] {
        let before = refund_mutation_snapshot(&pool, fixture).await;
        let outcome = repository
            .fail_admin_wallet_refund(FailAdminWalletRefundInput {
                wallet_id: fixture.wallet_id.clone(),
                refund_id: fixture.refund_id.clone(),
                reason: "must remain reserved".to_string(),
                operator_id: Some("admin-preserve-reservation".to_string()),
            })
            .await
            .expect("protected processing refund failure should resolve");
        assert!(matches!(outcome, WalletMutationOutcome::Invalid(_)));
        assert_eq!(refund_mutation_snapshot(&pool, fixture).await, before);
    }
}

#[tokio::test]
async fn sqlite_refund_rejects_foreign_or_uncredited_payment_order_without_side_effects() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let fixture = create_refund_mutation_fixture(&repository, "order-integrity").await;

    ensure_test_user(&pool, "refund-order-integrity-other").await;
    let other_wallet = repository
        .initialize_auth_user_wallet("refund-order-integrity-other", 0.0, false)
        .await
        .expect("other wallet initialization should run")
        .expect("other wallet should exist");
    let (_, other_order) = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: other_wallet.id,
            amount_usd: 10.0,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("admin-order-integrity".to_string()),
            description: Some("order integrity setup".to_string()),
            order_no: "refund-order-integrity-other-order".to_string(),
        })
        .await
        .expect("other recharge should run")
        .expect("other recharge should create an order");

    sqlx::query("UPDATE refund_requests SET payment_order_id = ? WHERE id = ?")
        .bind(&other_order.id)
        .bind(&fixture.refund_id)
        .execute(&pool)
        .await
        .expect("foreign payment order should be assignable for regression setup");
    let before_foreign = refund_mutation_snapshot(&pool, &fixture).await;
    let foreign_outcome = repository
        .process_admin_wallet_refund(ProcessAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            operator_id: Some("admin-order-integrity".to_string()),
        })
        .await
        .expect("foreign payment order refund should resolve");
    assert!(matches!(foreign_outcome, WalletMutationOutcome::Invalid(_)));
    assert_eq!(
        refund_mutation_snapshot(&pool, &fixture).await,
        before_foreign
    );

    sqlx::query("UPDATE refund_requests SET payment_order_id = ? WHERE id = ?")
        .bind(&fixture.payment_order_id)
        .bind(&fixture.refund_id)
        .execute(&pool)
        .await
        .expect("original payment order should be restored for regression setup");
    sqlx::query("UPDATE payment_orders SET status = 'pending' WHERE id = ?")
        .bind(&fixture.payment_order_id)
        .execute(&pool)
        .await
        .expect("payment order status should be corruptible for regression setup");
    let before_uncredited = refund_mutation_snapshot(&pool, &fixture).await;
    let uncredited_outcome = repository
        .process_admin_wallet_refund(ProcessAdminWalletRefundInput {
            wallet_id: fixture.wallet_id.clone(),
            refund_id: fixture.refund_id.clone(),
            operator_id: Some("admin-order-integrity".to_string()),
        })
        .await
        .expect("uncredited payment order refund should resolve");
    assert!(matches!(
        uncredited_outcome,
        WalletMutationOutcome::Invalid(_)
    ));
    assert_eq!(
        refund_mutation_snapshot(&pool, &fixture).await,
        before_uncredited
    );
}

#[tokio::test]
async fn sqlite_wallet_write_repository_handles_public_recharge_callback_and_refund() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(
        &pool,
        &[
            "user-write-1",
            "user-credit-1",
            "user-expire-1",
            "user-fail-1",
        ],
    )
    .await;
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-write-1".to_string()),
            user_id: "user-write-1".to_string(),
            amount_usd: 12.5,
            pay_amount: Some(12.5),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-order-write-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-no-write-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new wallet order should not already exist")
        }
    };
    assert_eq!(order.status, "pending");

    let callback = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-write-1".to_string(),
            order_no: Some("order-no-write-1".to_string()),
            gateway_order_id: Some("gateway-order-write-1".to_string()),
            amount_usd: 12.5,
            pay_amount: Some(12.5),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-hash-write-1".to_string(),
            payload: json!({
                "status": "paid",
                "client_secret": "pi_1_secret_replayable",
                "customer": {"email": "payer@example.com"},
                "authorization": "Bearer upstream-secret",
            }),
            signature_valid: true,
        })
        .await
        .expect("payment callback should process");
    let ProcessPaymentCallbackOutcome::Applied {
        wallet_id, order, ..
    } = callback
    else {
        panic!("callback should credit the order");
    };
    assert_eq!(wallet_id, "wallet-write-1");
    assert_eq!(order.status, "credited");
    let stored_callback_payload: Option<String> =
        sqlx::query_scalar("SELECT payload FROM payment_callbacks WHERE callback_key = ?")
            .bind("callback-write-1")
            .fetch_one(&pool)
            .await
            .expect("callback payload should query");
    assert_eq!(stored_callback_payload, None);
    let stored_gateway_response: String =
        sqlx::query_scalar("SELECT gateway_response FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(&pool)
            .await
            .expect("gateway response should query");
    let stored_gateway_response: serde_json::Value =
        serde_json::from_str(&stored_gateway_response).expect("gateway response should be JSON");
    assert_eq!(stored_gateway_response["gateway"], "alipay");
    assert_eq!(stored_gateway_response["payment_provider"], "alipay");
    assert_eq!(stored_gateway_response["payment_channel"], "alipay");
    assert_eq!(stored_gateway_response["order_no"], "order-no-write-1");
    assert_eq!(stored_gateway_response["amount_usd"], 12.5);
    assert!(stored_gateway_response.get("status").is_none());
    let stored_settlement_binding: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT payment_method, payment_provider, payment_channel FROM payment_orders WHERE id = ?",
    )
    .bind(&order.id)
    .fetch_one(&pool)
    .await
    .expect("settlement binding should query");
    assert_eq!(
        stored_settlement_binding,
        (
            "alipay".to_string(),
            Some("alipay".to_string()),
            Some("alipay".to_string()),
        )
    );
    let encoded_gateway_response = stored_gateway_response.to_string();
    for forbidden in [
        "client_secret",
        "replayable",
        "payer@example.com",
        "authorization",
        "upstream-secret",
    ] {
        assert!(
            !encoded_gateway_response.contains(forbidden),
            "persisted {forbidden}"
        );
    }

    let wallet = repository
        .find(WalletLookupKey::UserId("user-write-1"))
        .await
        .expect("wallet should query")
        .expect("wallet should exist");
    assert_eq!(wallet.balance, 12.5);
    assert_eq!(wallet.total_recharged, 12.5);

    let refund = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id.clone(),
            user_id: "user-write-1".to_string(),
            amount_usd: 4.0,
            payment_order_id: Some(order.id.clone()),
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: Some("requested".to_string()),
            idempotency_key: Some("idem-refund-write-1".to_string()),
            refund_no: "refund-no-write-1".to_string(),
        })
        .await
        .expect("refund request should create");
    let CreateWalletRefundRequestOutcome::Created(refund) = refund else {
        panic!("refund request should be created");
    };

    let duplicate = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id.clone(),
            user_id: "user-write-1".to_string(),
            amount_usd: 4.0,
            payment_order_id: Some(order.id.clone()),
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: Some("requested".to_string()),
            idempotency_key: Some("idem-refund-write-1".to_string()),
            refund_no: "refund-no-write-duplicate".to_string(),
        })
        .await
        .expect("duplicate refund request should resolve");
    assert!(matches!(
        duplicate,
        CreateWalletRefundRequestOutcome::Duplicate(_)
    ));

    let processed = repository
        .process_admin_wallet_refund(ProcessAdminWalletRefundInput {
            wallet_id: wallet.id.clone(),
            refund_id: refund.id.clone(),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("refund should process");
    let WalletMutationOutcome::Applied((wallet, refund, transaction)) = processed else {
        panic!("refund should be processed");
    };
    assert_eq!(wallet.balance, 8.5);
    assert_eq!(refund.status, "processing");
    assert_eq!(transaction.reason_code, "refund_out");

    let completed = repository
        .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
            wallet_id: wallet.id.clone(),
            refund_id: refund.id.clone(),
            gateway_refund_id: Some("gateway-refund-write-1".to_string()),
            payout_reference: Some("payout-ref-write-1".to_string()),
            payout_proof: Some(json!({ "proof": "ok" })),
        })
        .await
        .expect("refund should complete");
    let WalletMutationOutcome::Applied(completed_refund) = completed else {
        panic!("refund should be completed");
    };
    assert_eq!(completed_refund.status, "succeeded");
    assert_eq!(
        completed_refund.payout_proof.as_ref().unwrap()["proof"],
        "ok"
    );
    let repeated_completion = repository
        .complete_admin_wallet_refund(CompleteAdminWalletRefundInput {
            wallet_id: wallet.id.clone(),
            refund_id: refund.id.clone(),
            gateway_refund_id: Some("gateway-refund-attacker".to_string()),
            payout_reference: Some("payout-ref-attacker".to_string()),
            payout_proof: Some(json!({ "proof": "attacker" })),
        })
        .await
        .expect("completed refund replay should resolve");
    assert!(matches!(
        repeated_completion,
        WalletMutationOutcome::Invalid(_)
    ));
    let repeated_refund = repository
        .find_wallet_refund(&wallet.id, &refund.id)
        .await
        .expect("completed refund should still load")
        .expect("completed refund should still exist");
    assert_eq!(
        repeated_refund.gateway_refund_id.as_deref(),
        Some("gateway-refund-write-1")
    );
    assert_eq!(
        repeated_refund.payout_reference.as_deref(),
        Some("payout-ref-write-1")
    );
    assert_eq!(repeated_refund.payout_proof, completed_refund.payout_proof);

    let refund_to_fail = repository
        .create_wallet_refund_request(CreateWalletRefundRequestInput {
            wallet_id: wallet.id.clone(),
            user_id: "user-write-1".to_string(),
            amount_usd: 1.5,
            payment_order_id: Some(order.id.clone()),
            source_type: None,
            source_id: None,
            refund_mode: None,
            reason: Some("requested again".to_string()),
            idempotency_key: Some("idem-refund-write-2".to_string()),
            refund_no: "refund-no-write-2".to_string(),
        })
        .await
        .expect("second refund request should create");
    let CreateWalletRefundRequestOutcome::Created(refund_to_fail) = refund_to_fail else {
        panic!("second refund request should be created");
    };
    let before_fail = refund_mutation_snapshot(
        &pool,
        &RefundMutationFixture {
            wallet_id: wallet.id.clone(),
            payment_order_id: order.id.clone(),
            refund_id: refund_to_fail.id.clone(),
        },
    )
    .await;
    let failed = repository
        .fail_admin_wallet_refund(FailAdminWalletRefundInput {
            wallet_id: wallet.id.clone(),
            refund_id: refund_to_fail.id.clone(),
            reason: "manual failure".to_string(),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("second refund failure should resolve");
    assert!(matches!(&failed, WalletMutationOutcome::Applied(_)));
    let failed_refund = match failed {
        WalletMutationOutcome::Applied((_, refund, transaction)) => {
            assert!(transaction.is_none());
            refund
        }
        _ => unreachable!(),
    };
    assert_eq!(failed_refund.status, "failed");
    let after_fail = refund_mutation_snapshot(
        &pool,
        &RefundMutationFixture {
            wallet_id: wallet.id.clone(),
            payment_order_id: order.id.clone(),
            refund_id: refund_to_fail.id.clone(),
        },
    )
    .await;
    assert_eq!(after_fail.wallet_balance, before_fail.wallet_balance);
    assert_eq!(
        after_fail.wallet_total_refunded,
        before_fail.wallet_total_refunded
    );
    assert_eq!(after_fail.refund_status, "failed");

    let batch = repository
        .create_admin_redeem_code_batch(CreateAdminRedeemCodeBatchInput {
            name: "Write Batch".to_string(),
            amount_usd: 3.5,
            currency: "USD".to_string(),
            balance_bucket: "gift".to_string(),
            total_count: 1,
            expires_at_unix_secs: None,
            description: Some("write smoke".to_string()),
            created_by: Some("admin-1".to_string()),
        })
        .await
        .expect("redeem batch should create");
    assert_eq!(batch.batch.active_count, 1);
    let redeem_code = batch.codes[0].code.clone();

    let redeem = repository
        .redeem_wallet_code(RedeemWalletCodeInput {
            code: redeem_code,
            user_id: "user-write-1".to_string(),
            order_no: "redeem-order-write-1".to_string(),
        })
        .await
        .expect("redeem should apply");
    let RedeemWalletCodeOutcome::Redeemed {
        wallet,
        order,
        amount_usd,
        batch_name,
    } = redeem
    else {
        panic!("redeem should succeed");
    };
    assert_eq!(wallet.gift_balance, 3.5);
    assert_eq!(order.payment_method, "gift_code");
    assert_eq!(amount_usd, 3.5);
    assert_eq!(batch_name, "Write Batch");

    let disabled_batch = repository
        .create_admin_redeem_code_batch(CreateAdminRedeemCodeBatchInput {
            name: "Disabled Batch".to_string(),
            amount_usd: 1.25,
            currency: "USD".to_string(),
            balance_bucket: "gift".to_string(),
            total_count: 2,
            expires_at_unix_secs: None,
            description: Some("disable smoke".to_string()),
            created_by: Some("admin-1".to_string()),
        })
        .await
        .expect("disable batch should create");
    let disabled_code = repository
        .disable_admin_redeem_code(DisableAdminRedeemCodeInput {
            code_id: disabled_batch.codes[0].code_id.clone(),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("redeem code should disable");
    let WalletMutationOutcome::Applied(disabled_code) = disabled_code else {
        panic!("redeem code should be disabled");
    };
    assert_eq!(disabled_code.status, "disabled");

    let disabled_batch = repository
        .disable_admin_redeem_code_batch(DisableAdminRedeemCodeBatchInput {
            batch_id: disabled_batch.batch.id.clone(),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("redeem batch should disable");
    let WalletMutationOutcome::Applied(disabled_batch) = disabled_batch else {
        panic!("redeem batch should be disabled");
    };
    assert_eq!(disabled_batch.status, "disabled");
    assert_eq!(disabled_batch.active_count, 0);

    let deleted_batch = repository
        .delete_admin_redeem_code_batch(DeleteAdminRedeemCodeBatchInput {
            batch_id: disabled_batch.id,
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("disabled unredeemed batch should delete");
    assert!(matches!(deleted_batch, WalletMutationOutcome::Applied(_)));

    let (wallet, adjustment) = repository
        .adjust_wallet_balance(AdjustWalletBalanceInput {
            wallet_id: wallet.id.clone(),
            amount_usd: -2.0,
            balance_type: "gift".to_string(),
            operator_id: Some("admin-1".to_string()),
            description: Some("trim gift".to_string()),
        })
        .await
        .expect("adjustment should run")
        .expect("wallet should exist");
    assert_eq!(wallet.gift_balance, 1.5);
    assert_eq!(adjustment.reason_code, "adjust_admin");
    assert_eq!(
        adjustment.balance_after,
        wallet.balance + wallet.gift_balance
    );

    let (wallet, order) = repository
        .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
            wallet_id: wallet.id,
            amount_usd: 5.0,
            payment_method: "admin_manual".to_string(),
            operator_id: Some("admin-1".to_string()),
            description: Some("manual topup".to_string()),
            order_no: "manual-order-write-1".to_string(),
        })
        .await
        .expect("manual recharge should run")
        .expect("wallet should exist");
    assert_eq!(wallet.balance, 13.5);
    assert_eq!(order.status, "credited");
    assert_eq!(order.payment_method, "admin_manual");

    let credit_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-credit-1".to_string()),
            user_id: "user-credit-1".to_string(),
            amount_usd: 2.25,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            payment_method: "manual_gateway".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-order-credit-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-no-credit-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("credit order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new credit wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new credit order should not already exist")
        }
    };
    let credited = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: credit_order.id.clone(),
            gateway_order_id: Some("gateway-order-credit-paid-1".to_string()),
            pay_amount: Some(2.25),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            gateway_response_patch: Some(json!({ "settled": true })),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("credit order should apply");
    let WalletMutationOutcome::Applied((credited_order, applied)) = credited else {
        panic!("credit order should be applied");
    };
    assert!(applied);
    assert_eq!(credited_order.status, "credited");
    assert_eq!(
        credited_order.gateway_response.as_ref().unwrap()["manual_credit"],
        true
    );
    let credited_again = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: credit_order.id,
            gateway_order_id: None,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            gateway_response_patch: None,
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("credit order should be idempotent");
    assert!(matches!(
        credited_again,
        WalletMutationOutcome::Applied((_, false))
    ));

    let expiring_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-expire-1".to_string()),
            user_id: "user-expire-1".to_string(),
            amount_usd: 1.0,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            payment_method: "alipay".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-order-expire-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-no-expire-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("expiring order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new expire wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new expiring order should not already exist")
        }
    };
    let expired = repository
        .expire_admin_payment_order(&expiring_order.id)
        .await
        .expect("expire should run");
    assert!(matches!(expired, WalletMutationOutcome::Applied((_, true))));
    let expired_again = repository
        .expire_admin_payment_order(&expiring_order.id)
        .await
        .expect("expire should be idempotent");
    assert!(matches!(
        expired_again,
        WalletMutationOutcome::Applied((_, false))
    ));

    let failing_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-fail-1".to_string()),
            user_id: "user-fail-1".to_string(),
            amount_usd: 1.0,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            payment_method: "alipay".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-order-fail-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-no-fail-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("failing order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new fail wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new failing order should not already exist")
        }
    };
    let failed_order = repository
        .fail_admin_payment_order(&failing_order.id)
        .await
        .expect("fail should run");
    let WalletMutationOutcome::Applied(failed_order) = failed_order else {
        panic!("payment order should fail");
    };
    assert_eq!(failed_order.status, "failed");
}

#[tokio::test]
async fn sqlite_payment_callback_rejects_gateway_identifier_mismatch_without_crediting() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-identifier-mismatch").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-identifier-mismatch".to_string()),
            user_id: "user-callback-identifier-mismatch".to_string(),
            amount_usd: 15.0,
            pay_amount: Some(15.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-order-original".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-no-identifier-mismatch".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new identifier-mismatch order should not already exist")
        }
    };

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-identifier-mismatch".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-order-attacker".to_string()),
            amount_usd: 15.0,
            pay_amount: Some(15.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-identifier-mismatch".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("mismatched callback should resolve");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment gateway order mismatch"
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("wallet-callback-identifier-mismatch")
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (0.0, 0.0));

    let stored_order: (String, String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT status, gateway_order_id, paid_at, credited_at FROM payment_orders WHERE id = ?",
    )
    .bind(&order.id)
    .fetch_one(&pool)
    .await
    .expect("order should load");
    assert_eq!(stored_order.0, "pending");
    assert_eq!(stored_order.1, "gateway-order-original");
    assert_eq!(stored_order.2, None);
    assert_eq!(stored_order.3, None);

    let callback: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_message FROM payment_callbacks WHERE callback_key = ?",
    )
    .bind("callback-identifier-mismatch")
    .fetch_one(&pool)
    .await
    .expect("callback should load");
    assert_eq!(callback.0, "failed");
    assert_eq!(
        callback.1.as_deref(),
        Some("payment gateway order mismatch")
    );
}

#[tokio::test]
async fn sqlite_payment_callback_rejects_wallet_owner_mismatch_without_crediting() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(&pool, &["callback-order-owner", "callback-wallet-owner"]).await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-owner-mismatch".to_string()),
            user_id: "callback-wallet-owner".to_string(),
            amount_usd: 8.0,
            pay_amount: Some(8.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-owner-mismatch".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-owner-mismatch".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    // Simulate a corrupted/imported order that points at a different user
    // than the wallet selected at checkout.
    sqlx::query("UPDATE payment_orders SET user_id = ? WHERE id = ?")
        .bind("callback-order-owner")
        .bind(&order.id)
        .execute(&pool)
        .await
        .expect("order owner should update for regression setup");

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-owner-mismatch".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-owner-mismatch".to_string()),
            amount_usd: 8.0,
            pay_amount: Some(8.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-owner-mismatch".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("owner-mismatch callback should resolve");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment order wallet owner mismatch"
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind(&order.wallet_id)
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (0.0, 0.0));
    let stored_order: (String, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT status, paid_at, credited_at FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(&pool)
            .await
            .expect("order should load");
    assert_eq!(stored_order, ("pending".to_string(), None, None));
    let callback: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_message FROM payment_callbacks WHERE callback_key = ?",
    )
    .bind("callback-owner-mismatch")
    .fetch_one(&pool)
    .await
    .expect("callback should load");
    assert_eq!(callback.0, "failed");
    assert_eq!(
        callback.1.as_deref(),
        Some("payment order wallet owner mismatch")
    );
}

#[tokio::test]
async fn sqlite_payment_callback_credits_overdrawn_recharge_balance() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-overdrawn").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-overdrawn".to_string()),
            user_id: "user-callback-overdrawn".to_string(),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-callback-overdrawn".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-callback-overdrawn".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    sqlx::query("UPDATE wallets SET balance = -3.0, total_recharged = 0.0 WHERE id = ?")
        .bind(&order.wallet_id)
        .execute(&pool)
        .await
        .expect("wallet should be made overdrawn");

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-overdrawn".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-callback-overdrawn".to_string()),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-callback-overdrawn".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("payment callback should process");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));

    let wallet = repository
        .find(WalletLookupKey::UserId("user-callback-overdrawn"))
        .await
        .expect("wallet should query")
        .expect("wallet should exist");
    assert_eq!(wallet.balance, 2.0);
    assert_eq!(wallet.total_recharged, 5.0);

    let callback: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_message FROM payment_callbacks WHERE callback_key = ?",
    )
    .bind("callback-overdrawn")
    .fetch_one(&pool)
    .await
    .expect("callback should load");
    assert_eq!(callback.0, "processed");
    assert_eq!(callback.1, None);
}

#[tokio::test]
async fn sqlite_manual_credit_rejects_invalid_order_and_wallet_values() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-manual-credit-invalid").await;
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-manual-credit-invalid".to_string()),
            user_id: "user-manual-credit-invalid".to_string(),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "manual_gateway".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-manual-credit-invalid".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-manual-credit-invalid".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("credit order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    sqlx::query("UPDATE payment_orders SET amount_usd = ? WHERE id = ?")
        .bind(-5.0)
        .bind(&order.id)
        .execute(&pool)
        .await
        .expect("invalid order fixture should update");
    let invalid_order = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: order.id.clone(),
            gateway_order_id: None,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            gateway_response_patch: None,
            operator_id: Some("admin-invalid".to_string()),
        })
        .await
        .expect("invalid order credit should resolve");
    assert!(matches!(
        invalid_order,
        WalletMutationOutcome::Invalid(ref error) if error == "payment order amount is invalid"
    ));
    let order_status: String = sqlx::query_scalar("SELECT status FROM payment_orders WHERE id = ?")
        .bind(&order.id)
        .fetch_one(&pool)
        .await
        .expect("order status should query");
    assert_eq!(order_status, "pending");

    sqlx::query("UPDATE payment_orders SET amount_usd = ? WHERE id = ?")
        .bind(5.0)
        .bind(&order.id)
        .execute(&pool)
        .await
        .expect("order fixture should restore");
    sqlx::query("UPDATE wallets SET gift_balance = ? WHERE id = ?")
        .bind(-1.0)
        .bind(&order.wallet_id)
        .execute(&pool)
        .await
        .expect("invalid wallet fixture should update");
    let invalid_wallet = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: order.id.clone(),
            gateway_order_id: None,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            gateway_response_patch: None,
            operator_id: Some("admin-invalid".to_string()),
        })
        .await
        .expect("invalid wallet credit should resolve");
    assert!(matches!(
        invalid_wallet,
        WalletMutationOutcome::Invalid(ref error) if error == "wallet balance is invalid"
    ));
    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, gift_balance FROM wallets WHERE id = ?")
            .bind(&order.wallet_id)
            .fetch_one(&pool)
            .await
            .expect("wallet should query");
    assert_eq!(wallet, (0.0, -1.0));
    let order_status: String = sqlx::query_scalar("SELECT status FROM payment_orders WHERE id = ?")
        .bind(&order.id)
        .fetch_one(&pool)
        .await
        .expect("order status should query");
    assert_eq!(order_status, "pending");
}

#[tokio::test]
async fn sqlite_payment_callback_rejects_unknown_order_status_without_crediting() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-invalid-state").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-invalid-state".to_string()),
            user_id: "user-callback-invalid-state".to_string(),
            amount_usd: 11.0,
            pay_amount: Some(11.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-invalid-state".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-invalid-state".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    sqlx::query("UPDATE payment_orders SET status = 'cancelled' WHERE id = ?")
        .bind(&order.id)
        .execute(&pool)
        .await
        .expect("test order status should update");

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-invalid-state".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-invalid-state".to_string()),
            amount_usd: 11.0,
            pay_amount: Some(11.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-invalid-state".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("invalid-state callback should resolve");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment order is not creditable: cancelled"
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("wallet-callback-invalid-state")
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (0.0, 0.0));
    let stored_order: (String, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT status, paid_at, credited_at FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(&pool)
            .await
            .expect("order should load");
    assert_eq!(stored_order, ("cancelled".to_string(), None, None));
}

#[tokio::test]
async fn sqlite_payment_callback_recovers_failed_checkout_placeholder_without_losing_credit() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-failed-checkout").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-failed-checkout".to_string()),
            user_id: "user-callback-failed-checkout".to_string(),
            amount_usd: 12.0,
            pay_amount: Some(12.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "order-callback-failed-checkout".to_string(),
            gateway_response: json!({
                "gateway": "alipay",
                "gateway_order_id": "order-callback-failed-checkout",
                "order_kind": "wallet_recharge",
                "payment_channel": "alipay",
                "pay_amount": 12.0,
                "pay_currency": "USD",
                "integration_status": "checkout_pending",
                "checkout_claim_token": "claim-failed-checkout",
                "checkout_claimed_at_unix_secs": 1,
            }),
            order_no: "order-callback-failed-checkout".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    let claim_token = order
        .gateway_response
        .as_ref()
        .and_then(|response| response.get("checkout_claim_token"))
        .and_then(serde_json::Value::as_str)
        .expect("checkout claim token should persist")
        .to_string();

    let failed = repository
        .fail_wallet_recharge_checkout(FailWalletRechargeCheckoutInput {
            order_id: order.id.clone(),
            claim_token,
            reason: "checkout response timed out after provider acceptance".to_string(),
            provider_request_may_have_succeeded: true,
        })
        .await
        .expect("checkout failure should resolve");
    let WalletMutationOutcome::Applied(failed) = failed else {
        panic!("checkout placeholder should become failed");
    };
    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed
            .gateway_response
            .as_ref()
            .and_then(|response| response.get("integration_status"))
            .and_then(serde_json::Value::as_str),
        Some("checkout_uncertain")
    );

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-failed-checkout-recovery".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("provider-order-failed-checkout".to_string()),
            amount_usd: 12.0,
            pay_amount: Some(12.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-failed-checkout-recovery".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("provider callback should process");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind(&order.wallet_id)
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (12.0, 12.0));

    let stored_order: (String, Option<String>) =
        sqlx::query_as("SELECT status, gateway_order_id FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(&pool)
            .await
            .expect("order should load");
    assert_eq!(
        stored_order,
        (
            "credited".to_string(),
            Some("provider-order-failed-checkout".to_string())
        )
    );

    let callback_status: String =
        sqlx::query_scalar("SELECT status FROM payment_callbacks WHERE callback_key = ?")
            .bind("callback-failed-checkout-recovery")
            .fetch_one(&pool)
            .await
            .expect("callback should load");
    assert_eq!(callback_status, "processed");
}

#[tokio::test]
async fn sqlite_payment_callback_rejects_corrupt_stored_credit_amount() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-corrupt-amount").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-corrupt-amount".to_string()),
            user_id: "user-callback-corrupt-amount".to_string(),
            amount_usd: 11.0,
            pay_amount: Some(11.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-corrupt-amount".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-corrupt-amount".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    sqlx::query("UPDATE payment_orders SET amount_usd = -11 WHERE id = ?")
        .bind(&order.id)
        .execute(&pool)
        .await
        .expect("test order amount should update");

    let outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-corrupt-amount".to_string(),
            order_no: Some(order.order_no),
            gateway_order_id: Some("gateway-corrupt-amount".to_string()),
            amount_usd: 11.0,
            pay_amount: Some(11.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-corrupt-amount".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("corrupt-amount callback should resolve");
    assert!(matches!(
        outcome,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment order amount is invalid"
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("wallet-callback-corrupt-amount")
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (0.0, 0.0));
}

#[tokio::test]
async fn sqlite_payment_callback_reconstructs_legacy_provider_amount_from_order_terms() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(
        &pool,
        &["legacy-cny-callback-user", "legacy-usd-callback-user"],
    )
    .await;

    let cny_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("legacy-cny-callback-wallet".to_string()),
            user_id: "legacy-cny-callback-user".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "legacy-cny-callback-gateway".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "legacy-cny-callback-order".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("CNY recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    sqlx::query("UPDATE payment_orders SET pay_amount = NULL WHERE id = ?")
        .bind(&cny_order.id)
        .execute(&pool)
        .await
        .expect("legacy CNY order should drop provider amount");

    let cny_outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "legacy-cny-callback".to_string(),
            order_no: Some(cny_order.order_no),
            gateway_order_id: Some("legacy-cny-callback-gateway".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "legacy-cny-callback-payload".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("legacy CNY callback should resolve");
    assert!(matches!(
        cny_outcome,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));
    let cny_wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("legacy-cny-callback-wallet")
            .fetch_one(&pool)
            .await
            .expect("legacy CNY wallet should load");
    assert_eq!(cny_wallet, (10.0, 10.0));

    let usd_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("legacy-usd-callback-wallet".to_string()),
            user_id: "legacy-usd-callback-user".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(10.0),
            pay_currency: Some("USD".to_string()),
            // Old rows could retain the historical CNY default for USD.
            exchange_rate: Some(7.2),
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            gateway_order_id: "legacy-usd-callback-gateway".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "legacy-usd-callback-order".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("USD recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    sqlx::query("UPDATE payment_orders SET pay_amount = NULL WHERE id = ?")
        .bind(&usd_order.id)
        .execute(&pool)
        .await
        .expect("legacy USD order should drop provider amount");

    let wrong_usd_outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            callback_key: "legacy-usd-callback-wrong".to_string(),
            order_no: Some(usd_order.order_no.clone()),
            gateway_order_id: Some("legacy-usd-callback-gateway".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "legacy-usd-callback-wrong-payload".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("wrong USD callback should resolve");
    assert!(matches!(
        wrong_usd_outcome,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "callback amount mismatch"
    ));
    let usd_wallet_before: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("legacy-usd-callback-wallet")
            .fetch_one(&pool)
            .await
            .expect("legacy USD wallet should load before valid callback");
    assert_eq!(usd_wallet_before, (0.0, 0.0));

    let valid_usd_outcome = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            callback_key: "legacy-usd-callback-valid".to_string(),
            order_no: Some(usd_order.order_no),
            gateway_order_id: Some("legacy-usd-callback-gateway".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(10.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "legacy-usd-callback-valid-payload".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("valid USD callback should resolve");
    assert!(matches!(
        valid_usd_outcome,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));
    let usd_wallet_after: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("legacy-usd-callback-wallet")
            .fetch_one(&pool)
            .await
            .expect("legacy USD wallet should load after valid callback");
    assert_eq!(usd_wallet_after, (10.0, 10.0));
}

#[tokio::test]
async fn sqlite_payment_callback_requires_provider_namespace_to_match_exactly() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-provider-boundary").await;

    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-provider-boundary".to_string()),
            user_id: "user-provider-boundary".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-provider-boundary".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-provider-boundary".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    let error = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: None,
            payment_channel: None,
            callback_key: "callback-provider-boundary".to_string(),
            order_no: Some(order.order_no),
            gateway_order_id: Some("gateway-provider-boundary".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "payload-provider-boundary".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect_err("provider-less official callback must be rejected at repository boundary");
    assert!(matches!(
        error,
        aether_data_contracts::DataLayerError::InvalidInput(ref detail)
            if detail == "official payment callback provider binding mismatch"
    ));
    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("wallet-provider-boundary")
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (0.0, 0.0));
}

#[tokio::test]
async fn sqlite_legacy_epay_channel_order_without_provider_is_compatible() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-legacy-epay").await;

    // Rows written before payment_provider/payment_channel were introduced
    // stored the selected EPay channel as payment_method and left both new
    // columns NULL.
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-legacy-epay".to_string()),
            user_id: "user-legacy-epay".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payment_method: "alipay".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-legacy-epay".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-legacy-epay".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("legacy order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    let wrong_channel = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("wxpay".to_string()),
            callback_key: "callback-legacy-epay-wrong-channel".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-legacy-epay".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "payload-legacy-epay-wrong-channel".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("wrong-channel callback should resolve");
    assert!(matches!(
        wrong_channel,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment channel mismatch"
    ));

    let applied = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-legacy-epay-success".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-legacy-epay".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(72.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.2),
            payload_hash: "payload-legacy-epay-success".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("legacy EPay callback should process");
    assert!(matches!(
        applied,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind(&order.wallet_id)
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (10.0, 10.0));
}

#[tokio::test]
async fn sqlite_payment_callback_validates_placeholder_gateway_binding_and_conflicts() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(
        &pool,
        &[
            "user-callback-placeholder-a",
            "user-callback-placeholder-b",
            "user-callback-placeholder-c",
        ],
    )
    .await;

    // Order B already owns the real provider transaction id.
    let order_b = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-placeholder-b".to_string()),
            user_id: "user-callback-placeholder-b".to_string(),
            amount_usd: 7.0,
            pay_amount: Some(7.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-b".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-b".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("order B should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    // Order A stores its merchant order number as a provider-id placeholder.
    let order_a = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-placeholder-a".to_string()),
            user_id: "user-callback-placeholder-a".to_string(),
            amount_usd: 7.0,
            pay_amount: Some(7.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "order-a".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-a".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("order A should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    let conflict = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-placeholder-conflict".to_string(),
            order_no: Some(order_a.order_no.clone()),
            gateway_order_id: Some("gateway-b".to_string()),
            amount_usd: 7.0,
            pay_amount: Some(7.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-placeholder-conflict".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("placeholder conflict callback should resolve");
    assert!(matches!(
        conflict,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment gateway order belongs to another payment order"
    ));
    let order_a_state: (String, String) =
        sqlx::query_as("SELECT status, gateway_order_id FROM payment_orders WHERE id = ?")
            .bind(&order_a.id)
            .fetch_one(&pool)
            .await
            .expect("order A should load");
    assert_eq!(
        order_a_state,
        ("pending".to_string(), "order-a".to_string())
    );
    let order_b_state: (String, f64) =
        sqlx::query_as("SELECT status, amount_usd FROM payment_orders WHERE id = ?")
            .bind(&order_b.id)
            .fetch_one(&pool)
            .await
            .expect("order B should load");
    assert_eq!(order_b_state.0, "pending");
    assert_eq!(order_b_state.1, 7.0);

    // A fresh provider id that is not owned by another order is bound and
    // credited atomically, replacing the placeholder.
    let order_c = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-placeholder-c".to_string()),
            user_id: "user-callback-placeholder-c".to_string(),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "order-c".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-c".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("order C should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };
    let applied = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-placeholder-success".to_string(),
            order_no: Some(order_c.order_no.clone()),
            gateway_order_id: Some("gateway-c".to_string()),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-placeholder-success".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("placeholder binding callback should apply");
    assert!(matches!(
        applied,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));
    let order_c_state: (String, String) =
        sqlx::query_as("SELECT status, gateway_order_id FROM payment_orders WHERE id = ?")
            .bind(&order_c.id)
            .fetch_one(&pool)
            .await
            .expect("order C should load");
    assert_eq!(
        order_c_state,
        ("credited".to_string(), "gateway-c".to_string())
    );
    let wallet_c_balance: f64 = sqlx::query_scalar("SELECT balance FROM wallets WHERE id = ?")
        .bind("wallet-callback-placeholder-c")
        .fetch_one(&pool)
        .await
        .expect("wallet C should load");
    assert_eq!(wallet_c_balance, 5.0);
}

#[tokio::test]
async fn sqlite_payment_gateway_order_identifier_is_unique_within_payment_method() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(
        &pool,
        &[
            "user-gateway-unique-first",
            "user-gateway-unique-second",
            "user-gateway-other-method",
            "user-gateway-case-distinct",
        ],
    )
    .await;

    let first = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-gateway-unique-first".to_string()),
            user_id: "user-gateway-unique-first".to_string(),
            amount_usd: 3.0,
            pay_amount: Some(3.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: " EPAY ".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "shared-provider-transaction".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-gateway-unique-first".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("first order should create");
    assert!(matches!(
        first,
        CreateWalletRechargeOrderOutcome::Created(ref order) if order.payment_method == "epay"
    ));

    let duplicate = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-gateway-unique-second".to_string()),
            user_id: "user-gateway-unique-second".to_string(),
            amount_usd: 3.0,
            pay_amount: Some(3.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "shared-provider-transaction".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-gateway-unique-second".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await;
    assert!(duplicate.is_err());
    assert!(repository
        .find(WalletLookupKey::UserId("user-gateway-unique-second"))
        .await
        .expect("conflicting order must not leave a wallet")
        .is_none());

    let other_method = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-gateway-other-method".to_string()),
            user_id: "user-gateway-other-method".to_string(),
            amount_usd: 3.0,
            pay_amount: Some(3.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "stripe".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "shared-provider-transaction".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-gateway-other-method".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("another payment method may reuse the identifier");
    assert!(matches!(
        other_method,
        CreateWalletRechargeOrderOutcome::Created(_)
    ));

    let case_distinct_identifier = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-gateway-case-distinct".to_string()),
            user_id: "user-gateway-case-distinct".to_string(),
            amount_usd: 3.0,
            pay_amount: Some(3.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "Shared-Provider-Transaction".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-gateway-case-distinct".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("opaque identifiers that differ by case may coexist");
    assert!(matches!(
        case_distinct_identifier,
        CreateWalletRechargeOrderOutcome::Created(_)
    ));
}

#[tokio::test]
async fn sqlite_payment_callback_failure_does_not_rebind_existing_identifiers() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-failure-preserve").await;
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-failure-preserve".to_string()),
            user_id: "user-callback-failure-preserve".to_string(),
            amount_usd: 6.0,
            pay_amount: Some(6.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-preserve".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-failure-preserve".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => panic!("wallet should be active"),
        CreateWalletRechargeOrderOutcome::Existing(_) => panic!("order should be new"),
    };

    let first_failure = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-failure-preserve".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-preserve".to_string()),
            amount_usd: 6.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-failure-preserve".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("first failure should resolve");
    assert!(matches!(
        first_failure,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "callback amount mismatch"
    ));

    let second_failure = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-failure-preserve".to_string(),
            order_no: Some(order.order_no.clone()),
            gateway_order_id: Some("gateway-attacker-retry".to_string()),
            amount_usd: 6.0,
            pay_amount: Some(6.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payload_hash: "payload-failure-preserve".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("second failure should resolve");
    assert!(matches!(
        second_failure,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment gateway order mismatch"
    ));

    let callback: (String, String, Option<String>) = sqlx::query_as(
        "SELECT gateway_order_id, status, error_message FROM payment_callbacks WHERE callback_key = ?",
    )
    .bind("callback-failure-preserve")
    .fetch_one(&pool)
    .await
    .expect("callback should load");
    assert_eq!(callback.0, "gateway-preserve");
    assert_eq!(callback.1, "failed");
    assert_eq!(
        callback.2.as_deref(),
        Some("payment gateway order mismatch")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_payment_callback_registration_is_atomic_under_concurrency() {
    let database_path = std::env::temp_dir().join(format!(
        "aether-sqlite-payment-callback-race-{}.db",
        uuid::Uuid::new_v4()
    ));
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&database_path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(30));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "user-callback-race").await;
    let order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-callback-race".to_string()),
            user_id: "user-callback-race".to_string(),
            amount_usd: 9.0,
            pay_amount: Some(9.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "alipay".to_string(),
            payment_provider: Some("alipay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-callback-race".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-callback-race".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new callback-race order should not already exist")
        }
    };

    let callback_input = ProcessPaymentCallbackInput {
        payment_method: "alipay".to_string(),
        payment_provider: Some("alipay".to_string()),
        payment_channel: Some("alipay".to_string()),
        callback_key: "callback-key-race".to_string(),
        order_no: Some(order.order_no.clone()),
        gateway_order_id: Some("gateway-callback-race".to_string()),
        amount_usd: 9.0,
        pay_amount: Some(9.0),
        pay_currency: Some("USD".to_string()),
        exchange_rate: Some(1.0),
        payload_hash: "payload-hash-race".to_string(),
        payload: json!({ "status": "paid" }),
        signature_valid: true,
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_repository = repository.clone();
    let first_barrier = barrier.clone();
    let first_input = callback_input.clone();
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_repository.process_payment_callback(first_input).await
    });
    let second_repository = repository.clone();
    let second_barrier = barrier.clone();
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_repository
            .process_payment_callback(callback_input)
            .await
    });
    barrier.wait().await;

    let outcomes = [
        first
            .await
            .expect("first callback task should join")
            .expect("first callback should resolve"),
        second
            .await
            .expect("second callback task should join")
            .expect("second callback should resolve"),
    ];
    let mut applied = 0;
    let mut duplicate = 0;
    for outcome in outcomes {
        match outcome {
            ProcessPaymentCallbackOutcome::Applied {
                duplicate: false, ..
            } => applied += 1,
            ProcessPaymentCallbackOutcome::DuplicateProcessed { .. }
            | ProcessPaymentCallbackOutcome::Applied {
                duplicate: true, ..
            }
            | ProcessPaymentCallbackOutcome::AlreadyCredited {
                duplicate: true, ..
            } => duplicate += 1,
            other => panic!("unexpected concurrent callback outcome: {other:?}"),
        }
    }
    assert_eq!(applied, 1, "exactly one callback should apply the credit");
    assert_eq!(duplicate, 1, "the other callback should be a duplicate");

    let wallet: (f64, f64) =
        sqlx::query_as("SELECT balance, total_recharged FROM wallets WHERE id = ?")
            .bind("wallet-callback-race")
            .fetch_one(&pool)
            .await
            .expect("wallet should load");
    assert_eq!(wallet, (9.0, 9.0));
    let callback_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payment_callbacks WHERE callback_key = ?")
            .bind("callback-key-race")
            .fetch_one(&pool)
            .await
            .expect("callback count should query");
    assert_eq!(callback_count, 1);

    pool.close().await;
    let _ = std::fs::remove_file(&database_path);
    let _ = std::fs::remove_file(format!("{}-wal", database_path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", database_path.display()));
}

#[tokio::test]
async fn sqlite_payment_callback_binds_settlement_amount_before_usd_conversion() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    ensure_test_user(repository.pool(), "user-fee-callback").await;
    let order = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-fee-callback".to_string()),
            user_id: "user-fee-callback".to_string(),
            amount_usd: 10.0,
            pay_amount: Some(73.5),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.0),
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-fee-callback".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-fee-callback".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("recharge order should be created");
    assert!(matches!(
        order,
        CreateWalletRechargeOrderOutcome::Created(_)
    ));

    let mismatched = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-fee-mismatch".to_string(),
            order_no: Some("order-fee-callback".to_string()),
            gateway_order_id: Some("gateway-fee-callback".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(73.49),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.0),
            payload_hash: "payload-fee-mismatch".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("mismatched callback should resolve");
    assert!(matches!(
        mismatched,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "callback amount mismatch"
    ));

    let wrong_currency = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-fee-wrong-currency".to_string(),
            order_no: Some("order-fee-callback".to_string()),
            gateway_order_id: Some("gateway-fee-callback".to_string()),
            amount_usd: 10.0,
            pay_amount: Some(73.5),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(7.0),
            payload_hash: "payload-fee-wrong-currency".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("wrong-currency callback should resolve");
    assert!(matches!(
        wrong_currency,
        ProcessPaymentCallbackOutcome::Failed { ref error, .. }
            if error == "payment currency mismatch"
    ));

    let applied = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-fee-applied".to_string(),
            order_no: Some("order-fee-callback".to_string()),
            gateway_order_id: Some("gateway-fee-callback".to_string()),
            // Gateway callbacks derive USD from the fee-inclusive settlement
            // amount. The stored order's USD amount remains the net credit.
            amount_usd: 10.5,
            pay_amount: Some(73.5000005),
            pay_currency: Some("cny".to_string()),
            // A callback may carry a conflicting provider-side rate. The
            // order's checkout-time rate is the settlement proof and must
            // remain unchanged.
            exchange_rate: Some(99.0),
            payload_hash: "payload-fee-applied".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("fee-inclusive callback should process");
    assert!(matches!(
        applied,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));

    let wallet = repository
        .find(WalletLookupKey::UserId("user-fee-callback"))
        .await
        .expect("wallet should query")
        .expect("wallet should exist");
    assert_eq!(wallet.balance, 10.0);
    assert_eq!(wallet.total_recharged, 10.0);
    let persisted_terms: (Option<f64>, Option<String>, Option<f64>) = sqlx::query_as(
        "SELECT pay_amount, pay_currency, exchange_rate FROM payment_orders WHERE order_no = ?",
    )
    .bind("order-fee-callback")
    .fetch_one(repository.pool())
    .await
    .expect("settlement terms should query");
    assert_eq!(
        persisted_terms,
        (Some(73.5), Some("CNY".to_string()), Some(7.0))
    );

    repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some(wallet.id),
            user_id: "user-fee-callback".to_string(),
            amount_usd: 5.0,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            payment_method: "manual".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-usd-fallback".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-usd-fallback".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("fallback order should be created");
    let fallback = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "manual".to_string(),
            payment_provider: None,
            payment_channel: None,
            callback_key: "callback-usd-fallback".to_string(),
            order_no: Some("order-usd-fallback".to_string()),
            // The order already has a verified gateway id. A generic retry
            // may identify it by order number without repeating that id.
            gateway_order_id: None,
            amount_usd: 5.0,
            pay_amount: None,
            pay_currency: None,
            exchange_rate: None,
            payload_hash: "payload-usd-fallback".to_string(),
            payload: json!({ "status": "paid" }),
            signature_valid: true,
        })
        .await
        .expect("USD fallback callback should process");
    assert!(matches!(
        fallback,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));
    let persisted_gateway: (Option<String>, String) = sqlx::query_as(
        "SELECT gateway_order_id, gateway_response FROM payment_orders WHERE order_no = ?",
    )
    .bind("order-usd-fallback")
    .fetch_one(repository.pool())
    .await
    .expect("fallback order should remain queryable");
    assert_eq!(persisted_gateway.0.as_deref(), Some("gateway-usd-fallback"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted_gateway.1)
            .expect("gateway response should be JSON")
            .get("gateway_order_id"),
        Some(&json!("gateway-usd-fallback"))
    );
}

#[tokio::test]
async fn sqlite_plan_purchase_rejects_missing_user_without_creating_wallet_or_order() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    let input = CreatePlanPurchaseOrderInput {
        preferred_wallet_id: Some("missing-user-wallet".to_string()),
        user_id: "missing-plan-user".to_string(),
        amount_usd: 1.0,
        pay_amount: 1.0,
        pay_currency: "USD".to_string(),
        exchange_rate: 1.0,
        payment_method: "stripe".to_string(),
        payment_provider: Some("stripe".to_string()),
        payment_channel: Some("card".to_string()),
        gateway_order_id: "missing-user-gateway".to_string(),
        gateway_response: json!({"checkout": true}),
        order_no: "missing-user-order".to_string(),
        product_id: "missing-user-plan".to_string(),
        product_snapshot: json!({
            "id": "missing-user-plan",
            "duration_unit": "month",
            "duration_value": 1,
            "purchase_limit_scope": "unlimited",
            "entitlements": []
        }),
        expires_at_unix_secs: 4_102_444_800,
    };
    let error = repository
        .create_plan_purchase_order(input)
        .await
        .expect_err("missing user must be rejected");
    assert!(matches!(error, DataLayerError::InvalidInput(message) if message == "user not found"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM wallets WHERE id = 'missing-user-wallet'",
        )
        .fetch_one(&pool)
        .await
        .expect("wallet count should query"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM payment_orders WHERE order_no = 'missing-user-order'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment order count should query"),
        0
    );
}

#[tokio::test]
async fn sqlite_plan_purchase_rejects_preferred_wallet_id_owned_by_another_user() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_users(&pool, &["plan-wallet-owner", "plan-wallet-conflict"]).await;
    let existing_wallet = repository
        .initialize_auth_user_wallet("plan-wallet-owner", 0.0, false)
        .await
        .expect("owner wallet initialization should run")
        .expect("owner wallet should exist");

    let result = repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: Some(existing_wallet.id.clone()),
            user_id: "plan-wallet-conflict".to_string(),
            amount_usd: 1.0,
            pay_amount: 1.0,
            pay_currency: "USD".to_string(),
            exchange_rate: 1.0,
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            gateway_order_id: "gateway-plan-wallet-conflict".to_string(),
            gateway_response: json!({"checkout": true}),
            order_no: "order-plan-wallet-conflict".to_string(),
            product_id: "plan-wallet-conflict-product".to_string(),
            product_snapshot: json!({
                "id": "plan-wallet-conflict-product",
                "duration_unit": "month",
                "duration_value": 1,
                "purchase_limit_scope": "unlimited",
                "entitlements": []
            }),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await;

    assert!(matches!(
        result,
        Err(DataLayerError::InvalidInput(message))
            if message == "wallet identifier already belongs to another owner"
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM wallets WHERE user_id = 'plan-wallet-conflict'",
        )
        .fetch_one(&pool)
        .await
        .expect("conflicting user wallet count should query"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM payment_orders WHERE order_no = 'order-plan-wallet-conflict'",
        )
        .fetch_one(&pool)
        .await
        .expect("conflicting plan order count should query"),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_plan_purchase_initializes_one_wallet_under_concurrency() {
    let database_path = std::env::temp_dir().join(format!(
        "aether-sqlite-plan-wallet-race-{}.db",
        uuid::Uuid::new_v4()
    ));
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&database_path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(30));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());
    ensure_test_user(&pool, "plan-wallet-race-user").await;

    let plan_snapshot = json!({
        "id": "plan-wallet-race-product",
        "duration_unit": "month",
        "duration_value": 1,
        "purchase_limit_scope": "unlimited",
        "entitlements": []
    });
    let first_input = CreatePlanPurchaseOrderInput {
        preferred_wallet_id: Some("plan-wallet-race-first".to_string()),
        user_id: "plan-wallet-race-user".to_string(),
        amount_usd: 1.0,
        pay_amount: 1.0,
        pay_currency: "USD".to_string(),
        exchange_rate: 1.0,
        payment_method: "stripe".to_string(),
        payment_provider: Some("stripe".to_string()),
        payment_channel: Some("card".to_string()),
        gateway_order_id: "gateway-plan-wallet-race-first".to_string(),
        gateway_response: json!({"checkout": true}),
        order_no: "order-plan-wallet-race-first".to_string(),
        product_id: "plan-wallet-race-product".to_string(),
        product_snapshot: plan_snapshot.clone(),
        expires_at_unix_secs: 4_102_444_800,
    };
    let second_input = CreatePlanPurchaseOrderInput {
        preferred_wallet_id: Some("plan-wallet-race-second".to_string()),
        gateway_order_id: "gateway-plan-wallet-race-second".to_string(),
        order_no: "order-plan-wallet-race-second".to_string(),
        ..first_input.clone()
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_repository = repository.clone();
    let first_barrier = barrier.clone();
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_repository
            .create_plan_purchase_order(first_input)
            .await
    });
    let second_repository = repository.clone();
    let second_barrier = barrier.clone();
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_repository
            .create_plan_purchase_order(second_input)
            .await
    });
    barrier.wait().await;

    let first = first
        .await
        .expect("first plan task should join")
        .expect("first plan task should resolve");
    let second = second
        .await
        .expect("second plan task should join")
        .expect("second plan task should resolve");
    let first_wallet_id = match first {
        CreatePlanPurchaseOrderOutcome::Created(order) => order.wallet_id,
        other => panic!("first concurrent plan should be created, got {other:?}"),
    };
    let second_wallet_id = match second {
        CreatePlanPurchaseOrderOutcome::Created(order) => order.wallet_id,
        other => panic!("second concurrent plan should be created, got {other:?}"),
    };
    assert_eq!(first_wallet_id, second_wallet_id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM wallets WHERE user_id = 'plan-wallet-race-user'",
        )
        .fetch_one(&pool)
        .await
        .expect("wallet count should query"),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM payment_orders WHERE user_id = 'plan-wallet-race-user' AND order_kind = 'plan_purchase'",
        )
        .fetch_one(&pool)
        .await
        .expect("plan order count should query"),
        2
    );

    pool.close().await;
    let _ = std::fs::remove_file(database_path);
}

#[tokio::test]
async fn sqlite_wallet_recharge_rejects_missing_user_without_creating_wallet_or_order() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool.clone());

    let error = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("missing-recharge-wallet".to_string()),
            user_id: "missing-recharge-user".to_string(),
            amount_usd: 2.0,
            pay_amount: Some(2.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            gateway_order_id: "missing-recharge-gateway".to_string(),
            gateway_response: json!({"checkout": true}),
            order_no: "missing-recharge-order".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect_err("missing user must be rejected");
    assert!(matches!(
        error,
        DataLayerError::InvalidInput(message) if message == "user not found"
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM wallets WHERE id = 'missing-recharge-wallet'",
        )
        .fetch_one(&pool)
        .await
        .expect("wallet count should query"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM payment_orders WHERE order_no = 'missing-recharge-order'",
        )
        .fetch_one(&pool)
        .await
        .expect("payment order count should query"),
        0
    );
}

#[tokio::test]
async fn sqlite_plan_purchase_blocks_duplicate_pending_active_period_order_and_manual_credit_fulfills(
) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    sqlx::query(
            "INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("user-active-period-1")
        .bind("Active Period Buyer")
        .bind("active-period@example.com")
        .bind("local")
        .bind(1_i64)
        .bind(1_i64)
        .execute(repository.pool())
        .await
        .expect("user should seed");

    let _wallet_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-active-period-1".to_string()),
            user_id: "user-active-period-1".to_string(),
            amount_usd: 1.0,
            pay_amount: Some(1.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "bootstrap".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-bootstrap-active-period-1".to_string(),
            gateway_response: json!({ "bootstrap": true }),
            order_no: "order-bootstrap-active-period-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("wallet should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new bootstrap order should not already exist")
        }
    };

    let plan_snapshot = json!({
        "id": "active-period-plan",
        "title": "每日额度月卡",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "active_period",
        "entitlements": [
            {
                "type": "daily_quota",
                "daily_quota_usd": 50.0,
                "reset_timezone": "Asia/Shanghai",
                "allow_wallet_overage": false
            }
        ]
    });
    sqlx::query(
        r#"
INSERT INTO billing_plans (
  id, title, price_amount, price_currency, duration_unit, duration_value,
  max_active_per_user, purchase_limit_scope, entitlements_json, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
    )
    .bind("active-period-plan")
    .bind("每日额度月卡")
    .bind(100.0_f64)
    .bind("CNY")
    .bind("month")
    .bind(1_i64)
    .bind(1_i64)
    .bind("active_period")
    .bind(plan_snapshot["entitlements"].to_string())
    .bind(1_i64)
    .bind(1_i64)
    .execute(repository.pool())
    .await
    .expect("billing plan should seed");

    let first_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-active-period-1".to_string(),
            amount_usd: 13.8,
            pay_amount: 100.0,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.24637681,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-plan-active-period-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-plan-active-period-1".to_string(),
            product_id: "active-period-plan".to_string(),
            product_snapshot: plan_snapshot.clone(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("first active period order should create")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("first active period order should be created, got {other:?}"),
    };
    let duplicate_pending = repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-active-period-1".to_string(),
            amount_usd: 13.8,
            pay_amount: 100.0,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.24637681,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-plan-active-period-2".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-plan-active-period-2".to_string(),
            product_id: "active-period-plan".to_string(),
            product_snapshot: plan_snapshot.clone(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("duplicate active period order should resolve");
    assert!(matches!(
        duplicate_pending,
        CreatePlanPurchaseOrderOutcome::ActivePlanLimitReached
    ));

    let credited = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: first_order.id.clone(),
            gateway_order_id: Some("gateway-plan-active-period-paid-1".to_string()),
            pay_amount: Some(100.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.24637681),
            gateway_response_patch: Some(json!({ "settled": true })),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("manual plan credit should run");
    let WalletMutationOutcome::Applied((credited_order, applied)) = credited else {
        panic!("manual plan credit should be applied");
    };
    assert!(applied);
    assert_eq!(credited_order.status, "credited");
    assert_eq!(credited_order.refundable_amount_usd, 0.0);

    let entitlement_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_plan_entitlements WHERE user_id = ? AND plan_id = ?",
    )
    .bind("user-active-period-1")
    .bind("active-period-plan")
    .fetch_one(repository.pool())
    .await
    .expect("entitlement count should query");
    assert_eq!(entitlement_count, 1);

    let wallet_balance: f64 = sqlx::query_scalar("SELECT balance FROM wallets WHERE id = ?")
        .bind("wallet-active-period-1")
        .fetch_one(repository.pool())
        .await
        .expect("wallet balance should query");
    assert_eq!(wallet_balance, 0.0);

    // A malformed wallet_credit must abort fulfillment rather than silently
    // activating the plan without delivering its promised balance.
    let malformed_snapshot = json!({
        "id": "active-period-plan",
        "duration_unit": "month",
        "duration_value": 1,
        "purchase_limit_scope": "unlimited",
        "entitlements": [{
            "type": "wallet_credit",
            "amount_usd": 5.0,
            "balance_bucket": "not-a-wallet-bucket"
        }]
    });
    let valid_legacy_snapshot = json!({
        "id": "active-period-plan",
        "duration_unit": "month",
        "duration_value": 1,
        "purchase_limit_scope": "unlimited",
        "entitlements": [{
            "type": "wallet_credit",
            "amount_usd": 5.0,
            "balance_bucket": "gift"
        }]
    });
    let malformed_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-active-period-1".to_string(),
            amount_usd: 2.0,
            pay_amount: 2.0,
            pay_currency: "USD".to_string(),
            exchange_rate: 1.0,
            payment_method: "stripe".to_string(),
            payment_provider: Some("stripe".to_string()),
            payment_channel: Some("card".to_string()),
            gateway_order_id: "gateway-malformed-wallet-credit".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-malformed-wallet-credit".to_string(),
            product_id: "active-period-plan".to_string(),
            // Create through the validated boundary first; the malformed
            // snapshot is installed below to simulate a legacy/corrupt row
            // that predates the boundary validator.
            product_snapshot: valid_legacy_snapshot,
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("malformed plan order should be persisted for fulfillment test")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("malformed plan order should be created, got {other:?}"),
    };
    sqlx::query("UPDATE payment_orders SET product_snapshot = ? WHERE id = ?")
        .bind(malformed_snapshot.to_string())
        .bind(&malformed_order.id)
        .execute(repository.pool())
        .await
        .expect("malformed legacy snapshot should be installed");
    let credit_result = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: malformed_order.id.clone(),
            gateway_order_id: Some("gateway-malformed-wallet-credit-paid".to_string()),
            pay_amount: Some(2.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            gateway_response_patch: Some(json!({ "settled": true })),
            operator_id: Some("admin-1".to_string()),
        })
        .await;
    assert!(matches!(
        credit_result,
        Err(DataLayerError::InvalidInput(_))
    ));
    let malformed_status: String =
        sqlx::query_scalar("SELECT status FROM payment_orders WHERE id = ?")
            .bind(&malformed_order.id)
            .fetch_one(repository.pool())
            .await
            .expect("malformed order status should remain queryable");
    assert_eq!(malformed_status, "pending");
    let malformed_entitlements: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_plan_entitlements WHERE payment_order_id = ?",
    )
    .bind(&malformed_order.id)
    .fetch_one(repository.pool())
    .await
    .expect("malformed entitlement count should query");
    assert_eq!(malformed_entitlements, 0);
    let wallet_balance_after: f64 = sqlx::query_scalar("SELECT balance FROM wallets WHERE id = ?")
        .bind("wallet-active-period-1")
        .fetch_one(repository.pool())
        .await
        .expect("wallet balance after rejected credit should query");
    assert_eq!(wallet_balance_after, 0.0);
}

#[tokio::test]
async fn sqlite_finds_reusable_pending_plan_purchase_order() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    sqlx::query(
            "INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("user-pending-plan-1")
        .bind("Pending Buyer")
        .bind("pending-plan@example.com")
        .bind("local")
        .bind(1_i64)
        .bind(1_i64)
        .execute(repository.pool())
        .await
        .expect("user should seed");

    let _wallet_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-pending-plan-1".to_string()),
            user_id: "user-pending-plan-1".to_string(),
            amount_usd: 1.0,
            pay_amount: Some(1.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "bootstrap".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-bootstrap-pending-plan-1".to_string(),
            gateway_response: json!({ "bootstrap": true }),
            order_no: "order-bootstrap-pending-plan-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("wallet should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new pending-plan order should not already exist")
        }
    };

    let plan_snapshot = json!({
        "id": "pending-plan",
        "title": "每日额度月卡",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "active_period",
        "entitlements": [
            {
                "type": "daily_quota",
                "daily_quota_usd": 50.0,
                "reset_timezone": "Asia/Shanghai",
                "allow_wallet_overage": false
            }
        ]
    });
    let pending_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-pending-plan-1".to_string(),
            amount_usd: 13.8,
            pay_amount: 100.0,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.24637681,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-pending-plan-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-pending-plan-1".to_string(),
            product_id: "pending-plan".to_string(),
            product_snapshot: plan_snapshot.clone(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("pending plan order should create")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("pending plan order should be created, got {other:?}"),
    };
    let now = chrono::Utc::now().timestamp().max(0);
    for (id, order_no, status, product_id, user_id, expires_at, created_at) in [
        (
            "expired-pending-plan-order",
            "order-expired-pending-plan",
            "pending",
            "pending-plan",
            "user-pending-plan-1",
            now - 10,
            now + 10,
        ),
        (
            "credited-pending-plan-order",
            "order-credited-pending-plan",
            "credited",
            "pending-plan",
            "user-pending-plan-1",
            now + 3_600,
            now + 20,
        ),
        (
            "other-user-pending-plan-order",
            "order-other-user-pending-plan",
            "pending",
            "pending-plan",
            "other-user",
            now + 3_600,
            now + 30,
        ),
    ] {
        sqlx::query(
            r#"
INSERT INTO payment_orders (
  id, order_no, wallet_id, user_id, amount_usd, pay_amount, pay_currency,
  exchange_rate, refunded_amount_usd, refundable_amount_usd, payment_method,
  payment_provider, payment_channel, order_kind, product_id, product_snapshot,
  fulfillment_status, gateway_order_id, gateway_response, status, created_at, expires_at
) VALUES (?, ?, ?, ?, 13.8, 100.0, 'CNY', 7.24637681, 0, 0, 'alipay',
  'epay', 'alipay', 'plan_purchase', ?, ?, 'pending', ?, ?, ?, ?, ?)
                "#,
        )
        .bind(id)
        .bind(order_no)
        .bind("wallet-pending-plan-1")
        .bind(user_id)
        .bind(product_id)
        .bind(plan_snapshot.to_string())
        .bind(format!("gateway-{id}"))
        .bind(json!({ "checkout": id }).to_string())
        .bind(status)
        .bind(created_at)
        .bind(expires_at)
        .execute(repository.pool())
        .await
        .expect("extra payment order should seed");
    }

    let found = repository
        .find_pending_plan_purchase_order_by_user_id("user-pending-plan-1", "pending-plan")
        .await
        .expect("pending plan lookup should run")
        .expect("pending plan order should be found");
    assert_eq!(found.id, pending_order.id);
    assert_eq!(
        repository
            .find_pending_plan_purchase_order_by_user_id("user-pending-plan-1", "missing-plan")
            .await
            .expect("missing plan lookup should run"),
        None
    );
    assert_eq!(
        repository
            .find_pending_plan_purchase_order_by_user_id("missing-user", "pending-plan")
            .await
            .expect("missing user lookup should run"),
        None
    );
}

#[tokio::test]
async fn sqlite_plan_purchase_replaces_same_class_entitlements_on_manual_credit() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    sqlx::query(
            "INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("user-upgrade-1")
        .bind("Upgrade Buyer")
        .bind("upgrade@example.com")
        .bind("local")
        .bind(1_i64)
        .bind(1_i64)
        .execute(repository.pool())
        .await
        .expect("user should seed");

    let _wallet_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-upgrade-1".to_string()),
            user_id: "user-upgrade-1".to_string(),
            amount_usd: 1.0,
            pay_amount: Some(1.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "bootstrap".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-bootstrap-upgrade-1".to_string(),
            gateway_response: json!({ "bootstrap": true }),
            order_no: "order-bootstrap-upgrade-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("wallet should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new upgrade order should not already exist")
        }
    };

    let low_snapshot = json!({
        "id": "pro-basic",
        "title": "Pro Basic",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "active_period",
        "entitlements": [{"type": "daily_quota", "daily_quota_usd": 10.0}]
    });
    let high_snapshot = json!({
        "id": "pro-plus",
        "title": "Pro Plus",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "active_period",
        "entitlements": [{"type": "daily_quota", "daily_quota_usd": 50.0}]
    });
    for (id, title, snapshot) in [
        ("pro-basic", "Pro Basic", &low_snapshot),
        ("pro-plus", "Pro Plus", &high_snapshot),
    ] {
        sqlx::query(
            r#"
INSERT INTO billing_plans (
  id, title, price_amount, price_currency, duration_unit, duration_value,
  max_active_per_user, purchase_limit_scope, entitlements_json, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
        )
        .bind(id)
        .bind(title)
        .bind(100.0_f64)
        .bind("CNY")
        .bind("month")
        .bind(1_i64)
        .bind(1_i64)
        .bind("active_period")
        .bind(snapshot["entitlements"].to_string())
        .bind(1_i64)
        .bind(1_i64)
        .execute(repository.pool())
        .await
        .expect("billing plan should seed");
    }

    let low_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-upgrade-1".to_string(),
            amount_usd: 13.8,
            pay_amount: 100.0,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.24637681,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-pro-basic-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-pro-basic-1".to_string(),
            product_id: "pro-basic".to_string(),
            product_snapshot: low_snapshot,
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("low order should create")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("low order should be created, got {other:?}"),
    };
    let WalletMutationOutcome::Applied((_, true)) = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: low_order.id,
            gateway_order_id: Some("gateway-pro-basic-paid-1".to_string()),
            pay_amount: Some(100.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.24637681),
            gateway_response_patch: Some(json!({ "settled": true })),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("low plan credit should run")
    else {
        panic!("low plan credit should apply");
    };

    let high_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-upgrade-1".to_string(),
            amount_usd: 13.8,
            pay_amount: 100.0,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.24637681,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-pro-plus-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-pro-plus-1".to_string(),
            product_id: "pro-plus".to_string(),
            product_snapshot: high_snapshot,
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("high order should create")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("high order should be created, got {other:?}"),
    };
    let WalletMutationOutcome::Applied((_, true)) = repository
        .credit_admin_payment_order(CreditAdminPaymentOrderInput {
            order_id: high_order.id,
            gateway_order_id: Some("gateway-pro-plus-paid-1".to_string()),
            pay_amount: Some(100.0),
            pay_currency: Some("CNY".to_string()),
            exchange_rate: Some(7.24637681),
            gateway_response_patch: Some(json!({ "settled": true })),
            operator_id: Some("admin-1".to_string()),
        })
        .await
        .expect("high plan credit should run")
    else {
        panic!("high plan credit should apply");
    };

    let low_status: String = sqlx::query_scalar(
        "SELECT status FROM user_plan_entitlements WHERE user_id = ? AND plan_id = ?",
    )
    .bind("user-upgrade-1")
    .bind("pro-basic")
    .fetch_one(repository.pool())
    .await
    .expect("low entitlement status should query");
    assert_eq!(low_status, "replaced");
    let active_high_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_plan_entitlements WHERE user_id = ? AND plan_id = ? AND status = 'active'",
        )
        .bind("user-upgrade-1")
        .bind("pro-plus")
        .fetch_one(repository.pool())
        .await
        .expect("high entitlement count should query");
    assert_eq!(active_high_count, 1);
}

#[tokio::test]
async fn sqlite_plan_replacement_stacks_usage_policies_unless_groups_match() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(repository.pool())
        .await
        .expect("foreign keys should be disabled for isolated replacement fixtures");

    let fixtures = [
        (
            "usage-plain",
            json!([{"type": "usage_policy", "policy_id": "weekly", "rules": []}]),
        ),
        (
            "usage-pro",
            json!([{
                "type": "usage_policy",
                "replacement_group": "pro-tier",
                "rules": []
            }]),
        ),
        (
            "usage-team",
            json!([{
                "type": "usage_policy",
                "replacement_group": "team-tier",
                "rules": []
            }]),
        ),
        (
            "daily-legacy",
            json!([{"type": "daily_quota", "daily_quota_usd": 10.0}]),
        ),
    ];
    for (id, entitlements) in fixtures {
        sqlx::query(
            r#"
INSERT INTO user_plan_entitlements (
  id, user_id, plan_id, payment_order_id, status, starts_at, expires_at,
  entitlements_snapshot, created_at, updated_at
) VALUES (?, 'replacement-user', ?, ?, 'active', 1, 4102444800, ?, 1, 1)
            "#,
        )
        .bind(id)
        .bind(format!("plan-{id}"))
        .bind(format!("order-{id}"))
        .bind(entitlements.to_string())
        .execute(repository.pool())
        .await
        .expect("entitlement fixture should seed");
    }

    let mut tx = repository.pool().begin().await.expect("tx should start");
    replace_matching_plan_entitlements_sqlite(
        &mut tx,
        "replacement-user",
        &json!({
            "entitlements": [{"type": "usage_policy", "policy_id": "five-hour", "rules": []}]
        }),
        100,
    )
    .await
    .expect("ungrouped usage policy replacement should run");
    tx.commit().await.expect("tx should commit");
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_plan_entitlements WHERE user_id = ? AND status = 'active'",
    )
    .bind("replacement-user")
    .fetch_one(repository.pool())
    .await
    .expect("active entitlement count should query");
    assert_eq!(active_count, 4);

    let mut tx = repository.pool().begin().await.expect("tx should start");
    replace_matching_plan_entitlements_sqlite(
        &mut tx,
        "replacement-user",
        &json!({
            "entitlements": [{
                "type": "usage_policy",
                "replacement_group": "pro-tier",
                "rules": []
            }]
        }),
        200,
    )
    .await
    .expect("grouped usage policy replacement should run");
    tx.commit().await.expect("tx should commit");

    let statuses = sqlx::query_as::<_, (String, String)>(
        "SELECT id, status FROM user_plan_entitlements ORDER BY id",
    )
    .fetch_all(repository.pool())
    .await
    .expect("entitlement statuses should query")
    .into_iter()
    .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        statuses.get("usage-pro").map(String::as_str),
        Some("replaced")
    );
    assert_eq!(
        statuses.get("usage-plain").map(String::as_str),
        Some("active")
    );
    assert_eq!(
        statuses.get("usage-team").map(String::as_str),
        Some("active")
    );
    assert_eq!(
        statuses.get("daily-legacy").map(String::as_str),
        Some("active")
    );

    let mut tx = repository.pool().begin().await.expect("tx should start");
    replace_matching_plan_entitlements_sqlite(
        &mut tx,
        "replacement-user",
        &json!({
            "entitlements": [{"type": "daily_quota", "daily_quota_usd": 50.0}]
        }),
        300,
    )
    .await
    .expect("legacy daily quota replacement should run");
    tx.commit().await.expect("tx should commit");
    let daily_status: String =
        sqlx::query_scalar("SELECT status FROM user_plan_entitlements WHERE id = 'daily-legacy'")
            .fetch_one(repository.pool())
            .await
            .expect("daily entitlement status should query");
    assert_eq!(daily_status, "replaced");
}

#[tokio::test]
async fn sqlite_plan_purchase_respects_lifetime_purchase_limit() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");

    let repository = SqliteWalletReadRepository::new(pool);
    sqlx::query(
            "INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("user-lifetime-1")
        .bind("Lifetime Buyer")
        .bind("lifetime@example.com")
        .bind("local")
        .bind(1_i64)
        .bind(1_i64)
        .execute(repository.pool())
        .await
        .expect("user should seed");

    let _wallet_order = match repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-lifetime-1".to_string()),
            user_id: "user-lifetime-1".to_string(),
            amount_usd: 1.0,
            pay_amount: Some(1.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "bootstrap".to_string(),
            payment_provider: None,
            payment_channel: None,
            gateway_order_id: "gateway-bootstrap-lifetime-1".to_string(),
            gateway_response: json!({ "bootstrap": true }),
            order_no: "order-bootstrap-lifetime-1".to_string(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("wallet should be created")
    {
        CreateWalletRechargeOrderOutcome::Created(order) => order,
        CreateWalletRechargeOrderOutcome::WalletInactive => {
            panic!("new wallet should be active")
        }
        CreateWalletRechargeOrderOutcome::Existing(_) => {
            panic!("new lifetime order should not already exist")
        }
    };

    let plan_snapshot = json!({
        "id": "first-plan",
        "title": "首购特惠包",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "lifetime",
        "entitlements": [
            {
                "type": "wallet_credit",
                "amount_usd": 1.0,
                "balance_bucket": "gift"
            }
        ]
    });
    sqlx::query(
        r#"
INSERT INTO billing_plans (
  id, title, price_amount, price_currency, duration_unit, duration_value,
  max_active_per_user, purchase_limit_scope, entitlements_json, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
    )
    .bind("first-plan")
    .bind("首购特惠包")
    .bind(7.2_f64)
    .bind("CNY")
    .bind("month")
    .bind(1_i64)
    .bind(1_i64)
    .bind("lifetime")
    .bind(plan_snapshot["entitlements"].to_string())
    .bind(1_i64)
    .bind(1_i64)
    .execute(repository.pool())
    .await
    .expect("billing plan should seed");

    let first_order = match repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-lifetime-1".to_string(),
            amount_usd: 1.0,
            pay_amount: 7.2,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.2,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-plan-lifetime-1".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-plan-lifetime-1".to_string(),
            product_id: "first-plan".to_string(),
            product_snapshot: plan_snapshot.clone(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("first plan order should create")
    {
        CreatePlanPurchaseOrderOutcome::Created(order) => order,
        other => panic!("first plan order should be created, got {other:?}"),
    };
    assert_eq!(first_order.status, "pending");

    let callback = repository
        .process_payment_callback(ProcessPaymentCallbackInput {
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            callback_key: "callback-plan-lifetime-1".to_string(),
            order_no: Some("order-plan-lifetime-1".to_string()),
            gateway_order_id: Some("gateway-plan-lifetime-1".to_string()),
            amount_usd: 1.0,
            pay_amount: Some(7.2000005),
            pay_currency: Some("cny".to_string()),
            exchange_rate: Some(99.0),
            payload_hash: "payload-plan-lifetime-1".to_string(),
            payload: json!({ "trade_status": "TRADE_SUCCESS" }),
            signature_valid: true,
        })
        .await
        .expect("plan payment callback should process");
    assert!(matches!(
        callback,
        ProcessPaymentCallbackOutcome::Applied { .. }
    ));
    let persisted_plan_terms: (f64, String, f64) = sqlx::query_as(
        "SELECT pay_amount, pay_currency, exchange_rate FROM payment_orders WHERE order_no = ?",
    )
    .bind("order-plan-lifetime-1")
    .fetch_one(repository.pool())
    .await
    .expect("plan settlement terms should query");
    assert_eq!(
        persisted_plan_terms,
        (7.2, "CNY".to_string(), 7.2),
        "callback data must not overwrite checkout-time settlement terms"
    );

    let entitlement_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_plan_entitlements WHERE user_id = ? AND plan_id = ?",
    )
    .bind("user-lifetime-1")
    .bind("first-plan")
    .fetch_one(repository.pool())
    .await
    .expect("entitlement count should query");
    assert_eq!(entitlement_count, 1);

    let second_order = repository
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: None,
            user_id: "user-lifetime-1".to_string(),
            amount_usd: 1.0,
            pay_amount: 7.2,
            pay_currency: "CNY".to_string(),
            exchange_rate: 7.2,
            payment_method: "alipay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "gateway-plan-lifetime-2".to_string(),
            gateway_response: json!({ "checkout": true }),
            order_no: "order-plan-lifetime-2".to_string(),
            product_id: "first-plan".to_string(),
            product_snapshot: plan_snapshot.clone(),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .expect("second plan order should resolve");
    assert!(matches!(
        second_order,
        CreatePlanPurchaseOrderOutcome::ActivePlanLimitReached
    ));

    let unlimited_snapshot = json!({
        "id": "unlimited-plan",
        "title": "不限购余额包",
        "duration_unit": "month",
        "duration_value": 1,
        "max_active_per_user": 1,
        "purchase_limit_scope": "unlimited",
        "entitlements": [
            {
                "type": "wallet_credit",
                "amount_usd": 1.0,
                "balance_bucket": "gift"
            }
        ]
    });
    sqlx::query(
        r#"
INSERT INTO billing_plans (
  id, title, price_amount, price_currency, duration_unit, duration_value,
  max_active_per_user, purchase_limit_scope, entitlements_json, created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#,
    )
    .bind("unlimited-plan")
    .bind("不限购余额包")
    .bind(7.2_f64)
    .bind("CNY")
    .bind("month")
    .bind(1_i64)
    .bind(1_i64)
    .bind("unlimited")
    .bind(unlimited_snapshot["entitlements"].to_string())
    .bind(1_i64)
    .bind(1_i64)
    .execute(repository.pool())
    .await
    .expect("unlimited billing plan should seed");

    for index in 1..=2 {
        let order_no = format!("order-plan-unlimited-{index}");
        let gateway_order_id = format!("gateway-plan-unlimited-{index}");
        let order = match repository
            .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
                preferred_wallet_id: None,
                user_id: "user-lifetime-1".to_string(),
                amount_usd: 1.0,
                pay_amount: 7.2,
                pay_currency: "CNY".to_string(),
                exchange_rate: 7.2,
                payment_method: "alipay".to_string(),
                payment_provider: Some("epay".to_string()),
                payment_channel: Some("alipay".to_string()),
                gateway_order_id: gateway_order_id.clone(),
                gateway_response: json!({ "checkout": true }),
                order_no: order_no.clone(),
                product_id: "unlimited-plan".to_string(),
                product_snapshot: unlimited_snapshot.clone(),
                expires_at_unix_secs: 4_102_444_800,
            })
            .await
            .expect("unlimited plan order should create")
        {
            CreatePlanPurchaseOrderOutcome::Created(order) => order,
            other => panic!("unlimited plan order should be created, got {other:?}"),
        };
        assert_eq!(order.status, "pending");

        let callback = repository
            .process_payment_callback(ProcessPaymentCallbackInput {
                payment_method: "alipay".to_string(),
                payment_provider: Some("epay".to_string()),
                payment_channel: Some("alipay".to_string()),
                callback_key: format!("callback-plan-unlimited-{index}"),
                order_no: Some(order_no),
                gateway_order_id: Some(gateway_order_id),
                amount_usd: 1.0,
                pay_amount: Some(7.2),
                pay_currency: Some("CNY".to_string()),
                exchange_rate: Some(7.2),
                payload_hash: format!("payload-plan-unlimited-{index}"),
                payload: json!({ "trade_status": "TRADE_SUCCESS" }),
                signature_valid: true,
            })
            .await
            .expect("unlimited plan payment callback should process");
        assert!(matches!(
            callback,
            ProcessPaymentCallbackOutcome::Applied { .. }
        ));
    }

    let unlimited_entitlement_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_plan_entitlements WHERE user_id = ? AND plan_id = ?",
    )
    .bind("user-lifetime-1")
    .bind("unlimited-plan")
    .fetch_one(repository.pool())
    .await
    .expect("unlimited entitlement count should query");
    assert_eq!(unlimited_entitlement_count, 2);
}

impl SqliteWalletReadRepository {
    fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }
}

#[tokio::test]
async fn sqlite_recharge_checkout_update_rejects_expired_order() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite pool should connect");
    run_migrations(&pool)
        .await
        .expect("sqlite migrations should run");
    let repository = SqliteWalletReadRepository::new(pool);

    sqlx::query(
        "INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("user-expired-checkout")
    .bind("Expired Checkout")
    .bind("expired-checkout@example.com")
    .bind("local")
    .bind(1_i64)
    .bind(1_i64)
    .execute(repository.pool())
    .await
    .expect("user should seed");

    let created = repository
        .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some("wallet-expired-checkout".to_string()),
            user_id: "user-expired-checkout".to_string(),
            amount_usd: 3.0,
            pay_amount: Some(3.0),
            pay_currency: Some("USD".to_string()),
            exchange_rate: Some(1.0),
            payment_method: "epay".to_string(),
            payment_provider: Some("epay".to_string()),
            payment_channel: Some("alipay".to_string()),
            gateway_order_id: "order-expired-checkout".to_string(),
            gateway_response: json!({
                "order_kind": "wallet_recharge",
                "integration_status": "checkout_pending"
            }),
            order_no: "order-expired-checkout".to_string(),
            expires_at_unix_secs: 1,
        })
        .await
        .expect("expired recharge order should be creatable for regression setup");
    let CreateWalletRechargeOrderOutcome::Created(order) = created else {
        panic!("expected a newly created recharge order");
    };

    let result = repository
        .update_wallet_recharge_checkout(UpdateWalletRechargeCheckoutInput {
            order_id: order.id.clone(),
            gateway_order_id: "provider-expired-checkout".to_string(),
            gateway_response: json!({
                "order_kind": "wallet_recharge",
                "payment_url": "https://pay.example.test/expired"
            }),
        })
        .await
        .expect("expired checkout update should resolve");
    assert!(matches!(result, WalletMutationOutcome::Invalid(_)));

    let persisted: (Option<String>, String) =
        sqlx::query_as("SELECT gateway_order_id, status FROM payment_orders WHERE id = ?")
            .bind(&order.id)
            .fetch_one(repository.pool())
            .await
            .expect("expired recharge order should remain queryable");
    assert_eq!(persisted.0.as_deref(), Some("order-expired-checkout"));
    assert_eq!(persisted.1, "pending");
}

async fn seed_rows(pool: &sqlx::SqlitePool) {
    sqlx::query(
        r#"
INSERT INTO users (id, username, email, auth_source, created_at, updated_at)
VALUES ('user-1', 'Alice', 'alice@example.com', 'local', 1, 1)
"#,
    )
    .execute(pool)
    .await
    .expect("user should seed");

    sqlx::query(
        r#"
INSERT INTO wallets (
  id, user_id, balance, gift_balance, total_recharged, total_consumed,
  total_refunded, total_adjusted, created_at, updated_at
) VALUES (
  'wallet-1', 'user-1', 10.0, 2.0, 20.0, 4.0, 1.0, 3.0, 1, 2
)
"#,
    )
    .execute(pool)
    .await
    .expect("wallet should seed");

    sqlx::query(
        r#"
INSERT INTO payment_orders (
  id, order_no, wallet_id, user_id, amount_usd, refunded_amount_usd,
  refundable_amount_usd, payment_method, gateway_response, status, created_at
) VALUES (
  'order-1', 'order-no-1', 'wallet-1', 'user-1', 5.0, 1.0, 4.0,
  'redeem_code', '{"ok":true}', 'credited', 3
)
"#,
    )
    .execute(pool)
    .await
    .expect("payment order should seed");

    sqlx::query(
        r#"
INSERT INTO payment_callbacks (
  id, payment_order_id, payment_method, callback_key, order_no,
  signature_valid, payload, created_at
) VALUES (
  'callback-1', 'order-1', 'redeem_code', 'callback-key-1',
  'order-no-1', 1, '{"event":"paid"}', 4
)
"#,
    )
    .execute(pool)
    .await
    .expect("callback should seed");

    sqlx::query(
        r#"
INSERT INTO refund_requests (
  id, refund_no, wallet_id, user_id, payment_order_id, source_type,
  refund_mode, amount_usd, status, payout_proof, created_at, updated_at
) VALUES (
  'refund-1', 'refund-no-1', 'wallet-1', 'user-1', 'order-1',
  'payment_order', 'offline_payout', 1.0, 'completed',
  '{"proof":"ok"}', 5, 6
)
"#,
    )
    .execute(pool)
    .await
    .expect("refund should seed");

    sqlx::query(
        r#"
INSERT INTO wallet_transactions (
  id, wallet_id, category, reason_code, amount, balance_before,
  balance_after, recharge_balance_before, recharge_balance_after,
  gift_balance_before, gift_balance_after, created_at
) VALUES (
  'tx-1', 'wallet-1', 'credit', 'manual_adjustment', 3.0, 7.0, 10.0,
  5.0, 8.0, 2.0, 2.0, 7
)
"#,
    )
    .execute(pool)
    .await
    .expect("transaction should seed");

    sqlx::query(
        r#"
INSERT INTO redeem_code_batches (
  id, name, amount_usd, total_count, created_at, updated_at
) VALUES (
  'batch-1', 'Batch One', 5.0, 1, 8, 9
)
"#,
    )
    .execute(pool)
    .await
    .expect("redeem batch should seed");

    sqlx::query(
        r#"
INSERT INTO redeem_codes (
  id, batch_id, code_hash, code_prefix, code_suffix, status,
  redeemed_by_user_id, redeemed_wallet_id, redeemed_payment_order_id,
  redeemed_at, created_at, updated_at
) VALUES (
  'code-1', 'batch-1', 'hash-1', 'ABCD', 'WXYZ', 'redeemed',
  'user-1', 'wallet-1', 'order-1', 10, 8, 10
)
"#,
    )
    .execute(pool)
    .await
    .expect("redeem code should seed");

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
