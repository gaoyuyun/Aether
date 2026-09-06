# SQLite 性能修复计划（时区快路 + 连接调优 + 慢接口跟进）

> 状态：**已实施，2026-09-11 修正历史覆盖范围和时区边界回归**。
>
> 背景：线上仪表盘/统计接口卡顿排查结论（2026-09-07，DMIT 实测）的完整修复方案。

## 回归修正（2026-09-11）

- 升级后的 `stats_hourly_model_provider` 可能只有近期数据。最新汇总时间不能证明此前的历史已回填，旧 `stats_hourly` 的进度也不能证明新表已覆盖历史。SQLite/PostgreSQL 现在依据实际存在的小时桶读取汇总，仅补查未覆盖的连续时间段，再按日期、模型、供应商合并；SQLite 同时保留没有明细的导入日汇总。
- 小时汇总不含用户维度，用户筛选必须使用相应用户的数据；不能读取全站小时汇总。
- 只有完整落在查询范围和同一本地日期内的 UTC 小时才能直接使用。30/45 分钟时区跨午夜的小时、查询两端的不足一小时区间均读取原始记录，以保持半开区间边界。
- PostgreSQL 新小时表的时间字段为整数秒，读取、聚合写入和手动回填工具均按此类型处理。
- 回归覆盖历史未回填、小时中间缺口、已清理明细但保留汇总、导入历史、用户隔离、正负时区及部分小时。基准工具逐字段比较统计结果，不再以“有返回行”作为正确性依据。

以下为原始性能方案；涉及“任意时区都由 24 个整 UTC 小时精确重组”及“旧小时进度自动覆盖新表”的假设，以以上修正规则为准。

## 0. 诊断结论（已实测确认）

| 症状 | 根因 | 实测证据 |
|---|---|---|
| `/api/dashboard/daily-stats` 卡 1.4-1.8s（无响应缓存时 9.7s） | `tz_offset_minutes != 0` 时日汇总快路整体失效，回退 raw 全表 GROUP BY usage（245MB） | 同型 SQL 实测 9.7s vs 快路 38ms，差 250 倍 |
| `/api/dashboard/stats` 卡 1.4s | 同上（`summarize_dashboard_usage` 家族的 UTC 对齐判定） | 同上 |
| `/api/admin/stats/leaderboard/api-keys` 卡 12.8s | 无任何快路，raw + LEFT JOIN 全表 | 实测 4-12.8s |
| 用量页 summary 卡 ~1.5s | raw 聚合 + settlement JOIN | 实测 1.47s |
| 查询期间整机抖动 | SQLite 页缓存仅 2MB（sqlx 默认），479MB 库全靠磁盘；958Mi 内存机器页缓存被反复冲刷 | 磁盘读 450MB/s 仍占查询耗时 70%+；PSI full ≈ 1.2% |

**关键机制**：前端 `useDateRange.ts` 无条件携带用户浏览器时区偏移（`tz_offset_minutes`）。
而非零时区的本地自然日与 `stats_daily*` 表的 UTC 自然日边界不重合，`usage.rs:3410` 的分发逻辑
在 `tz != 0` 时直接放弃快路走 raw 全表扫描。中国用户（+480）与英国用户（+60）全部中招。

**上游同步性**：该债务上游 `fawney19/Aether` v0.7.16（PG-only）同样存在
（`postgres/src/usage/mod.rs:5063/5666/5853` 三处 `tz != 0` 回退 raw）。本计划的 PG 侧修复
未来可作为 PR 反哺上游。

**目标**：不迁 PostgreSQL、不写死任何时区（多时区并存需求：英国 + 中国），保持三库同修的
fork 纪律，把上述接口全部压到 **< 100ms**，消除整机抖动。

---

## Phase 0：SQLite 连接层调优

**改动文件**：`crates/aether-data/adapters/sqlite/src/pool.rs:36-49`（`connect_options()`）

在 `SqliteConnectOptions` 上追加：

