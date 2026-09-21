# Provider 质量提升计划（对标 CLIProxyAPI）

> 状态：**本地实现与收尾验证完成，真实客户端验收待完成；2026-09-21**。
>
> 进度：P0–P6 代码已落地，各阶段实现口径见对应「实施记录」；P0–P4 经只读审查后的修正见「审查后修正」，P5/P6 与关联配置、额度修正见 §7「收尾审查」。真实环境验收仍待完成：用真实 Claude Code 2.1.x 抓包核对 P2 oracle 与 P5 TLS 参数、确认 Antigravity 真实客户端版本。节点隧道当前不支持 `browser_wreq`，不会静默降级为 rustls。
>
> 背景：2026-09-19 对 CLIProxyAPI（commit `c93978c`）做只读对比，覆盖 Codex、Claude Code、Antigravity、Gemini CLI 四个 OAuth 上游。本计划只收录已在本地代码中逐条核实的差距，未核实的推测不进入范围。
>
> 原则：横切层（冷却、限流信号、回放缓存）优先于单上游细节；透传优先于改写；任何改写用户请求体的功能必须在请求详情中可见。

---

## 0. 核实过的差距清单

| # | 差距 | 本地证据 | 影响 | 阶段 |
|---|---|---|---|---|
| 1 | 无 `signature` 的 `redacted_thinking` 块被整块丢弃 | `crates/aether-provider/transport/src/claude_code/request.rs:188-198`，测试 `:284-285` 固化了错误假设 | 真实 API 的 redacted 块只有 `data`，多轮对话丢上下文，可能触发 400 | P0 |
| 2 | 合法 `response.incomplete` 被记成失败 | `apps/aether-gateway/src/execution_runtime/attempt_lifecycle.rs:238-241` 注释自认 | 健康分误扣、失败率虚高 | P0 |
| 3 | PKCE verifier 用 UUID 拼接而非 CSPRNG | `crates/aether-oauth/src/core/pkce.rs:11-18` | 熵够用但不符合 RFC 7636 建议 | P0 |
| 4 | Gemini CLI 的 loadCodeAssist 写死 `ideType: ANTIGRAVITY` | `crates/aether-model-fetch/src/transport.rs:335-341` | 与 `GeminiCLI/0.1.5` UA 不一致，tier 可能误判 | P0 |
| 5 | 429 冷却固定 5 分钟，不解析 `Retry-After`、`RetryInfo.retryDelay`、`usage_limit_reached.resets_at` | `orchestration/adaptive.rs:30`、`orchestration/effects.rs:2047-2084`；全库无 `RetryInfo`；池写入路径只解析 `quotaResetDelay` | 秒级限流被冷却过久，真正耗尽 5 分钟后反复撞 | P1 |
| 6 | 全库不消费 `anthropic-ratelimit-unified-*` 头 | grep 无结果；`response_header_rules.rs:102` 已采集但无人读 | 5h/7d 窗口耗尽的 Key 被反复选中 | P1 |
| 7 | Claude Code 无 `cch=` 请求签名 | grep 无结果 | 与原生 CLI 指纹不一致 | P2 |
| 8 | 第三方客户端裸发，`anthropic-beta` 只增不减 | `claude_code/profile.rs:234-251`、`client_session_affinity.rs:442-452` 只用于亲和 | 非 Claude Code 客户端请求形状暴露 | P2 |
| 9 | thinking `budget_tokens` 与 `max_tokens` 无关联校验，adaptive 靠模型名硬编码 | `formats/shared/model_directives.rs:124-133,635-641` | 新模型误发 `budget_tokens`，budget ≥ max_tokens 触发 400 | P2 |
| 10 | claude_code 未注册额度刷新 | `handlers/admin/provider/oauth/quota/dispatch.rs:29-46` | 管理端看不到 5h/7d 窗口 | P2 |
| 11 | Antigravity 与 Gemini CLI 都没有 onboardUser | 全库 grep 无结果 | 免费 tier 新账号无 project 直接失败 | P3 |
| 12 | Antigravity 客户端版本硬编码 4.3.0 | `transport/src/antigravity/auth.rs:8-9` | 上游淘汰旧版本时需改代码发版 | P3 |
| 13 | 无跨轮次的 Codex 加密推理 / Gemini thoughtSignature 回放缓存 | `ai_serving/planner/standard/deepseek.rs:31` 只有过滤策略；`antigravity/request.rs:547` 只填首个占位符 | 跨格式多轮 tool-use 后续轮次被拒 | P4 |
| 14 | Codex tool schema 清洗只剥 hosted name 与 cache_control | `formats/openai/responses/codex.rs:1563-1598` | MCP 工具的 `$schema`、超长名、`oneOf const` 触发 400 | P4 |
| 15 | 出站 TLS 指纹是 rustls 默认值 | `network.rs:168-190` claude_code 固定 `reqwest_rustls`；`docs/operations/tls-fingerprint-capture.md` 记录 `observed: false` | 网络层可识别为非官方客户端 | P5 |
| 16 | 无敏感词混淆能力 | 无对应物 | 第三方客户端系统提示词中的代理特征词直达上游 | P6 |

**不纳入范围**：CLIProxyAPI 的五套翻译器、散装 WebSocket 实现、compaction 固定密钥胶囊、mcp 工具名别名化、count_tokens 本地估算。原因见对比结论，均与 Aether 已有的 canonical 层或透传原则冲突。

---

## 1. 阶段总览

| 阶段 | 内容 | 后端 | **前端** | 依赖 | 工作量 | 风险 |
|---|---|---|---|---|---|---|
| P0 | 正确性修复 | 是 | 否 | 无 | 1.5 天 | 低 |
| P1 | 上游限流信号驱动冷却 | 是 | **是**（冷却原因与截止时间展示） | 无 | 4 天 | 中 |
| P2 | Claude Code 指纹与客户端策略 | 是 | **是**（5h/7d 额度、客户端策略开关） | P0 | 6 天 | 中 |
| P3 | Google 系 onboarding 与版本 | 是 | 否 | 无 | 3 天 | 低 |
| P4 | 推理回放缓存与 Codex 清洗 | 是 | 否 | P0 | 6 天 | 中 |
| P5 | TLS 指纹仿真 | 是 | **是**（传输 profile 选择器） | P2 | 5 天 | 中高 |
| P6 | 敏感词零宽混淆 | 是 | **⚠️ 必做，见 §8** | P2（签名顺序） | 后端 2 天 + **前端 3 天** | 中 |

建议顺序：P0 → P1 → P2 → P3 与 P4 并行 → P5 → P6。P6 依赖 P2 是因为混淆必须在 CCH 签名之前执行，顺序错了签名就失效。

---

## Phase 0：正确性修复

### 0.1 `redacted_thinking` 保留

**改动**：`crates/aether-provider/transport/src/claude_code/request.rs:178-202` 的 `keep_claude_code_block`。`redacted_thinking` 只要求 `data` 非空即保留；`thinking` 仍要求非占位签名。同步修正 `:278-290` 的测试用例，把"drop-no-signature"的 redacted 块改为期望保留。

**验收**：新增用例，assistant 消息里同时含正常 thinking、空签名 thinking、仅 `data` 的 redacted，输出保留第一和第三块。

### 0.2 `response.incomplete` 记账

**改动**：`apps/aether-gateway/src/execution_runtime/attempt_lifecycle.rs` 与共享 usage 判定。引入终态归一：

