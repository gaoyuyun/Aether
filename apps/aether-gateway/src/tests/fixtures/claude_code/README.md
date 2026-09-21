# Claude Code 冻结 oracle

`native_cli_2_1_161_messages.{headers,body}.json` 是一份原生 Claude Code 2.1.161 发往
`/v1/messages` 的请求快照（header 顺序与 body 字节均按线上抓包形状固化；凭据与标识符已替换为
测试值，`cch` 为占位签名）。

差分测试 `tests::ai_execute::claude_code_oracle` 用它验证：

- 原生请求经网关后 body **逐字节**不变（执行计划走 `body_bytes_b64`），身份头逐字节不变；
- 同一 body 由第三方客户端（SDK UA）发出时，网关补齐 `metadata.user_id` JSON 形状、有序 beta、
  `cache_control` 与 `cch=`，且 CCH 对最终 body 校验通过。

更新 profile 版本（`claude_code/profile.rs`）时重新抓包并替换本目录文件，同时核对
`signing.rs` 的 seed 与归一化规则是否变化。

## `tls_probe_synthetic.json`（P5，合成）

一份 **合成的** `https://tls.peet.ws/api/all` 回显：JSON 形状按 peet.ws 的公开响应固化，
但 JA3 / JA4 / peetprint 的值不是真实抓包，只用于固定 `handlers::admin::provider::tls_probe`
的解析与落库形状（`tls_probe::tests::synthetic_probe_fixture_parses_into_the_persisted_record_shape`）
与探针接口集成测试 `tests::control::admin::endpoints::tls_probe`。

拿到真实探针结果（管理端「探测 TLS 指纹」按钮或直接 `curl`）后：

1. 用真实响应替换本文件，删除 `_synthetic` 与 `_note` 两个字段；
2. 把真实的 JA3 hash / JA4 填进 `docs/operations/tls-fingerprint-capture.md` 的核对表，并与
   原生 Claude Code 抓包逐项比对（密码套件顺序、扩展顺序、ALPN、签名算法）；
3. 若有差异，改 `crates/aether-provider/transport/src/claude_code/tls_profile.rs` 的规格常量。
