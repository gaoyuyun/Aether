# TLS Fingerprint Capture

Aether stores per-request TLS capture under `usage.request_metadata.tls_fingerprint`.

```json
{
  "tls_fingerprint": {
    "incoming": {
      "source": "forwarded_header",
      "ja3": "...",
      "ja3_hash": "...",
      "ja4": "...",
      "protocol": "TLSv1.3",
      "cipher": "TLS_AES_128_GCM_SHA256",
      "sni": "api.example.com",
      "alpn": "h2"
    },
    "outgoing": {
      "source": "aether_transport_config",
      "observed": false,
      "transport_path": "direct",
      "backend": "reqwest_rustls",
      "http_mode": "auto",
      "tls_stack": "rustls",
      "tls_versions_offered": ["TLS1.3", "TLS1.2"],
      "alpn_offered": ["h2", "http/1.1"]
    }
  }
}
```

`incoming` is the client-to-Aether TLS fingerprint. It can be populated by Aether native TLS capture in direct deployments or by trusted reverse-proxy headers when TLS terminates before Aether.

`outgoing` is the Aether-to-provider TLS transport record. The gateway records the exact transport configuration it controls. On the default `reqwest_rustls` path it sets `observed: false` because rustls does not expose the emitted ClientHello bytes; the same is true for the `browser_wreq` (BoringSSL) path. `observed: true` only comes from the admin **TLS probe** (below), whose result is copied into the record as `ja3`, `ja3_hash`, `ja4`, `probed_at_unix_secs` and `probe_url`.

### Persistence

Only `tls_fingerprint.outgoing` is persisted into `usage.request_metadata` (see `crates/aether-data/contracts/src/repository/usage/metadata_policy.rs`). Every field is validated before it is written: `source` must be `aether_transport_config`, `backend` / `http_mode` / `tls_stack` / `transport_path` are restricted to known enums, `emulation_profile` and `profile_id` are short identifier tokens, `ja3` is digits with `,`/`-`, `ja3_hash` is exactly 32 lowercase hex characters, `ja4` / `peetprint` / `akamai_fingerprint` are bounded tokens, and probe fields are only kept when `observed` is `true`. `tls_fingerprint.incoming` is never persisted.

JA4 is limited to 64 characters. PeetPrint and Akamai fingerprints have a separate 512-character limit and accept their structured separators, including dots in PeetPrint version numbers.

## Outbound TLS emulation (P5)

When a provider or key selects `fingerprint.transport_profile`, the outbound transport switches from rustls to BoringSSL via `wreq`, and the record gains two fields:

```json
{
  "outgoing": {
    "source": "aether_transport_config",
    "observed": true,
    "transport_path": "direct",
    "backend": "browser_wreq",
    "http_mode": "http1_only",
    "tls_stack": "boringssl_wreq",
    "emulation_profile": "claude_code_node_openssl",
    "tls_versions_offered": ["TLS1.3", "TLS1.2"],
    "alpn_offered": ["http/1.1"],
    "ja3": "771,4865-...",
    "ja3_hash": "3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5",
    "ja4": "t13d1716h1_5b57614c22b0_3d5db4fb5c1e",
    "probed_at_unix_secs": 1760000000,
    "probe_url": "https://tls.peet.ws/api/all",
    "profile_id": "claude_code_node_openssl",
    "pool_scope": "key"
  }
}
```

### Profiles

Configured through the provider form / key editor ("传输指纹 profile"), stored as provider `config.fingerprint.transport_profile` or key `fingerprint.transport_profile`; the key-level value wins. Specs live in `crates/aether-provider/transport/src/claude_code/tls_profile.rs`; the wreq mapping is `execution_runtime/transport.rs::claude_code_tls_emulation_from_spec`.

