import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'

import type { ProviderWithEndpointsSummary } from '@/api/endpoints/types'
import {
  extractProviderWriteWarnings,
  normalizeSensitiveWordList,
  parseSensitiveWordListText,
  sensitiveWordListsEqual,
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

async function setTextarea(selector: string, value: string) {
  const textarea = document.body.querySelector<HTMLTextAreaElement>(selector)
  if (!textarea) throw new Error(`Missing textarea ${selector}`)
  textarea.value = value
  textarea.dispatchEvent(new Event('input', { bubbles: true }))
  await settle()
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

describe('sensitive word list helpers', () => {
  it('dedupes case-insensitively, drops blanks and flags short words', () => {
    const parsed = parseSensitiveWordListText('Proxy\n\n api \nPROXY\na\n')
    expect(parsed.words).toEqual(['Proxy', 'api'])
    expect(parsed.tooShort).toEqual(['a'])
    expect(parsed.tooMany).toBe(false)
    expect(parsed.zeroWidth).toEqual([])
  })

  it('flags zero-width characters and the 256 entry limit', () => {
    expect(parseSensitiveWordListText('p​roxy').zeroWidth).toEqual(['p​roxy'])
    const many = Array.from({ length: 257 }, (_, index) => `word${index}`).join('\n')
    expect(parseSensitiveWordListText(many).tooMany).toBe(true)
  })

  it('compares word lists ignoring case and order', () => {
    expect(sensitiveWordListsEqual(['Proxy', 'api'], ['API', 'proxy'])).toBe(true)
    expect(sensitiveWordListsEqual(['proxy'], ['proxy', 'api'])).toBe(false)
  })

  it('uses Unicode simple case folding without expanding dotted I', () => {
    const words = ['İx', 'i\u0307x', 'Σx', 'ςx', 'σX', 'ſx', 'SX', 'Kx', 'kx', 'ẞx', 'ßX', 'ıx', 'Ix']
    const expected = ['İx', 'i\u0307x', 'Σx', 'ſx', 'Kx', 'ẞx', 'ıx', 'Ix']
    expect(parseSensitiveWordListText(words.join('\n')).words).toEqual(expected)
    expect(normalizeSensitiveWordList(words)).toEqual(expected)
    expect(sensitiveWordListsEqual(['Σx', 'ſx', 'Kx', 'ẞx'], ['ςX', 'sx', 'kX', 'ßX'])).toBe(true)
    expect(sensitiveWordListsEqual(['Σx', 'ςx'], ['σX'])).toBe(true)
    expect(sensitiveWordListsEqual(['İx'], ['i\u0307x'])).toBe(false)
    expect(sensitiveWordListsEqual(['ıx'], ['Ix'])).toBe(false)
  })

  it('dedupes words containing regex syntax as literals', () => {
    const words = ['a.b', 'axb', '[api]', 'API', 'a+b', 'a\\b', 'a$b', 'a^b', 'api?', 'a{2}']
    expect(parseSensitiveWordListText(words.join('\n')).words).toEqual(words)
    expect(sensitiveWordListsEqual(['a.b'], ['axb'])).toBe(false)
    expect(sensitiveWordListsEqual(['[API]'], ['[api]'])).toBe(true)
  })

  it('counts Unicode scalars for both word length limits', () => {
    const longest = '🙂'.repeat(256)
    const tooLong = '🙂'.repeat(257)
    const parsed = parseSensitiveWordListText(`🙂\n🙂🙂\n${longest}\n${tooLong}`)
    expect(parsed.words).toEqual(['🙂🙂', longest])
    expect(parsed.tooShort).toEqual(['🙂'])
    expect(parsed.tooLong).toEqual([tooLong])
    expect(parsed.tooMany).toBe(false)
  })

  it('extracts sibling warnings from write responses', () => {
    expect(extractProviderWriteWarnings({ id: 'p', warnings: ['提示词缓存将失效', '', 3] })).toEqual(['提示词缓存将失效'])
    expect(extractProviderWriteWarnings({ id: 'p' })).toEqual([])
    expect(extractProviderWriteWarnings(null)).toEqual([])
  })
})

describe('ProviderFormDialog sensitive word obfuscation group', () => {
  it('shows the group only for claude_code and antigravity providers', async () => {
    mountDialog(makeProvider({ provider_type: 'codex' }))
    await settle()
    expect(document.body.querySelector('[data-testid="sensitive-words-setting"]')).toBeNull()

    for (const { app, root } of mountedApps.splice(0)) {
      app.unmount()
      root.remove()
    }

    mountDialog(makeProvider({ provider_type: 'antigravity', cloak_sensitive_words: ['proxy'] }))
    await settle()
    expect(document.body.querySelector('[data-testid="sensitive-words-setting"]')).not.toBeNull()
    expect(document.body.querySelector<HTMLTextAreaElement>('#cloak-sensitive-words')?.value).toBe('proxy')
  })

  it('loads the saved list, dedupes on submit and warns about the prompt cache when it changed', async () => {
    mountDialog(makeProvider({ cloak_sensitive_words: ['proxy'] }))
    await settle()

    await setTextarea('#cloak-sensitive-words', 'Proxy\napi\nAPI\n\nproxy')
    expect(document.body.querySelector('[data-testid="sensitive-words-summary"]')?.textContent).toContain('2 / 256')
    expect(document.body.querySelector('[data-testid="sensitive-words-summary"]')?.textContent).toContain('词表变更会使提示词缓存失效')

    clickButton('保存')
    await settle()

    expect(endpointMocks.updateProvider).toHaveBeenCalledWith(
      'provider-1',
      expect.objectContaining({
        provider_type: 'claude_code',
        cloak_sensitive_words: ['Proxy', 'api'],
      }),
    )
    expect(endpointMocks.toastWarning).toHaveBeenCalledWith('敏感词词表已变更，提示词缓存将失效', '提示')
  })

  it('surfaces backend warnings from the write response', async () => {
    endpointMocks.updateProvider.mockResolvedValueOnce({
      ...makeProvider({ cloak_sensitive_words: ['proxy'] }),
      warnings: ['敏感词词表已变更，提示词缓存将失效'],
    })
    mountDialog(makeProvider({ cloak_sensitive_words: ['proxy'] }))
    await settle()

    clickButton('保存')
    await settle()

    expect(endpointMocks.toastWarning).toHaveBeenCalledTimes(1)
    expect(endpointMocks.toastWarning).toHaveBeenCalledWith('敏感词词表已变更，提示词缓存将失效', '提示')
  })

  it('does not warn when the list is unchanged', async () => {
    mountDialog(makeProvider({ cloak_sensitive_words: ['proxy', 'api'] }))
    await settle()

    await setTextarea('#cloak-sensitive-words', 'API\nProxy')
    expect(document.body.querySelector('[data-testid="sensitive-words-summary"]')?.textContent)
      .not.toContain('词表变更会使提示词缓存失效')

    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).toHaveBeenCalled()
    expect(endpointMocks.toastWarning).not.toHaveBeenCalled()
  })

  it('rejects words shorter than two characters before submitting', async () => {
    mountDialog(makeProvider())
    await settle()

    await setTextarea('#cloak-sensitive-words', 'proxy\nx')
    const error = document.body.querySelector('[data-testid="sensitive-words-error"]')
    expect(error?.textContent).toContain('过短')
    expect(error?.textContent).toContain('x')
    expect(document.body.querySelector<HTMLTextAreaElement>('#cloak-sensitive-words')?.getAttribute('aria-invalid')).toBe('true')

    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).not.toHaveBeenCalled()
    expect(endpointMocks.toastError).toHaveBeenCalledWith(expect.stringContaining('过短'), '验证失败')
  })

  it('rejects more than 256 entries', async () => {
    mountDialog(makeProvider())
    await settle()

    const many = Array.from({ length: 257 }, (_, index) => `word${index}`).join('\n')
    await setTextarea('#cloak-sensitive-words', many)
    expect(document.body.querySelector('[data-testid="sensitive-words-error"]')?.textContent).toContain('256')

    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).not.toHaveBeenCalled()
  })

  it('rejects words over 256 Unicode scalars and accepts the boundary', async () => {
    mountDialog(makeProvider())
    await settle()

    await setTextarea('#cloak-sensitive-words', '🙂'.repeat(257))
    expect(document.body.querySelector('[data-testid="sensitive-words-error"]')?.textContent).toContain('最多 256')
    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).not.toHaveBeenCalled()

    const longest = '🙂'.repeat(256)
    await setTextarea('#cloak-sensitive-words', longest)
    expect(document.body.querySelector('[data-testid="sensitive-words-error"]')).toBeNull()
    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).toHaveBeenCalledWith(
      'provider-1',
      expect.objectContaining({ cloak_sensitive_words: [longest] }),
    )
  })

  it('submits an empty list to clear the words and omits the field for other provider types', async () => {
    mountDialog(makeProvider({ cloak_sensitive_words: ['proxy'] }))
    await settle()

    await setTextarea('#cloak-sensitive-words', '')
    clickButton('保存')
    await settle()
    expect(endpointMocks.updateProvider).toHaveBeenCalledWith(
      'provider-1',
      expect.objectContaining({ cloak_sensitive_words: [] }),
    )

    for (const { app, root } of mountedApps.splice(0)) {
      app.unmount()
      root.remove()
    }
    endpointMocks.updateProvider.mockClear()

    mountDialog(makeProvider({ provider_type: 'codex' }))
    await settle()
    clickButton('保存')
    await settle()
    const payload = endpointMocks.updateProvider.mock.calls[0]?.[1] as Record<string, unknown>
    expect(payload).toBeDefined()
    expect('cloak_sensitive_words' in payload).toBe(false)
  })
})
