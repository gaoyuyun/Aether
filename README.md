<p align="center">
  <img src="frontend/public/aether_adaptive.svg" width="120" height="120" alt="Aether Logo">
</p>

<h1 align="center">Aether</h1>

<p align="center">
  <strong>一站式 AI 基础设施平台</strong><br>
  支持 Claude / OpenAI / Gemini 及其 CLI 客户端的统一接入、格式转换、正/反向代理, 致力于成为用户驱动AI服务的底座
</p>
<p align="center">
  <a href="#简介">简介</a> •
  <a href="#部署">部署</a> •
  <a href="#api-文档">API 文档</a> •
  <a href="#环境变量">环境变量</a> •
  <a href="#qa">Q&A</a>
</p>


---

## 简介

Aether 是一个自托管的 AI API 网关，为团队和个人提供多租户管理、智能负载均衡、成本配额控制和健康监控能力。通过统一的 API 入口，可以无缝对接 Claude、OpenAI、Gemini 等主流 AI 服务及其 CLI 工具。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/architecture/architecture-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/architecture/architecture-light.svg">
    <img src="docs/architecture/architecture-light.svg" width="680" alt="Aether Architecture">
  </picture>
</p>

页面预览: https://fawney19.github.io/Aether/

## 部署

### Docker Compose（推荐：预构建镜像）

```bash
# 1. 克隆代码
git clone https://github.com/fawney19/Aether.git
cd Aether

# 2. 配置环境变量
cp .env.example .env
# .env 包含数据库、JWT 和数据加密密钥，先限制为仅当前用户可读写
chmod 600 .env
# 生成 JWT / 加密 / Postgres / Redis / MySQL 独立随机密钥，并填入 .env
./generate_keys.sh
# 编辑 .env 设置 ADMIN_PASSWORD

# 3. 首次部署 / 更新 (从以下部署形态任选其一)
# Postgres + Redis (推荐)
docker compose pull && docker compose up -d
# Single Node：默认容器身份为 65532:65532，先停止旧容器并检查/迁移 SQLite bind 目录
docker compose -f docker-compose.single-node.yml stop app
mkdir -p data
test -z "$(find data ! -type d ! -type f -print -quit)" || { echo "data 中存在 symlink/FIFO/socket/device，拒绝迁移" >&2; exit 1; }
test -z "$(find data -type f -links +1 -print -quit)" || { echo "data 中存在硬链接，拒绝迁移" >&2; exit 1; }
sudo chown -R -P 65532:65532 ./data
sudo find data -type d -exec chmod 0700 {} +
sudo find data -type f -exec chmod 0600 {} +
docker compose -f docker-compose.single-node.yml pull && docker compose -f docker-compose.single-node.yml up -d
```

应用镜像默认以固定非 root 身份 `65532:65532` 运行；Compose 同时移除全部 Linux capabilities、禁止提权、启用只读根文件系统，并仅提供带 `nosuid,nodev,noexec` 的 `/tmp` 临时文件系统。若宿主机不适合使用固定 UID/GID，可在 `.env` 中把 `AETHER_CONTAINER_UID` / `AETHER_CONTAINER_GID` 改成其他非零数字身份，并让 Single Node 的 `./data` 归该身份所有。`install.sh --mode compose-single-node` 会按安装用户自动生成这两个值；使用 `sudo` 运行时会采用原调用用户身份，并安全迁移已有 SQLite 数据。

从旧版 root 容器升级 Single Node 时，必须先停止旧 `app` 容器，再在第一次启动新版 Compose 前完成一次数据目录迁移；安装器检测到容器仍在运行会拒绝迁移，避免并发改写造成检查竞态。停止容器后用 `sudo` 重新执行一键安装器会自动处理；非 root 安装器发现旧数据所有权不匹配时会拒绝启动并提示迁移，不会放宽目录权限。手工部署且仍使用默认身份时执行上面的检查、`chown` 和 `find ... chmod` 命令即可。迁移只改变 `./data` 的所有权和权限，不会删除数据库、WAL 或备份文件。

### 一键更新

Docker Compose 部署后，可在部署目录直接执行：

```bash
./update.sh
```

`update.sh` 会拉取最新 `app` 镜像并重建 `app` 容器，Docker named volumes、`./data` 和 `./logs` 不会被删除。Single Node 部署也可显式指定：

```bash
./update.sh --mode single-node
```