| id | selectable for | backend / HTTP | ALPN | source |
|---|---|---|---|---|
| *(unset)* | all | `reqwest_rustls`, auto | `h2, http/1.1` | rustls defaults (behaviour unchanged) |
| `claude_code_node_openssl` | claude_code | `browser_wreq`, `http1_only` | `http/1.1` | CLIProxyAPI `internal/runtime/executor/helps/utls_client.go::claudeCodeTLSClientHelloSpec` (Claude Code 2.1.220, macOS arm64) |
| `claude_code_oauth_control_plane` | chosen automatically for claude_code OAuth token / profile / roles requests when a claude_code emulation profile is configured | `browser_wreq`, HTTP/1.1 (no ALPN extension) | none | CLIProxyAPI `internal/auth/claude/utls_transport.go::claudeOAuthTLSClientHelloSpec` |
| `chatgpt_com_chrome` | claude_code, codex (also chosen automatically for codex OAuth control-plane requests) | `browser_wreq`, auto | `h2, http/1.1` | `wreq-util` built-in Chrome 136 |

`claude_code_node_openssl` / `claude_code_oauth_control_plane` share the Node/OpenSSL parameters: 17 cipher suites in OpenSSL default order, curves `X25519:P-256:P-384`, 9 signature algorithms including `rsa_pkcs1_sha1`, no GREASE, no extension permutation, a single X25519 key share, extension order pinned via `SSL_CTX_set_extension_order`, `aes_hw_override` on so the TLS 1.3 suite order does not depend on the host CPU. HTTP/1.1 header order follows the captured Axios / undici shapes (`messages`, `count_tokens`, `oauth_refresh`, `oauth_inspect`).

The `chatgpt_com_chrome` profile preserves the API request's explicit headers and disables Chrome's default browser headers. Existing browser cookie flows retain their browser defaults. Runtime profile selection and the admin form use the same provider allow list; the OAuth control-plane profile is generated internally.

### Node tunnel support

TLS emulation is available for direct connections and conventional HTTP/SOCKS proxy URLs. Node tunnel requests retain their selected profile and are sent through the configured node. The current tunnel worker supports only rustls backends and rejects `browser_wreq` with an unsupported-backend error. OAuth refresh uses the same rule: explicit emulation is never silently downgraded to rustls. Leave the emulation profile unset when using a node that lacks this backend.

### Deviation from the plan and unverified parameters

The plan (`provider-quality-improvement-plan.md` §5.2) listed `ALPN h2, http/1.1` for the inference profile. The reference capture advertises **only `http/1.1`** for `/v1/messages` (Node fetch/undici) and **no ALPN extension at all** on the OAuth control plane, so the profiles follow the capture and no HTTP/2 frame fingerprint is needed for Claude Code.

Everything below is taken from CLIProxyAPI's replica of Claude Code **2.1.220** while the local identity profile is **2.1.161** (`claude_code/profile.rs`). None of it has been checked against a real capture yet — treat the whole table as *to be verified*:

| item | status |
|---|---|
| cipher suite list and order (17) | from replica, unverified |
| signature algorithms (9, incl. `rsa_pkcs1_sha1`) | from replica, unverified |
| supported groups `X25519:P-256:P-384` | from replica, unverified |
| extension order | from replica; BoringSSL appends `padding`(21) automatically only when the ClientHello is 256–511 bytes, so its presence/length may differ |
| `session_ticket` payload | empty on the first ClientHello (matches capture); later connections may carry a ticket |
| HTTP/1.1 header order | from replica, unverified |
| JA3 / JA4 of the real CLI | **not captured** — the fixture `apps/aether-gateway/src/tests/fixtures/claude_code/tls_probe_synthetic.json` is synthetic |

To verify: run the native CLI against a probe service (or capture with Wireshark), run the admin probe for a key using the profile, and diff `ja4` / `peetprint` / `http1_header_order`. Update the spec constants in `tls_profile.rs` and replace the synthetic fixture when they differ.

## Admin TLS probe

