import { describe, expect, it } from 'vitest'

import type { ModuleStatus } from '@/api/modules'
import { buildBuiltinTools } from '@/config/builtin-tools'

function moduleStatus(overrides: Partial<ModuleStatus>): ModuleStatus {
  return {
    name: 'module',
    available: true,
    enabled: false,
    active: false,
    config_validated: true,
    config_error: null,
    display_name: 'Module',
    description: 'Module description',
    category: 'integration',
    kind: 'extension',
    group: 'integration',
    depends_on: [],
    admin_route: '/admin/module',
    admin_menu_icon: null,
    admin_menu_group: null,
    admin_menu_order: 1,
    health: 'healthy',
    ...overrides,
  }
}

describe('built-in module tools', () => {
  it('derives built-in cards from backend module metadata', () => {
    const wallet = moduleStatus({
      name: 'wallet',
      display_name: '钱包管理',
      kind: 'builtin',
      group: 'commerce',
      admin_route: '/admin/wallets',
      admin_menu_icon: 'Wallet',
      admin_menu_order: 65,
    })
    const plans = moduleStatus({
      name: 'billing_plans',
      display_name: '套餐管理',
      kind: 'builtin',
      group: 'commerce',
      depends_on: ['wallet'],
      admin_route: '/admin/billing-plans',
      admin_menu_icon: 'Package',
      admin_menu_order: 66,
    })
    const extension = moduleStatus({ name: 's3_backup' })

    const cards = buildBuiltinTools([extension, plans, wallet])
    const moduleNames = cards.flatMap(card => card.module ? [card.module.name] : [])

    expect(moduleNames).toEqual(['wallet', 'billing_plans'])
    expect(cards.find(card => card.module?.name === 'billing_plans')?.module?.depends_on)
      .toEqual(['wallet'])
  })
})