- `incomplete` 且 `output` 非空、`incomplete_details.reason ∈ {max_output_tokens, content_filter}` → 成功，账单照记。
- `incomplete` 且 `output` 为空 → 记 502，投射供应商失败。
- 流结束但没有 `response.completed` / `message_stop` → 记 408，投射供应商失败。

参考 CLIProxyAPI `internal/runtime/executor/codex_executor_terminal.go:17-47,332-376` 与 `helps/codex_terminal_incomplete.go`。

**验收**：`settlement_table_row_legitimate_incomplete_*` 系列用例改为"记账成功"，新增空 incomplete 与缺终态两条。

### 0.3 PKCE 改 CSPRNG

**改动**：`crates/aether-oauth/src/core/pkce.rs:11-18`，`generate_pkce_verifier` 改为 `rand::rngs::OsRng` 生成 64 字节后 base64url 无填充编码（长度 86，落在 RFC 7636 的 43–128 范围）。`generate_oauth_nonce` 同样处理。

### 0.4 Gemini CLI `ideType`

**改动**：`crates/aether-model-fetch/src/transport.rs:306-349` 的 `build_gemini_cli_load_code_assist_plan`，`ideType` 改为 Gemini CLI 官方值，并与 `planner/gemini_cli.rs:67-68` 的 UA 保持同一来源常量。需要先抓一次真实 Gemini CLI 的 loadCodeAssist 请求确认字段值，不能照抄 CLIProxyAPI（它已把 Gemini CLI 插件化）。

#### P0 实施记录（2026-09-20）

- 0.1：`keep_claude_code_block` 对 `redacted_thinking` 只要求 `data` 非空；两条既有集成测试改为期望保留。
- 0.2：`execution_runtime/stream/execution.rs` 与 `attempt_lifecycle.rs`：合法 incomplete 记成功；空 incomplete 记 502；流在 2xx 下结束却没有终态事件记 408 并投射供应商失败（`stream_terminal_status_code_for_missing_terminal`）。审查曾担心「只发 `[DONE]`、不带 finish_reason 与 usage 的 OpenAI 兼容上游会被误判」，经探针测试核实不成立：解析器在流结束时会合成 Finish，`observed_finish` 为真，只有 Responses 族要求显式终态事件。
- 0.3：`pkce.rs` 改 CSPRNG（verifier 86 字符、nonce 43 字符 base64url）；`is_generated_oauth_nonce` 同时接受发布前的 64 位十六进制旧形状，网关两处校验改用它。
- 0.4：Gemini CLI `ideType = IDE_UNSPECIFIED`（`gemini_cli/url.rs::GEMINI_CLI_IDE_TYPE`），与 UA 同源；Antigravity 仍是 `ANTIGRAVITY`。

---

## Phase 1：上游限流信号驱动冷却

目标：把"固定 5 分钟"改成"上游告诉多久就冷却多久，没告诉就指数退避"，并支持模型级冷却。

### 1.1 统一的重试提示提取

新增 `crates/aether-provider/transport/src/retry_hint.rs`：

```rust
pub struct UpstreamRetryHint {
    pub source: RetryHintSource,   // RetryAfterHeader | AnthropicRateLimitWindow | GoogleRetryInfo | CodexResetsAt | GrokText | None
    pub retry_after: Option<Duration>,
    pub reset_at_unix_secs: Option<u64>,
    pub scope: RetryHintScope,     // Key | KeyModel
}
pub fn extract_upstream_retry_hint(provider_type, status, headers, body) -> UpstreamRetryHint
```

来源规则：

| 供应商 | 读取位置 | 参考 |
|---|---|---|
| 所有 | HTTP `Retry-After`（秒或 HTTP-date） | CLIProxyAPI `conductor_cooldown.go:1682` |
| claude_code | `anthropic-ratelimit-unified-5h-reset`、`-7d-reset`、`-status`、`-overage-*`，取最晚者加抖动 | `helps/claude_ratelimit.go:24-233` |
| codex | 错误体 `usage_limit_reached.resets_at`，排除 `rate_limit_error` | `codex_executor_terminal.go:402-445` |
| antigravity / gemini_cli | `error.details[].@type=RetryInfo.retryDelay`、`ErrorInfo.reason ∈ {QUOTA_EXHAUSTED, RATE_LIMIT_EXCEEDED}`、`quotaResetDelay` | `antigravity_executor_credits.go:216-272`、`helps/json_retry_helpers.go:27-57` |
| grok | 已有中英文等待时长正则 | `report_effects.rs:141-192`，迁入本模块 |

### 1.2 冷却决策

改 `apps/aether-gateway/src/orchestration/effects.rs:2047-2084` 的 `pool_score_hard_state_for_status`，返回值增加 `cooldown_until` 与 `backoff_level`：

- 有提示且 `< 3s`：不冷却，同 Key 立即重试一次（接入现有 `same_key_retries`，见记忆 [同 Key 重试口径]）。
- 有提示且 `< 5min`：短冷却到提示时刻，换 Key。
- 有提示且 `≥ 5min`：标记 `QuotaExhausted`，`reset_at` 写入池成员 quota 元数据，调度层已支持按 `reset_at` 过期（`pool/src/lib.rs:833` 有测试）。
- 无提示的 429：指数退避阶梯，基数 30s、上限 30min，**同一窗口内的并发失败只升一级**（参考 `quotaCooldownAfterFailure`）。
- 401 / 402 / 403：30 分钟；404：12 小时；5xx / 408 / 520–526：60 秒（可配置）。
- 后来的失败只延长仍在生效的冷却，不缩短。

### 1.3 模型级冷却

`PoolMemberHardState::Cooldown` 目前是 Key 级。增加 `scope_kind = "key_model"` 的池分记录（`pool_scores/types.rs:95` 已有 `PoolScoreScope`），供应商级开关 `config.cooldown.model_level`（默认关）。Antigravity 的模型级耗尽隔离已存在（`pool/src/quota.rs:76-166`），本项把它推广到 claude_code 与 codex。

### 1.4 配置

供应商级 `config.cooldown`：`{ disable: bool, transient_error_seconds: u64, model_level: bool }`；Key 级 `auth_config.cooldown` 覆盖。沿用 `codex_fingerprint_convergence_enabled` 的写入模式（`handlers/admin/provider/write/provider/update.rs:291-315`）。

### 1.5 前端

- `PoolStatusCard.vue:98-117` 已显示 `cooldown_reason` 与 TTL。新增原因枚举的文案映射：`retry_after_header`、`ratelimit_window_5h`、`ratelimit_window_7d`、`google_retry_info`、`codex_resets_at`、`backoff_level_N`、`transient_upstream`。
- 冷却截止时间改为绝对时刻加倒计时，与 [使用记录耗时口径] 记忆一致，不用引用比较做变化检测。
- `ProviderFormDialog.vue` 增加"冷却策略"分组：禁用冷却、瞬时错误冷却秒数、模型级冷却开关。类型在 `frontend/src/api/endpoints/types/provider.ts`、`endpoints/providers.ts`，文案在 `i18n/messages.ts`。

**验收**：集成测试构造 Retry-After=2 → 同 Key 重试；`anthropic-ratelimit-unified-5h-reset` 为未来 3 小时 → Key 在 3 小时内不被调度且 UI 显示原因；连续 3 次无提示 429 → 冷却 30s、60s、120s。

#### P1 实施记录（2026-09-20）

