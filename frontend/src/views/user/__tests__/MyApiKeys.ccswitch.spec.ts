import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'

import MyApiKeys from '../MyApiKeys.vue'

const toastMock = vi.hoisted(() => ({
  success: vi.fn(),
  error: vi.fn(),
}))

const meApiMock = vi.hoisted(() => ({
  getApiKeys: vi.fn(),
  createApiKey: vi.fn(),
  getFullApiKey: vi.fn(),
  getClientConfig: vi.fn(),
  getAvailableModels: vi.fn(),
  getAvailableProviders: vi.fn(),
  createApiKeyInstallSession: vi.fn(),
  updateApiKey: vi.fn(),
  updateApiKeyProviders: vi.fn(),
  deleteApiKey: vi.fn(),
  toggleApiKey: vi.fn(),
}))

vi.mock('@/api/me', () => ({
  meApi: meApiMock,
}))

vi.mock('@/composables/useToast', () => ({
  useToast: () => toastMock,
}))

vi.mock('@/components/common', async () => {
  const { defineComponent, h } = await import('vue')

  return {
    LoadingState: defineComponent({
      props: { message: String },
      setup: props => () => h('div', props.message || 'loading'),
    }),
    EmptyState: defineComponent({
      props: { title: String, description: String, icon: [Object, Function] },
      setup: (props, { slots }) => () => h('div', [
        h('div', props.title || ''),
        h('div', props.description || ''),
        slots.actions?.(),
      ]),
    }),
    AlertDialog: defineComponent({
      emits: ['confirm', 'cancel'],
      setup: () => () => null,
    }),
    MultiSelect: defineComponent({
      props: {
        modelValue: { type: Array, default: () => [] },
        options: { type: Array, default: () => [] },
        disabled: Boolean,
        placeholder: String,
      },
      emits: ['update:modelValue'],
      setup: (props, { attrs, emit }) => () => h('button', {
        'data-testid': attrs['data-testid'] || 'multi-select',
        'data-disabled': props.disabled ? 'true' : 'false',
        disabled: props.disabled,
        onClick: () => {
          const firstOption = props.options[0] as { value?: unknown } | undefined
          if (typeof firstOption?.value === 'string') {
            emit('update:modelValue', [firstOption.value])
          }
        },
      }, props.placeholder || 'multi-select'),
    }),
  }
})

vi.mock('@/utils/logger', () => ({
  log: {
    error: vi.fn(),
    warn: vi.fn(),
    info: vi.fn(),
  },
}))

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

function apiKey(overrides: Record<string, unknown> = {}) {
  return {
    id: 'user-key-1',
    name: 'primary',
    key_display: 'sk-user...live',
    is_active: true,
    is_locked: false,
    created_at: '2026-05-29T00:00:00+00:00',
    total_requests: 0,
    total_cost_usd: 0,
    rate_limit: 0,
    concurrent_limit: 0,
    ip_rules: null,
    ...overrides,
  }
}

async function flushPromises() {
  await nextTick()
  await new Promise(resolve => setTimeout(resolve, 0))
  await nextTick()
}

async function mountMyApiKeys() {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(MyApiKeys)
  app.mount(root)
  mountedApps.push({ app, root })
  await flushPromises()
  return root
}

beforeEach(() => {
  vi.clearAllMocks()
  meApiMock.getClientConfig.mockResolvedValue({
    base_url: 'https://aether.example.com',
    site_name: 'Aether Local',
  })
  meApiMock.getAvailableProviders.mockImplementation(
    (options?: { view?: string }) => Promise.resolve(options?.view === 'access-options'
      ? [
          {
            id: 'provider-openai',
            name: 'OpenAI',
            endpoints: [{ api_format: 'openai:chat' }],
          },
          {
            id: 'provider-claude',
            name: 'Claude',
            endpoints: [{ api_format: 'claude:messages' }],
          },
        ]
      : [
          { id: 'provider-openai', name: 'OpenAI' },
          { id: 'provider-claude', name: 'Claude' },
        ]),
  )
  meApiMock.getAvailableModels.mockResolvedValue({
    models: [
      { id: 'gm-1', name: 'claude-haiku-4', display_name: 'Claude Haiku 4', is_active: true },
      { id: 'gm-2', name: 'claude-sonnet-4', display_name: 'Claude Sonnet 4', is_active: true },
      { id: 'gm-3', name: 'claude-opus-4', display_name: 'Claude Opus 4', is_active: true },
      { id: 'gm-4', name: 'gpt-5', display_name: 'GPT 5', is_active: true },
    ],
    total: 4,
  })
  meApiMock.createApiKeyInstallSession.mockResolvedValue({
    install_code: 'install-code',
    expires_at_unix_secs: 1,
    expires_in_seconds: 900,
    target_cli: 'claude_code',
    target_cli_label: 'Claude Code',
    target_system: 'linux',
    target_system_label: 'Linux',
    unix_command: 'curl install',
    powershell_command: 'irm install',
  })
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
  document.body.innerHTML = ''
})

