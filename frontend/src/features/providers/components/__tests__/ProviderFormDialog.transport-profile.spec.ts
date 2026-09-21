import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'

import type { ProviderWithEndpointsSummary } from '@/api/endpoints/types'
import {
  normalizeTransportProfile,
  providerTypeSupportsTransportProfile,
  transportProfileOptionsForProviderType,
} from '@/api/endpoints/types/provider'
import ProviderFormDialog from '../ProviderFormDialog.vue'

const endpointMocks = vi.hoisted(() => ({
  createProvider: vi.fn(),
  updateProvider: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
  toastWarning: vi.fn(),
}))

vi.mock('@/api/endpoints', () => ({
  createProvider: endpointMocks.createProvider,
  updateProvider: endpointMocks.updateProvider,
  normalizePoolAdvancedConfig: (value: unknown) => {
    if (value == null || value === false) return null
    if (value === true) return {}
    if (typeof value !== 'object' || Array.isArray(value)) return null
    return { ...value }
  },
}))

vi.mock('@/components/ui', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/components/ui')>()
  const { defineComponent, h } = await import('vue')
  const passthrough = (name: string) => defineComponent({
    name,
    setup: (_props, { slots }) => () => slots.default?.(),
  })

  return {
    ...actual,
    Select: defineComponent({
      name: 'SelectStub',
      props: { modelValue: String, disabled: Boolean },
      emits: ['update:modelValue'],
      setup: (props, { emit, slots }) => () => h('select', {
        value: props.modelValue,
        disabled: props.disabled,
        onChange: (event: Event) => emit('update:modelValue', (event.target as HTMLSelectElement).value),
      }, slots.default?.()),
    }),
    SelectTrigger: passthrough('SelectTriggerStub'),
    SelectValue: passthrough('SelectValueStub'),
    SelectContent: passthrough('SelectContentStub'),
    SelectItem: defineComponent({
      name: 'SelectItemStub',
      props: { value: { type: String, required: true }, disabled: Boolean },
      setup: (props, { slots }) => () => h('option', { value: props.value, disabled: props.disabled }, slots.default?.()),
    }),
  }
})

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({
    success: endpointMocks.toastSuccess,
    error: endpointMocks.toastError,
    warning: endpointMocks.toastWarning,
  }),
}))

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

function makeProvider(overrides: Partial<ProviderWithEndpointsSummary> = {}): ProviderWithEndpointsSummary {
  return {
    id: 'provider-1',
    name: 'Claude Provider',
    provider_type: 'claude_code',
    provider_priority: 100,
    keep_priority_on_conversion: false,
    enable_format_conversion: true,
    max_transfer_count: 0,
    max_transfer_timeout_seconds: 0,
    is_active: true,
    total_endpoints: 0,
    active_endpoints: 0,
    total_keys: 0,
    active_keys: 0,
    total_models: 0,
    active_models: 0,
    global_model_ids: [],
    avg_health_score: 1,
    unhealthy_endpoints: 0,
    api_formats: [],
    endpoint_health_details: [],
    ops_configured: false,
    created_at: '2026-09-20T00:00:00Z',
    updated_at: '2026-09-20T00:00:00Z',
    ...overrides,
  }
}

function mountDialog(provider?: ProviderWithEndpointsSummary | null) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(ProviderFormDialog, {
    modelValue: true,
    provider,
    'onUpdate:modelValue': vi.fn(),
  })
  app.mount(root)
  mountedApps.push({ app, root })
}

async function settle() {
  for (let index = 0; index < 4; index += 1) {
    await nextTick()
    await Promise.resolve()
  }
}


function clickButton(label: string) {
  const button = [...document.body.querySelectorAll<HTMLButtonElement>('button')]
    .find(candidate => candidate.textContent?.trim() === label)
  if (!button) throw new Error(`Missing button ${label}`)
  button.click()
}

