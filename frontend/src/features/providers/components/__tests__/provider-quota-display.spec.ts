import { describe, expect, it } from 'vitest'
import { createApp, defineComponent, h } from 'vue'

import ProviderMonthlyQuotaCard from '@/features/providers/components/ProviderMonthlyQuotaCard.vue'
import ProviderQuotaProgressRow from '@/features/providers/components/ProviderQuotaProgressRow.vue'
import ProviderQuotaSectionHeader from '@/features/providers/components/ProviderQuotaSectionHeader.vue'
import { createI18n } from '@/i18n'

function mount(component: Parameters<typeof createApp>[0], props?: Record<string, unknown>) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(component, props)
  app.use(createI18n())
  app.mount(root)

  return {
    root,
    unmount: () => {
      app.unmount()
      root.remove()
    },
  }
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
      .toContain('请检查该模型的价格配置')

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