describe('MyApiKeys CC Switch import', () => {
  it('opens the import dialog for an existing key without fetching the full key immediately', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-open-user-key-1"]')?.click()
    await flushPromises()

    expect(meApiMock.getFullApiKey).not.toHaveBeenCalled()
    expect(document.body.textContent).toContain('导入到 CC Switch')
    expect(document.querySelector<HTMLInputElement>('[data-testid="ccswitch-provider-name"]')?.value).toBe('Aether Local')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-haiku"]')?.textContent).toContain('claude-haiku-4')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-sonnet"]')?.textContent).toContain('claude-sonnet-4')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-opus"]')?.textContent).toContain('claude-opus-4')
  })

  it('switches non-Claude targets to a single default model without changing the site provider name', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-open-user-key-1"]')?.click()
    await flushPromises()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-target-codex"]')?.click()
    await flushPromises()

    expect(document.querySelector<HTMLInputElement>('[data-testid="ccswitch-provider-name"]')?.value).toBe('Aether Local')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-default"]')?.textContent).toContain('gpt-5')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-haiku"]')).toBeNull()
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-sonnet"]')).toBeNull()
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-opus"]')).toBeNull()
  })

  it('shows a specific message when an existing key cannot return full key material', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])
    meApiMock.getFullApiKey.mockRejectedValue({
      response: { data: { detail: '该密钥没有存储完整密钥信息' } },
    })

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-open-user-key-1"]')?.click()
    await flushPromises()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-confirm"]')?.click()
    await flushPromises()

    expect(toastMock.error).toHaveBeenCalledWith('该密钥缺少完整密钥信息，请重新创建 API Key')
  })

  it('does not fall back to an unlisted model when no available models are returned', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])
    meApiMock.getAvailableModels.mockResolvedValueOnce({
      models: [],
      total: 0,
    })

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-open-user-key-1"]')?.click()
    await flushPromises()

    const confirmButton = document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-confirm"]')
    expect(document.body.textContent).toContain('暂无可用模型，请联系管理员配置可用模型后再导入。')
    expect(document.querySelector<HTMLElement>('[data-testid="ccswitch-model-select-haiku"]')?.textContent).not.toContain('gpt-5')
    expect(confirmButton?.disabled).toBe(true)

    confirmButton?.click()
    await flushPromises()

    expect(meApiMock.getFullApiKey).not.toHaveBeenCalled()
  })

  it('can open CC Switch import from the newly created key dialog without refetching the key', async () => {
    const createdKey = apiKey({ id: 'created-key-1', name: 'new key', key: 'sk-created-live' })
    meApiMock.getApiKeys.mockResolvedValueOnce([]).mockResolvedValue([createdKey])
    meApiMock.createApiKey.mockResolvedValue(createdKey)

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[title="创建新 API Key"]')?.click()
    await flushPromises()

    const nameInput = document.querySelector<HTMLInputElement>('#key-name')
    nameInput!.value = 'new key'
    nameInput!.dispatchEvent(new Event('input', { bubbles: true }))
    await flushPromises()

    Array.from(document.querySelectorAll<HTMLButtonElement>('button'))
      .find(button => button.textContent?.trim() === '创建')
      ?.click()
    await flushPromises()

    document.querySelector<HTMLButtonElement>('[data-testid="ccswitch-open-created-key"]')?.click()
    await flushPromises()

    expect(meApiMock.getFullApiKey).not.toHaveBeenCalled()
    expect(document.body.textContent).toContain('导入到 CC Switch')
  })

  it('sends the desired inactive state when disabling an active key', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])
    meApiMock.toggleApiKey.mockResolvedValue({
      id: 'user-key-1',
      is_active: false,
    })

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[title="禁用"]')?.click()
    await flushPromises()

    expect(meApiMock.toggleApiKey).toHaveBeenCalledWith('user-key-1', false)
    expect(toastMock.success).toHaveBeenCalledWith('密钥已禁用')
  })

  it('omits unchanged provider restrictions from a name-only edit', async () => {
    meApiMock.getApiKeys.mockResolvedValue([
      apiKey({ allowed_providers: ['provider-openai'] }),
    ])
    meApiMock.updateApiKey.mockResolvedValue({ message: 'updated' })

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[title="编辑"]')?.click()
    await flushPromises()

    const nameInput = document.querySelector<HTMLInputElement>('#key-name')
    nameInput!.value = 'renamed'
    nameInput!.dispatchEvent(new Event('input', { bubbles: true }))
    await flushPromises()
    Array.from(document.querySelectorAll<HTMLButtonElement>('button'))
      .find(button => button.textContent?.trim() === '保存')
      ?.click()
    await flushPromises()

    expect(meApiMock.updateApiKey).toHaveBeenCalledTimes(1)
    const [, payload] = meApiMock.updateApiKey.mock.calls[0]
    expect(payload).not.toHaveProperty('allowed_providers')
    expect(payload).not.toHaveProperty('allowed_api_formats')
    expect(payload).not.toHaveProperty('allowed_models')
    expect(payload).toMatchObject({ name: 'renamed' })
  })

  it('submits provider, endpoint, and model restrictions when creating a key', async () => {
    const createdKey = apiKey({
      id: 'restricted-key-1',
      key: 'sk-created-live',
      allowed_providers: ['provider-openai'],
      allowed_api_formats: ['openai:chat'],
      allowed_models: ['gpt-5'],
    })
    meApiMock.getApiKeys.mockResolvedValue([])
    meApiMock.getAvailableModels.mockResolvedValue({
      models: [
        { id: 'gm-4', name: 'gpt-5', display_name: 'GPT 5', is_active: true },
      ],
      total: 1,
    })
    meApiMock.getAvailableProviders.mockImplementation(
      (options?: { view?: string }) => Promise.resolve(options?.view === 'access-options'
        ? [{
            id: 'provider-openai',
            name: 'OpenAI',
            endpoints: [{ api_format: 'openai:chat' }],
          }]
        : [{ id: 'provider-openai', name: 'OpenAI' }]),
    )
    meApiMock.createApiKey.mockResolvedValue(createdKey)

    await mountMyApiKeys()
    document.querySelector<HTMLButtonElement>('[title="创建新 API Key"]')?.click()
    await flushPromises()

    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-providers-unrestricted"]')?.click()
    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-api-formats-unrestricted"]')?.click()
    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-models-unrestricted"]')?.click()
    await flushPromises()

    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-providers"]')?.click()
    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-api-formats"]')?.click()
    document.querySelector<HTMLButtonElement>('[data-testid="user-api-key-models"]')?.click()
    const nameInput = document.querySelector<HTMLInputElement>('#key-name')
    nameInput!.value = 'restricted key'
    nameInput!.dispatchEvent(new Event('input', { bubbles: true }))
    await flushPromises()

    Array.from(document.querySelectorAll<HTMLButtonElement>('button'))
      .find(button => button.textContent?.trim() === '创建')
      ?.click()
    await flushPromises()

    expect(meApiMock.createApiKey).toHaveBeenCalledWith(expect.objectContaining({
      name: 'restricted key',
      allowed_providers: ['provider-openai'],
      allowed_api_formats: ['openai:chat'],
      allowed_models: ['gpt-5'],
    }))
  })

  it('caches empty access-control option responses when dialogs are reopened', async () => {
    meApiMock.getApiKeys.mockResolvedValue([apiKey()])
    meApiMock.getAvailableProviders.mockResolvedValue([])

    await mountMyApiKeys()
    expect(meApiMock.getAvailableProviders).toHaveBeenCalledTimes(1)

    document.querySelector<HTMLButtonElement>('[title="编辑"]')?.click()
    await flushPromises()
    expect(meApiMock.getAvailableProviders).toHaveBeenCalledTimes(2)

    document.querySelector<HTMLButtonElement>('[title="编辑"]')?.click()
    await flushPromises()

    expect(meApiMock.getAvailableProviders).toHaveBeenCalledTimes(2)
  })
})
