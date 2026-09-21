import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, ref, type App } from 'vue'
import type { RequestDetail } from '@/api/dashboard'
import RequestDetailDrawer from '../RequestDetailDrawer.vue'

const ZW = '\u200B'
const mocks = vi.hoisted(() => ({
  getRequestDetail: vi.fn(),
  getRequestBody: vi.fn(),
  getCurlData: vi.fn(),
  load: vi.fn(),
  copyToClipboard: vi.fn(),
  toastWarning: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}))
vi.mock('@/composables/useClipboard', () => ({ useClipboard: () => ({ copyToClipboard: mocks.copyToClipboard }) }))
vi.mock('@/composables/useToast', () => ({ useToast: () => ({
  success: mocks.toastSuccess, error: mocks.toastError, warning: mocks.toastWarning, info: vi.fn(), showToast: vi.fn(), toast: vi.fn(),
}) }))
vi.mock('@/api/dashboard', async importOriginal => {
  const actual = await importOriginal<typeof import('@/api/dashboard')>()
  return { ...actual, dashboardApi: { ...actual.dashboardApi, getRequestDetail: mocks.getRequestDetail, getRequestBody: mocks.getRequestBody, getCurlData: mocks.getCurlData } }
})
vi.mock('../../utils/body-document', () => ({ BodyDocument: { load: mocks.load } }))
vi.mock('../HorizontalRequestTimeline.vue', () => ({ default: { render: () => null } }))
vi.mock('../RequestDetailDrawer/JsonContent.vue', async () => {
  const { defineComponent, h } = await import('vue')
  return { default: defineComponent({
    props: { data: { type: null, default: null }, bodyDocument: { type: Object, default: null } },
    setup: props => () => h('pre', { 'data-testid': 'captured-body' }, JSON.stringify(props.bodyDocument?.display ?? props.data)),
  }) }
})

const mountedApps: Array<{ app: App, root: HTMLElement }> = []
function body(value: unknown) { return { bytes: new TextEncoder().encode(JSON.stringify(value)).buffer, encoding: 'json' as const } }

function buildDetail(metadata?: Record<string, unknown>): RequestDetail {
  return {
    id: 'usage-zwsp', request_id: 'req-zwsp',
    user: { id: 'user-1', username: 'test-user', email: 'test@example.com' },
    api_key: { id: 'key-1', name: 'test-key', display: 'test-key' },
    provider: 'test-provider', api_format: 'claude:messages', model: 'test-model',
    tokens: { input: 10, output: 20, total: 30 }, cost: { input: 0, output: 0, total: 0 },
    request_type: 'chat', is_stream: false, status: 'completed', status_code: 200,
    response_time_ms: 10, created_at: '2026-09-20T00:00:00Z',
    request_headers: { 'content-type': 'application/json' },
    has_request_body: true, has_provider_request_body: false,
    has_response_body: true, has_client_response_body: false,
    metadata,
  }
}

beforeEach(() => {
  mocks.getRequestDetail.mockImplementation(async id => ({ ...buildDetail(), id, request_id: `req-${id}` }))
  mocks.getRequestBody.mockImplementation(async () => body({ system: `p${ZW}roxy` }))
  mocks.getCurlData.mockResolvedValue({ curl: `curl -d '{"system":"p${ZW}roxy A${ZW}PI"}' https://example.test` })
  mocks.copyToClipboard.mockResolvedValue(true)
  mocks.load.mockImplementation(async (bytes: ArrayBuffer, _encoding, signal: AbortSignal) => {
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError')
    const display = JSON.parse(new TextDecoder().decode(bytes))
    const raw = JSON.stringify(display, null, 2)
    return {
      display, byteLength: bytes.byteLength, dispose: vi.fn(),
      copy: vi.fn(async (_conversation: unknown, options?: { stripZeroWidth?: boolean }) => options?.stripZeroWidth ? raw.replace(/\u200B/g, '') : raw),
    }
  })
})
afterEach(async () => {
  for (const { app, root } of mountedApps.splice(0)) { app.unmount(); root.remove() }
  await nextTick()
  document.body.replaceChildren()
  vi.resetAllMocks()
})

