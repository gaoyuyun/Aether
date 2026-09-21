<template>
  <dl
    class="grid grid-cols-2 gap-x-4 gap-y-1.5 text-xs sm:grid-cols-4"
    data-testid="tls-transport-facts"
  >
    <div class="flex min-w-0 items-baseline justify-between gap-3 sm:block">
      <dt class="text-muted-foreground">
        出站 TLS 栈
      </dt>
      <dd
        class="truncate font-mono font-medium text-foreground sm:mt-0.5"
        :title="tlsStackLabel"
        data-testid="tls-transport-stack"
      >
        {{ tlsStackLabel }}
      </dd>
    </div>
    <div class="flex min-w-0 items-baseline justify-between gap-3 sm:block">
      <dt class="text-muted-foreground">
        仿真 profile
      </dt>
      <dd
        class="truncate font-mono font-medium text-foreground sm:mt-0.5"
        :title="emulationLabel"
        data-testid="tls-transport-emulation"
      >
        {{ emulationLabel }}
      </dd>
    </div>
    <div class="flex min-w-0 items-baseline justify-between gap-3 sm:block">
      <dt class="text-muted-foreground">
        HTTP 模式
      </dt>
      <dd
        class="truncate font-mono font-medium text-foreground sm:mt-0.5"
        :title="httpModeLabel"
      >
        {{ httpModeLabel }}
      </dd>
    </div>
    <div class="flex min-w-0 items-baseline justify-between gap-3 sm:block">
      <dt class="text-muted-foreground">
        指纹
      </dt>
      <dd
        class="truncate font-mono font-medium text-foreground sm:mt-0.5"
        :title="fingerprintTitle"
        data-testid="tls-transport-fingerprint"
      >
        <template v-if="outgoing.observed">
          <span data-testid="tls-transport-ja4">{{ outgoing.ja4 ?? '-' }}</span>
          <span
            v-if="outgoing.ja3_hash"
            class="ml-1 text-muted-foreground"
            data-testid="tls-transport-ja3-hash"
          >JA3 {{ outgoing.ja3_hash }}</span>
        </template>
        <span
          v-else
          class="text-muted-foreground"
          data-testid="tls-transport-unobserved"
        >未探测（{{ outgoing.backend ?? '-' }} 配置推断）</span>
      </dd>
    </div>
  </dl>
</template>

<script setup lang="ts">
import { computed } from 'vue'

/** `usage.request_metadata.tls_fingerprint.outgoing`（落库白名单只保留 outgoing）。 */
export interface OutgoingTlsFingerprint {
  observed: boolean
  backend?: string | null
  http_mode?: string | null
  tls_stack?: string | null
  transport_path?: string | null
  emulation_profile?: string | null
  profile_id?: string | null
  alpn_offered?: string[] | null
  ja3?: string | null
  ja3_hash?: string | null
  ja4?: string | null
  probed_at_unix_secs?: number | null
  probe_url?: string | null
}

const props = defineProps<{
  outgoing: OutgoingTlsFingerprint
}>()

const TLS_STACK_LABELS: Record<string, string> = {
  rustls: 'rustls',
  boringssl_wreq: 'BoringSSL (wreq)',
}

const EMULATION_LABELS: Record<string, string> = {
  claude_code_node_openssl: 'Claude Code Node/OpenSSL',
  claude_code_oauth_control_plane: 'Claude Code OAuth 控制面',
  chatgpt_com_chrome: 'Chrome (chatgpt.com)',
}

const tlsStackLabel = computed(() => {
  const stack = String(props.outgoing.tls_stack ?? '').trim()
  return (stack && TLS_STACK_LABELS[stack]) || stack || '-'
})

const emulationLabel = computed(() => {
  const profile = String(props.outgoing.emulation_profile ?? '').trim()
  if (!profile) return '无（系统默认）'
  return EMULATION_LABELS[profile] ?? profile
})

const httpModeLabel = computed(() => {
  const mode = String(props.outgoing.http_mode ?? '').trim()
  const alpn = Array.isArray(props.outgoing.alpn_offered) ? props.outgoing.alpn_offered : null
  const alpnText = alpn ? (alpn.length > 0 ? `ALPN ${alpn.join(',')}` : '无 ALPN') : ''
  return [mode || 'auto', alpnText].filter(Boolean).join(' · ')
})

const fingerprintTitle = computed(() => {
  if (!props.outgoing.observed) return '尚未用探针核对，出站 TLS 记录只反映网关自己的配置'
  const parts = [
    props.outgoing.ja4 ? `JA4 ${props.outgoing.ja4}` : null,
    props.outgoing.ja3_hash ? `JA3 ${props.outgoing.ja3_hash}` : null,
    props.outgoing.probed_at_unix_secs
      ? `探测于 ${new Date(props.outgoing.probed_at_unix_secs * 1000).toLocaleString()}`
      : null,
    props.outgoing.probe_url ? `来源 ${props.outgoing.probe_url}` : null,
  ]
  return parts.filter(Boolean).join(' · ')
})
</script>
