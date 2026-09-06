import { describe, expect, it } from 'vitest'
import type { RouteLocationNormalizedLoaded } from 'vue-router'

import { buildBreadcrumbs, buildNavigation } from '@/layouts/main-layout/navigation'
import type { MessageKey } from '@/i18n'
import type { ModuleStatus } from '@/api/modules'

const translate = (key: MessageKey) => `tx:${key}`

function moduleStatus(overrides: Partial<ModuleStatus>): ModuleStatus {
  return {
    name: 'test', available: true, enabled: true, active: false,
    config_validated: true, config_error: null, display_name: 'Test module',
    description: '', category: 'integration', kind: 'builtin', group: 'management',
    depends_on: [], admin_route: null, admin_menu_icon: null, admin_menu_group: null,
    admin_menu_order: 0, health: 'healthy', ...overrides,
  }
}

function route(path: string, name?: string, meta: Record<string, unknown> = {}): RouteLocationNormalizedLoaded {
  return {
    path,
    fullPath: path,
    query: {},
    hash: '',
    name,
    params: {},
    matched: [],
    meta,
    redirectedFrom: undefined,
  } as RouteLocationNormalizedLoaded
}

describe('main layout navigation builder', () => {
  it('builds user navigation from translation keys and active modules', () => {
    const navigation = buildNavigation({
      canAccessAdmin: false,
      modules: {},
      isModuleActive: (name) => name === 'referral',
      t: translate,
    })

    expect(navigation.map(group => group.title)).toEqual([
      'tx:nav.group.overview',
      'tx:nav.group.resources',
      'tx:nav.group.account',
    ])
    const itemNames = navigation.flatMap(group => group.items.map(item => item.name))
    expect(itemNames).toContain('tx:nav.myReferral')
    expect(itemNames).not.toContain('tx:nav.walletCenter')
    expect(itemNames).not.toContain('tx:nav.billingCenter')
  })

  it('shows user wallet and billing entries only while their modules are active', () => {
    const navigation = buildNavigation({
      canAccessAdmin: false,
      modules: {},
      isModuleActive: name => name === 'wallet' || name === 'billing_plans',
      t: translate,
    })

    const itemNames = navigation.flatMap(group => group.items.map(item => item.name))
    expect(itemNames).toContain('tx:nav.walletCenter')
    expect(itemNames).toContain('tx:nav.billingCenter')
  })

  it('builds admin navigation with dynamic module menu items sorted by menu order', () => {
    const navigation = buildNavigation({
      canAccessAdmin: true,
      modules: {
        first: moduleStatus({
          active: true,
          admin_route: '/admin/first',
          admin_menu_group: 'management',
          admin_menu_order: 2,
          admin_menu_icon: 'Gift',
          display_name: 'First module',
        }),
        second: moduleStatus({
          active: true,
          admin_route: '/admin/second',
          admin_menu_group: 'management',
          admin_menu_order: 1,
          admin_menu_icon: 'Key',
          display_name: 'Second module',
        }),
      },
      isModuleActive: () => false,
      t: translate,
    })

    const managementItems = navigation.find(group => group.title === 'tx:nav.group.management')?.items ?? []
    expect(managementItems.map(item => item.name)).toEqual(expect.arrayContaining(['Second module', 'First module']))
    expect(managementItems.findIndex(item => item.name === 'Second module')).toBeLessThan(
      managementItems.findIndex(item => item.name === 'First module')
    )
    expect(navigation.flatMap(group => group.items.map(item => item.name)))
      .not.toContain('tx:nav.announcements')
  })

  it('shows the standalone keys entry only while its built-in module is active', () => {
    const standaloneModule = moduleStatus({
      active: true,
      admin_route: '/admin/keys',
      admin_menu_group: 'management',
      admin_menu_order: 60,
      admin_menu_icon: 'KeyRound',
      display_name: '独立密钥',
    })
    const activeNavigation = buildNavigation({
      canAccessAdmin: true,
      modules: { standalone_keys: standaloneModule },
      isModuleActive: () => false,
      t: translate,
    })
    const inactiveNavigation = buildNavigation({
      canAccessAdmin: true,
      modules: { standalone_keys: { ...standaloneModule, active: false } },
      isModuleActive: () => false,
      t: translate,
    })

    const activeEntries = activeNavigation
      .flatMap(group => group.items)
      .filter(item => item.href === '/admin/keys')
    const inactiveEntries = inactiveNavigation
      .flatMap(group => group.items)
      .filter(item => item.href === '/admin/keys')

    expect(activeEntries).toHaveLength(1)
    expect(activeEntries[0]?.name).toBe('独立密钥')
    expect(inactiveEntries).toHaveLength(0)
  })

  it('builds translated breadcrumbs for settings and routing detail pages', () => {
    const navigation = buildNavigation({
      canAccessAdmin: true,
      modules: {},
      isModuleActive: () => false,
      t: translate,
    })

    expect(buildBreadcrumbs({
      route: route('/dashboard/settings'),
      navigation,
      modules: {},
      isNavActive: () => false,
      t: translate,
    })).toEqual([
      { label: 'tx:nav.group.account' },
      { label: 'tx:breadcrumb.personalSettings' },
    ])

    expect(buildBreadcrumbs({
      route: route('/dashboard/wallet', 'WalletCenter', { module: 'wallet' }),
      navigation,
      modules: {
        wallet: moduleStatus({ display_name: '钱包管理' }),
      },
      isNavActive: () => false,
      t: translate,
    })).toEqual([
      { label: 'tx:nav.group.account' },
      { label: '钱包管理' },
    ])

    expect(buildBreadcrumbs({
      route: route('/admin/routing/new', 'RoutingProfileCreate'),
      navigation,
      modules: {},
      isNavActive: href => href === '/admin/routing',
      t: translate,
    })).toEqual([
      { label: 'tx:nav.group.management' },
      { label: 'tx:nav.routing', href: '/admin/routing' },
      { label: 'tx:breadcrumb.routingCreate' },
    ])
  })
})