```rust
.pragma("cache_size", "-65536")       // 64MB 页缓存（现状 sqlx 默认 2MB）
.pragma("mmap_size", "268435456")     // 256MB mmap 读
.pragma("synchronous", "NORMAL")      // WAL 下安全
```

- 页缓存大小走可配置项：`SqlPoolConfig` 增加 `sqlite_cache_mb` 字段 + 环境变量
  `AETHER_GATEWAY_SQLITE_CACHE_MB`（默认 64，小内存机器可调低）。不写死。
- in-memory 库（`sqlite::memory:`）不应用 mmap/synchronous 调整（沿用现有
  `is_memory` 判断分支）。
- **收益**：所有仍走 raw 的路径（Phase 1/2 完成前的过渡期）立即提速 5-10 倍；热页常驻
  后整机抖动消失。
- **风险**：极低。`synchronous=NORMAL` + WAL 是 SQLite 官方推荐组合（崩溃至多丢最后
  一个事务的 durability，不损完整性）。64MB 常驻占用在 958Mi 机器上可承受。
- **工作量**：0.5 天。
- **不可破坏**：`PoolFactory` 单测（`pool.rs` tests 模块）全部保持通过；新增一条断言
  pragma 生效的连接测试。

---

## Phase 1：时区无关节流——hourly 基座 + 查询重写（核心）

设计原则：**UTC 小时是唯一物化粒度**。任何时区的本地自然日都能由 24 个整 UTC 小时精确
重组（`本地日 [D, D+1) = UTC 小时区间 [D-OffsetSecs, D+1-OffsetSecs)`），因此不需要为
每个时区物化数据，也不写死任何默认时区。支持任意 ±14h 偏移与 30/45 分钟粒度时区
（印度/尼泊尔）。

### 1a. Schema：新增 `stats_hourly_model_provider`（三库）

migration 命名：`20260908000000_add_stats_hourly_model_provider.sql`
（sqlite / postgres / mysql 三份，字段口径一致，类型按各方言）：

```sql
CREATE TABLE stats_hourly_model_provider (
    id TEXT PRIMARY KEY,
    hour_utc INTEGER NOT NULL,
    model TEXT NOT NULL,
    provider_name TEXT NOT NULL,
    total_requests INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    total_cost REAL NOT NULL DEFAULT 0,
    settled_total_cost REAL NOT NULL DEFAULT 0,   -- 对齐 settlement 口径
    response_time_sum_ms REAL NOT NULL DEFAULT 0,
    response_time_samples INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (hour_utc, model, provider_name)
);
```

**为什么新建表**（已核对现有 schema，非过度设计）：
- 现有 `stats_hourly_model`（仅 model）与 `stats_hourly_provider`（仅 provider）是分离维度，
  无法重组出 daily breakdown 需要的 model × provider 组合。
- 现有 hourly 家族缺 `response_time_sum_ms` / `settled_total_cost` 等字段（daily 变体有，
  hourly 没有）——daily breakdown 的 `total_cost_usd` 取
  `COALESCE(settlement.billing_total_cost_usd, usage.total_cost_usd)`，需要 settled 口径。
- 字段集对齐现有 `stats_daily_model_provider`（线上已存在，见
  `20260725020000_add_advanced_stats_parity.sql`），便于 Phase 4 的数值 equality 测试比对。

**同步项**：`crates/aether-data/schema` 的 bootstrap baseline（postgres 侧）
`generated/postgres/baseline/*.sql` 与 manifest 需追加该表（fork 惯例：快照迁移三库同修，
参考 `96565db38` 的 schema 改动面）。

### 1b. 聚合 worker 扩展

**改动文件**：`crates/aether-data/runtime/src/backend/stats/sqlite.rs`
（+ `stats/postgres_daily/*`、`stats/mysql.rs` 对应实现）

- `perform_sqlite_stats_hourly_aggregation` 增加 `INSERT INTO stats_hourly_model_provider`
  分支（模式照抄同文件 `stats_hourly_model` 的 `L487` 写入分支，ON CONFLICT upsert）。