仓库自带的 Docker Compose 默认把应用日志输出到容器 `stdout/stderr`，直接用 `docker compose logs -f app` 查看，并由 Docker 轮转日志，避免非 root 用户被宿主机日志目录权限拖垮启动。如果你确实需要文件日志，需要在 compose 里把 `AETHER_LOG_DESTINATION` 改成 `file|both`，额外挂载目录到 `/opt/aether/logs`，并让它归 `.env` 中配置的容器 UID/GID 所有；只读根文件系统不会阻止显式可写挂载。

管理后台右上角“版本信息”会检测新版本。Docker Compose 部署只提示版本，实际更新继续执行 `./update.sh`；systemd / launchd / 二进制部署才使用后台自更新，流程是下载对应平台的 GitHub Release 包、强制校验 `SHA256SUMS`、解压到 `/opt/aether/releases/<version>`，再切换 `/opt/aether/current` 并退出进程，交给 systemd / launchd 拉起新版本。

正式 Release 还会发布由 GitHub Actions OIDC / Sigstore 签发的 SLSA build provenance。需要验证发布者身份时，下载目标 tarball 和 `AETHER_RELEASE_PROVENANCE.sigstore.json`，并把 `TAG` 设置为对应 Release tag：

```bash
gh attestation verify "aether-${TAG}-linux-amd64.tar.gz" \
  --repo fawney19/Aether \
  --signer-workflow fawney19/Aether/.github/workflows/release.yml \
  --source-ref "refs/tags/${TAG}" \
  --bundle AETHER_RELEASE_PROVENANCE.sigstore.json
```

`docker-compose.yml` 中的官方 PostgreSQL、Redis 和 MySQL 镜像均固定到多架构 OCI index digest。升级这些依赖时应在发布变更中显式更新 digest，避免同名 tag 在无人审查的情况下改变部署内容。

正式发布到 GHCR 和 Docker Hub 的多架构 Aether 镜像也带有同一 GitHub Actions OIDC / Sigstore provenance；生产 `Dockerfile.app` 的 BusyBox 与 Distroless 基础镜像同样固定到多架构 OCI index digest。

源码或本地构建版本不会启用后台在线更新，请继续使用源码更新流程。Docker Compose 用户如果希望“容器重建后也保持镜像层面的新版本”，仍建议定期运行 `./update.sh` 拉取并重建 app 镜像。服务器访问 GitHub 需要代理时，可设置 `AETHER_UPDATE_PROXY_URL`，也兼容 `UPDATE_PROXY_URL`、`HTTPS_PROXY`、`ALL_PROXY`、`HTTP_PROXY` 以及 `NO_PROXY`。共享出口触发 GitHub API 限流时，可设置只读 `AETHER_UPDATE_GITHUB_TOKEN`，也兼容 `GITHUB_TOKEN` / `GH_TOKEN`。下载总超时默认 600 秒，连续无响应/无数据默认 30 秒，可通过 `AETHER_UPDATE_DOWNLOAD_TIMEOUT_SECS` 和 `AETHER_UPDATE_DOWNLOAD_IDLE_TIMEOUT_SECS` 调整。

标准 Docker Compose 使用 Docker named volumes 存放 Postgres/Redis/MySQL 数据；Single Node 使用部署目录下的 `./data` 存放 SQLite 数据。

### 商业模块开关运维约定

钱包和套餐开关面向个人及小范围单节点部署，按维护操作处理，不支持在持续业务流量下热切换。变更前应先停止入口流量并等待在途请求结束，变更完成后重启应用再恢复流量。若明确选择不重启，应在最后一次变更成功后继续保持流量停止至少 60 秒。多实例部署需要停止并重启全部应用实例。

详细步骤和缓存边界见 [商业模块维护手册](docs/operations/commerce-modules-runbook.md)。

如果是本地源码构建镜像的部署，继续使用：

```bash
./deploy.sh
```

如果要在本机联调“管理后台在线更新”本身，可启动仓库内置的 release-layout 测试环境：

```bash
docker compose -f docker-compose.release-local.yml up -d --build
```

这套环境会用当前源码构建一个本地测试镜像，但编译为 `release` 类型，并默认伪装成 `v0.7.0`，这样后台会按正式发布版逻辑开放“立即更新”。默认监听 `http://127.0.0.1:18085`，数据目录使用 `./data-release-local`；日志默认走 `docker logs`，不会影响你正在跑的源码构建容器。

