import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'
import OAuthKeyEditDialog from '@/features/providers/components/OAuthKeyEditDialog.vue'
import type { EndpointAPIKey } from '@/api/endpoints'

const endpointMocks = vi.hoisted(() => ({
  updateProviderKey: vi.fn(),
  clearReasoningReplayCache: vi.fn(),
  confirmWarning: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}))

vi.mock('@/api/endpoints', () => ({
  updateProviderKey: endpointMocks.updateProviderKey,
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

function createKey(): EndpointAPIKey {
  return {
    id: 'key-codex-1',
    provider_id: 'provider-codex',
    api_formats: ['openai:responses'],
    api_key_masked: '***',
    auth_type: 'oauth',
    name: 'Codex 主账号',
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
  }
}

function mountDialog(providerType: string | null) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(OAuthKeyEditDialog, {
    open: true,
    editingKey: createKey(),
    providerType,
  })
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
  endpointMocks.updateProviderKey.mockReset()
  endpointMocks.clearReasoningReplayCache.mockReset()
  endpointMocks.confirmWarning.mockReset()
  endpointMocks.toastSuccess.mockReset()
  endpointMocks.toastError.mockReset()
  endpointMocks.confirmWarning.mockResolvedValue(true)
  endpointMocks.clearReasoningReplayCache.mockResolvedValue({ message: '已清除 Key Codex 主账号 的推理回放缓存', cleared: 2 })
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

describe('OAuthKeyEditDialog reasoning replay cache action', () => {
  it('only shows the action for providers that replay reasoning signatures', async () => {
    const codexRoot = mountDialog('codex')
    await settle()
    expect(codexRoot.querySelector('[data-testid="reasoning-replay-section"]')).not.toBeNull()

    const antigravityRoot = mountDialog('antigravity')
    await settle()
    expect(antigravityRoot.querySelector('[data-testid="reasoning-replay-section"]')).not.toBeNull()

    const claudeRoot = mountDialog('claude_code')
    await settle()
    expect(claudeRoot.querySelector('[data-testid="reasoning-replay-section"]')).toBeNull()

    const unknownRoot = mountDialog(null)
    await settle()
    expect(unknownRoot.querySelector('[data-testid="reasoning-replay-section"]')).toBeNull()
  })

  it('confirms, calls the clear endpoint with provider and key ids, then shows the cleared count', async () => {
    const root = mountDialog('codex')
    await settle()
    const button = root.querySelector<HTMLButtonElement>('[data-testid="clear-reasoning-replay"]')
    expect(button).not.toBeNull()
    button?.click()
    await settle()

    expect(endpointMocks.confirmWarning).toHaveBeenCalledTimes(1)
    expect(endpointMocks.clearReasoningReplayCache).toHaveBeenCalledWith('provider-codex', 'key-codex-1')
    expect(endpointMocks.toastSuccess).toHaveBeenCalledWith('已清除 Key Codex 主账号 的推理回放缓存', '成功')
    expect(root.querySelector('[data-testid="reasoning-replay-section"]')?.textContent).toContain('已清除 2 条本机缓存')
  })

  it('does not call the endpoint when the confirmation is declined', async () => {
    endpointMocks.confirmWarning.mockResolvedValueOnce(false)
    const root = mountDialog('gemini_cli')
    await settle()
    root.querySelector<HTMLButtonElement>('[data-testid="clear-reasoning-replay"]')?.click()
    await settle()
    expect(endpointMocks.clearReasoningReplayCache).not.toHaveBeenCalled()
  })

  it('surfaces API failures through the error toast', async () => {
    endpointMocks.clearReasoningReplayCache.mockRejectedValueOnce(new Error('boom'))
    const root = mountDialog('codex')
    await settle()
    root.querySelector<HTMLButtonElement>('[data-testid="clear-reasoning-replay"]')?.click()
    await settle()
    expect(endpointMocks.toastError).toHaveBeenCalledTimes(1)
  })
})
