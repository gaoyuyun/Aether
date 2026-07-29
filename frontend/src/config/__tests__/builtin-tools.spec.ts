import { describe, expect, it } from 'vitest'

import { BUILTIN_TOOLS } from '@/config/builtin-tools'

describe('built-in module tools', () => {
  it('exposes wallet and billing plans as toggleable built-in modules', () => {
    expect(BUILTIN_TOOLS).toEqual(expect.arrayContaining([
      expect.objectContaining({
        name: '钱包管理',
        href: '/admin/wallets',
        moduleName: 'wallet',
      }),
      expect.objectContaining({
        name: '套餐管理',
        href: '/admin/billing-plans',
        moduleName: 'billing_plans',
      }),
    ]))
  })
})
