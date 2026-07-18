import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, ref, type App, type Ref } from 'vue'

import type { RequestDetail } from '@/api/dashboard'
import RequestDetailDrawer from '../RequestDetailDrawer.vue'

const apiMocks = vi.hoisted(() => ({
  getRequestDetail: vi.fn(),
}))

vi.mock('@/api/dashboard', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/dashboard')>()
  return {
    ...actual,
    dashboardApi: {
      ...actual.dashboardApi,
      getRequestDetail: apiMocks.getRequestDetail,
    },
  }
})

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
  apiMocks.getRequestDetail.mockReset()
})

function buildEmbeddingDetail(): RequestDetail {
  return {
    id: 'usage-embedding-1',
    request_id: 'req-embedding-1',
    user: {
      id: 'user-1',
      username: 'embedding-user',
      email: 'embedding@example.com',
    },
    api_key: {
      id: 'key-1',
      name: 'test-key',
      display: 'test-key',
    },
    provider: 'embedding-provider',
    model: 'embedding-model',
    tokens: { input: 100, output: 0, total: 100 },
    cost: { input: 0.00001, output: 0, total: 0.00001 },
    request_type: 'embedding',
    is_stream: false,
    status: 'completed',
    status_code: 200,
    response_time_ms: 10,
    created_at: '2026-07-16T00:00:00Z',
    request_headers: { 'content-type': 'application/json' },
    settlement: {
      settlement_snapshot: {
        pricing_snapshot: {
          pricing_source: 'global_default',
          tiered_pricing: {
            tiers: [{ up_to: null, input_price_per_1m: 0.1 }],
          },
        },
      },
    },
  }
}

