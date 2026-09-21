//! Cloud Code（Antigravity / Gemini CLI 共用后端）的 onboardUser 流程。
//!
//! 免费 tier 的新账号在 loadCodeAssist 响应里没有 `cloudaicompanionProject`；
//! 官方客户端会取 `allowedTiers[isDefault].id` 调 onboardUser，服务端返回一个
//! 长时操作（LRO），轮询到 `done: true` 后 `response.cloudaicompanionProject`
//! 才是可用的 project。两个客户端只差 `ideType` 与 UA，请求形状与轮询逻辑相同。
//!
//! 参考：CLIProxyAPI `internal/auth/antigravity/auth.go` `OnboardUser`，
//! Gemini CLI `packages/core/src/code_assist/setup.ts` `setupUser`。

use std::time::Duration;

use aether_contracts::ExecutionResult;
use aether_provider_transport::GatewayProviderTransportSnapshot;
use serde_json::Value;

use crate::transport::{
    build_antigravity_onboard_user_plan, build_gemini_cli_onboard_user_plan,
    ModelFetchTransportRuntime,
};

/// onboardUser 最多轮询次数。
pub const CLOUD_CODE_ONBOARD_MAX_ATTEMPTS: usize = 5;
/// 两次轮询之间的间隔。
pub const CLOUD_CODE_ONBOARD_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudCodeOnboardingClient {
    Antigravity,
    GeminiCli,
}