- 决策与写入分层：`handlers/admin/provider/pool/cooldown.rs` 是纯决策（`decide_provider_cooldown`），`pool/runtime/writes.rs` 负责写 KV，`orchestration/effects.rs::pool_score_hard_state_for_decision` 把决策映射成池分硬状态（QuotaExhausted / Cooldown / 不改）。
- KV 布局：原因码键不变（`ap:{provider}:cooldown:{key}`），新增 `cooldown_meta:{key}`（决策 JSON：source、until、backoff_level、reset_at、scope）、`cooldown_backoff:{key}`（退避等级，冷却结束后再保留 30 分钟，成功即清）、`cooldown_model:{key}:{hash(model)}` + `cooldown_model_idx:{key}`（模型级）。KV TTL 硬上限保持 32 分钟；≥5 分钟的上游提示同时把 `reset_at` 写进 `status_snapshot.quota.windows[]`，调度层按 reset_at 过期。
- 1.3 的偏离：模型级冷却用 KV 而不是 `scope_kind="key_model"` 的池分行。调度器只按 account 作用域查池分，真正挡请求的是冷却 KV；`dispatch/pool_scheduler.rs` 在 Key 级冷却之后再查模型级冷却，跳过原因 `pool_model_cooldown`。
- 原因码：429 保留 `rate_limited_429` / `quota_exhausted_429`（管理端过滤依赖），无提示退避写 `backoff_level_N`；401/402/403 账号级终态仍不写池冷却（由池分硬状态与熔断处理），软 403 写 `forbidden_403` 30 分钟；`not_found_404` 12 小时；5xx/408/520–526 用 `config.cooldown.transient_error_seconds`（默认 60，号池 `overload_cooldown_seconds=0` 时整体关闭，保持旧语义）。
- 立即重试：提示 < 3s 不写冷却，`effects.rs` 记进进程内 `POOL_IMMEDIATE_RETRY_HINTS`，`orchestration/attempt.rs::next_same_key_retry_attempt` 取一次即清，在同 Key 重试预算为 0 时额外给一次；非号池 Key 也生效。端到端用例 `tests/ai_execute/sync/chat/retry_hint.rs`。
- `orchestration/adaptive.rs` 的固定 5 分钟改为 60 秒学习暂停（取上游 Retry-After 较大者）；Key 冷却本身由号池负责。
- claude_code 5h/7d 窗口被动采集（原 2.6 的数据来源）：`aether-admin quota.rs::parse_claude_code_rate_limit_headers` → `upstream_metadata.claude_code.windows` → `catalog.rs::build_claude_code_quota_status_snapshot`；`report_effects.rs` 在 sync/stream 报告效果里对成功与失败响应都采集，只覆盖本次响应携带的窗口。
- 前端：`features/pool/utils/poolCooldown.ts` 统一原因文案（含 `backoff_level_N`、来源枚举）与「绝对截止时刻 + 倒计时」；PoolStatusCard 与 PoolManagement 每秒重算但按内容替换条目；ProviderFormDialog「冷却策略」分组写 `cooldown {disable, transient_error_seconds, model_level}`；链路展示按原因码映射并显示模型级冷却。
- **审查后修正（2026-09-21）**：(1) 「同 Key 立即重试」提示每个 (request, provider, key) 只发放一次，消费后留下已用标记，上游持续返回短 Retry-After 时最多多撞一回随后正常换 Key；候选循环在重试前按提示等待（封顶 3s）而不是无间隔重发（`effects.rs` / `attempt.rs::next_same_key_retry_attempt_with_wait` / `executor/candidate_loop.rs`）。(2) 404 的冷却作用域改为 Key+模型（一把 Key 服务多个模型，一个错的模型映射不再把整把 Key 关 12 小时；调用方没有模型名时仍退回 Key 级）。(3) `transient_error_seconds = 0` 现在也关闭带 Retry-After 的 5xx 冷却，与号池 `overload_cooldown_seconds = 0` 的历史语义一致。(4) 调度热路径不再逐 Key 读模型级冷却列表（`read_admin_provider_pool_runtime_state` 只做点查），列表由管理端展示路径的 `attach_admin_provider_pool_model_cooldowns` 单独加载。(5) 冷却索引集合的过期只延长不缩短（新增 `RuntimeState::key_ttl_seconds`）。(6) 软 401/403 的池分硬状态保持 `AuthInvalid`。(7) adaptive 学习暂停真正接入上游 `Retry-After`（记进 429 观测记录，扩容判定按其延长）。(8) Claude Code 5h/7d 窗口采集只在窗口事实变化时写库，不再每个响应 CAS 写库并清目录缓存。审查提出的「模型级冷却写入名与调度读取名不同源」经核实不成立：两侧都来自候选的 `selected_provider_model_name`（模型指令映射不改写 `mapped_model`）。

---

## Phase 2：Claude Code 指纹与客户端策略

### 2.1 客户端识别

新增 `crates/aether-provider/transport/src/claude_code/client_detection.rs`：综合 `x-app`、UA 正则、`anthropic-beta` 集合、`metadata.user_id` 形状四个信号，输出 `NativeClaudeCode | ThirdParty | Unknown`。参考 `helps/claude_client_detection.go:66-151`。现有 `client_session_affinity.rs:238-309` 的家族探测可复用但不合并，两者用途不同。

策略（供应商级 `config.cloak.mode`，`auto | always | off`，默认 `auto`）：

- 原生 Claude Code：完全透传，不改 system、不加 beta、不重写 user_id。
- 第三方：`auto` 与 `always` 时应用 2.2–2.5。
- `off`：只做现有的 billing header 版本同步。

### 2.2 beta 列表按条件组装

`claude_code/profile.rs:234-251` 从"固定集合 ∪ 客户端传入"改为按凭据类型、请求类型（messages / count_tokens）、是否子代理、thinking 是否启用有序生成，并剔除已知无效项。参考 `claude_executor_request.go:117-213`。

### 2.3 `metadata.user_id` 与设备 profile

生成 `{"device_id","account_uuid","session_id"}` JSON 形状的 user_id，`device_id` 由 Key 派生并持久化到 `auth_config.device_profile`，7 天内只升不降。参考 `helps/claude_credential_identity.go:288-309`、`helps/claude_device_profile.go:380-443`。

### 2.4 CCH 签名

新增 `claude_code/signing.rs`：对最终请求体做归一化（清空 `model` 值、剔除 `max_tokens` 与 fallbacks），xxHash64（seed 见 `claude_signing.go:201-236`），低 20 位写入 billing header 的 `cch=xxxxx;`。**执行顺序固定为：敏感词混淆（P6）→ 身份与 beta（2.2/2.3）→ cache_control 治理（2.5）→ CCH 签名。** 签名后不得再改动 body。

需要 `Cargo.toml` 新增 `xxhash-rust` 依赖（`xxh64` feature）。CLIProxyAPI 复刻的是 2.1.220，本地 profile 是 2.1.161（`profile.rs:88`），升级 profile 版本时要同步验证 seed 与归一化规则是否变化。

### 2.5 cache_control 治理与 thinking 归一化

- cache_control 断点最多 4 个，OAuth 凭据升到 `ttl: 1h`。参考 `claude_executor_execute.go:214-247`。
- thinking：`budget_tokens < max_tokens` 且 ≥ 模型最小值；`tool_choice` 强制时剥 thinking；adaptive 判定改为按模型能力表而非模型名前缀。参考 `internal/thinking/provider/claude/apply.go:171-208`。

### 2.6 刷新与额度

- `crates/aether-oauth/src/provider/providers/claude_code.rs` 交换与刷新后调 `/api/oauth/profile` 与 `claude_cli/roles` 补 org/account；刷新 429 走 P1 的 Retry-After、5xx 重试 3 次。
- `quota/dispatch.rs:29-46` 注册 `claude_code`，数据来源是 P1 采集的 5h/7d 窗口头（被动），不新增主动接口。

