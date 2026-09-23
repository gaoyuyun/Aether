import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'
import OAuthKeyEditDialog from '@/features/providers/components/OAuthKeyEditDialog.vue'
import type { EndpointAPIKey } from '@/api/endpoints'

const endpointMocks = vi.hoisted(() => ({
  updateProviderKey: vi.fn(),
  probeProviderKeyTlsFingerprint: vi.fn(),
  resetProviderKeyClaudeCodeDevice: vi.fn(),
  clearReasoningReplayCache: vi.fn(),
  confirmWarning: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
  copyToClipboard: vi.fn(),
}))

vi.mock('@/api/endpoints', () => ({
  updateProviderKey: endpointMocks.updateProviderKey,
  probeProviderKeyTlsFingerprint: endpointMocks.probeProviderKeyTlsFingerprint,
  resetProviderKeyClaudeCodeDevice: endpointMocks.resetProviderKeyClaudeCodeDevice,
}))

vi.mock('@/api/endpoints/pool', () => ({
  clearReasoningReplayCache: endpointMocks.clearReasoningReplayCache,
}))

vi.mock('@/components/ui', async () => {
  const { defineComponent, h } = await import('vue')
  const Dialog = defineComponent({
    name: 'DialogStub',
    props: { modelValue: Boolean },
    setup(props, { slots }) {
      return () => (props.modelValue ? h('section', [slots.default?.(), slots.footer?.()]) : null)
    },
  })
  const Input = defineComponent({
    name: 'InputStub',
    inheritAttrs: false,
    props: { modelValue: { type: [String, Number], default: '' } },
    emits: ['update:modelValue'],
    setup(props, { attrs, emit }) {
      return () => h('input', {
        ...attrs,
        value: props.modelValue ?? '',
        onInput: (event: Event) => emit('update:modelValue', (event.target as HTMLInputElement).value),
      })
    },
  })
  const Textarea = defineComponent({
    name: 'TextareaStub',
    inheritAttrs: false,
    props: { modelValue: { type: String, default: '' } },
    emits: ['update:modelValue'],
    setup(props, { attrs, emit }) {
      return () => h('textarea', {
        ...attrs,
        value: props.modelValue ?? '',
        onInput: (event: Event) => emit('update:modelValue', (event.target as HTMLTextAreaElement).value),
      })
    },
  })
  const Label = defineComponent({
    name: 'LabelStub',
    inheritAttrs: false,
    props: { for: String },
    setup(props, { attrs, slots }) {
      return () => h('label', { ...attrs, for: props.for }, slots.default?.())
    },
  })
  const Button = defineComponent({
    name: 'ButtonStub',
    inheritAttrs: false,
    props: { disabled: Boolean, variant: String, size: String },
    setup(props, { attrs, slots }) {
      return () => h('button', { ...attrs, disabled: props.disabled, type: attrs.type ?? 'button' }, slots.default?.())
    },
  })
  const Switch = defineComponent({
    name: 'SwitchStub',
    inheritAttrs: false,
    props: { modelValue: Boolean },
    emits: ['update:modelValue'],
    setup(props, { attrs, emit }) {
      return () => h('input', {
        ...attrs,
        type: 'checkbox',
        checked: props.modelValue,
        onChange: (event: Event) => emit('update:modelValue', (event.target as HTMLInputElement).checked),
      })
    },
  })
  return { Dialog, Input, Textarea, Label, Button, Switch }
})

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({ success: endpointMocks.toastSuccess, error: endpointMocks.toastError }),
}))

vi.mock('@/composables/useConfirm', () => ({
  useConfirm: () => ({ confirmWarning: endpointMocks.confirmWarning }),
}))

vi.mock('@/composables/useClipboard', () => ({
  useClipboard: () => ({ copyToClipboard: endpointMocks.copyToClipboard }),
}))