如果这套容器在 `prepare-update` 时访问 GitHub 失败，而你本机是通过代理出网，请在 `.env` 里把 `AETHER_UPDATE_PROXY_URL` 写成宿主机地址，例如 `http://host.docker.internal:7890`；容器内的 `127.0.0.1` 指向容器自身，不是宿主机。

如果想重置这套联调环境（包括 `/opt/aether/current` 和已下载的历史版本），执行：

```bash
docker compose -f docker-compose.release-local.yml down -v
```

可选变量：

- `AETHER_RELEASE_LOCAL_VERSION`：本地联调镜像对外声明的当前版本，默认 `v0.7.0`
- `AETHER_RELEASE_LOCAL_PORT`：本地联调端口，默认 `18085`
- `LOCAL_RELEASE_APP_IMAGE`：本地联调镜像名，默认 `aether-app:release-local`

### 一键安装（默认 Single Node：Linux systemd / macOS launchd + SQLite）

```bash
git clone https://github.com/fawney19/Aether.git
cd Aether
curl -fsSL https://raw.githubusercontent.com/fawney19/Aether/main/install.sh | sudo bash
```

### Nightly（每日 main 构建）

Nightly workflow 每天从 `main` 的固定 commit 构建并发布滚动的 GitHub Release `nightly`，同时推送多架构 GHCR 镜像 `ghcr.io/fawney19/aether:nightly`。Nightly 是预发布版本，适合验证最新代码，不保证与正式版相同的稳定性。滚动 Release 需要仓库保持关闭 GitHub Release immutability。

安装最新 nightly（Linux systemd / macOS launchd + SQLite）：

```bash
curl -fsSL https://raw.githubusercontent.com/fawney19/Aether/main/install.sh | sudo bash -s -- --channel nightly
```

Docker Compose 用户可在部署目录的 `.env` 中设置 `APP_IMAGE=ghcr.io/fawney19/aether:nightly`，然后运行 `./update.sh` 获取下一次 nightly。二进制方式可重新执行上述安装命令升级；当前管理后台的在线更新列表只跟踪正式版/RC/Beta，不会自动提示下一次 nightly。

## 本地开发

依赖 Docker、Rust toolchain、Node.js 和 make。
首次启动前需要在 `.env` 中设置 `ADMIN_PASSWORD`，用于创建本地管理员。

```bash
make dev
```

`make dev` 会同时启动后端 `aether-gateway` 和前端 `frontend` 的 Vite dev server。需要单独启动时可使用 `make dev-backend` 或 `make dev-frontend`。
Postgres / Redis 本地依赖未就绪时，`make dev` 会自动执行 `docker compose up -d postgres redis`。
`make dev` 会先完成后端编译，再开始计算服务健康检查超时。数据库 schema 和必要的派生数据准备也会在启动时自动完成；通常不需要手动区分 migration 与 backfill。升级不会主动重写或清除已有业务历史记录，新写入会直接遵循当前的数据持久化策略。排查或部署前预执行时可使用：

```bash
make db-status
make db-prepare
```

## Codex 远程协同

`aether-vscodex/` 是独立的 VS Code Codex 协同模块：同步模式跟随 VS Code 官方 Codex 面板当前会话且不另起进程；异步模式使用独立 app-server，让浏览器自行列出、恢复、新建和切换会话。两种模式都能从本机 URL 或 Aether 云端查看输出、发送消息和处理授权，模块内的 Vue 前端提供中英文界面。

安装、云端配对和安全边界请参阅 [`aether-vscodex/README.md`](aether-vscodex/README.md)。

## Aether Tunnel (可选)

Aether Tunnel 是配套的正向代理节点，部署在海外 VPS 上，为墙内的 Aether 实例中转 API 流量。

- Docker Compose 部署或下载预编译二进制直接运行
- 提供 macOS/Linux 与 Windows 一键脚本，自动下载最新 `tunnel-v*` 制品并向现有 `aether-tunnel.toml` 追加 `[[servers]]`
- 通过 `aether-tunnel setup` 完成交互式配置，自动注册为系统服务
- 详细文档见 [apps/aether-tunnel/README.md](apps/aether-tunnel/README.md)

## API 文档

- Embeddings: [OpenAI compatible `POST /v1/embeddings`](docs/api/embeddings.md)
- Rerank: [OpenAI/Jina compatible `POST /v1/rerank`](docs/api/rerank.md)
- Responses WebSocket mode: [protocol and Aether behavior](docs/WebSocket-Mode.md)
- WebSocket probes: [Codex](docs/operations/codex-responses-websocket-probe.md) · [OpenAI Responses](docs/operations/openai-responses-websocket-probe.md)