- 回填机制沿用现有 `next_sqlite_stats_hourly_bucket()`（`L53`）的追赶逻辑：从最后一个
  `is_complete` 小时继续补。worker 每小时运行（`schedule.rs:156`
  `next_stats_hourly_aggregation_run_after`），线上 hourly 历史与 usage 同起点（2026-07-17），
  上线后自动追赶，无需人工干预。
- 兜底冷启动工具：新增
  `crates/aether-data/adapters/sqlite/examples/recover_hourly_model_provider.rs`，
  对指定时间范围从 raw 重建（模式照抄现有 `examples/recover_provider_quota.rs`，Postgres
  侧同步一份）。用于 hourly 有洞、或新装部署想立刻预热快路的场景。

### 1c. 查询重写（核心中的核心）

**改动文件**：`crates/aether-data/adapters/sqlite/src/usage.rs:3402-3500`
（`list_dashboard_daily_breakdown`）及同文件 `summarize_dashboard_usage` 家族（`L2414` 起）

现逻辑（`usage.rs:3410`）：

```rust
if query.tz_offset_minutes != 0 {
    let raw_rows = self.list_dashboard_daily_breakdown_raw(query).await?; // ← 全表扫描
    ...
}
```

新逻辑（任意 tz 统一走快路）：

```
本地日 [D, D+1) 切分为两段：
  ① 已聚合段（hour_utc < 最后一个 is_complete 小时末尾）：
     SELECT date((hour_utc + :offset_secs) / 86400, 'unixepoch') AS date,
            model, provider_name,
            SUM(total_requests),
            SUM(input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens),
            SUM(settled_total_cost),
            SUM(response_time_sum_ms), SUM(response_time_samples)
     FROM stats_hourly_model_provider
     WHERE hour_utc >= :aligned_from AND hour_utc < :aggregated_until
     GROUP BY date, model, provider_name
  ② 未聚合尾部（最后一个 complete 小时之后，最多 ~2 小时窗口）：
     原有 raw 查询，但 WHERE 范围限定在尾部区间，配合 Phase 0 缓存为毫秒级
```

**回退保障**：若 hourly 数据存在洞（`aggregated_until <= aligned_from`，例如刚迁移的新
部署），自动回退现有 `list_dashboard_daily_breakdown_raw` 全量路径——**行为不劣于现状**。
`tz == 0` 的旧快路（`stats_daily*`）原样保留，零破坏。

**同样模式顺次修复**（同一次提交内）：
1. `summarize_dashboard_usage` 家族（`L2414` 起）：user 维度总计走现有 `stats_hourly_user`
   （字段已足够，已核对线上 schema）；`/api/dashboard/stats` 的总量数字。
2. `list_dashboard_daily_breakdown_from_daily_totals` / `_from_daily_aggregates` 中
   `date_expr` 的 UTC 对齐段——保留不动（tz==0 快路），仅时区分支改走 ①+②。

### 1d. 时区语义（多时区需求）

- **前端/契约零改动**：`tz_offset_minutes` 继续由用户浏览器逐请求传递
  （`frontend/src/features/usage/composables/useDateRange.ts` 现状保留）。
- 不写死时区、不缓存"某时区的结果"；英国（+60/+0）与中国（+480）用户各取所需，
  同一份 UTC 小时数据不同组合，互不干扰，零冗余存储。
- 保留"时间是绝对存储、时区是呈现层概念"的架构边界。

### Phase 1 验收标准

| 接口（tz=480，52 天 / 29k 行数据集） | 现状 | 目标 |
|---|---|---|
| `/api/dashboard/daily-stats` | 9.7s（无缓存） | **< 100ms** |
| `/api/dashboard/stats` | 1.4s | **< 100ms** |
| 数值正确性 | — | 新快路与 raw 路径数值 equality（测试断言，见 Phase 4） |

**工作量**：2 天（含三库同修 + 测试）。

---

## Phase 2：其余慢接口跟进