### 2.7 前端

- `ProviderFormDialog.vue` 对 `provider_type = claude_code` 显示"客户端伪装模式"三选一与说明。
- `ProviderQuotaProgressRow.vue` 支持 claude_code 的 5h / 7d 两条窗口，字段沿用 `provider-quota-windows.md` 的形状。
- `OAuthKeyEditDialog.vue` 显示设备 profile 摘要（只读）与"重置设备身份"按钮。

**验收**：原生 Claude Code 请求经网关后 body 与 header 逐字节不变；第三方请求带 `cch=`、user_id 为 JSON、beta 有序；冻结一份真实 CLI 请求作为 oracle 做差分测试。

---

#### P2 实施记录（2026-09-20）

- **模块**（`crates/aether-provider/transport/src/claude_code/`）：`client_detection.rs`（四信号识别 + `config.cloak.mode`）、`beta.rs`（2.2 有序组装）、`identity.rs`（2.3 设备 profile 与 `metadata.user_id`）、`signing.rs`（2.4 CCH）、`cache_control.rs` 与 `thinking.rs`（2.5）、`cloak.rs`（流水线单入口）。thinking 能力表在 `aether-ai-formats/formats/shared/claude_thinking_capabilities.rs`，`claude_model_uses_adaptive_effort` 已改为委托能力表。
- **顺序**：`cloak.rs::apply_claude_code_cloak_pipeline` 固定为 敏感词混淆（P6 挂钩 `ClaudeCodeCloakPipeline::sensitive_words_hook`，在所有身份改写之前）→ thinking 归一化 → `metadata.user_id` / 计费头兜底 → cache_control → CCH 签名；签名后 body 不再改动。网关侧接线在 `ai_serving/planner/claude_code_cloak.rs`，由 `passthrough/provider/family/request.rs` 在模型映射、body_rules、操作不变量之后调用；P6 的词表解析（`resolve_sensitive_word_list`）已经挂在这个 hook 上，词表为空时 no-op。
- **原生透传**：识别为原生时跳过历史的 body 清洗（thinking 块过滤、计费头版本同步），执行计划走 `body_bytes_b64`，头走 `ClaudeCodeWirePolicy::NativePassthrough`（客户端 UA / x-app / anthropic-* / x-stainless-* 逐字节保留，只换凭据，accept 也不改）。第三方走 `Cloaked { beta_header }`，beta 由最终 body 组装。
- **beta 顺序变更**：`context-1m-2025-08-07` 紧跟 `oauth-2025-04-20` 之后（原生 CLI 顺序），客户端自定义 beta 追加在末尾；两条既有集成测试已按此更新。第三方请求会得到 cache_control 补点（OAuth 凭据 ttl 1h）。
- **report_context**：`claude_code_cloak`（`applied / mode / passthrough / client{kind,signals} / sensitive_words_obfuscation / thinking / identity_rewritten / billing_header_injected / cache_control / cch / session_id / device_profile`）；同时在 `request_body_compatibility_edits` 里记一条 `claude_code_cloak`。
- **设备 profile 持久化**：运行时走 `upstream_metadata.claude_code_device`（CAS 元数据通道，不碰加密 auth_config），`auth_config.device_profile` 作为兼容读取来源；每 Key 每分钟最多写一次；管理端 `POST /api/admin/endpoints/keys/{id}/reset-claude-code-device` 写入 `reset_generation`，下一次请求用新种子派生 device_id。Key payload 新增只读 `claude_code_device_profile` 摘要（device_id 只给前缀）。
- **2.6**：`aether-oauth` 的 `ClaudeCodeProviderOAuthAdapter` 在交换/刷新成功后请求 `/api/oauth/profile` 与 `claude_cli/roles`（尽力而为，失败不影响令牌；网关 `state/exchange.rs` 把补齐字段并入 token payload，`aether-admin state.rs` 合并 `org_name / claude_cli_roles / profile`）；刷新遇 5xx 重试 3 次（500ms 起步），429 抛 `OAuthError::RateLimited { retry_after_secs }`（新增变体，`OAuthHttpResponse.retry_after_secs` 从 `Retry-After` 解析），`oauth_refresh/mod.rs` 按提示退避（封顶 5 分钟）。`quota/dispatch.rs` 注册 `claude_code` 为**被动**刷新：只把 P1 采集的 `upstream_metadata.claude_code.windows` 重新物化为快照，不发上游请求；池适配器 `ClaudeCodePassiveQuotaProviderPoolAdapter` 声明 `quota_refresh`。
- **配置**：供应商 `claude_code_cloak_mode`（写 `config.cloak.mode`，非 claude_code 自动移除；摘要接口回读）。
- **前端**：ProviderFormDialog「客户端伪装模式」三选一（仅 claude_code）；ProviderDetailDrawer 的 claude_code 5h / 7d / 7d_oi 窗口行（复用 ProviderQuotaProgressRow，刷新为被动重物化）；OAuthKeyEditDialog 设备 profile 摘要 + 「重置设备身份」；号池页 claude_code 允许刷新额度。
- **冻结 oracle**：`tests/fixtures/claude_code/native_cli_2_1_161_messages.{headers,body}.json`；差分测试 `tests/ai_execute/claude_code_oracle.rs` 三条：原生逐字节透传、第三方伪装 + CCH 自校验、`cloak.mode=off` 不签名。
- **未做 / 偏离**：`config.cloak.mode=always` 对原生客户端仍透传（原生识别优先，避免破坏真实 CLI 的签名），`always` 的语义是"不区分 Unknown/ThirdParty 都改写"；oracle 是按线上形状固化的合成快照，真实抓包替换时按 README 更新。

## Phase 3：Google 系 onboarding 与版本

### 3.1 onboardUser

`apps/aether-gateway/src/state/integrations.rs:83-238` 的 project 惰性补全：loadCodeAssist 取不到 `cloudaicompanionProject` 时，取 `allowedTiers[isDefault].id` 调 `onboardUser`，轮询 LRO `done` 最多 5 次、间隔 2s，成功后写回 `auth_config.project_id`。Antigravity 与 Gemini CLI 共用，只是 `ideType` 与 UA 不同。参考 `internal/auth/antigravity/auth.go:248-406`。

### 3.2 Antigravity 版本动态化

`transport/src/antigravity/auth.rs:8-9` 的版本改为系统配置 `antigravity.client_version`，后台任务每 6 小时从 hub manifest 拉取，失败保留旧值，硬下限 2.9.1。参考 `internal/misc/antigravity_version.go:18-117`。`build_antigravity_static_identity_headers` 的 `_client_version` 入参真正生效。

### 3.3 claude 分支与输出上限

`antigravity/request.rs`：模型名以 `claude-` 开头时强制 `functionCallingConfig.mode = VALIDATED`；非 claude 删 `maxOutputTokens`；按模型卡截断 `maxOutputTokens`。参考 `antigravity_executor_request.go:62-94`。

**验收**：模拟 loadCodeAssist 无 project → onboardUser 两次轮询后成功；版本拉取失败不影响请求。

#### P3 实施记录（2026-09-20）

