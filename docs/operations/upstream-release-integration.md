# 上游 release 合并与三库兼容

本分支沿用 v0.7.18 合并时的维护边界：以正式 release tag 为合并对象，保留 fork 的 PostgreSQL、MySQL、SQLite 三库支持、月卡配额与恢复逻辑、统计优化、商业模块开关、部署模式和发布工作流。新功能通过共享数据合同及各库适配器提供一致行为。

已发布的 SQL 迁移不改写、不删除，也不复用已有版本号；确有结构变更时新增增量迁移并维护逻辑 schema、生成文件和 bootstrap。上游的 PostgreSQL 专用 SQL 不能直接用于另外两库。

## v0.7.19

合并来源：`fawney19/Aether` 的 `v0.7.19`，提交 `361952ada9e3d01ac180846cee497cd1f55ad1c3`。相对已合入的 v0.7.18 共 7 个提交，涵盖策略级故障转移、断连处理、流式恢复、格式转换诊断、DNS、模型测试及支付订单查询。

- 策略级故障转移和断连选项仍保存在各库现有的路由 JSON 配置中，无需新迁移。默认值及操作说明见 [故障转移](routing-failover.md) 和 [断连策略](client-disconnect-policy.md)。
- 断连后的计费保留本分支的分发时价格快照，避免结算受后续价格、月卡类型或周期调整影响。取消时只保留配置的单次费用及倍率；无单次费用时不扣钱包。`cancelled_request_fee` 随三库审计元数据保存，重放终态及重复结算不重复扣费。
- 流式预读取发现错误时使用上游的重试判断；心跳响应的收尾同时保留本分支的延迟终态及月卡配额处理。
- PostgreSQL 支付订单查询补齐 `payment_provider`、`order_kind`；MySQL、SQLite 已使用包含这些字段的统一查询。修复 MySQL 将二进制排序规则的支付流水号识别为 `VARBINARY` 后无法读取的问题，仍按原列进行大小写敏感的唯一性判断。新增三库运行测试覆盖人工充值、过期、失败、充值与套餐入账、两种兑换码，以及账本、流水号匹配和重复操作保护。
- MySQL、SQLite 的候选诊断回归覆盖新增的阶段、源／目标格式、显示权限和结构化详情，并验证迟到的状态写入不会覆盖原诊断。
- 前端继续使用 RFC3339 时间，保留模型编辑器默认折叠行为，同时接入新的路由策略编辑器。

此合并不新增数据库迁移；合并前的 171 个 SQL 迁移文件逐一保持内容不变。

## 验证入口

常规检查沿用 CI：Rust 格式、全工作区目标检查与测试，前端类型检查、Vitest 和构建。Schema 使用 `bash crates/aether-data/runtime/schema/compose_schema.sh check` 验证。文件权限用例的临时目录需禁止组写；本地终端若使用 `umask 002`，在测试子进程设置 `umask 077` 后运行，不放宽产品的权限校验。

`apps/aether-gateway/tests/multidb_accounting.rs` 使用相同业务场景验证三库。SQLite 用例默认运行；另外两库需要专用临时数据库，测试会自动迁移并写入测试数据：

```bash
AETHER_TEST_POSTGRES_URL='postgresql://…/test_database' \
AETHER_TEST_MYSQL_URL='mysql://…/test_database' \
cargo test --workspace --locked --test multidb_accounting -- --include-ignored --test-threads=1
```

月卡故障后的恢复和后续请求验证位于 `apps/aether-gateway/tests/monthly_quota_failover.rs`，覆盖 13 个 HTTP 场景，包括业务输出前断流可重试、输出后断流不重放，以及无备用渠道的情况。Responses WebSocket 的继续执行、立即取消及取消按次计费使用真实网关与临时 SQLite 验证。数据库适配器中的 `failed_candidate_diagnostics_survive_late_success_and_streaming` 用例验证诊断持久化。

此次合并已完成 Rust 格式与全目标检查、全工作区回归及失败项复测；前端 223 个测试文件的 1,652 项用例、类型检查和构建通过。三库记账实测、MySQL 的 101 项适配器与 19 项运行时检查、PostgreSQL 的 5 项钱包实测、13 个 HTTP 月卡场景及 13 项 Responses WebSocket 用例均通过。

构建前及大型测试间检查磁盘余量；可按包使用 `cargo clean -p …` 删除旧构建产物，保留依赖缓存。临时测试容器用完即清理。