1. **`/api/admin/stats/leaderboard/api-keys`（12.8s → < 100ms）**
   - 改动：`summarize_usage_leaderboard`（`usage.rs:4408` 附近）。
   - 现状核对：hourly 家族没有 api_key 维度变体。**方案：本期暂不新增表**，
     leaderboard 限管理员使用且访问频率低，接受 raw + Phase 0 页缓存加速（预计
     12.8s → ~1.5s）。若上线后仍不可接受，再立后续项补 `stats_hourly_api_key`。
2. **用量页 `summarize_usage_audits`（1.5s）**
   - 套用 ①+② 模式：已聚合段读小时表，尾部 raw。`LEFT JOIN
     usage_settlement_snapshots` 的成本总量改为 hourly 的 `settled_total_cost` 口径。
3. **`/api/admin/stats/cost/*`（1.5s）**
   - 已有部分快路（`sqlite_cost_savings_aggregate_table`，fork 提交 `26256927a`）。
     检查 `read_cost_savings_daily_cutoff_unix_secs` 在 `tz != 0` 时的行为，套用同一
     小时重组模式补齐时区分支。

**工作量**：1 天。

---

## Phase 3：数据与运维配套（DMIT 服务器侧）

1. **保留策略收紧**：`usage_http_audits`（78MB/52 天）与 `usage_settlement_snapshots`
   （76MB）确认走现有 `run_audit_cleanup_once` / `run_usage_cleanup_once` 的保留天数
   配置并收紧到 90 天（预计回收 ~30% 库体积）。**用已有 worker，不造新轮子**。
2. 新镜像部署后执行一次 `PRAGMA wal_checkpoint(TRUNCATE)`，低峰期 `VACUUM` 一次
   （479MB 库、20G 盘，VACUUM 需与库等大的临时空间，当前余量充足）。
3. 部署时顺带更新线上镜像：当前 `ghcr.io/gaoyuyun/aether:beta` 构建于 2026-08-19，
   不含 `26256927a`（2026-09-06）的 cost-savings 快路。本计划发布时 fork/dev 一并带上。
4. （可选，非必需）内存升级 2GB 档，给 Phase 0 的 64MB 页缓存更舒适水位。

**工作量**：0.5 天（验证与执行）。

---

## Phase 4：测试与验收（贯穿各 Phase）

### 测试要求

- **数值 equality 对测**：每条新快路与对应 raw 路径，同数据集结果必须逐字段相等。
  复用/扩展现有 `crates/aether-data/adapters/sqlite/src/usage/tests.rs` 的上海时区边界
  回归测试（`26256927a` 引入的测试模式）。
- **时区矩阵**：`tz_offset_minutes ∈ {0, 60, 480, -300, 330}` 全部断言（覆盖 UTC、英国
  夏令时、中国、美东、印度 30 分钟粒度）。
- **边界 cases**：hourly 有洞时回退 raw（行为不劣于现状）；跨本地日边界的请求归属；
  未聚合尾部的今日数据；`is_complete` 小时的追赶幂等性（重复跑 worker 不重复计数）。
- **基准**：`crates/aether-testing/loadtools` 增加可复现脚本（52 天 × 3 万行模拟数据），
  输出 before/after 对比表，作为本计划的验收附件记录在 PR 描述中。
- **三库纪律**：Phase 1a/1b/1c/2 每一步 sqlite → postgres → mysql 同步落地。PG 侧顺带修
  `postgres/src/usage/mod.rs:5063/5666/5853` 三处上游同源回退。

### 收尾提交纪律（重要，务必按此执行）

本计划全部实现并验证通过后，**最终提交不新建独立 commit，而是合并（squash）进本地
未推送的 `26256927a perf(frontend): 优化前端性能`**。原因：该提交正是本次排查发现
"快路时区失效"的源头提交（cost-savings 快路同样存在 tz 分支缺口），本次修复属于同一
主题的完整性收尾，合并后历史更干净。

操作方式（注意 `26256927a` **不是 HEAD**，其上还有 3 个本地提交，不能直接
`git commit --amend`）：

