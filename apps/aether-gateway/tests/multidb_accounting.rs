//! Accounting contracts shared by all supported SQL drivers. The ignored cases
//! require disposable, migrated-on-demand databases via AETHER_TEST_*_URL.

use std::sync::Arc;

use aether_billing::{
    enrich_usage_event_with_billing, BillingModelContextLookup, BillingModelPricingSnapshot,
};
use aether_data::backend::DataBackends;
use aether_data::lifecycle::migrate::{
    run_migrations, run_mysql_migrations, run_sqlite_migrations,
};
use aether_data::{DataLayerConfig, DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
use aether_data_contracts::repository::billing::{
    AdminBillingMutationOutcome, BillingPlanWriteInput, StoredBillingModelContext,
};
use aether_data_contracts::repository::settlement::{
    SettlementWriteRepository, StoredUsageSettlement, UsageSettlementInput,
};
use aether_data_contracts::repository::wallet::{
    CreateAdminRedeemCodeBatchInput, CreateManualWalletRechargeInput, CreatePlanPurchaseOrderInput,
    CreatePlanPurchaseOrderOutcome, CreateWalletRechargeOrderInput,
    CreateWalletRechargeOrderOutcome, CreditAdminPaymentOrderInput, RedeemWalletCodeInput,
    RedeemWalletCodeOutcome, WalletLookupKey, WalletMutationOutcome,
};
use aether_data_contracts::DataLayerError;
use aether_usage_runtime::{
    build_upsert_usage_record_from_event, settle_usage_if_needed, UsageEvent, UsageEventData,
    UsageEventType, UsageSettlementWriter,
};
use async_trait::async_trait;
use serde_json::json;

async fn accounting_fixture(driver: DatabaseDriver, url: String) -> (DataBackends, String) {
    let data = DataBackends::from_config(DataLayerConfig::from_database(SqlDatabaseConfig {
        driver,
        url,
        pool: SqlPoolConfig {
            min_connections: 1,
            max_connections: 1,
            ..SqlPoolConfig::default()
        },
    }))
    .unwrap();
    let user_id = uuid::Uuid::new_v4().to_string();
    let username = format!("accounting-{user_id}");
    let email = format!("{user_id}@accounting.example");
    match driver {
        DatabaseDriver::Postgres => {
            let pool = data.postgres().unwrap().pool();
            run_migrations(pool).await.unwrap();
            sqlx::query("INSERT INTO users (id, username, email, email_verified, auth_source, created_at, updated_at) VALUES ($1, $2, $3, false, 'local', NOW(), NOW())")
                .bind(&user_id).bind(&username).bind(&email).execute(pool).await.unwrap();
        }
        DatabaseDriver::Mysql => {
            let pool = data.mysql().unwrap().pool();
            run_mysql_migrations(pool).await.unwrap();
            sqlx::query("INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, 'local', 1, 1)")
                .bind(&user_id).bind(&username).bind(&email).execute(pool).await.unwrap();
        }
        DatabaseDriver::Sqlite => {
            let pool = data.sqlite().unwrap().pool();
            run_sqlite_migrations(pool).await.unwrap();
            sqlx::query("INSERT INTO users (id, username, email, auth_source, created_at, updated_at) VALUES (?, ?, ?, 'local', 1, 1)")
                .bind(&user_id).bind(&username).bind(&email).execute(pool).await.unwrap();
        }
    }
    data.read()
        .wallets()
        .unwrap()
        .initialize_auth_user_wallet(&user_id, 3.0, false)
        .await
        .unwrap()
        .unwrap();
    (data, user_id)
}

async fn payment_order_lifecycle(data: &DataBackends, user_id: &str) {
    let reader = data.read().wallets().unwrap();
    let writer = data.write().wallets().unwrap();
    let wallet = reader
        .find(WalletLookupKey::UserId(user_id))
        .await
        .unwrap()
        .unwrap();
    let manual = CreateManualWalletRechargeInput {
        wallet_id: wallet.id.clone(),
        amount_usd: 5.0,
        payment_method: "admin_manual".into(),
        operator_id: None,
        description: Some("multi-database recharge regression".into()),
        order_no: uuid::Uuid::new_v4().to_string(),
    };
    let (recharged, order) = writer
        .create_manual_wallet_recharge(manual.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((recharged.balance, recharged.gift_balance), (5.0, 3.0));
    assert_eq!(order.payment_provider, None);
    assert_eq!(order.order_kind, "wallet_recharge");
    assert_eq!(order.status, "credited");
    assert_eq!(
        reader.find_admin_payment_order(&order.id).await.unwrap(),
        Some(order)
    );
    let ledger_count = reader
        .list_admin_wallet_transactions(&wallet.id, 100, 0)
        .await
        .unwrap()
        .total;
    assert!(writer
        .create_manual_wallet_recharge(manual.clone())
        .await
        .is_err());
    for amount_usd in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(writer
            .create_manual_wallet_recharge(CreateManualWalletRechargeInput {
                amount_usd,
                order_no: uuid::Uuid::new_v4().to_string(),
                ..manual.clone()
            })
            .await
            .is_err());
    }
    assert_eq!(
        reader
            .find(WalletLookupKey::WalletId(&wallet.id))
            .await
            .unwrap(),
        Some(recharged)
    );
    assert_eq!(
        reader
            .list_admin_wallet_transactions(&wallet.id, 100, 0)
            .await
            .unwrap()
            .total,
        ledger_count
    );

    let case_sensitive_id = format!("Pi_{}", uuid::Uuid::new_v4());
    for (status, gateway_id) in [
        ("expired", case_sensitive_id.clone()),
        ("failed", case_sensitive_id.to_lowercase()),
        ("credited", format!("pi_{}", uuid::Uuid::new_v4())),
    ] {
        let input = CreateWalletRechargeOrderInput {
            preferred_wallet_id: Some(wallet.id.clone()),
            user_id: user_id.into(),
            amount_usd: 5.0,
            pay_amount: Some(5.0),
            pay_currency: Some("USD".into()),
            exchange_rate: Some(1.0),
            payment_method: "stripe".into(),
            payment_provider: Some("stripe".into()),
            payment_channel: Some("card".into()),
            gateway_order_id: gateway_id.clone(),
            gateway_response: json!({"gateway": "stripe", "fixture": "metadata"}),
            order_no: uuid::Uuid::new_v4().to_string(),
            expires_at_unix_secs: 4_102_444_800,
        };
        let outcome = writer
            .create_wallet_recharge_order(input.clone())
            .await
            .unwrap();
        let CreateWalletRechargeOrderOutcome::Created(order) = outcome else {
            panic!("pending recharge should be created: {outcome:?}");
        };
        assert_eq!(order.gateway_order_id.as_deref(), Some(gateway_id.as_str()));
        assert!(
            writer
                .create_wallet_recharge_order(CreateWalletRechargeOrderInput {
                    order_no: uuid::Uuid::new_v4().to_string(),
                    ..input
                })
                .await
                .is_err(),
            "an exact gateway ID cannot identify another order"
        );
        let updated = match status {
            "expired" => {
                let WalletMutationOutcome::Applied((updated, true)) =
                    writer.expire_admin_payment_order(&order.id).await.unwrap()
                else {
                    panic!("pending order should expire");
                };
                assert!(matches!(
                    writer.expire_admin_payment_order(&order.id).await.unwrap(),
                    WalletMutationOutcome::Applied((_, false))
                ));
                updated
            }
            "failed" => {
                let WalletMutationOutcome::Applied(updated) =
                    writer.fail_admin_payment_order(&order.id).await.unwrap()
                else {
                    panic!("pending order should fail");
                };
                updated
            }
            _ => {
                let input = credit_input(&order.id);
                let WalletMutationOutcome::Applied((updated, true)) = writer
                    .credit_admin_payment_order(input.clone())
                    .await
                    .unwrap()
                else {
                    panic!("pending order should be credited");
                };
                assert!(matches!(
                    writer.credit_admin_payment_order(input).await.unwrap(),
                    WalletMutationOutcome::Applied((_, false))
                ));
                assert!(matches!(
                    writer.expire_admin_payment_order(&order.id).await.unwrap(),
                    WalletMutationOutcome::Invalid(_)
                ));
                assert!(matches!(
                    writer.fail_admin_payment_order(&order.id).await.unwrap(),
                    WalletMutationOutcome::Invalid(_)
                ));
                updated
            }
        };
        assert_eq!(updated.status, status);
        assert_eq!(updated.payment_provider.as_deref(), Some("stripe"));
        assert_eq!(updated.order_kind, "wallet_recharge");
        assert_eq!(
            reader.find_admin_payment_order(&order.id).await.unwrap(),
            Some(updated)
        );
    }
    let wallet = reader
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (wallet.balance, wallet.gift_balance, wallet.total_recharged),
        (10.0, 3.0, 10.0)
    );
    assert_eq!(
        reader
            .list_admin_wallet_transactions(&wallet.id, 100, 0)
            .await
            .unwrap()
            .total,
        ledger_count + 1
    );

    let entitlements =
        json!([{"type": "wallet_credit", "amount_usd": 4.0, "balance_bucket": "gift"}]);
    let AdminBillingMutationOutcome::Applied(plan) = data
        .read()
        .billing()
        .unwrap()
        .create_billing_plan(&BillingPlanWriteInput {
            title: "Test plan".into(),
            description: None,
            price_amount: 5.0,
            price_currency: "USD".into(),
            duration_unit: "month".into(),
            duration_value: 1,
            enabled: true,
            sort_order: 0,
            max_active_per_user: 1,
            purchase_limit_scope: "unlimited".into(),
            entitlements_json: entitlements.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("billing plan should be created");
    };
    let plan_id = plan.id;
    let outcome = writer
        .create_plan_purchase_order(CreatePlanPurchaseOrderInput {
            preferred_wallet_id: Some(wallet.id.clone()),
            user_id: user_id.into(),
            amount_usd: 5.0,
            pay_amount: 5.0,
            pay_currency: "USD".into(),
            exchange_rate: 1.0,
            payment_method: "stripe".into(),
            payment_provider: Some("stripe".into()),
            payment_channel: Some("card".into()),
            gateway_order_id: format!("pi_{}", uuid::Uuid::new_v4()),
            gateway_response: json!({"gateway": "stripe"}),
            order_no: uuid::Uuid::new_v4().to_string(),
            product_id: plan_id.clone(),
            product_snapshot: json!({
                "id": plan_id, "title": "Test plan", "duration_unit": "month", "duration_value": 1,
                "purchase_limit_scope": "unlimited",
                "entitlements": entitlements,
            }),
            expires_at_unix_secs: 4_102_444_800,
        })
        .await
        .unwrap();
    let CreatePlanPurchaseOrderOutcome::Created(order) = outcome else {
        panic!("plan order should be created: {outcome:?}");
    };
    let input = credit_input(&order.id);
    let WalletMutationOutcome::Applied((credited, true)) = writer
        .credit_admin_payment_order(input.clone())
        .await
        .unwrap()
    else {
        panic!("plan should be fulfilled");
    };
    assert_eq!(credited.order_kind, "plan_purchase");
    assert_eq!(credited.payment_provider.as_deref(), Some("stripe"));
    assert_eq!(credited.refundable_amount_usd, 0.0);
    assert_eq!(
        reader.find_admin_payment_order(&credited.id).await.unwrap(),
        Some(credited)
    );
    assert!(matches!(
        writer.credit_admin_payment_order(input).await.unwrap(),
        WalletMutationOutcome::Applied((_, false))
    ));
    let fulfilled = reader
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((fulfilled.balance, fulfilled.gift_balance), (10.0, 7.0));

    for bucket in ["recharge", "gift"] {
        let batch = writer
            .create_admin_redeem_code_batch(CreateAdminRedeemCodeBatchInput {
                name: format!("accounting-{}", uuid::Uuid::new_v4()),
                amount_usd: 2.0,
                currency: "USD".into(),
                balance_bucket: bucket.into(),
                total_count: 1,
                expires_at_unix_secs: None,
                description: None,
                created_by: None,
            })
            .await
            .unwrap();
        let input = RedeemWalletCodeInput {
            code: batch.codes[0].code.clone(),
            user_id: user_id.into(),
            order_no: uuid::Uuid::new_v4().to_string(),
        };
        let outcome = writer.redeem_wallet_code(input.clone()).await.unwrap();
        let RedeemWalletCodeOutcome::Redeemed { order, .. } = outcome else {
            panic!("fresh code should redeem: {outcome:?}");
        };
        assert_eq!(order.order_kind, "wallet_recharge");
        assert_eq!(order.payment_provider, None);
        assert_eq!(
            order.refundable_amount_usd,
            if bucket == "gift" { 0.0 } else { 2.0 }
        );
        assert_eq!(
            reader.find_admin_payment_order(&order.id).await.unwrap(),
            Some(order)
        );
        assert!(matches!(
            writer.redeem_wallet_code(input).await.unwrap(),
            RedeemWalletCodeOutcome::CodeRedeemed
        ));
    }
    let final_wallet = reader
        .find(WalletLookupKey::WalletId(&wallet.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            final_wallet.balance,
            final_wallet.gift_balance,
            final_wallet.total_recharged
        ),
        (12.0, 9.0, 14.0)
    );
    assert_eq!(
        reader
            .list_admin_wallet_transactions(&wallet.id, 100, 0)
            .await
            .unwrap()
            .total,
        ledger_count + 4
    );
}

fn credit_input(order_id: &str) -> CreditAdminPaymentOrderInput {
    CreditAdminPaymentOrderInput {
        order_id: order_id.into(),
        gateway_order_id: None,
        pay_amount: None,
        pay_currency: None,
        exchange_rate: None,
        gateway_response_patch: None,
        operator_id: None,
    }
}

struct DispatchPricing(BillingModelPricingSnapshot);

#[async_trait]
impl BillingModelContextLookup for DispatchPricing {
    async fn find_dispatch_pricing_snapshot(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<BillingModelPricingSnapshot>, DataLayerError> {
        Ok(Some(self.0.clone()))
    }

    async fn find_billing_model_context(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
    ) -> Result<Option<StoredBillingModelContext>, DataLayerError> {
        panic!("settlement must retain dispatch-time pricing");
    }
}

struct SettlementWriter(Arc<dyn SettlementWriteRepository>);

#[async_trait]
impl UsageSettlementWriter for SettlementWriter {
    fn has_usage_settlement_writer(&self) -> bool {
        true
    }

    async fn settle_usage(
        &self,
        input: UsageSettlementInput,
    ) -> Result<Option<StoredUsageSettlement>, DataLayerError> {
        self.0.settle_usage(input).await
    }
}

async fn cancelled_request_accounting(data: &DataBackends, user_id: &str) {
    let wallets = data.read().wallets().unwrap();
    let usage_reader = data.read().usage().unwrap();
    let usage_writer = data.write().usage().unwrap();
    let settlement = SettlementWriter(data.write().settlement().unwrap());
    for (request_type, fee) in [("chat", None), ("chat", Some(0.02)), ("image", Some(0.02))] {
        let before = wallets
            .find(WalletLookupKey::UserId(user_id))
            .await
            .unwrap()
            .unwrap();
        let pricing = DispatchPricing(BillingModelPricingSnapshot {
            provider_id: "accounting-provider".into(),
            provider_billing_type: Some("pay_as_you_go".into()),
            provider_quota_epoch_start_unix_secs: None,
            provider_api_key_id: None,
            provider_api_key_rate_multipliers: Some(json!({"openai:responses": 0.5})),
            provider_api_key_cache_ttl_minutes: None,
            global_model_id: "accounting-model".into(),
            global_model_name: "accounting-model".into(),
            global_model_config: None,
            default_price_per_request: fee,
            default_tiered_pricing: Some(
                json!({"tiers": [{"up_to": null, "input_price_per_1m": 3.0, "output_price_per_1m": 15.0, "cache_read_price_per_1m": 0.3}]}),
            ),
            model_id: None,
            model_provider_model_name: None,
            model_config: None,
            model_price_per_request: None,
            model_tiered_pricing: None,
        });
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut event = UsageEvent::new(
            UsageEventType::Cancelled,
            &request_id,
            UsageEventData {
                user_id: Some(user_id.into()),
                candidate_id: Some(uuid::Uuid::new_v4().to_string()),
                provider_name: "accounting-provider".into(),
                model: "accounting-model".into(),
                request_type: Some(request_type.into()),
                api_format: Some("openai:responses".into()),
                endpoint_api_format: Some("openai:responses".into()),
                input_tokens: Some(1000),
                output_tokens: Some(500),
                cache_read_input_tokens: Some(100),
                status_code: Some(499),
                request_metadata: Some(json!({"cancelled_request_fee": true, "image_count": 3})),
                ..UsageEventData::default()
            },
        );
        enrich_usage_event_with_billing(&pricing, &mut event)
            .await
            .unwrap();
        assert_eq!(event.data.total_cost_usd, Some(fee.unwrap_or(0.0)));
        assert_eq!(
            event.data.actual_total_cost_usd,
            Some(fee.unwrap_or(0.0) * 0.5)
        );
        let record = build_upsert_usage_record_from_event(&event).unwrap();
        usage_writer.upsert(record.clone()).await.unwrap();
        let audit = usage_reader
            .find_by_request_id(&request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audit.status, "cancelled");
        assert_eq!(
            audit.billing_status,
            if fee.is_some() { "pending" } else { "void" }
        );
        assert_eq!(
            audit
                .request_metadata
                .as_ref()
                .and_then(|m| m.get("cancelled_request_fee"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            fee.is_some()
        );
        settle_usage_if_needed(&settlement, &audit).await.unwrap();
        // Replayed terminal events and settlement retries must not charge again.
        usage_writer.upsert(record).await.unwrap();
        settle_usage_if_needed(&settlement, &audit).await.unwrap();
        let after = wallets
            .find(WalletLookupKey::UserId(user_id))
            .await
            .unwrap()
            .unwrap();
        let charge = fee.unwrap_or(0.0) * 0.5;
        assert!((before.balance - after.balance - charge).abs() < 1e-9);
        assert_eq!(after.gift_balance, before.gift_balance);
        assert!((after.total_consumed - before.total_consumed - charge).abs() < 1e-9);
        let stored = usage_reader
            .find_by_request_id(&request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, "cancelled");
        assert_eq!(
            stored.billing_status,
            if fee.is_some() { "settled" } else { "void" }
        );
    }
}

async fn run_accounting_contracts(driver: DatabaseDriver, url: String) {
    let (data, user_id) = accounting_fixture(driver, url).await;
    payment_order_lifecycle(&data, &user_id).await;
    cancelled_request_accounting(&data, &user_id).await;
    match driver {
        DatabaseDriver::Postgres => data.postgres().unwrap().pool().close().await,
        DatabaseDriver::Mysql => data.mysql().unwrap().pool().close().await,
        DatabaseDriver::Sqlite => data.sqlite().unwrap().pool().close().await,
    }
}

#[tokio::test]
async fn sqlite_payment_orders_and_cancelled_request_fees() {
    run_accounting_contracts(DatabaseDriver::Sqlite, "sqlite::memory:".into()).await;
}

#[tokio::test]
#[ignore = "requires a disposable AETHER_TEST_MYSQL_URL database"]
async fn mysql_payment_orders_and_cancelled_request_fees() {
    run_accounting_contracts(
        DatabaseDriver::Mysql,
        std::env::var("AETHER_TEST_MYSQL_URL").unwrap(),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires a disposable AETHER_TEST_POSTGRES_URL database"]
async fn postgres_payment_orders_and_cancelled_request_fees() {
    run_accounting_contracts(
        DatabaseDriver::Postgres,
        std::env::var("AETHER_TEST_POSTGRES_URL").unwrap(),
    )
    .await;
}