`POST /api/admin/endpoints/keys/{key_id}/tls-probe` (route kind `endpoints_manage:tls_probe`) sends one GET to a probe service using the key's resolved transport profile and proxy, parses the echoed fingerprint and stores it under the key's `upstream_metadata.tls_probe`:

```json
{
  "message": "已完成 TLS 指纹探测",
  "key_id": "key-…",
  "probe": {
    "observed": true,
    "probe_url": "https://tls.peet.ws/api/all",
    "probed_at_unix_secs": 1760000000,
    "emulation_profile": "claude_code_node_openssl",
    "profile_id": "claude_code_node_openssl",
    "backend": "browser_wreq",
    "tls_stack": "boringssl_wreq",
    "http_version": "h1",
    "ja3": "771,…",
    "ja3_hash": "…",
    "ja4": "t13d1716h1_…",
    "peetprint": "…",
    "peetprint_hash": "…",
    "tls_version_negotiated": "772",
    "akamai_fingerprint": "… (HTTP/2 only)",
    "http1_header_order": ["Host", "Accept", "User-Agent", "…"]
  }
}
```

The probe URL is not accepted from the caller. It defaults to `https://tls.peet.ws/api/all`; the system config key `transport.tls_probe_url` may switch it to another entry of the fixed allow list (`https://tls.browserleaks.com/json`). Failures (non-2xx, non-JSON, no `ja3_hash`/`ja4`) return HTTP 502 and write nothing. Header *values* echoed by the probe service are never stored, only header names.

Both services' response shapes are supported: Peet's nested `tls` / `http2` fields and Browserleaks' root-level `ja3_text`, `ja3_hash`, `ja4`, and `akamai_text` fields are normalized into the same probe record.

On subsequent requests `resolve_transport_profile` attaches the stored summary to the resolved profile (`extra.tls_probe`) **only when the probe's `emulation_profile` equals the current profile**, and the outgoing record then reports `observed: true` with the probed JA3/JA4. The key editor shows the summary and a "探测 TLS 指纹" button; the request detail drawer shows the outgoing record in the "出站 TLS" facts row.

## Nginx TLS Termination

When nginx terminates HTTPS and proxies HTTP to Aether, Aether cannot see the original ClientHello. Configure nginx to forward the TLS fields it can observe:

```nginx
server {
    listen 443 ssl http2;
    server_name api.example.com;

    ssl_certificate     /etc/letsencrypt/live/api.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/api.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;

        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        proxy_set_header X-Aether-TLS-Source nginx;
        proxy_set_header X-Aether-TLS-Protocol $ssl_protocol;
        proxy_set_header X-Aether-TLS-Cipher $ssl_cipher;
        proxy_set_header X-Aether-TLS-SNI $ssl_server_name;
    }
}
```

Stock nginx does not provide JA3/JA4 variables. The forwarded record is still useful, but it is not a complete TLS fingerprint. To forward JA3/JA4 through nginx, use an nginx build/module or edge layer that computes them and set:

```nginx
proxy_set_header X-Aether-TLS-JA3      $ja3;
proxy_set_header X-Aether-TLS-JA3-Hash $ja3_hash;
proxy_set_header X-Aether-TLS-JA4      $ja4;
```

Only accept these headers from trusted infrastructure. Do not expose Aether directly to public clients while also trusting client-supplied `X-Aether-TLS-*` headers.

## Nginx TCP Passthrough

If Aether terminates TLS itself, nginx can pass TCP through without decrypting:

```nginx
stream {
    map $ssl_preread_server_name $aether_backend {
        api.example.com 127.0.0.1:3443;
        default         127.0.0.1:3443;
    }

    server {
        listen 443;
        proxy_pass $aether_backend;
        ssl_preread on;
    }
}
```

In this mode nginx cannot inject HTTP headers because it never sees HTTP. Aether native TLS capture is responsible for populating `tls_fingerprint.incoming`.