- **3.1 onboardUser**：共享实现在 `crates/aether-model-fetch/src/onboarding.rs`（轮询 ≤5 次、间隔 2s、非 2xx 立即失败、`done` 但无 project 立即失败；tier 取 `allowedTiers[isDefault].id` → `currentTier.id` → 客户端兜底）。`transport.rs` 新增 `build_antigravity_onboard_user_plan`（daily 域名）与 `build_gemini_cli_onboard_user_plan`（prod 域名、`coreClientMetadata`）；`strategy.rs` 公开 `hydrate_antigravity_project` / `hydrate_gemini_cli_project`，模型拉取路径同步接入，`upstream_metadata.project_source = "onboard_user"`。网关 `state/integrations.rs` 两条惰性补全改用共享实现，新增 `persist_provider_catalog_key_auth_config_project_id`（解密 → 写 `project_id` → 重新加密 → 凭据 CAS 写回 → 清 transport 缓存；只在 auth_config 已是 JSON 对象时写，CAS 冲突不重试，upstream_metadata 已先落盘所以功能不受影响）。
- **3.2 版本动态化**：`transport/src/antigravity/version.rs`（三段纯数字校验、硬下限 2.9.1、Key 级 `client_version` 覆盖、UA 生成、hub manifest 解析）；后台任务 `maintenance/runtime/antigravity_client_version.rs` 每 6 小时拉取，系统配置键 `antigravity.client_version`，环境变量 `ANTIGRAVITY_CLIENT_VERSION_REFRESH_ENABLED`（默认 true）、`ANTIGRAVITY_HUB_MANIFEST_URL`；任务键 `provider.antigravity.client_version.refresh.worker`，在 `state/core.rs` 紧随预设目录刷新 worker 注册。`build_antigravity_static_client_headers` 的 `client_version` 入参真正生效（合法且 ≥ 2.9.1 才采用），UA 与 `x-client-version` 同源。
- **注意**：内嵌默认值仍是 4.3.0（来自 2026-09-03 上游提交 45c840b8d，来源不明），而 hub manifest 当前返回 2.15.0（CLIProxyAPI 的兜底是 2.9.1）。按计划实现后，首次刷新成功会把出站版本切到 manifest 的值；若确认 4.3.0 才是真实抓包值，把 `ANTIGRAVITY_CLIENT_VERSION_REFRESH_ENABLED=false`。
- **3.3**：`antigravity/request.rs` 对 `claude-*` 模型强制 `toolConfig.functionCallingConfig.mode = VALIDATED`；非 claude 删除 `maxOutputTokens` / `max_output_tokens`（与 CLIProxyAPI 一致，Antigravity IDE 直连的 checkpoint 信封同样适用）；按内置模型卡截断（`build_antigravity_safe_v1internal_request_with_policy` 可传显式上限，旧签名不变）。
- **§7 零测试文件**：`planner/gemini_cli.rs`（5）、`quota/gemini_cli.rs`（4，抽出纯函数 `classify_gemini_cli_quota_refresh_result` / `build_gemini_cli_quota_result_payload`）、`adaptation/private_envelope/sync.rs`（8）。验收用例：`strategy::tests::antigravity_project_hydration_onboards_a_free_tier_account_after_two_polls`、`gemini_cli_project_hydration_onboards_with_the_gemini_cli_identity`、`antigravity_client_version::tests::a_failed_fetch_keeps_the_current_version`。
- **审查后修正（2026-09-21）**：project 惰性补全按 Key 串行化（`state/integrations.rs::PROJECT_HYDRATION_LOCKS`），持锁后重读快照，新账号首批并发请求只发一次 loadCodeAssist / onboardUser。

---

## Phase 4：推理回放缓存与 Codex 清洗

### 4.1 回放账本

新增 `crates/aether-cache` 下的 `reasoning_replay` 模块，按 `(provider_key_id, session_scope, model)` 存最近一轮的：

- Codex：`reasoning.encrypted_content` 与对应 `call_id`，Fernet 形状校验。参考 `codex_executor_reasoning.go:85-120,217-281,760-826`、`internal/signature/gpt_validation.go`。
- Gemini / Antigravity：`thoughtSignature` 与原生 functionCall id。参考 `antigravity_reasoning_replay.go:236-294,1654-1686`。

回放时机：客户端跨格式回传历史丢失签名时按 `call_id` 锚定插回；上游 400 且错误文本含 `signature` 时 CAS 清缓存并降级为占位符。TTL 1 小时、容量 10240，Redis 部署下走现有 Redis 运行时。会话 scope 复用 `client_session_affinity.rs` 的产物。

### 4.2 Codex 清洗

`formats/openai/responses/codex.rs:1563-1598` 扩展：删 `$schema`、剔 `\p{}` 类不支持正则、`strict: true` 且 schema 不满足时降级、工具名 > 64 哈希、`web_search_preview → web_search`、`oneOf const → enum`。input id 补前缀、超长哈希。参考 `helps/codex_tool_schema.go:221-301`、`helps/codex_input_ids.go:29-194`。

### 4.3 模型目录兜底

`crates/aether-model-fetch` 内嵌一份 Codex 模型 JSON 作为拉取失败时的兜底，随版本更新。参考 `internal/registry/codex_client_models_updater.go`。

**验收**：OpenAI Chat 入口走 Codex 上游做三轮 tool-use，第二、三轮带回放；构造含 `$schema` 与 70 字符工具名的 MCP 工具，上游不返回 400。

#### P4 实施记录（2026-09-20）

- **4.1 回放账本**：`crates/aether-cache/src/reasoning_replay.rs`。键 `(provider_key_id, session_scope, model)`，session_scope 复用 `client_session_affinity` 的 session_key，无会话信号时退回 `api_key_id`；TTL 1h、容量 10240、单条目最多 256 条签名（跨轮累积，第三轮同时带前两轮锚点）。Codex `encrypted_content` 过 Fernet 形状校验；Gemini 捕获 `functionCall` 上的 `thoughtSignature`（含 thought part 归属、Antigravity / Gemini CLI `response` 外壳）。只在跨格式（`needs_conversion` 或不同族）时启用；上游 400 且文本含 signature/encrypted_content 时按代次 CAS 清理；Redis 部署走运行时 KV，载荷用目录密钥密封。存储键 `reasoning_replay:{codex|gemini}:{provider_key_id}:{sha256(session_scope\0model)[:32]}`。网关接线 `orchestration/reasoning_replay.rs`：sync / stream / WebSocket 三条路径的候选槽之后回放，effects 层成功/失败钩子捕获与清理；`report_context.reasoning_replay {provider, storage_key, generation, applied, inserted, restored_ids, anchors, skipped, unmatched}`。
- **4.2 Codex 清洗**：`formats/openai/responses/codex_sanitize.rs`。`$schema` 删除、`\p{}` / `\P{}` 正则剥离（仅 schema 关键字位置）、纯 const `oneOf/anyOf` 折叠 enum、strict 不满足时降级、名字 > 64 缩短（`mcp__server__leaf` 先试 `mcp__leaf`，否则 sha256 后缀）并在 `tool_choice` / 历史 `function_call` 同步改名，响应侧四处按原始工具表还原；input id 补前缀（message / function_call / custom_tool_call(_output)）、超长哈希、超长加密推理项丢弃。推理项 id 不改前缀（交给既有严格回放过滤）。
- **4.3 模型目录兜底**：远程预设目录（ba51ac6fb）不含 codex；改为内嵌 `formats/openai/responses/codex_client_models.json`（同步 CLIProxyAPI c93978c 的 8 张卡，保留 Aether 历史的 gpt-5.4 / 5.4-mini / 5.2），启动时校验 slug / id / 字段形状，调用链不变。
- **§7 随机差分**：`aether-cache` 1000 组随机历史断言回放 id 一一对应且幂等；`codex_sanitize` 1000 组随机工具定义断言清洗后无 `$schema` / 坏正则、strict 保留时满足 strict 模式、名字 ≤ 64 且可还原、input id 合规且幂等。三轮 tool-use 集成测试 `tests/ai_execute/stream/codex_reasoning_replay.rs`。
- **§8 按 Key 清缓存**：`POST /api/admin/providers/{provider_id}/pool/clear-reasoning-replay/{key_id}`（route_kind `clear_reasoning_replay`，返回 `{message, cleared}`，带审计）；前端 `clearReasoningReplayCache`，按钮在 OAuthKeyEditDialog（仅 codex / antigravity / gemini_cli / vertex_ai 显示）。
- **未做**：WebSocket 路径只接了请求前回放，其成功捕获经 `apply_local_stream_success_effects` 走通用钩子，未单独写 WS 集成测试；请求详情里 `reasoning_replay` 字段未做可视化（计划 P4 前端列为「否」）。
- **审查后修正（2026-09-20）**：Gemini 系没有会话信号时不再退回 `api_key:{id}` 作用域（`report_context.reasoning_replay.skipped_reason = "no_session_scope"`），Codex 因有 `call_id` 强锚点保留退回；Gemini 条目新增 `args_digest`（`sha256(canonical_json(functionCall.args))` 前 16 位 hex），无 `call_id` 时按 `(name, args_digest)` 锚定，两侧都缺摘要时只在该名唯一时按名回放，不再按名 FIFO 猜；Codex 工具名缩短改为集合级去碰撞（`codex_tool_name_map`，碰撞时递增哈希后缀），请求侧与响应侧从各自工具表按同一规则推导；账本代次改为进程随机种子与计数混合，多实例共享 Redis 不撞车；管理端清缓存分批删除并封顶 100000 个键。
- **已知限制**：`record_level=basic` 时流正文分析上限 5MB，超长响应的终态事件不进入捕获，回放静默失效。