## 环境变量

- `APP_PORT`：`aether-gateway` 唯一监听端口，固定绑定 `0.0.0.0:${APP_PORT}`
- `DATABASE_URL`：数据库连接串；SQLite 例如 `sqlite:///opt/aether/data/aether.db`，Postgres 例如 `postgresql://postgres:aether@postgres:5432/aether`
- `AETHER_GATEWAY_DATA_POSTGRES_MIN_CONNECTIONS` / `AETHER_GATEWAY_DATA_POSTGRES_MAX_CONNECTIONS`：数据库连接池手动覆盖值；未配置时 SQLite 固定 `1/1`，Postgres/MySQL 按每核 `4` 条自动推导，总池范围为 `32-100`。该预算按进程计算，多实例部署应按数据库连接上限显式分配
- `AETHER_GATEWAY_MAX_IN_FLIGHT_REQUESTS`：单实例请求并发上限；未配置时按 CPU 自动推导（基础范围 `512-65536`），低文件描述符预算时会进一步下调
- `AETHER_GATEWAY_REQUEST_BODY_BUFFER_BUDGET_MB`：单实例同时读取和解压请求体的加权内存预算，默认 `256MB`
- `AETHER_GATEWAY_REQUEST_BODY_READ_TIMEOUT_MS`：可选的请求体完整读取超时；默认或显式设为 `0` 时关闭，非零值限制在 `1000-600000ms`
- `AETHER_MAX_REQUEST_BODY_MB`：单请求解压后请求体上限，默认 `256MB`；显式设为 `0` 表示不再收紧默认值，但仍受 `256MB` 安全硬上限约束
- `AETHER_MAX_INTERNAL_BUFFERED_BODY_MB`：heartbeat、管理探测等内部整包响应体上限，默认 `64MB`；显式设为 `0` 表示不再收紧默认值，但仍受 `256MB` 安全硬上限约束
- `AETHER_TUNNEL_NODE_STATUS_QUEUE_CAPACITY`：隧道节点状态上报队列容量，默认 `1024`；满载时拒绝新事件，避免控制面故障导致无界内存增长
- `AETHER_TUNNEL_RELAY_ALLOW_PRIVATE_TARGETS`：跨网关 owner relay 解析到私有/保留地址时的显式运维开关，默认关闭；仅当多网关 relay URL 是受控的内网 HTTPS 地址时设置为 `true`。它不改变普通 provider 请求的 DNS/代理策略，也不允许明文 HTTP 非 loopback relay
- `AETHER_TUNNEL_RELAY_PRIVATE_HOST_ALLOWLIST`：更窄的 owner relay 私网例外，填写逗号分隔的精确主机名（例如 `gateway-a.internal,gateway-b.internal`，忽略大小写和末尾点）；仅这些主机解析出的私有地址会被允许，并且请求仍使用解析后地址 pin。不要填写通配符或 `.internal` 这类后缀
- `AETHER_INTERNAL_GATEWAY_AUTH_SECRET`：旧版 `/api/internal/gateway/*` 高权限控制面的独立 HMAC 密钥，至少 `32` 字节；未配置时该控制面返回 `404`。不要复用 JWT、数据加密或 tunnel relay 密钥，多节点必须使用同一值及共享 Redis 防重放
- `AETHER_GATEWAY_SECURITY_CACHE_TTL_MS`：IP 黑白名单本地缓存时间，默认 `1000ms`，写操作会主动失效相关缓存
- `AETHER_MAX_REDACTED_SYNC_RESPONSE_BODY_MB`：PII 恢复同步响应缓冲上限，默认 `64MB`；显式设为 `0` 表示不再收紧默认值，但仍受 `256MB` 安全硬上限约束
- `REDIS_URL`：Redis 连接串；仅 Postgres + Redis 的 Docker Compose 部署需要配置
- `AETHER_RUNTIME_BACKEND=memory|redis`：运行时缓存/协调后端。SQLite 默认用 `memory`，不会连接 Redis；多节点部署和需要跨 gateway 重启恢复 OpenAI Responses continuation history 的部署必须使用共享 Redis
- `AETHER_GATEWAY_DATABASE_MODE=auto|verify-only`：数据库启动策略，默认 `auto`，自动完成挂起的 schema migration 和 backfill；`verify-only` 仅检查并在数据库落后时拒绝启动
- `AETHER_GATEWAY_AUTO_PREPARE_DATABASE`：旧版兼容开关；新配置请使用 `AETHER_GATEWAY_DATABASE_MODE`
- `JWT_SECRET_KEY` / `ENCRYPTION_KEY`：认证和敏感数据加密所需密钥
- `AETHER_BACKUP_ENCRYPTION_KEY`：推荐的 S3 备份独立加密密钥；缺省回退到 `ENCRYPTION_KEY`。新备份使用带 key ID 的 AES-256-GCM v2 envelope，轮换前必须保留旧密钥
- `API_KEY_PREFIX`：用户和管理员新建 API Key 时使用的前缀，默认 `sk`
- `ADMIN_USERNAME` / `ADMIN_PASSWORD` / `ADMIN_EMAIL`：首次启动时自举首个本地管理员；`install.sh` 会提示输入管理员密码
- `CORS_ORIGINS` / `CORS_ALLOW_CREDENTIALS`：前端跨域来源控制；如果要跨域带登录 Cookie，`CORS_ORIGINS` 不能写 `*`
- `RUST_LOG`：Rust 日志过滤，例如 `aether_gateway=info`、`aether_gateway=debug,sqlx=warn`
- `DB_PASSWORD` / `REDIS_PASSWORD` / `MYSQL_PASSWORD` / `MYSQL_ROOT_PASSWORD`：Docker Compose 后端密码，首次安装时分别随机生成；手工部署必须替换示例占位值，不要互相复用