impl CloudCodeOnboardingClient {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::GeminiCli => "gemini_cli",
        }
    }

    /// loadCodeAssist 既没有默认 tier 也没有 currentTier 时的兜底：Antigravity
    /// 客户端用 `free-tier`，Gemini CLI 官方实现用 `legacy-tier`。
    pub const fn fallback_tier_id(self) -> &'static str {
        match self {
            Self::Antigravity => "free-tier",
            Self::GeminiCli => "legacy-tier",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudCodeOnboardingOutcome {
    pub project_id: String,
    pub tier_id: String,
    /// 实际发出的 onboardUser 请求次数（含最后一次成功）。
    pub attempts: usize,
}

/// 从 loadCodeAssist 响应里选 onboarding 用的 tier：`allowedTiers` 里
/// `isDefault: true` 的那个优先，其次 `currentTier.id`，最后按客户端兜底。
pub fn cloud_code_onboard_tier_id(
    load_code_assist: &Value,
    client: CloudCodeOnboardingClient,
) -> String {
    let default_tier = load_code_assist
        .get("allowedTiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tier| {
            tier.get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .find_map(|tier| non_empty_string(tier.get("id")));
    default_tier
        .or_else(|| non_empty_string(load_code_assist.pointer("/currentTier/id")))
        .unwrap_or_else(|| client.fallback_tier_id().to_string())
}

/// `cloudaicompanionProject` / `projectId` / `project`，字符串或 `{id}` 对象。
pub fn extract_cloud_code_project_id(value: &Value) -> Option<String> {
    for key in [
        "cloudaicompanionProject",
        "cloudAiCompanionProject",
        "projectId",
        "project",
    ] {
        let Some(raw) = value.get(key) else {
            continue;
        };
        if let Some(text) = non_empty_string(Some(raw)) {
            return Some(text);
        }
        if let Some(id) = raw.as_object().and_then(|object| {
            non_empty_string(
                object
                    .get("id")
                    .or_else(|| object.get("project_id"))
                    .or_else(|| object.get("projectId")),
            )
        }) {
            return Some(id);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudCodeOnboardPoll {
    /// LRO 完成且带回 project。
    Done(String),
    /// LRO 完成但响应里没有 project：再轮询也不会有，直接失败。
    DoneWithoutProject,
    /// LRO 未完成，稍后再问。
    Pending,
}

/// 解析一次 onboardUser 响应（`google.longrunning.Operation` 形状）。
pub fn classify_cloud_code_onboard_response(body: &Value) -> CloudCodeOnboardPoll {
    let done = body.get("done").and_then(Value::as_bool).unwrap_or(false);
    if !done {
        return CloudCodeOnboardPoll::Pending;
    }
    let project_id = body
        .get("response")
        .and_then(extract_cloud_code_project_id)
        .or_else(|| extract_cloud_code_project_id(body));
    match project_id {
        Some(project_id) => CloudCodeOnboardPoll::Done(project_id),
        None => CloudCodeOnboardPoll::DoneWithoutProject,
    }
}

/// 执行 onboardUser 并轮询到完成。非 2xx 直接失败；`done: false` 时等待
/// [`CLOUD_CODE_ONBOARD_POLL_INTERVAL`] 后重试，最多 [`CLOUD_CODE_ONBOARD_MAX_ATTEMPTS`] 次。
pub async fn onboard_cloud_code_user(
    runtime: &(impl ModelFetchTransportRuntime + ?Sized),
    transport: &GatewayProviderTransportSnapshot,
    client: CloudCodeOnboardingClient,
    load_code_assist: &Value,
) -> Result<CloudCodeOnboardingOutcome, String> {
    let tier_id = cloud_code_onboard_tier_id(load_code_assist, client);
    let plan = match client {
        CloudCodeOnboardingClient::Antigravity => {
            build_antigravity_onboard_user_plan(runtime, transport, &tier_id).await?
        }
        CloudCodeOnboardingClient::GeminiCli => {
            build_gemini_cli_onboard_user_plan(runtime, transport, &tier_id).await?
        }
    };
    let label = client.label();

    for attempt in 1..=CLOUD_CODE_ONBOARD_MAX_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(CLOUD_CODE_ONBOARD_POLL_INTERVAL).await;
        }
        // 运行时错误文本可能带上游 URL / 响应片段，这里不透传，调用方只需要知道
        // onboarding 在哪一步失败。
        let result = runtime
            .execute_model_fetch_execution_plan(&plan)
            .await
            .map_err(|_| format!("{label}: onboardUser request failed (attempt {attempt})"))?;
        if !(200..300).contains(&result.status_code) {
            return Err(format!(
                "{label}: onboardUser failed with status {}",
                result.status_code
            ));
        }
        let body = onboard_response_json(&result);
        match classify_cloud_code_onboard_response(&body) {
            CloudCodeOnboardPoll::Done(project_id) => {
                return Ok(CloudCodeOnboardingOutcome {
                    project_id,
                    tier_id,
                    attempts: attempt,
                });
            }
            CloudCodeOnboardPoll::DoneWithoutProject => {
                return Err(format!(
                    "{label}: onboardUser completed without a project id"
                ));
            }
            CloudCodeOnboardPoll::Pending => {}
        }
    }

    Err(format!(
        "{label}: onboardUser did not complete after {CLOUD_CODE_ONBOARD_MAX_ATTEMPTS} attempts"
    ))
}

fn onboard_response_json(result: &ExecutionResult) -> Value {
    result
        .body
        .as_ref()
        .and_then(|body| body.json_body.clone())
        .unwrap_or(Value::Null)
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        classify_cloud_code_onboard_response, cloud_code_onboard_tier_id,
        extract_cloud_code_project_id, CloudCodeOnboardPoll, CloudCodeOnboardingClient,
    };

    #[test]
    fn tier_prefers_the_default_allowed_tier_then_current_tier_then_client_fallback() {
        let both = json!({
            "allowedTiers": [
                { "id": "standard-tier", "isDefault": false },
                { "id": "free-tier", "isDefault": true }
            ],
            "currentTier": { "id": "current-tier" }
        });
        assert_eq!(
            cloud_code_onboard_tier_id(&both, CloudCodeOnboardingClient::Antigravity),
            "free-tier"
        );

        let current_only = json!({ "currentTier": { "id": "current-tier" } });
        assert_eq!(
            cloud_code_onboard_tier_id(&current_only, CloudCodeOnboardingClient::GeminiCli),
            "current-tier"
        );

        let empty = json!({ "allowedTiers": [{ "id": " ", "isDefault": true }] });
        assert_eq!(
            cloud_code_onboard_tier_id(&empty, CloudCodeOnboardingClient::Antigravity),
            "free-tier"
        );
        assert_eq!(
            cloud_code_onboard_tier_id(&empty, CloudCodeOnboardingClient::GeminiCli),
            "legacy-tier"
        );
    }

    #[test]
    fn project_id_accepts_string_and_object_shapes() {
        assert_eq!(
            extract_cloud_code_project_id(&json!({ "cloudaicompanionProject": " p-1 " }))
                .as_deref(),
            Some("p-1")
        );
        assert_eq!(
            extract_cloud_code_project_id(&json!({ "cloudaicompanionProject": { "id": "p-2" } }))
                .as_deref(),
            Some("p-2")
        );
        assert_eq!(
            extract_cloud_code_project_id(&json!({ "projectId": "p-3" })).as_deref(),
            Some("p-3")
        );
        assert_eq!(
            extract_cloud_code_project_id(&json!({ "project": { "projectId": "p-4" } })).as_deref(),
            Some("p-4")
        );
        assert_eq!(
            extract_cloud_code_project_id(&json!({ "cloudaicompanionProject": {} })),
            None
        );
        assert_eq!(extract_cloud_code_project_id(&json!({})), None);
    }

    #[test]
    fn onboard_response_classification_follows_the_lro_done_flag() {
        assert_eq!(
            classify_cloud_code_onboard_response(&json!({ "name": "op/1", "done": false })),
            CloudCodeOnboardPoll::Pending
        );
        assert_eq!(
            classify_cloud_code_onboard_response(&json!({ "name": "op/1" })),
            CloudCodeOnboardPoll::Pending
        );
        assert_eq!(
            classify_cloud_code_onboard_response(&json!({
                "done": true,
                "response": { "cloudaicompanionProject": { "id": "p-9" } }
            })),
            CloudCodeOnboardPoll::Done("p-9".to_string())
        );
        assert_eq!(
            classify_cloud_code_onboard_response(&json!({ "done": true, "response": {} })),
            CloudCodeOnboardPoll::DoneWithoutProject
        );
    }
}