---

## Phase 5：TLS 指纹仿真

### 5.1 现状与可行性

Aether 已经有两个后端：`reqwest_rustls`（默认）与 `browser_wreq`（`execution_runtime/transport.rs:4585-4640`，仅 Grok 使用）。`wreq 6.0.0-rc.28` 基于 BoringSSL，`TlsOptions` 支持 `cipher_list`、`curves_list`、`sigalgs_list`、`alpn_protocols`、`extension_permutation`、`grease_enabled`、`session_ticket` 等；`Emulation` 还能带 `Http2Options` 与 `orig_headers`（请求头顺序）。这意味着不需要引入新 TLS 库，CLIProxyAPI 用 uTLS 做的事在这里用 wreq 自定义 emulation 即可完成。之前"不建议"的判断据此修正为"可做，放在 P5"。

### 5.2 新增 emulation profile

`crates/aether-provider/transport/src/claude_code/tls_profile.rs`：

- `claude_code_node_openssl`：TLS 1.2–1.3，密码套件顺序、曲线、签名算法照 Node/OpenSSL 抓包；ALPN `h2, http/1.1`；不启用 GREASE 与扩展乱序（OpenSSL 不做）；`Http2Options` 的 SETTINGS 与 WINDOW_UPDATE 按 Node 默认值；`orig_headers` 按 Claude Code 实际发出顺序。
- `claude_code_oauth_control_plane`：无 ALPN、HTTP/1.1，头顺序照 Axios。用于 `aether-oauth` 的交换、刷新、profile 请求（当前用 reqwest，`crates/aether-oauth/src/network/executor.rs:73`）。
- `chatgpt_com_chrome`：直接用 `wreq_util::Emulation::Chrome1xx`，`transport.rs:4706` 的映射已存在，只需让 codex 类型的 OAuth 控制面按域名选择。

参考 CLIProxyAPI `internal/auth/claude/utls_transport.go` 的 ClientHelloSpec 作为起点，**但必须用真实 Claude Code 2.1.x 抓包核对**，不能直接照抄：它复刻的是 2.1.220，本地 profile 是 2.1.161，版本升级后 Node 内嵌的 OpenSSL 可能变。

### 5.3 接线

- `network.rs:168-190` 的 `resolve_claude_code_transport_profile`：当供应商或 Key 的 `fingerprint.transport_profile` 指定 `claude_code_node_openssl` 时返回 `backend = browser_wreq`；未指定时保持 `reqwest_rustls`，默认行为不变。
- `execution_runtime/transport.rs:4706` 的 `browser_wreq_emulation_from_profile` 增加自定义 profile 分支。
- 连接池按 Key 隔离（`pool_scope = key` 已是默认）。
- `tls-fingerprint-capture.md` 的 outgoing 记录增加 `tls_stack: boringssl_wreq`、`emulation_profile`；wreq 同样不暴露 ClientHello 字节，`observed` 仍为 false，但要新增一个管理端探针：对 `https://tls.peet.ws/api/all` 一类回显服务发一次请求，把 JA3/JA4 写回 `observed: true`。

### 5.4 前端

- `ProviderFormDialog.vue` 与 `OAuthKeyEditDialog.vue` 增加"传输指纹 profile"下拉（当前前端没有任何 `transport_profile` 编辑入口），选项：系统默认、Claude Code Node/OpenSSL、Chrome。
- 请求详情抽屉的 TLS 事实区显示 `emulation_profile` 与探针结果。

**风险**：wreq 处于 rc 版本，API 可能变；HTTP/2 帧层参数与 TLS 层同属指纹，只做 TLS 不做 H2 等于做了一半；BoringSSL 编译时间长，CI 缓存要调整（见 `ci-performance.md`）。

**验收**：探针回显的 JA4 与真实 Claude Code 抓包一致；关闭 profile 后行为与现在逐字节相同。

#### P5 实施记录（2026-09-21）

- **规格与映射**：`crates/aether-provider/transport/src/claude_code/tls_profile.rs` 只放纯数据规格（`ClaudeCodeTlsEmulationSpec`：版本区间、密码套件、曲线、签名算法、ALPN、扩展顺序、session_ticket / renegotiation / psk_dhe_ke / OCSP / SCT、`http_mode`、按请求类型的头顺序），三个常量 `claude_code_node_openssl`、`claude_code_oauth_control_plane`、`chatgpt_com_chrome`；网关 `execution_runtime/transport.rs::browser_wreq_emulation_from_profile` 把规格映射成 `wreq::Emulation`（`TlsOptions` + `Http1Options` + `OrigHeaderMap`，扩展顺序走 `extension_permutation`），`chatgpt_com_chrome` 直接用 `wreq_util::Emulation::Chrome136`。
- **与计划的偏离（以参考抓包为准）**：推理面 ALPN 只有 `http/1.1`（Claude Code 走 Node fetch/undici 不用 h2），控制面不发 ALPN 扩展；因此 `claude_code_node_openssl` 是 `http1_only`，不需要 H2 帧指纹；控制面 profile 用 `http_mode=auto` 靠不发 ALPN 落到 HTTP/1.1。所有参数取自 CLIProxyAPI 复刻的 2.1.220，本地 profile 是 2.1.161，**未用真实抓包核对**，待核对清单在 `tls-fingerprint-capture.md`。
- **接线**：`network.rs` 内置 profile 表——供应商 `config.fingerprint.transport_profile` 或 Key `fingerprint.transport_profile` 为上述 id 时 `backend = browser_wreq`、`extra.emulation_profile`；未配置时 `resolve_claude_code_transport_profile` 与改动前逐字节相同（测试固化）。控制面：网关本地刷新（`state/oauth.rs`）与 `aether-oauth` 的交换 / 刷新 / profile / roles 请求在供应商或 Key 选了仿真 profile 时用 `claude_code_oauth_control_plane`（codex 用 `chatgpt_com_chrome`），默认仍是 reqwest；cookie 登录流程沿用 `claude_oauth_chrome136` 未动；交换阶段的上下文带供应商 `config`（`state/exchange.rs`）。
- **探针**：`POST /api/admin/endpoints/keys/{id}/tls-probe`（route_kind `tls_probe`）对固定清单（`tls.peet.ws/api/all` 默认、`tls.browserleaks.com/json`；系统配置 `transport.tls_probe_url` 只能选清单项）发一次请求，解析 JA3 / JA3 hash / JA4 / peetprint / http 版本 / 头顺序，CAS 写入 Key `upstream_metadata.tls_probe`；`resolve_transport_profile` 只在探针的 `emulation_profile` 与当前 profile 一致时附到 `extra.tls_probe`，出站记录据此 `observed=true`。
- **出站记录与落库**：`decision_payload.rs` 写 `tls_stack`（`rustls` / `boringssl_wreq`）、`emulation_profile`、按 profile 的 `alpn_offered`、探针命中时的 JA3/JA4；落库白名单（`metadata_policy.rs`、`request_metadata.rs`）从「整体丢弃 tls_fingerprint」改为只投影逐字段校验过的 `outgoing`，`incoming` 继续丢弃。
- **前端**：ProviderFormDialog 与 OAuthKeyEditDialog「传输指纹 profile」下拉（claude_code：系统默认 / Node/OpenSSL / Chrome；codex：系统默认 / Chrome），Key 编辑对话框「探测 TLS 指纹」按钮，请求详情事实区 `TlsTransportFacts.vue`（backend / tls_stack / emulation_profile / http_mode，observed 时显示 JA3 hash 与 JA4）。
- **fixture**：`tests/fixtures/claude_code/tls_probe_synthetic.json` 是合成的（README 标注），探针解析测试用它。

