import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, reactive, nextTick } from 'vue'

import ProviderMonthlyQuotaCard from '@/features/providers/components/ProviderMonthlyQuotaCard.vue'
import ProviderQuotaProgressRow from '@/features/providers/components/ProviderQuotaProgressRow.vue'
import ProviderQuotaSectionHeader from '@/features/providers/components/ProviderQuotaSectionHeader.vue'
import { createI18n } from '@/i18n'

const api = vi.hoisted(() => ({ getProviderStats: vi.fn(), resetProviderQuota: vi.fn() }))
vi.mock('@/api/provider-strategy', () => api)
const cleanups: Array<() => void> = []
afterEach(() => { cleanups.splice(0).forEach(fn => fn()); vi.useRealTimers(); vi.resetAllMocks() })
async function settle() { for (let i = 0; i < 8; i++) await Promise.resolve(); await nextTick() }

function mount(component: Parameters<typeof createApp>[0], props?: Record<string, unknown>) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const state = reactive({ ...props })
  const app = createApp({ render: () => h(component, state) })
  app.use(createI18n())
  app.mount(root)

  let mounted = true
  const unmount = () => { if (mounted) { mounted = false; app.unmount(); root.remove() } }
  cleanups.push(unmount)
  return { root, unmount, setProps: (props: Record<string, unknown>) => Object.assign(state, props) }
}

describe('provider quota display components', () => {
  it('renders monthly quota usage and reset day', () => {
    const { root, unmount } = mount(ProviderMonthlyQuotaCard, {
      used: 25,
      quota: 100,
      resetDay: 15,
    })

    expect(root.querySelector('[data-testid="provider-monthly-quota-card"]')).toBeTruthy()
    expect(root.querySelector('[data-testid="provider-monthly-quota-percent"]')?.textContent).toContain('25.0%')
    expect(root.querySelector('[data-testid="provider-monthly-quota-amount"]')?.textContent).toContain('$25.00 / $100.00')
    expect(root.querySelector('[data-testid="provider-monthly-quota-reset"]')?.textContent).toContain('每 15 天重置')

    unmount()
  })

  it('normalizes quota progress and renders fallback footer text', () => {
    const { root, unmount } = mount(ProviderQuotaProgressRow, {
      label: 'Daily',
      remainingPercent: 120,
      meterClass: 'text-green-600',
      barClass: 'bg-green-500',
      resetText: '2h reset',
    })

    expect(root.querySelector('[data-testid="provider-quota-progress-meter"]')?.textContent?.trim()).toBe('100.0%')
    expect((root.querySelector('[data-testid="provider-quota-progress-bar"]') as HTMLElement).style.width).toBe('100%')
    expect(root.querySelector('[data-testid="provider-quota-progress-reset"]')?.textContent).toBe('2h reset')

    unmount()
  })

  it('renders configured quota windows', () => {
    const { root, unmount } = mount(ProviderMonthlyQuotaCard, {
      used: 10,
      quota: 100,
      windows: [
        {
          duration_secs: 86_400,
          limit_usd: 5,
          used_usd: 2.5,
          status: 'ready',
          rolling_start: '2026-08-17T00:00:00Z',
        },
        { duration_secs: 604_800, limit_usd: 20, status: 'rebuilding' },
        {
          duration_secs: 2_592_000,
          limit_usd: 50,
          status: 'failed',
          rebuild_error: 'quota cost is unavailable for a dispatched monthly request',
        },
      ],
    })

    const text = root.querySelector('[data-testid="provider-quota-windows"]')?.textContent
    expect(text).toContain('$2.50 / $5.00')
    expect(text).toContain('$20.00')
    expect(text).toContain('统计重建中')
    expect(root.querySelector('[data-testid="provider-quota-window-error"]')?.textContent)
      .toContain('费用尚未完成核算')

    unmount()
  })

  it('renders section loading and updated state', () => {
    const Probe = defineComponent({
      setup() {
        return () => h(ProviderQuotaSectionHeader, {
          title: 'Account quota',
          loading: true,
          updatedText: '10:30',
        })
      },
    })

    const { root, unmount } = mount(Probe)

    expect(root.textContent).toContain('Account quota')
    expect(root.querySelector('[data-testid="provider-quota-header-loading"]')).toBeTruthy()
    expect(root.querySelector('[data-testid="provider-quota-header-updated"]')?.textContent).toBe('10:30')

    unmount()
  })
})