beforeEach(() => {
  for (const mock of Object.values(endpointMocks)) mock.mockReset()
  endpointMocks.updateProvider.mockImplementation(async (_id: string, payload: Record<string, unknown>) => ({
    ...makeProvider(),
    ...payload,
  }))
  endpointMocks.createProvider.mockResolvedValue({ id: 'provider-new', name: 'created' })
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

async function selectTransportProfile(value: string) {
  const select = document.body.querySelector<HTMLSelectElement>('[data-testid="transport-profile-setting"] select')
  if (!select) throw new Error('Missing transport profile select')
  select.value = value
  select.dispatchEvent(new Event('change', { bubbles: true }))
  await settle()
}

describe('transport profile helpers', () => {
  it('normalizes builtin ids case- and separator-insensitively and rejects unknown ones', () => {
    expect(normalizeTransportProfile('Claude-Code-Node-OpenSSL')).toBe('claude_code_node_openssl')
    expect(normalizeTransportProfile(' chatgpt_com_chrome ')).toBe('chatgpt_com_chrome')
    expect(normalizeTransportProfile('claude_code_oauth_control_plane')).toBeNull()
    expect(normalizeTransportProfile('chrome_136')).toBeNull()
    expect(normalizeTransportProfile(null)).toBeNull()
  })

  it('offers the node profile only to claude_code and chrome to claude_code / codex', () => {
    expect(transportProfileOptionsForProviderType('claude_code')).toEqual(['claude_code_node_openssl', 'chatgpt_com_chrome'])
    expect(transportProfileOptionsForProviderType('codex')).toEqual(['chatgpt_com_chrome'])
    expect(transportProfileOptionsForProviderType('gemini_cli')).toEqual([])
    expect(providerTypeSupportsTransportProfile('CODEX')).toBe(true)
    expect(providerTypeSupportsTransportProfile('antigravity')).toBe(false)
  })
})

describe('ProviderFormDialog transport profile group', () => {
  it('shows the group only for claude_code / codex and reads back the saved profile', async () => {
    mountDialog(makeProvider({ provider_type: 'antigravity' }))
    await settle()
    expect(document.body.querySelector('[data-testid="transport-profile-setting"]')).toBeNull()
    for (const { app, root } of mountedApps.splice(0)) {
      app.unmount()
      root.remove()
    }

    mountDialog(makeProvider({ provider_type: 'claude_code', transport_profile: 'claude_code_node_openssl' }))
    await settle()
    const select = document.body.querySelector<HTMLSelectElement>('[data-testid="transport-profile-setting"] select')
    expect(select).not.toBeNull()
    expect(select?.value).toBe('claude_code_node_openssl')
    const options = [...(select?.querySelectorAll('option') ?? [])].map(option => option.value)
    expect(options).toEqual(['__system_default__', 'claude_code_node_openssl', 'chatgpt_com_chrome'])
    for (const { app, root } of mountedApps.splice(0)) {
      app.unmount()
      root.remove()
    }

    mountDialog(makeProvider({ provider_type: 'codex', transport_profile: null }))
    await settle()
    const codexSelect = document.body.querySelector<HTMLSelectElement>('[data-testid="transport-profile-setting"] select')
    expect(codexSelect?.value).toBe('__system_default__')
    expect([...(codexSelect?.querySelectorAll('option') ?? [])].map(option => option.value))
      .toEqual(['__system_default__', 'chatgpt_com_chrome'])
  })

  it('submits the selected profile and null for the system default', async () => {
    mountDialog(makeProvider({ provider_type: 'claude_code', transport_profile: null }))
    await settle()

    await selectTransportProfile('claude_code_node_openssl')
    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).toHaveBeenCalledWith(
      'provider-1',
      expect.objectContaining({ transport_profile: 'claude_code_node_openssl' }),
    )

    endpointMocks.updateProvider.mockClear()
    await selectTransportProfile('__system_default__')
    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).toHaveBeenCalledWith(
      'provider-1',
      expect.objectContaining({ transport_profile: null }),
    )
  })

  it('does not send transport_profile for provider types without options', async () => {
    mountDialog(makeProvider({ provider_type: 'antigravity' }))
    await settle()
    clickButton('保存')
    await settle()
    const payload = endpointMocks.updateProvider.mock.calls[0]?.[1] as Record<string, unknown>
    expect(payload).toBeDefined()
    expect('transport_profile' in payload).toBe(false)
  })
})
