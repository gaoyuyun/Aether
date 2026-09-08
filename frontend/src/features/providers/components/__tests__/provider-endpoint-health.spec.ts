import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, type App, type Component } from 'vue'

import type { ProviderWithEndpointsSummary } from '@/api/endpoints'
import { createI18n } from '@/i18n'
import ProviderTableRow from '../ProviderTableRow.vue'
import ProviderMobileCard from '../ProviderMobileCard.vue'

vi.mock('../ProviderBalanceCell.vue', () => ({
  default: { render: () => null },
}))

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

function mountProvider(component: Component, healthScore: number | null) {
  const provider: ProviderWithEndpointsSummary = {
    id: 'provider-1',
    name: 'Provider One',
    provider_type: 'custom',
    provider_priority: 100,
    keep_priority_on_conversion: false,
    enable_format_conversion: true,
    is_active: true,
    total_endpoints: 1,
    active_endpoints: 1,
    total_keys: 1,
    active_keys: 1,
    total_models: 0,
    active_models: 0,
    global_model_ids: [],
    avg_health_score: healthScore,
    unhealthy_endpoints: healthScore === 0 ? 1 : 0,
    api_formats: ['openai:chat'],
    endpoint_health_details: [{
      api_format: 'openai:chat',
      health_score: healthScore,
      is_active: true,
      total_keys: 1,
      active_keys: 1,
    }],
    ops_configured: false,
    created_at: '2026-09-06T00:00:00Z',
    updated_at: '2026-09-06T00:00:00Z',
  }
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(component, {
    provider,
    editingDescriptionId: null,
    isBalanceLoading: () => false,
    getProviderBalance: () => null,
    getProviderBalanceBreakdown: () => null,
    getProviderBalanceError: () => null,
    getProviderCheckin: () => null,
    getProviderCookieExpired: () => null,
    getProviderBalanceExtra: () => [],
    formatBalanceDisplay: () => '-',
    formatResetCountdown: () => '',
    getQuotaUsedColorClass: () => '',
  })
  app.use(createI18n())
  app.mount(root)
  mountedApps.push({ app, root })
  return root
}

describe.each([
  ['desktop provider row', ProviderTableRow],
  ['mobile provider card', ProviderMobileCard],
] as const)('%s endpoint health', (_name, component) => {
  it.each([
    { score: null, label: '-', width: '100%', color: 'bg-muted-foreground/40' },
    { score: 0, label: '0%', width: '5%', color: 'bg-red-500' },
    { score: 0.8, label: '80%', width: '80%', color: 'bg-green-500' },
  ])('renders $score without confusing unknown health with zero', ({ score, label, width, color }) => {
    const root = mountProvider(component, score)
    const health = root.querySelector('[title*="健康"]')
    const bar = health?.querySelector<HTMLElement>('.transition-all')

    expect(health?.querySelectorAll('span')[1]?.textContent?.trim()).toBe(label)
    expect(bar?.style.width).toBe(width)
    expect(bar?.classList.contains(color)).toBe(true)
  })
})