async function openDrawer() {
  const isOpen = ref(false)
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(defineComponent({ setup: () => () => h(RequestDetailDrawer, { isOpen: isOpen.value, requestId: 'usage-zwsp' }) }))
  app.mount(root)
  mountedApps.push({ app, root })
  isOpen.value = true
  await nextTick()
  await vi.waitFor(() => expect(findButton('请求体')).toBeDefined())
}
function findButton(label: string) {
  return [...document.body.querySelectorAll('button')].find(button => button.textContent?.trim() === label)
}
function query<T extends HTMLElement>(selector: string) { return document.body.querySelector<T>(selector) }

describe('RequestDetailDrawer zero width copy actions (F5)', () => {
  it('copies raw bytes by default, warns about the count and then offers stripping', async () => {
    await openDrawer()
    findButton('请求体')!.click()
    await vi.waitFor(() => expect(query('[data-testid="captured-body"]')?.textContent).toContain('roxy'))
    expect(query('[data-testid="copy-body-strip-zwsp"]')).toBeNull()

    query<HTMLButtonElement>('button[title="复制"]')!.click()
    await vi.waitFor(() => expect(mocks.copyToClipboard).toHaveBeenCalledOnce())
    const [copied] = mocks.copyToClipboard.mock.calls[0]
    expect(copied).toContain(ZW)
    expect(mocks.toastWarning).toHaveBeenCalledWith(expect.stringContaining('1 个零宽字符'))

    await vi.waitFor(() => expect(query('[data-testid="copy-body-strip-zwsp"]')).not.toBeNull())
    query<HTMLButtonElement>('[data-testid="copy-body-strip-zwsp"]')!.click()
    await vi.waitFor(() => expect(mocks.copyToClipboard).toHaveBeenCalledTimes(2))
    const [stripped] = mocks.copyToClipboard.mock.calls[1]
    expect(stripped).not.toContain(ZW)
    expect(JSON.parse(stripped)).toEqual({ system: 'proxy' })
    expect(mocks.toastSuccess).toHaveBeenCalledWith('已去除零宽字符并复制')
  })

  it('shows the obfuscation badge from metadata and exposes both cURL copy actions', async () => {
    mocks.getRequestDetail.mockImplementation(async id => ({
      ...buildDetail({ sensitive_words_obfuscation: { applied: true, replaced: 2, fields: ['system[0]'] } }),
      id, request_id: `req-${id}`,
    }))
    await openDrawer()
    await vi.waitFor(() => expect(query('[data-testid="sensitive-words-obfuscation-badge"]')?.textContent).toContain('已应用敏感词混淆 2 处'))
    findButton('请求体')!.click()
    await vi.waitFor(() => expect(query('[data-testid="copy-curl-strip-zwsp"]')).not.toBeNull())

    query<HTMLButtonElement>('button[title="复制 cURL（保留原始字节）"]')!.click()
    await vi.waitFor(() => expect(mocks.copyToClipboard).toHaveBeenCalledOnce())
    expect(mocks.copyToClipboard.mock.calls[0][0]).toContain(ZW)
    expect(mocks.toastWarning).toHaveBeenCalledWith(expect.stringContaining('2 个零宽字符'))

    query<HTMLButtonElement>('[data-testid="copy-curl-strip-zwsp"]')!.click()
    await vi.waitFor(() => expect(mocks.copyToClipboard).toHaveBeenCalledTimes(2))
    const stripped = mocks.copyToClipboard.mock.calls[1][0] as string
    expect(stripped).not.toContain(ZW)
    expect(stripped).toContain('"proxy API"')
    expect(mocks.toastSuccess).toHaveBeenCalledWith(expect.stringContaining('已去除 2 个零宽字符'))
  })
})