describe('RequestDetailDrawer settlement pricing', () => {
  it('renders an input-only embedding tier without treating the missing output price as zero', async () => {
    apiMocks.getRequestDetail.mockResolvedValue(buildEmbeddingDetail())

    let isOpen!: Ref<boolean>
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-embedding-1',
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    await vi.waitFor(() => {
      expect(document.body.textContent).toContain('输入 $0.1/M')
      expect(document.body.textContent).toContain('输出 -')
    })
    expect(document.body.textContent).not.toContain('输出 $0/M')
  })

  it('shows mapping, reasoning, Fast, and Cyber together in the model header', async () => {
    apiMocks.getRequestDetail.mockResolvedValue({
      ...buildEmbeddingDetail(),
      id: 'usage-cyber-risk-demo',
      request_id: 'req_usage-cyber-risk-demo',
      model: 'gpt-5',
      target_model: 'gpt-5.1',
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: 'max',
      request_body: {
        model: 'gpt-5',
        reasoning: { effort: 'xhigh' },
      },
      service_tier: 'priority',
      // A response-side tier must not be used for the Fast badge or billing.
      actual_service_tier: 'default',
      provider_request_body: {
        model: 'gpt-5.1',
        reasoning: { effort: 'max' },
        service_tier: 'priority',
      },
      status: 'failed',
      status_code: 400,
      error_message: 'This content was flagged for possible cybersecurity risk. To get authorized for security work, join the Trusted Access for Cyber program: https://chatgpt.com/cyber',
      response_body: {
        error: {
          type: 'invalid_request',
          message: 'This content was flagged for possible cybersecurity risk.',
          code: 400,
        },
      },
    })

    let isOpen!: Ref<boolean>
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-cyber-risk-demo',
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    await vi.waitFor(() => {
      expect(document.body.querySelector('[data-request-detail-model-display]')?.textContent)
        .toContain('gpt-5')
      expect(document.body.querySelector('[data-request-detail-model-display]')?.textContent)
        .toContain('gpt-5.1')
      expect(document.body.querySelector('[data-request-detail-model-badge="reasoning"]')?.textContent)
        .toContain('xhigh -> max')
      expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')?.textContent)
        .toContain('Fast')
      expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')?.textContent?.trim())
        .toBe('Fast')
      expect(document.body.querySelector('[data-request-detail-model-badge="cyber"]')?.textContent)
        .toContain('Cyber')
      const modelLayout = document.body.querySelector(
        '[data-request-detail-model-layout="inline"]',
      )
      expect(modelLayout?.firstElementChild?.textContent).toContain('gpt-5')
      expect(modelLayout?.firstElementChild?.textContent).toContain('->')
      expect(modelLayout?.firstElementChild?.textContent).toContain('gpt-5.1')
      expect(modelLayout?.firstElementChild?.textContent).toContain('xhigh -> max')
      expect(modelLayout?.firstElementChild?.textContent).toContain('Fast')
      expect(modelLayout?.firstElementChild?.textContent).toContain('Cyber')
      expect(modelLayout?.querySelector('[data-request-detail-model-badges-row]')).toBeNull()
      const serviceTierFacts = document.body.querySelector('[data-testid="service-tier-facts"]')
      expect([...serviceTierFacts?.querySelectorAll('dt') ?? []].map(node => node.textContent?.trim()))
        .toEqual(['上游请求层级', '计费层级'])
      expect([...serviceTierFacts?.querySelectorAll('dd') ?? []].map(node => node.textContent?.trim()))
        .toEqual(['Fast', 'Fast'])
    })
  })

  it('keeps the Cyber badge stable from the selected row while lightweight detail loads', async () => {
    let resolveDetail!: (value: RequestDetail) => void
    apiMocks.getRequestDetail.mockReturnValue(new Promise<RequestDetail>((resolve) => {
      resolveDetail = resolve
    }))

    let isOpen!: Ref<boolean>
    const requestId = ref('usage-cyber-summary')
    const summaryRecord = ref<Record<string, unknown>>({
      id: 'usage-cyber-summary',
      model: 'gpt-5',
      target_model: 'gpt-5.1',
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: 'max',
      service_tier: 'priority',
      error_message: 'This content was flagged for possible cybersecurity risk. https://chatgpt.com/cyber',
    })
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: requestId.value,
          summaryRecord: summaryRecord.value as never,
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    expect(apiMocks.getRequestDetail).toHaveBeenCalledTimes(1)
    expect(document.body.querySelector('[data-request-detail-model-badge="cyber"]')?.textContent)
      .toContain('Cyber')
    expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')?.textContent)
      .toContain('Fast')

    resolveDetail({
      ...buildEmbeddingDetail(),
      id: 'usage-cyber-summary',
      request_id: 'usage-cyber-summary',
      model: 'gpt-5',
      // Lightweight detail can legitimately omit these final-provider facts.
      target_model: null,
      requested_reasoning_effort: null,
      reasoning_effort: null,
      service_tier: null,
      status: 'failed',
      status_code: 400,
      error_message: 'execution runtime stream returned non-success status 400',
      response_body: null,
    })
    await vi.waitFor(() => {
      expect(document.body.querySelector('[data-request-detail-model-badge="cyber"]')?.textContent)
        .toContain('Cyber')
      expect(document.body.querySelector('[data-usage-model-target]')?.textContent)
        .toContain('gpt-5.1')
      expect(document.body.querySelector('[data-request-detail-model-badge="reasoning"]')?.textContent)
        .toContain('xhigh -> max')
      expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')?.textContent)
        .toContain('Fast')
      const tierValues = [
        ...document.body.querySelectorAll('[data-testid="service-tier-facts"] dd'),
      ].map(node => node.textContent?.trim())
      expect(tierValues).toEqual(['Fast', 'Fast'])
    })
  })

  it('clears a stale summary Cyber badge when newer detail completed successfully', async () => {
    apiMocks.getRequestDetail.mockResolvedValue({
      ...buildEmbeddingDetail(),
      id: 'usage-cyber-recovered',
      request_id: 'usage-cyber-recovered',
      model: 'gpt-5',
      status: 'completed',
      status_code: 200,
      error_message: undefined,
      updated_at: '2026-07-17T00:00:02Z',
    })

    let isOpen!: Ref<boolean>
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-cyber-recovered',
          summaryRecord: {
            id: 'usage-cyber-recovered',
            model: 'gpt-5',
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost: 0,
            is_stream: false,
            status: 'failed',
            status_code: 400,
            error_message: 'This content was flagged for possible cybersecurity risk. https://chatgpt.com/cyber',
            created_at: '2026-07-17T00:00:00Z',
            updated_at: '2026-07-17T00:00:01Z',
          },
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    await vi.waitFor(() => {
      expect(apiMocks.getRequestDetail).toHaveBeenCalledTimes(1)
      expect(document.body.querySelector('[data-request-detail-model-badge="cyber"]'))
        .toBeNull()
    })
  })

  it('uses populated detail fallbacks without overriding a populated summary tier', async () => {
    apiMocks.getRequestDetail.mockResolvedValue({
      ...buildEmbeddingDetail(),
      id: 'usage-standard-summary',
      request_id: 'usage-standard-summary',
      model: 'gpt-5',
      target_model: 'gpt-5.1',
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: 'max',
      // The non-empty summary tier remains authoritative over this stale fact.
      service_tier: 'priority',
    })

    let isOpen!: Ref<boolean>
    const summaryRecord = ref<Record<string, unknown>>({
      id: 'usage-standard-summary',
      model: 'gpt-5',
      target_model: null,
      model_version: null,
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: null,
      service_tier: 'default',
    })
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-standard-summary',
          summaryRecord: summaryRecord.value as never,
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    await vi.waitFor(() => {
      expect(apiMocks.getRequestDetail).toHaveBeenCalledTimes(1)
      expect(document.body.querySelector('[data-usage-model-target]')?.textContent?.trim())
        .toBe('->gpt-5.1')
      expect(document.body.querySelector('[data-request-detail-model-badge="reasoning"]')?.textContent?.trim())
        .toBe('xhigh -> max')
      expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')).toBeNull()
      const tierValues = [
        ...document.body.querySelectorAll('[data-testid="service-tier-facts"] dd'),
      ].map(node => node.textContent?.trim())
      expect(tierValues).toEqual(['default', 'default'])
    })
  })

  it('uses detail model_version when the lightweight summary has null model facts', async () => {
    apiMocks.getRequestDetail.mockResolvedValue({
      ...buildEmbeddingDetail(),
      id: 'usage-version-summary',
      request_id: 'usage-version-summary',
      model: 'gpt-5',
      target_model: null,
      model_version: 'gpt-5.1-2026-07-17',
    })

    let isOpen!: Ref<boolean>
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-version-summary',
          summaryRecord: {
            id: 'usage-version-summary',
            model: 'gpt-5',
            target_model: null,
            model_version: null,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost: 0,
            is_stream: false,
            created_at: '2026-07-17T00:00:00Z',
          },
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await nextTick()

    await vi.waitFor(() => {
      expect(document.body.querySelector('[data-usage-model-target]')?.textContent?.trim())
        .toBe('->gpt-5.1-2026-07-17')
    })
  })

  it('lets a newer final-provider summary clear facts cached from an earlier candidate', async () => {
    apiMocks.getRequestDetail.mockResolvedValue({
      ...buildEmbeddingDetail(),
      id: 'usage-final-candidate',
      request_id: 'usage-final-candidate',
      model: 'gpt-5',
      target_model: 'gpt-5.1',
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: 'max',
      service_tier: 'priority',
      status: 'streaming',
      updated_at: '2026-07-17T00:00:01Z',
    })

    let isOpen!: Ref<boolean>
    const summaryRecord = ref<Record<string, unknown>>({
      id: 'usage-final-candidate',
      model: 'gpt-5',
      target_model: 'gpt-5.1',
      requested_reasoning_effort: 'xhigh',
      reasoning_effort: 'max',
      service_tier: 'priority',
      status: 'streaming',
      updated_at: '2026-07-17T00:00:01Z',
    })
    const Host = defineComponent({
      setup() {
        isOpen = ref(false)
        return () => h(RequestDetailDrawer, {
          isOpen: isOpen.value,
          requestId: 'usage-final-candidate',
          summaryRecord: summaryRecord.value as never,
        })
      },
    })

    const root = document.createElement('div')
    document.body.appendChild(root)
    const app = createApp(Host)
    app.mount(root)
    mountedApps.push({ app, root })

    isOpen.value = true
    await vi.waitFor(() => {
      expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')).not.toBeNull()
    })

    summaryRecord.value = {
      ...summaryRecord.value,
      target_model: null,
      reasoning_effort: null,
      service_tier: null,
      updated_at: '2026-07-17T00:00:02Z',
    }
    await nextTick()

    expect(document.body.querySelector('[data-usage-model-target]')).toBeNull()
    expect(document.body.querySelector('[data-request-detail-model-badge="reasoning"]')?.textContent?.trim())
      .toBe('xhigh')
    expect(document.body.querySelector('[data-request-detail-model-badge="fast"]')).toBeNull()
    expect([...document.body.querySelectorAll('[data-testid="service-tier-facts"] dd')]
      .some(node => node.textContent?.trim() === 'Fast')).toBe(false)
  })
})
