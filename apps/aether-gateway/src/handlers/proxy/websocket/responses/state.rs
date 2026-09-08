//! Mutable state owned by one Responses WebSocket connection.
//!
//! The session loop is intentionally kept separate from these containers.  A
//! connection may survive many `response.create` turns, while the turn
//! lifecycle and upstream binding are replaced independently.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tokio::task::JoinHandle;

use super::adapter::{ResponsesWebSocketDrainDirective, ResponsesWebSocketProtocolAdapter};
use super::binding::UpstreamBindingIdentity;
use super::redaction::ResponsesWebSocketRedactionRestorer;
use super::request::ResponsesLiteStaticConfig;
use super::turn_state::ResponsesTurnState;
use crate::ai_serving::{AiExecutionDecision, ResponsesWebSocketBodyNormalization};

const EXHAUSTED_KEY_EXCLUSION_FALLBACK_SECONDS: u64 = 300;
const MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS: usize = 1_024;

#[derive(Debug, Default)]
struct BoundedContinuationResponseIds {
    ids: BTreeSet<String>,
    insertion_order: VecDeque<String>,
}

impl BoundedContinuationResponseIds {
    fn contains(&self, response_id: &str) -> bool {
        self.ids.contains(response_id)
    }

    fn remember(&mut self, response_id: &str) {
        if !self.ids.insert(response_id.to_string()) {
            return;
        }
        self.insertion_order.push_back(response_id.to_string());
        while self.ids.len() > MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            self.ids.remove(oldest.as_str());
        }
    }

    fn forget(&mut self, response_id: &str) {
        if self.ids.remove(response_id) {
            self.insertion_order
                .retain(|remembered| remembered != response_id);
        }
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.insertion_order.clear();
    }
}

/// Principal-proved response IDs for the currently bound response chain.
///
/// Connection-local IDs prove that the provider's current physical socket can
/// resolve a parent, including `store=false` responses. Persisted IDs are kept
/// separately after a successful principal-scoped registry write (or a proved
/// cross-socket bootstrap), so a 4xx/5xx eviction of the provider's local cache
/// does not suppress the provider's documented `store=true` hydration fallback.
/// Starting an independent chain or replacing the physical upstream clears
/// both bounded sets.
#[derive(Debug, Default)]
pub(super) struct ContinuationResponseIds {
    connection_local: BoundedContinuationResponseIds,
    persisted: BoundedContinuationResponseIds,
}

impl ContinuationResponseIds {
    pub(super) fn contains(&self, response_id: &str) -> bool {
        self.connection_local.contains(response_id) || self.persisted.contains(response_id)
    }

    pub(super) fn remember_connection_local(&mut self, response_id: &str) {
        self.connection_local.remember(response_id);
    }

    pub(super) fn remember_persisted(&mut self, response_id: &str) {
        self.persisted.remember(response_id);
    }

    pub(super) fn forget_connection_local(&mut self, response_id: &str) {
        self.connection_local.forget(response_id);
    }

    pub(super) fn clear(&mut self) {
        self.connection_local.clear();
        self.persisted.clear();
    }
}

/// All mutable state associated with the physical upstream connection.
pub(super) struct BoundResponsesConnection {
    pub(super) upstream: Option<wreq::ws::WebSocket>,
    pub(super) adapter: &'static dyn ResponsesWebSocketProtocolAdapter,
    pub(super) client_model: String,
    pub(super) provider_model: String,
    pub(super) decision_template: AiExecutionDecision,
    /// Reproduces this binding's provider-body normalization for continuation
    /// turns, which must not re-enter the planner. Replaced whenever the
    /// binding or its decision is replaced.
    pub(super) body_normalization: ResponsesWebSocketBodyNormalization,
    /// Static Responses Lite configuration already represented in the current
    /// response chain. A continuation may repeat it, but may not append a
    /// changed synthetic prefix to the inherited history.
    pub(super) responses_lite_static_config: Option<ResponsesLiteStaticConfig>,
    pub(super) binding_identity: UpstreamBindingIdentity,
    /// IDs observed on the current physical upstream and current independent
    /// response chain. A continuation must reference one of these IDs; the
    /// cross-socket bootstrap path seeds the already registry-proved parent.
    pub(super) continuation_response_ids: ContinuationResponseIds,
    /// 这条连接上「有没有正在进行的 logical turn」的唯一事实来源。
    pub(super) turn_state: ResponsesTurnState,
    /// 这条连接迄今 mask 出来的映射，用于把 provider 事件里的占位符换回真实值。
    ///
    /// 刻意按连接持有而不是按 turn 持有：WS 的会话历史留在上游，continuation 只发
    /// 增量输入，所以后面几轮的响应可能回显更早那几轮的占位符（理由详见
    /// [`super::redaction`]）。上游重绑时不重置。
    pub(super) redaction_restorer: ResponsesWebSocketRedactionRestorer,
    pub(super) next_turn_index: u64,
    pub(super) upstream_response_headers: BTreeMap<String, String>,
    pub(super) pending_adapter_drain: Option<ResponsesWebSocketDrainDirective>,
    pub(super) pending_adapter_observation: Option<JoinHandle<()>>,
    pub(super) exhausted_exclusions: ExhaustedResponsesWebSocketExclusions,
    pub(super) pending_turn_finalization: Option<JoinHandle<()>>,
}

