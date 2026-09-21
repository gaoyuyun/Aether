import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'
import OAuthKeyEditDialog from '@/features/providers/components/OAuthKeyEditDialog.vue'
import type { EndpointAPIKey } from '@/api/endpoints'

const endpointMocks = vi.hoisted(() => ({
  updateProviderKey: vi.fn(),
  resetProviderKeyClaudeCodeDevice: vi.fn(),
  clearReasoningReplayCache: vi.fn(),
  confirmWarning: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}))

vi.mock('@/api/endpoints', () => ({
  updateProviderKey: endpointMocks.updateProviderKey,
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
  return { Dialog, Input, Label, Button, Switch }
})

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({ success: endpointMocks.toastSuccess, error: endpointMocks.toastError }),
}))

vi.mock('@/composables/useConfirm', () => ({
  useConfirm: () => ({ confirmWarning: endpointMocks.confirmWarning }),
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

const profile = {
  device_id_prefix: '0123456789ab',
  cli_version: '2.1.161',
  package_version: '0.94.0',
  runtime_version: 'v24.3.0',
  os: 'Linux',
  arch: 'arm64',
  created_at_unix_secs: 1_700_000_000,
  updated_at_unix_secs: 1_700_000_000,
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

beforeEach(() => {
  for (const mock of Object.values(endpointMocks)) mock.mockReset()
  endpointMocks.confirmWarning.mockResolvedValue(true)
  endpointMocks.resetProviderKeyClaudeCodeDevice.mockResolvedValue({
    message: '已重置设备身份，下一次请求将派生新的设备标识',
    reset: true,
  })
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

describe('OAuthKeyEditDialog Claude Code device identity', () => {
  it('shows the section only for claude_code providers', async () => {
    const claudeRoot = mountDialog('claude_code', createKey({ claude_code_device_profile: profile }))
    await settle()
    expect(claudeRoot.querySelector('[data-testid="claude-code-device-section"]')).not.toBeNull()
    expect(claudeRoot.querySelector('[data-testid="claude-code-device-summary"]')?.textContent)
      .toContain('0123456789ab… · CLI 2.1.161 · SDK 0.94.0 · Node v24.3.0 · Linux/arm64')

    const codexRoot = mountDialog('codex', createKey({ claude_code_device_profile: profile }))
    await settle()
    expect(codexRoot.querySelector('[data-testid="claude-code-device-section"]')).toBeNull()
  })

  it('disables reset until a profile exists and explains the pending state', async () => {
    const root = mountDialog('claude_code', createKey({ claude_code_device_profile: null }))
    await settle()
    const button = root.querySelector<HTMLButtonElement>('[data-testid="reset-claude-code-device"]')
    expect(button?.disabled).toBe(true)
    expect(root.querySelector('[data-testid="claude-code-device-summary"]')?.textContent).toContain('尚未生成')
  })

  it('confirms, calls the reset endpoint, then clears the summary', async () => {
    const root = mountDialog('claude_code', createKey({ claude_code_device_profile: profile }))
    await settle()
    root.querySelector<HTMLButtonElement>('[data-testid="reset-claude-code-device"]')?.click()
    await settle()

    expect(endpointMocks.confirmWarning).toHaveBeenCalledTimes(1)
    expect(endpointMocks.resetProviderKeyClaudeCodeDevice).toHaveBeenCalledWith('key-claude-1')
    expect(endpointMocks.toastSuccess).toHaveBeenCalledWith('已重置设备身份，下一次请求将派生新的设备标识', '成功')
    expect(root.querySelector('[data-testid="claude-code-device-summary"]')?.textContent).toContain('尚未生成')
  })

  it('does not call the endpoint when the confirmation is declined', async () => {
    endpointMocks.confirmWarning.mockResolvedValueOnce(false)
    const root = mountDialog('claude_code', createKey({ claude_code_device_profile: profile }))
    await settle()
    root.querySelector<HTMLButtonElement>('[data-testid="reset-claude-code-device"]')?.click()
    await settle()
    expect(endpointMocks.resetProviderKeyClaudeCodeDevice).not.toHaveBeenCalled()
  })
})