vi.mock('lucide-vue-next', async () => {
  const { defineComponent, h } = await import('vue')
  const Icon = defineComponent({ name: 'IconStub', setup: () => () => h('span') })
  return { SquarePen: Icon }
})

const mountedApps: Array<{ app: App; root: HTMLElement }> = []

function createKey(overrides: Partial<EndpointAPIKey> = {}): EndpointAPIKey {
  return {
    id: 'key-claude-1',
    provider_id: 'provider-claude',
    api_formats: ['claude:messages'],
    api_key_masked: '***',
    auth_type: 'oauth',
    name: 'Claude 主账号',
    rate_multipliers: null,
    internal_priority: 10,
    rpm_limit: null,
    concurrent_limit: null,
    allowed_models: null,
    capabilities: null,
    cache_ttl_minutes: 5,
    max_probe_interval_minutes: 32,
    health_score: 100,
    consecutive_failures: 0,
    request_count: 0,
    success_count: 0,
    error_count: 0,
    success_rate: 1,
    avg_response_time_ms: 0,
    is_active: true,
    note: '',
    created_at: '2026-09-20T00:00:00Z',
    updated_at: '2026-09-20T00:00:00Z',
    auto_fetch_models: false,
    model_include_patterns: [],
    model_exclude_patterns: [],
    ...overrides,
  }
}

function mountDialog(providerType: string | null, key: EndpointAPIKey) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(OAuthKeyEditDialog, { open: true, editingKey: key, providerType })
  app.mount(root)
  mountedApps.push({ app, root })
  return root
}

async function settle() {
  await nextTick()
  await Promise.resolve()
  await nextTick()
}

function clickSave(root: HTMLElement) {
  const button = [...root.querySelectorAll<HTMLButtonElement>('button')]
    .find(candidate => candidate.textContent?.trim() === '保存')
  if (!button) throw new Error('Missing save button')
  button.click()
}