describe('subscription quota loading and operations', () => {
  it('shows all eight windows when total quota is zero and distinguishes unknown from zero', async () => {
    api.getProviderStats.mockResolvedValue({ billing_info: { monthly_quota_usd: 0, monthly_used_usd: 0,
      quota_windows: Array.from({ length: 8 }, (_, i) => ({ duration_secs: (i + 1) * 60, limit_usd: 2,
        used_usd: i ? null : 0, status: i ? 'rebuilding' : 'ready' })) } })
    const { root } = mount(ProviderMonthlyQuotaCard, { providerId: 'a', quota: 0 })
    await settle()
    expect(root.querySelectorAll('[data-duration]')).toHaveLength(8)
    expect(root.querySelector('[data-duration="60"]')?.textContent).toContain('$0.00 / $2.00')
    expect(root.querySelector('[data-duration="120"]')?.textContent).toContain('待统计 / $2.00')
  })

  it('offers retry after an error and clears pending reset and empty windows authoritatively', async () => {
    api.getProviderStats.mockRejectedValueOnce(new Error('offline')).mockResolvedValueOnce({ billing_info: {
      monthly_used_usd: 0, pending_quota_reset_at: null, quota_windows: [] } })
    const { root } = mount(ProviderMonthlyQuotaCard, { providerId: 'a', pendingResetAt: '2030-01-01T00:00:00Z', windows: [{ duration_secs: 60, limit_usd: 2 }] })
    await settle()
    expect(root.querySelector('[data-testid="provider-quota-load-error"]')).toBeTruthy()
    ;(root.querySelector('[data-testid="provider-quota-refresh"]') as HTMLButtonElement).click()
    await settle()
    expect(root.querySelector('[data-testid="provider-quota-load-error"]')).toBeNull()
    expect(root.querySelector('[data-testid="provider-monthly-quota-pending-reset"]')).toBeNull()
    expect(root.querySelectorAll('[data-duration]')).toHaveLength(0)
  })

  it('ignores responses from an old provider and refreshes only while visible', async () => {
    vi.useFakeTimers()
    let finishOld!: (value: unknown) => void
    api.getProviderStats.mockImplementationOnce(() => new Promise(resolve => { finishOld = resolve }))
      .mockResolvedValue({ billing_info: { monthly_used_usd: 8, quota_windows: [] } })
    const view = mount(ProviderMonthlyQuotaCard, { providerId: 'a', quota: 100 })
    view.setProps({ providerId: 'b' }); await settle()
    finishOld({ billing_info: { monthly_used_usd: 90 } }); await settle()
    expect(view.root.textContent).toContain('$8.00 / $100.00')
    view.setProps({ active: false }); await settle()
    const count = api.getProviderStats.mock.calls.length
    await vi.advanceTimersByTimeAsync(60000)
    expect(api.getProviderStats).toHaveBeenCalledTimes(count)
    view.setProps({ active: true }); await settle()
    expect(api.getProviderStats).toHaveBeenCalledTimes(count + 1)
  })

  it('provides two reset choices and submits usage-only as a scheduled explicit operation', async () => {
    vi.useFakeTimers(); vi.setSystemTime(new Date('2026-09-06T10:00:00Z'))
    api.getProviderStats.mockResolvedValue({ billing_info: { monthly_used_usd: 5, quota_windows: [] } })
    api.resetProviderQuota.mockResolvedValue({ effective_at: '2026-09-06T10:01:00Z', pending: true })
    const { root } = mount(ProviderMonthlyQuotaCard, { providerId: 'a', quota: 100 })
    await settle()
    ;(root.querySelector('[data-testid="provider-monthly-quota-reset-button"]') as HTMLButtonElement).click()
    await settle()
    expect(document.querySelectorAll('input[name="quota-reset-mode"]')).toHaveLength(2)
    document.querySelector('form')?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
    await settle()
    expect(api.resetProviderQuota).toHaveBeenCalledWith('a', { mode: 'usage_only', reset_usage: true, effective_at: '2026-09-06T10:01:00.000Z' })
  })
})
