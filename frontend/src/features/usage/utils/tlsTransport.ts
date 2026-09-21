import type { OutgoingTlsFingerprint } from '../components/TlsTransportFacts.vue'

const TLS_STACKS = new Set(['rustls', 'boringssl_wreq'])

function optionalString(record: Record<string, unknown>, key: string): string | null {
  const value = record[key]
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null
}

/**
 * 从请求详情的 `metadata.tls_fingerprint.outgoing` 解析出站 TLS 记录。
 * 只在 `source === 'aether_transport_config'` 且 `backend` 为字符串时返回；`observed`
 * 缺失按 false 处理（旧记录）。
 */
export function resolveOutgoingTlsFingerprint(
  metadata: Record<string, unknown> | null | undefined,
): OutgoingTlsFingerprint | null {
  const tls = metadata?.tls_fingerprint
  if (!tls || typeof tls !== 'object' || Array.isArray(tls)) return null
  const outgoing = (tls as Record<string, unknown>).outgoing
  if (!outgoing || typeof outgoing !== 'object' || Array.isArray(outgoing)) return null
  const record = outgoing as Record<string, unknown>
  if (record.source !== 'aether_transport_config') return null
  const backend = optionalString(record, 'backend')
  if (!backend) return null
  const tlsStack = optionalString(record, 'tls_stack')
  const probedAt = Number(record.probed_at_unix_secs)
  const alpn = Array.isArray(record.alpn_offered)
    ? record.alpn_offered.filter((item): item is string => typeof item === 'string')
    : null
  return {
    observed: record.observed === true,
    backend,
    http_mode: optionalString(record, 'http_mode'),
    tls_stack: tlsStack && TLS_STACKS.has(tlsStack) ? tlsStack : tlsStack,
    transport_path: optionalString(record, 'transport_path'),
    emulation_profile: optionalString(record, 'emulation_profile'),
    profile_id: optionalString(record, 'profile_id'),
    alpn_offered: alpn,
    ja3: optionalString(record, 'ja3'),
    ja3_hash: optionalString(record, 'ja3_hash'),
    ja4: optionalString(record, 'ja4'),
    probed_at_unix_secs: Number.isFinite(probedAt) && probedAt > 0 ? Math.floor(probedAt) : null,
    probe_url: optionalString(record, 'probe_url'),
  }
}