### S3 备份离线恢复

先从 S3 下载完整的 `.json.zst.aes256gcm` 对象，再使用原始的完整 S3 object key 做认证解密。恢复工具只验证并输出本地 JSON，不会直接写数据库；数据库导入仍应在维护窗口通过管理端完成。

```bash
AETHER_BACKUP_ENCRYPTION_KEY='原备份密钥' \
  cargo run -p aether-gateway --bin aether-backup-restore -- \
  --input ./backup.json.zst.aes256gcm \
  --object-key 'aether/backups/aether-data-backup-20260822-010000.json.zst.aes256gcm' \
  --output ./restored-backup.json
```

工具默认拒绝覆盖，输出采用原子写并在 Unix 上设置为 `0600`；Unix 可用 `--overwrite` 原子替换，Windows 为避免非原子删除窗口会要求选择新输出路径。密钥不能作为命令行参数。可使用 `AETHER_BACKUP_ENCRYPTION_KEY`、兼容用 `AETHER_GATEWAY_DATA_ENCRYPTION_KEY` / `ENCRYPTION_KEY`、受保护的 `--key-file`，或 `AETHER_BACKUP_KEYRING_FILE`。Keyring JSON 格式为 `{"version":1,"keys":["当前或历史 v2 secret"],"legacy_v1":["旧 v1 secret"]}`；条目也可写成 `{"secret":"..."}`（兼容字段名 `key`）。也可由 `AETHER_BACKUP_HISTORICAL_KEYS_JSON` 提供同一结构。密钥文件必须是非符号链接的普通文件，Unix 下权限需为 `0600` 或更严格。

默认限制密文为 `512MiB`、解压后 JSON 为 `1GiB`，可通过受限的 `--max-encrypted-mib` / `--max-json-mib` 调整。网关最多扫描同一备份前缀下 10,000 个对象，并且不会自动删除 S3 对象：`backup_s3_retention_count` 只用于报告超出保留数量的清理候选。旧明文备份在创建并验证加密副本后仍会保留，必须通过 bucket lifecycle 或支持版本条件的外部清理工具移除；启用 Versioning 时还需清理 noncurrent versions，Object Lock/retention 可能阻止物理删除。

---

## 许可证

本项目采用 [Aether 非商业开源许可证](LICENSE)。允许个人学习、教育研究、非盈利组织及企业内部非盈利性质的使用；禁止用于盈利目的。商业使用请联系获取商业许可。

## 联系作者

<p align="center">
  <img src="docs/author/qq_qrcode.jpg" width="200" alt="QQ二维码">
  &nbsp;&nbsp;&nbsp;&nbsp;
  <img src="docs/author/qrcode_1770574997172.jpg" width="200" alt="QQ群二维码">
</p>

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=fawney19/Aether&type=date&legend=top-left)](https://www.star-history.com/?repos=fawney19%2FAether&type=date&legend=top-left)