/// Connection-local fallback in addition to the distributed account breaker.
/// A key and its provider account are excluded until the upstream's reset
/// deadline (or a short fallback when the terminal payload lacks one), so an
/// unusually long-lived client socket does not keep it unavailable after the
/// quota has recovered.
#[derive(Debug, Default)]
pub(super) struct ExhaustedResponsesWebSocketExclusions {
    expires_at_by_key: BTreeMap<String, u64>,
    expires_at_by_codex_account: BTreeMap<String, u64>,
}

impl ExhaustedResponsesWebSocketExclusions {
    pub(super) fn exclude(
        &mut self,
        key_id: String,
        codex_account_id: Option<String>,
        reset_at_unix_secs: Option<u64>,
        now_unix_secs: u64,
    ) -> u64 {
        self.prune(now_unix_secs);
        let requested_expiry = reset_at_unix_secs
            .filter(|reset_at| *reset_at > now_unix_secs)
            .unwrap_or_else(|| {
                now_unix_secs.saturating_add(EXHAUSTED_KEY_EXCLUSION_FALLBACK_SECONDS)
            });
        let expiry = self
            .expires_at_by_key
            .entry(key_id)
            .and_modify(|existing| *existing = (*existing).max(requested_expiry))
            .or_insert(requested_expiry);
        if let Some(account_id) = codex_account_id {
            self.expires_at_by_codex_account
                .entry(account_id)
                .and_modify(|existing| *existing = (*existing).max(requested_expiry))
                .or_insert(requested_expiry);
        }
        *expiry
    }

    pub(super) fn codex_account_ids(&mut self, now_unix_secs: u64) -> BTreeSet<String> {
        self.prune(now_unix_secs);
        self.expires_at_by_codex_account.keys().cloned().collect()
    }

    pub(super) fn key_ids(&mut self, now_unix_secs: u64) -> BTreeSet<String> {
        self.prune(now_unix_secs);
        self.expires_at_by_key.keys().cloned().collect()
    }

    pub(super) fn len(&mut self, now_unix_secs: u64) -> usize {
        self.prune(now_unix_secs);
        self.expires_at_by_key.len() + self.expires_at_by_codex_account.len()
    }

    fn prune(&mut self, now_unix_secs: u64) {
        self.expires_at_by_key
            .retain(|_, expires_at| *expires_at > now_unix_secs);
        self.expires_at_by_codex_account
            .retain(|_, expires_at| *expires_at > now_unix_secs);
    }
}

#[cfg(test)]
mod tests {
    use super::{ContinuationResponseIds, MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS};

    #[test]
    fn continuation_ids_are_bounded_and_clear_with_the_chain() {
        let mut ids = ContinuationResponseIds::default();
        for index in 0..=MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS {
            ids.remember_connection_local(format!("resp_{index}").as_str());
        }

        assert!(!ids.contains("resp_0"));
        assert!(ids.contains("resp_1"));
        assert!(
            ids.contains(format!("resp_{MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS}").as_str())
        );
        assert_eq!(
            ids.connection_local.ids.len(),
            MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS
        );
        assert_eq!(
            ids.connection_local.insertion_order.len(),
            MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS
        );

        ids.forget_connection_local("resp_1");
        assert!(!ids.contains("resp_1"));
        assert_eq!(
            ids.connection_local.insertion_order.len(),
            MAX_CONNECTION_LOCAL_CONTINUATION_RESPONSE_IDS - 1
        );

        ids.clear();
        assert!(ids.connection_local.ids.is_empty());
        assert!(ids.connection_local.insertion_order.is_empty());
        assert!(ids.persisted.ids.is_empty());
        assert!(ids.persisted.insertion_order.is_empty());
    }

    #[test]
    fn local_eviction_keeps_only_registered_persisted_ownership() {
        let mut ids = ContinuationResponseIds::default();
        ids.remember_connection_local("resp_store_false");
        ids.remember_connection_local("resp_store_true");
        ids.remember_persisted("resp_store_true");

        ids.forget_connection_local("resp_store_false");
        ids.forget_connection_local("resp_store_true");

        assert!(
            !ids.contains("resp_store_false"),
            "store=false has no persisted hydration fallback after local eviction"
        );
        assert!(
            ids.contains("resp_store_true"),
            "a registered store=true parent remains eligible for persisted hydration"
        );
    }
}