---

## Phase 6：敏感词零宽混淆

### 6.1 后端

新增 `crates/aether-provider/transport/src/sensitive_words.rs`：

- 词表来源：供应商级 `config.cloak.sensitive_words: string[]`，Key 级 `auth_config.cloak_sensitive_words` 覆盖。每词 2–256 个 Unicode 标量、最多 256 条，按 Unicode 简单大小写折叠去重，按长度降序编译一个正则。
- 替换规则：匹配词在第一个 Unicode 标量后插入 `U+200B`，已含零宽的词跳过（幂等）。
- 作用范围：**只处理** claude_code 的 `system[].text`（跳过以 `x-anthropic-billing-header:` 开头的块）与 `messages[].content[].text`；antigravity 的 `request.systemInstruction.parts[].text`。**绝不处理** `tool_use.input`、`tool_result.content`、thinking 文本、`name` 字段、JSON schema。
- 执行顺序：混淆在所有身份改写之前、CCH 签名之前（见 2.4）。
- 只在客户端识别为第三方且 `cloak.mode != off` 时应用；原生 Claude Code 透传。
- `report_context` 写入 `sensitive_words_obfuscation: { applied: true, replaced: N, fields: ["system[0]", "messages[3].content[0]"] }`，与 `provider_request_body` 一起落库。
- 提示词缓存：词表变更会让 prompt cache 失效，写入配置时管理端返回提示。

### 6.2 ⚠️ 前端必做清单（不做前端等于功能不完整，禁止只合后端）

零宽字符对人眼不可见。后端一旦插入，管理端看到的"供应商请求体"与实际发出的字节不一致，排障时会误导，复制出去的 cURL 也带着看不见的字符。以下每一项都是 P6 的验收条件：

| # | 位置 | 改动 | 文件 |
|---|---|---|---|
| F1 | 供应商表单 | 增加"敏感词混淆"分组：词表编辑（逐行输入、去重、长度校验）、仅对 `claude_code` 与 `antigravity` 类型显示、保存时提示会影响提示词缓存 | `frontend/src/features/providers/components/ProviderFormDialog.vue`，类型 `frontend/src/api/endpoints/types/provider.ts`、`frontend/src/api/endpoints/providers.ts`，文案 `frontend/src/i18n/messages.ts` |
| F2 | OAuth Key 编辑 | Key 级词表覆盖，显示"继承供应商 / 覆盖" | `OAuthKeyEditDialog.vue` |
| F3 | 请求详情正文渲染 | 把 `U+200B` 渲染为可见占位符（例如带 tooltip 的 `⟨ZWSP⟩` 或高亮色块），JSON 视图与对话视图都要做；Worker 分段不能把占位符拆到两段 | `RequestDetailDrawer/JsonContent.vue`、`BlockRenderer.vue`、`VirtualBodyContent.vue`、`frontend/src/features/usage/utils/body-document.worker.ts` |
| F4 | 请求详情事实区 | 徽标"已应用敏感词混淆 N 处"，点击列出字段路径；数据来自 `report_context.sensitive_words_obfuscation` | `RequestDetailDrawer.vue`，新增组件与 `ServiceTierFacts.vue` 同级 |
| F5 | 复制正文 / 复制 cURL | 默认复制原始字节并弹出提示"内容含 N 个零宽字符"；提供"去除零宽字符后复制"第二个动作 | `RequestDetailDrawer.vue:651-663` 的 cURL 复制与正文复制动作 |
| F6 | 重放对话框 | 重放会再次经过网关，混淆是幂等的，但要在对话框里说明"重放请求将按当前词表重新混淆" | `ReplayDialog.vue` |
| F7 | 用量搜索 | 服务端对 `provider_request_body` 的文本搜索在匹配前剥离 `U+200B`，前端搜索框提示"混淆后的词按原词搜索" | 后端 usage 查询与 `UsageRecordsTable.vue` |
| F8 | 前端单测 | F3 的占位符渲染、F5 的两种复制、F1 的校验 | `frontend/src/features/usage/components/__tests__/`、`frontend/src/features/providers/__tests__/` |

**验收**：配置词表 `["proxy","API"]` 后，第三方客户端请求经网关，`provider_request_body` 中 `proxy` 变为 `p​roxy`；请求详情中可见占位符与徽标；"去除零宽字符后复制"得到原文；原生 Claude Code 请求不受影响；CCH 签名对混淆后的 body 校验通过。

#### P6 实施记录（2026-09-21）

