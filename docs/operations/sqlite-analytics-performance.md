# SQLite 成本分析与活跃天数优化验证

2026-09-10，在 `86b917db4` 的基础上验证。

## 查询变化

- 成本分析的排行、趋势与供应商统计只需要标量字段，但原查询通过时间索引回表，读取包含请求体的大行；Token 汇总还逐请求读取完整结算快照。
- 新增 SQLite 覆盖索引 `idx_usage_analytics_covering` 和 `idx_usage_settlement_analytics_covering`，分别保存查询所需的统计字段。供应商成功率使用的 `error_message IS NULL` 仅索引布尔表达式，不复制错误文本。
- 全局排行明确使用时间范围覆盖索引，结算查询明确使用结算覆盖索引，避免 SQLite 为分组或主键查找选择需要回表的索引。用户筛选仍允许优化器选择现有用户索引。
- 排行先分组，再按每个 Key/用户查询一次名称。保留现有 Token 优先级、费用口径、筛选条件和删除 Key 的历史记录。
- 活跃天数继续以日汇总为准，只扫描未被汇总覆盖的连续时间段。历史缺口、空汇总、未聚合尾部和部分日期起点均保留原始记录兜底，不使用最大汇总日期截断历史。

迁移文件为 `20260910000000_add_usage_analytics_covering_indexes.sql`，通过现有数据库准备流程创建索引，无须回填新的统计表。变更仅涉及 SQLite 物理索引及其查询实现，没有改变三库共享的数据契约。

## 真实快照验证

通过 SSH 从 DMIT 的 Aether 数据库使用 SQLite online backup API 获取一致性快照；原库以只读方式打开，测试、迁移和写入测量均使用本地副本。

- 数据库约 487.3 MiB，包含 29,794 条使用记录、29,566 条结算快照。
- 记录覆盖 2026-07-17 至 2026-09-10 08:42:55 UTC。
- `usage` 占 251.73 MiB，`usage_settlement_snapshots` 占 80.15 MiB。
- 已有 42 条全局日汇总、43 条用户日汇总；小时 model/provider 汇总为空，因此本次收益不依赖小时汇总回填。
- 正式迁移在副本执行成功，新增索引耗时约 1.65 秒，`PRAGMA quick_check` 返回 `ok`；上述业务表行数保持一致。

以下为同一机器、同一快照、相同连接设置（64 MiB 页缓存、256 MiB mmap）下，直接调用生产 Repository 方法的五次运行中位数。成本相关查询使用快照最后 30 个本地日期、UTC+8；热力图使用最近 365 天。数据来自本地测量，不代表线上 HTTP 端到端延迟。

| 查询 | 优化前 | 优化后 | 加速 |
| --- | ---: | ---: | ---: |
| API Key 用量排行 | 189.75 ms | 53.23 ms | 3.6 倍 |
| 用户排行 | 186.84 ms | 51.85 ms | 3.6 倍 |
| 模型排行 | 131.78 ms | 53.38 ms | 2.5 倍 |
| 成本趋势预测 | 40.70 ms | 25.18 ms | 1.6 倍 |
| 缓存节省 | 1.22 ms | 1.24 ms | 基本持平 |
| 供应商统计 | 200.34 ms | 123.21 ms | 1.6 倍 |
| 总体活跃天数 / 管理员热力图 | 158.63 ms | 4.28 ms | 37.1 倍 |
| 用户热力图 | 3.39 ms | 1.15 ms | 3.0 倍 |

六种时区偏移 `0, 60, 480, -300, 330, 345` 下，八类查询逐字段对比通过：标识、计数、Token、日期和数组顺序精确相同；浮点数仅允许 `max(1e-9, abs(value) * 1e-12)` 的求和舍入误差。

索引大小分别为 6.922 MiB 和 1.863 MiB，合计约数据库的 1.8%。在副本上插入 1,000 条携带 8 KiB 正文的使用记录及对应结算记录，事务内耗时中位数为 19.39 ms → 24.17 ms；每轮回滚。该测量只评估新增索引的写入开销，不包含提交同步或网关的其他写入逻辑。

## 自动验证与复现

SQLite 单元测试 216 项通过；完整数据层运行时测试 430 项通过、1 项忽略，包含新增迁移版本清单、旧库升级和生成 schema 的兼容性检查。新增回归覆盖：

- 保留导入的汇总历史，补齐中间缺口、空汇总和未聚合尾部，正确处理部分日期和用户隔离；无汇总时仍能返回原始记录。
- 结算有效输入、总输入上下文、已记录总 Token、旧缓存 Token 的优先级，以及未完成请求、占位供应商、删除 Key、名称更新和六种时区的半开时间边界。
- 对实际排行 QueryBuilder 执行 `EXPLAIN QUERY PLAN`，要求使用两个覆盖索引，防止退回大行读取。

使用新增的只读基准工具，在修改前的工作树保存基线，在修改后的工作树使用完成迁移的同源副本对比：

```sh
AETHER_ANALYTICS_BENCHMARK_DATABASE_URL=sqlite:///private/before.db \
  cargo run -p aether-data-sqlite --example analytics_query_benchmark -- \
  /private/before.json

AETHER_ANALYTICS_BENCHMARK_DATABASE_URL=sqlite:///private/after.db \
  cargo run -p aether-data-sqlite --example analytics_query_benchmark -- \
  /private/after.json /private/before.json
```

基准工具需复制到修改前的工作树才能生成基线；它使用只读连接，不自动迁移数据库。可通过 `AETHER_ANALYTICS_BENCHMARK_TZ` 和 `AETHER_ANALYTICS_BENCHMARK_ITERATIONS` 更改时区和重复次数。报告含使用标识及名称，应存放在仓库外的私有目录，数据库和原始报告不提交。