beforeEach(() => {
  for (const mock of Object.values(endpointMocks)) mock.mockReset()
  endpointMocks.confirmWarning.mockResolvedValue(true)
  endpointMocks.copyToClipboard.mockResolvedValue(true)
  endpointMocks.updateProviderKey.mockImplementation(async (_id: string, payload: Record<string, unknown>) => ({
    ...createKey(),
    ...payload,
  }))
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

const probeResult = {
  observed: true,
  probe_url: 'https://tls.peet.ws/api/all',
  probed_at_unix_secs: 1_760_000_000,
  emulation_profile: 'claude_code_node_openssl',
  profile_id: 'claude_code_node_openssl',
  backend: 'browser_wreq',
  tls_stack: 'boringssl_wreq',
  http_version: 'h1',
  ja3: '771,4865-4866-4867,0-11-10-35-16-13-43-45-51,29-23-24,0',
  ja3_hash: '3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5',
  ja4: 't13d1716h1_5b57614c22b0_3d5db4fb5c1e',
  peetprint: '771-772|2-1.1|29-23-24',
  akamai_fingerprint: '1:65536;2:0;4:6291456|15663105|0|m,a,s,p',
  tls_version_negotiated: '772',
  http1_header_order: ['host', 'accept', 'user-agent'],
}

async function selectKeyTransportProfile(root: HTMLElement, index: number) {
  const select = root.querySelector<HTMLSelectElement>('[data-testid="transport-profile-select"]')
  if (!select) throw new Error('Missing transport profile select')
  select.selectedIndex = index
  select.dispatchEvent(new Event('change', { bubbles: true }))
  await settle()
}

describe('OAuthKeyEditDialog transport profile and TLS probe', () => {
  it('shows the section only for claude_code / codex providers', async () => {
    const claudeRoot = mountDialog('claude_code', createKey())
    await settle()
    expect(claudeRoot.querySelector('[data-testid="transport-profile-section"]')).not.toBeNull()
    expect([...(claudeRoot.querySelectorAll<HTMLOptionElement>('[data-testid="transport-profile-select"] option') ?? [])].length).toBe(3)

    const codexRoot = mountDialog('codex', createKey())
    await settle()
    expect([...(codexRoot.querySelectorAll<HTMLOptionElement>('[data-testid="transport-profile-select"] option') ?? [])].length).toBe(2)

    const antigravityRoot = mountDialog('antigravity', createKey())
    await settle()
    expect(antigravityRoot.querySelector('[data-testid="transport-profile-section"]')).toBeNull()
  })

  it('loads the saved profile and submits the tri-state value', async () => {
    const root = mountDialog('claude_code', createKey({ transport_profile: 'claude_code_node_openssl' }))
    await settle()
    const select = root.querySelector<HTMLSelectElement>('[data-testid="transport-profile-select"]')
    expect(select?.selectedIndex).toBe(1)

    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ transport_profile: 'claude_code_node_openssl' }),
    )

    endpointMocks.updateProviderKey.mockClear()
    await selectKeyTransportProfile(root, 0)
    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ transport_profile: null }),
    )
  })

  it('keeps fingerprints out of the summary and copies the complete latest probe result', async () => {
    endpointMocks.probeProviderKeyTlsFingerprint.mockResolvedValue({
      message: '已完成 TLS 指纹探测',
      key_id: 'key-claude-1',
      probe: { ...probeResult, ja4: 't13d1716h1_refreshed_after_probe' },
    })
    const root = mountDialog('claude_code', createKey({ tls_probe: probeResult }))
    await settle()
    const summary = root.querySelector('[data-testid="tls-probe-summary"]')
    expect(summary?.textContent).toContain('已探测')
    expect(summary?.textContent).toContain(new Date(probeResult.probed_at_unix_secs * 1000).toLocaleString())
    expect(root.textContent).not.toContain(probeResult.ja3_hash)
    expect(root.textContent).not.toContain(probeResult.ja4)

    root.querySelector<HTMLButtonElement>('[data-testid="copy-tls-fingerprint"]')?.click()
    await settle()
    expect(JSON.parse(endpointMocks.copyToClipboard.mock.calls[0]![0])).toEqual(probeResult)

    root.querySelector<HTMLButtonElement>('[data-testid="probe-tls-fingerprint"]')?.click()
    await settle()
    expect(endpointMocks.probeProviderKeyTlsFingerprint).toHaveBeenCalledWith('key-claude-1')
    expect(endpointMocks.toastSuccess).toHaveBeenCalledWith('已完成 TLS 指纹探测', '成功')
    expect(root.textContent).not.toContain('t13d1716h1_refreshed_after_probe')

    root.querySelector<HTMLButtonElement>('[data-testid="copy-tls-fingerprint"]')?.click()
    await settle()
    expect(JSON.parse(endpointMocks.copyToClipboard.mock.calls[1]![0])).toEqual({
      ...probeResult,
      ja4: 't13d1716h1_refreshed_after_probe',
    })
  })

  it('explains the pending state and surfaces probe failures', async () => {
    endpointMocks.probeProviderKeyTlsFingerprint.mockRejectedValue(new Error('探针服务返回 HTTP 503'))
    const root = mountDialog('codex', createKey({ tls_probe: null }))
    await settle()
    expect(root.querySelector('[data-testid="tls-probe-summary"]')?.textContent).toContain('尚未探测')
    expect(root.querySelector('[data-testid="copy-tls-fingerprint"]')).toBeNull()

    root.querySelector<HTMLButtonElement>('[data-testid="probe-tls-fingerprint"]')?.click()
    await settle()
    expect(endpointMocks.toastError).toHaveBeenCalledWith(expect.stringContaining('503'), '错误')
    expect(root.querySelector('[data-testid="tls-probe-summary"]')?.textContent).toContain('尚未探测')
    expect(root.querySelector('[data-testid="copy-tls-fingerprint"]')).toBeNull()
  })
})