- **后端模块**：`crates/aether-provider/transport/src/sensitive_words.rs`（词表归一化 ≥2 字符、不区分大小写去重、≤256 条、按长度降序编译单个正则；匹配词第一个 Unicode 标量后插 U+200B，已含零宽的词跳过；作用范围严格按 6.1；`apply_sensitive_word_obfuscation(body, provider_type, word_list) -> SensitiveWordObfuscationReport {applied, replaced, fields}`；`resolve_sensitive_word_list(provider_config, raw_auth_config)`：Key 级 `auth_config.cloak_sensitive_words` 存在即整体覆盖，空数组＝关闭，否则读供应商 `config.cloak.sensitive_words`）。
- **接线**：Claude Code 走 `cloak.rs` 流水线第一步的 `sensitive_words_hook`（先于身份、cache_control、CCH），只在识别为第三方且 `cloak.mode != off` 时应用；Antigravity 收口在 `ai_serving/planner/antigravity_sensitive_words.rs::apply_antigravity_sensitive_words`，信封构建成功后各调用点调用一次（跨格式三条 + 同格式透传），词表非空即应用。报告写入 report_context **顶层** `sensitive_words_obfuscation`（`report_context::insert_sensitive_words_obfuscation_report`，仅 `applied == true`），落库白名单只保留 `{applied, replaced, fields[]}` 且字段路径形状受限。
- **配置接口**：供应商 `cloak_sensitive_words: string[] | null`（写 `config.cloak.sensitive_words`，null / 空数组删键并保留 `cloak.mode`，只允许 claude_code / antigravity）；Key `cloak_sensitive_words` 三态（字段缺席＝不动、`null`＝删除覆盖即继承、数组＝覆盖，空数组＝关闭），写入加密的 `auth_config.cloak_sensitive_words`；Key 载荷回读 `cloak_sensitive_words: string[] | null`。词表实际变化时创建 / 更新响应带 `warnings: ["敏感词词表已变更，提示词缓存将失效"]`。
- **F7 搜索**：`aether-admin` 的 `admin_usage_matches_search` 对关键词与 haystack 两边剥 U+200B；网关 `parse_admin_usage_search_keywords` 进 SQL LIKE 前剥零宽（正文本身不参与 LIKE，只搜 model / provider / username / api_key_name）；前端 `recordSearch.ts` 同样剥离。
- **前端 F1–F8**：ProviderFormDialog「敏感词混淆」分组（逐行编辑、去重、短词 / 零宽 / 256 上限校验并高亮、缓存失效提示、保存后 toast `warnings`）；OAuthKeyEditDialog「继承供应商 / 覆盖」单选 + 编辑器；`ZeroWidthText.vue` 把 U+200B 渲染为带 tooltip 的 `⟨ZWSP⟩`（JSON 视图与对话视图，Worker 分段不拆占位符）；`SensitiveWordObfuscationFacts.vue` 徽标「已应用敏感词混淆 N 处」点击列字段路径；复制正文 / cURL 默认原始字节并提示零宽个数，另有「去除零宽字符后复制」；ReplayDialog 说明重放按当前词表重新混淆；UsageRecordsTable 搜索框提示「混淆后的词按原词搜索」；单测覆盖以上各项。
- **端到端验收**：`tests/ai_execute/sensitive_words.rs` 五条（第三方混淆 + CCH 自校验 + 顶层报告落库、原生逐字节不变、`mode=off` 不混淆、Key 覆盖、Antigravity）。

---

## 7. 测试策略

- **冻结 oracle**：P2 与 P5 各抓一份真实 Claude Code 请求（header 顺序、body、TLS 探针结果）存入 `tests/fixtures/claude_code/`，差分测试逐字节比对。CLIProxyAPI 的 `antigravity_preupstream_rewrite_legacy_oracle_test.go` 是这种做法的样板。
- **随机 fixture 差分**：P4 的 schema 清洗与回放用随机生成的工具定义与历史消息跑 1000 组，断言清洗后 schema 合法且回放 id 一一对应。
- **零测试文件补齐**：`planner/gemini_cli.rs`、`quota/gemini_cli.rs`、`adaptation/private_envelope/sync.rs` 目前没有测试，P3 时补。
- 本机跑测试沿用 `umask 022` 与 `RUST_MIN_STACK=8388608`（见记忆 [网关测试环境前缀]）。

### 收尾审查（2026-09-21）

- **Key 配置与凭据分离**：修改词表仍重新加密 `auth_config`，但凭据变化判定排除 `cloak_sensitive_words`，保留 OAuth 失效标记、错误计数与状态快照。真实 token 变化仍重置运行状态，旧密文损坏也能被替换；CAS 继续比较完整旧密文。Key 的有效词表变化返回缓存失效 `warnings`，前端展示；等效覆盖不提示。
- **词表写入**：供应商切换到不支持混淆的类型时清理旧词表，显式提交不支持的配置仍报错。Key 创建、更新的原始 `auth_config` 与专用字段共用校验。前后端统一每词 2–256 个 Unicode 标量、最多 256 条；`İ` 不被扩展为多个标量，`Σ/ς` 等按正则引擎的简单大小写折叠去重。
- **混淆幂等**：匹配时忽略已有 U+200B，再映射回原文字节范围；已混淆长词保护其内部子词，解决 `["proxy server", "server"]` 重放时继续插字符的问题。原始正文与报告仍只记录本次实际改写。
- **TLS**：配置与运行时共用供应商 profile 白名单；P5 Chrome profile 不再注入浏览器默认头，既有浏览器 cookie 流程保留。节点路线优先并保留 profile，OAuth 刷新同样保留；隧道 worker 当前不支持 wreq，明确拒绝该 backend。探针同时支持 Peet 嵌套字段和 Browserleaks 顶层字段；PeetPrint / Akamai 使用独立的 512 字符上限。
- **被动额度**：事实变化立即 CAS，事实相同时每 60 秒更新观测时间；同一窗口只合并本次携带的字段，新的 reset 清除旧窗口事实。进程内用有界缓存和分片锁记录毫秒观测水位，持久化也保留毫秒时间，阻止旧响应覆盖；缓存最多 50,000 条、30 分钟 TTL。总重置时间只来自仍有效的耗尽窗口。
- **额度顺序边界**：跨实例只能比较已持久化水位；缓存淘汰或进程重启后，未持久化的最新观测无法提供全序保证。保留一分钟心跳以避免每个成功请求都写库并清路由缓存。
- **验证**：网关全目标 5528 项通过、4 项忽略；其余工作区全目标 4434 项通过、28 项忽略。最后补充的空配置修正经 40 项 Key 定向回归验证通过。前端类型检查、全量测试（240 文件、1811 用例）、生产构建和本轮修改文件的 ESLint 均通过；`cargo fmt --all -- --check` 与 `git diff --check` 通过。工作区文档测试命令通过（43 个 crate，当前没有可执行的文档用例）。
- **未通过的全仓检查**：前端全仓 ESLint 扫描仍报 372 个错误、508 个警告，包含未改动文件的既存问题（例如 `views/shared/Usage.vue` 的 `Window` 和 `views/user/Settings.vue` 的未使用导入）；本轮没有执行批量自动修复。
- **外部验收**：P2/P5 fixture 仍是合成样本，本地测试不能替代真实客户端抓包验收。

## 8. 风险与回滚

| 阶段 | 风险 | 回滚 |
|---|---|---|
| P1 | 退避阶梯过激导致 Key 长期不可用 | 供应商级 `cooldown.disable`；池管理页已有"清除冷却" |
| P2 | 伪装策略误判原生客户端 | `cloak.mode = off` 恢复现状；识别结果写入 report_context 便于核对 |
| P4 | 回放缓存脏数据导致持续 400 | 上游 400 含 signature 时自动清；管理端提供按 Key 清缓存 |
| P5 | BoringSSL 构建失败或 wreq rc 变更 | 默认仍是 `reqwest_rustls`，profile 未配置时不进入 wreq 路径 |
| P6 | 混淆破坏签名或工具调用 | 顺序固定在签名前；作用范围白名单；`cloak.mode = off` 全关 |

## 9. 前端改动总清单

按阶段汇总，避免后端合并后前端遗漏：

- **P1**：冷却原因文案映射、绝对截止时间倒计时、冷却策略配置分组。
- **P2**：客户端伪装模式开关、claude_code 5h/7d 额度窗口、设备 profile 摘要与重置。
- **P5**：传输指纹 profile 下拉、TLS 事实区 emulation 与探针结果。
- **P6（⚠️ 必做，8 项，见 §6.2）**：词表编辑、Key 级覆盖、零宽字符可见渲染、混淆徽标、两种复制、重放提示、搜索剥离、单测。

每个阶段的 PR 必须同时包含前端改动或在描述中链接对应前端 PR，缺一不合并。
