# CI 耗时与覆盖范围

普通 CI 保留整个 Rust workspace 的 target 测试和 doctest。Gateway 与其余
workspace 成员在两个标准 runner 上并行执行；每组先用 `cargo test` 的
`--all-targets --no-run --locked --timings` 编译一次，执行阶段复用产物。
不再在前面串行运行覆盖相同目标的 `cargo check`。
`--all-targets` 不包含 doctest，因此文档测试有独立步骤。
统一的 `Rust workspace` 检查负责汇总两组结果，任意一组失败都会阻止通过。

## 构建缓存

- 固定 Rust 1.95，并在缓存步骤前卸载 runner 上未使用的工具链。
  当前 rust-cache 会将所有已安装的 Rust 版本放进缓存键；预装 stable
  的更新不应使本项目的固定版本缓存失效。
- 格式检查先于 rust-cache；只有成功任务保存 Cargo 缓存，避免失败任务
  把没有编译完成的产物固定在同一个不可变缓存键下。
- rust-cache 复用依赖产物，sccache 复用可缓存的编译结果。
  测试程序和链接步骤并非都能被 sccache 缓存。
- CI 使用 4 个 Cargo 构建任务，关闭增量编译及 dev/test 调试信息。
  本地开发保留工作区的调试信息设置，release 配置独立。
- Gateway 的 dev/test 配置使用 256 个代码生成单元，避免关闭增量编译后
  退回默认的 16 个单元。该设置不改变 release 配置。
- 前端的 npm 缓存键同时覆盖主前端与内嵌 VSCodex Web 的 lockfile。

## Docker 发布构建

`Dockerfile.app` 只打包产物；Rust 编译由 `publish-docker.yml` 中两个
`cross` 任务完成。发布任务同样固定工具链、使用四路编译、关闭增量编译，
并通过 rust-cache 保存依赖及工作区库。只构建入镜像的 `aether-gateway`
二进制，省去同包中备份和探测工具的编译链接。release 继续使用 `thin` LTO 和
8 个代码生成单元，不套用 dev/test 的 256 个代码生成单元。

sccache 0.17 的客户端在 cross 容器内执行编译，使用 host 网络连接 runner
上的本地磁盘缓存服务，并在容器内挂载相同缓存路径。`actions/cache` 按目标
架构、工具链和提交归档缓存，每个目标最多保留 2 GiB。批量归档避免了
sccache GHA 后端逐对象写入时遇到的限流丢失问题
（[sccache #2821](https://github.com/mozilla/sccache/issues/2821)）。
必须启用 `SCCACHE_CLIENT_SIDE`，
否则服务会尝试在 host 上访问容器中的编译器路径。
`.github/cross-release.toml` 显式传递构建版本、构建类型和编译包装器，
因为 cross 默认不会传递这些变量。构建摘要记录耗时与内存，
`release-build-timings-*` artifact 和 sccache 统计用于检查缓存效果。

GitHub 缓存有 ref 范围，不同版本 tag 无法直接读取彼此的缓存。发布前可在
默认分支手动运行 `Publish Docker Image`，将 `version` 设置为待发布版本，
例如 `v0.7.19.1`。手动运行只构建并保存缓存，可与 CI 并行；之后的 tag
构建可读取默认分支缓存。首次预热仍需冷编译。只有 tag push 会发布镜像，
并要求同一提交已经通过 CI。

稳定版本发布成功后，版本标签与 `latest` 指向同一个多架构镜像；当前流程
按发布完成的顺序更新 `latest`，不自动比较版本大小。四段版本号可以使用，
例如 `v0.7.19.1`，但不属于严格的三段 SemVer。

签名构建证明继续保存在 GitHub，BuildKit 的证明仍随镜像发布。
不再向 GHCR 单独推送 Sigstore 证明索引，避免出现额外的 `sha256-*` 标签。
发布后只清理内容确认为 Sigstore 证明索引、且所有标签均为该 SHA 格式的
旧 package version；含版本标签、`latest` 或其他别名的条目不会删除。

## 测试数据库

SQLite adapter 的普通单连接内存测试，通过 `test_support::migrated_pool()`
取得独立数据库。每个测试进程只初始化一次迁移模板，随后复制数据库字节，
不共享可写连接池，也不依赖另一个测试的 Tokio runtime。

迁移专项测试，以及文件数据库、多连接和锁竞争测试，继续显式创建数据库并
执行迁移。模板不写入仓库或跨进程缓存，迁移更新后会自动生成新的模板。

## 按变更选择任务

选择逻辑和回归测试在 `.github/scripts/ci_changes.py` 与
`.github/scripts/test_ci_changes.py`。

- Rust 源码、数据库迁移和 Cargo 配置运行 Rust 检查。
- 前端源码和 `frontend/vite.config.ts` 同时运行前后端检查，因为现有 Rust
  架构测试会读取这些文件。
- 前端静态资源及 VSCodex 变更运行前端检查。
- `docs/api/` 文档仍运行 Rust 检查，因为格式契约测试会嵌入 API 文档。
- 根 README 和其他 Markdown 文档可跳过编译任务。
- 未知路径、首次推送、无法取得 diff 和手动触发都执行完整检查。
  可复用工作流默认也执行完整检查，调用方可用 `full: false` 启用路径选择。
- 只有比较基准提交已经有成功的 CI，才允许跳过任务。连续推送取消了前一次
  检查、前一次检查失败，或无法确认基准状态时，都重新运行完整检查。

任务选择失败时，Rust 和前端任务继续运行，防止必需检查被错误跳过。

## 如何比较耗时

查看每次 Actions 的步骤摘要、`rust-build-timings-*` artifact 和 sccache
命中统计。摘要中的内存是每秒采样的构建进程 RSS 之和，包含 sccache 服务
启动的编译器；共享页可能被重复计数，不应当作精确的物理内存峰值。

比较时分别记录准备/缓存恢复、测试编译、测试执行和 doctest，并区分首次
填充缓存与已有缓存的运行。直接在已有 `target` 目录上重复测试，不能代表
新 GitHub runner 的耗时。小于 10 分钟是优化目标，需要实际 CI 验证。

本地复测涉及文件权限的完整测试时，使用与 Ubuntu CI 相同的 `umask 022`。
部分安全测试会主动拒绝组可写的临时目录；`umask 002` 会导致这些测试失败。

## 2026-09-10 本机验证

以下性能对照基于 `86b917d`，记录在该基线上应用本次优化后的验证结果。

在 4 核、约 16 GiB 内存的 Linux 环境中：

| 项目 | 结果 |
| --- | --- |
| SQLite adapter 测试执行对照 | 162.86 秒 → 22.66 秒；新增一个隔离性测试 |
| 其余 workspace 测试 | 4,211 成功、26 忽略；约 100 秒 |
| Gateway 测试 | 5,372 成功、2 忽略；约 146 秒 |
| Gateway doctest | 通过；约 9 秒 |
| 保留依赖和 sccache、移走 Gateway 产物后的重编译 | 约 378 秒；17 个重建产物与已验证版本逐字节一致 |

两组测试合计 9,583 成功、28 忽略，全部 doctest 通过。忽略项沿用原配置。
缓存模拟中的 Gateway 构建、测试、doctest 合计约 8 分 53 秒；这不包含
GitHub 排队、工具安装、缓存传输，以及其他工作区库从 sccache 恢复的开销。
这些数据用于评估优化方向，不是 GitHub 新 runner 的完整 CI 实测。
