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
  toastWarning: vi.fn(),
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
  useToast: () => ({
    success: endpointMocks.toastSuccess,
    error: endpointMocks.toastError,
    warning: endpointMocks.toastWarning,
  }),
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

async function setTextarea(root: HTMLElement, value: string) {
  const textarea = root.querySelector<HTMLTextAreaElement>('[data-testid="sensitive-words-textarea"]')
  if (!textarea) throw new Error('Missing sensitive words textarea')
  textarea.value = value
  textarea.dispatchEvent(new Event('input', { bubbles: true }))
  await settle()
}

async function chooseMode(root: HTMLElement, mode: 'inherit' | 'override') {
  const radio = root.querySelector<HTMLInputElement>(`[data-testid="sensitive-words-${mode}"]`)
  if (!radio) throw new Error(`Missing radio ${mode}`)
  radio.checked = true
  radio.dispatchEvent(new Event('change', { bubbles: true }))
  await settle()
}

beforeEach(() => {
  for (const mock of Object.values(endpointMocks)) mock.mockReset()
  endpointMocks.confirmWarning.mockResolvedValue(true)
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

describe('OAuthKeyEditDialog sensitive word override', () => {
  it('shows the section only for claude_code / antigravity providers', async () => {
    const claudeRoot = mountDialog('claude_code', createKey())
    await settle()
    expect(claudeRoot.querySelector('[data-testid="sensitive-words-section"]')).not.toBeNull()

    const antigravityRoot = mountDialog('antigravity', createKey({ cloak_sensitive_words: null }))
    await settle()
    expect(antigravityRoot.querySelector('[data-testid="sensitive-words-section"]')).not.toBeNull()

    const codexRoot = mountDialog('codex', createKey())
    await settle()
    expect(codexRoot.querySelector('[data-testid="sensitive-words-section"]')).toBeNull()
  })

  it('defaults to inherit and sends null so the override is removed', async () => {
    const root = mountDialog('claude_code', createKey({ cloak_sensitive_words: null }))
    await settle()
    expect(root.querySelector<HTMLInputElement>('[data-testid="sensitive-words-inherit"]')?.checked).toBe(true)
    expect(root.querySelector('[data-testid="sensitive-words-textarea"]')).toBeNull()

    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ cloak_sensitive_words: null }),
    )
  })

  it('loads an existing override and submits the deduped array', async () => {
    const root = mountDialog('claude_code', createKey({ cloak_sensitive_words: ['proxy', 'api'] }))
    await settle()
    expect(root.querySelector<HTMLInputElement>('[data-testid="sensitive-words-override"]')?.checked).toBe(true)
    expect(root.querySelector<HTMLTextAreaElement>('[data-testid="sensitive-words-textarea"]')?.value).toBe('proxy\napi')

    await setTextarea(root, 'Proxy\nPROXY\nrelay')
    expect(root.querySelector('[data-testid="sensitive-words-summary"]')?.textContent).toContain('2 / 256')

    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ cloak_sensitive_words: ['Proxy', 'relay'] }),
    )
  })

  it('surfaces cache invalidation warnings returned after saving', async () => {
    endpointMocks.updateProviderKey.mockResolvedValueOnce(createKey({
      cloak_sensitive_words: ['relay'],
      warnings: ['敏感词词表已变更，提示词缓存将失效'],
    }))
    const root = mountDialog('claude_code', createKey({ cloak_sensitive_words: ['proxy'] }))
    await settle()
    await setTextarea(root, 'relay')
    clickSave(root)
    await settle()

    expect(endpointMocks.toastWarning).toHaveBeenCalledExactlyOnceWith('敏感词词表已变更，提示词缓存将失效', '提示')
    expect(endpointMocks.toastError).not.toHaveBeenCalled()
  })

  it('submits an empty array when override is chosen with no words (obfuscation off)', async () => {
    const root = mountDialog('antigravity', createKey({ cloak_sensitive_words: null }))
    await settle()
    await chooseMode(root, 'override')
    expect(root.querySelector('[data-testid="sensitive-words-textarea"]')).not.toBeNull()

    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ cloak_sensitive_words: [] }),
    )
  })

  it('blocks saving while a word is too short', async () => {
    const root = mountDialog('claude_code', createKey({ cloak_sensitive_words: ['proxy'] }))
    await settle()
    await setTextarea(root, 'proxy\nq')
    expect(root.querySelector('[data-testid="sensitive-words-error"]')?.textContent).toContain('过短')

    const save = [...root.querySelectorAll<HTMLButtonElement>('button')]
      .find(candidate => candidate.textContent?.trim() === '保存')
    expect(save?.disabled).toBe(true)
    expect(endpointMocks.updateProviderKey).not.toHaveBeenCalled()
  })

  it('limits words to 256 Unicode scalars while preserving Unicode case distinctions', async () => {
    const root = mountDialog('claude_code', createKey({ cloak_sensitive_words: [] }))
    await settle()
    await setTextarea(root, '🙂'.repeat(257))
    expect(root.querySelector('[data-testid="sensitive-words-error"]')?.textContent).toContain('最多 256')
    const save = [...root.querySelectorAll<HTMLButtonElement>('button')]
      .find(candidate => candidate.textContent?.trim() === '保存')
    expect(save?.disabled).toBe(true)
    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).not.toHaveBeenCalled()

    const longest = '🙂'.repeat(256)
    await setTextarea(root, `${longest}\nİx\ni\u0307x\nΣx\nςx`)
    expect(root.querySelector('[data-testid="sensitive-words-error"]')).toBeNull()
    expect(save?.disabled).toBe(false)
    clickSave(root)
    await settle()
    expect(endpointMocks.updateProviderKey).toHaveBeenCalledWith(
      'key-claude-1',
      expect.objectContaining({ cloak_sensitive_words: [longest, 'İx', 'i\u0307x', 'Σx'] }),
    )
  })

  it('omits the field entirely for provider types without obfuscation', async () => {
    const root = mountDialog('codex', createKey())
    await settle()
    clickSave(root)
    await settle()
    const payload = endpointMocks.updateProviderKey.mock.calls[0]?.[1] as Record<string, unknown>
    expect(payload).toBeDefined()
    expect('cloak_sensitive_words' in payload).toBe(false)
  })
})