```bash
# 1. 正常在分支上开发、分多个 WIP commit 均可
# 2. 全部验证通过后：
git rebase -i 26256927a^
# 3. 在 TODO 列表里：
#    - pick 26256927a → 改为 reword（见下方新提交信息）
#    - 本计划产生的所有 WIP commit → 全部改为 fixup（合并进 26256927a）
#    - aeafbddbe / f7edb65bb / 3fe4a0eab 保持 pick 不动
# 4. reword 时更新提交信息，覆盖合并后的完整范围：
```

合并后的提交信息（reword 后）：

```
perf(data): SQLite 时区快路与连接层调优，修复统计接口全表扫描

- 仪表盘/统计接口在非 UTC 时区（tz_offset_minutes != 0）时快路整体失效，
  回退 raw 全表 GROUP BY（实测 9.7s vs 快路 38ms）。新增 stats_hourly_model_provider
  小时级基座（三库），任意时区本地日由 UTC 小时精确重组，支持 ±14h 与 30/45 分钟
  粒度时区，未聚合尾部限制在最后 complete 小时之后。
- summarize_dashboard_usage 家族与 daily breakdown 同步走 hourly 快路，
  hourly 有洞时自动回退 raw（行为不劣于现状）。
- SQLite 连接增加 cache_size/mmap_size/synchronous=NORMAL pragma，
  页缓存 2MB → 64MB（可配 AETHER_GATEWAY_SQLITE_CACHE_MB）。
- 用量页 summary、cost/* 时区分支套用同一模式；leaderboard 接受页缓存加速。
- 前端首屏加载优化（原 26256927a 内容保持不变：使用记录页/仪表盘首屏、
  SQLite 缓存节省统计日汇总表、图表异步加载等）。
```

**安全前提**（已核对，2026-09-07）：`26256927a` 属于 `origin/fork/dev..HEAD` 的 10 个本地
未推送提交之一，从未推送远端，squash 重写安全。若执行时发现该提交已被推送（例如期间
执行过 `git push`），**停止 rebase**，改为普通追加 commit，并在 PR 描述注明。

---

## 明确不做的事

| 不做 | 理由 |
|---|---|
| 迁移 PostgreSQL | 卡顿根源是查询路径不是引擎；958Mi 内存装不下 PG；修复后无性能遗憾 |
| 写死 UTC+8 或任何默认时区 | 多时区并存需求（英国 + 中国），时区必须是呈现层概念 |
| 物化多时区日表 | 写放大 ×N、时区集合写死，与需求矛盾 |
| 修 `test-model-failover`（125s）/ `batch/balance`（2-6s） | 非数据库问题（同步等上游 HTTP），属业务层异步化改造，另立提案 |
| 修改 `stats_daily*` 现有 UTC 语义 | 零破坏原则，只加不改 |
| 本期新增 `stats_hourly_api_key` | leaderboard 访问频率低，先接受 raw + 页缓存（~1.5s），观察后再立项 |

## 总投入与节奏

| Phase | 内容 | 工作量 | 可独立发布 |
|---|---|---|---|
| 0 | pragma 调优 | 0.5 天 | ✅ |
| 1 | hourly 基座 + 时区快路 | 2 天 | ✅ |
| 2 | leaderboard/audits summary/cost | 1 天 | ✅ |
| 3 | 运维清理 + 部署 | 0.5 天 | — |
| 4 | 测试（随 Phase 走） | — | — |

**合计约 4 个工作日**。风险最大的是 Phase 1c 查询重写，两道保险：raw 兜底回退 +
数值 equality 对测。schema 变更纯增量（新表 + 新 worker 分支），最坏情况是快路不生效、
行为退回现状。

## 发布策略

**按用户决定：先不发布。** 全部 Phase 完成、测试通过、squash 进 `26256927a` 之后，
停驻在本地分支（`fork/dev`），发布时机另行决定。发布时引用本路径：
`docs/operations/sqlite-performance-fix-plan.md` 作为变更依据。
