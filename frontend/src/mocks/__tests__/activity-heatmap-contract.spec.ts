import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('@/config/demo', () => ({
  isDemoMode: () => true,
  DEMO_ACCOUNTS: {
    admin: { email: 'admin@demo.aether.io', password: 'demo123' },
    user: { email: 'user@demo.aether.io', password: 'demo123' },
  },
}))

import { handleMockRequest, setMockUserToken } from '../handler'

function expectFullYearHeatmap(data: unknown) {
  expect(data).toMatchObject({
    total_days: 365,
  })
  expect((data as { days: unknown[] }).days).toHaveLength(365)
}

describe('activity heatmap demo contracts', () => {
  beforeEach(() => {
    setMockUserToken('demo-access-token-admin')
  })

  it('returns a full year for the admin usage page', async () => {
    const response = await handleMockRequest({
      method: 'GET',
      url: '/api/admin/usage/heatmap',
    })

    expectFullYearHeatmap(response?.data)
  })

  it('returns a full year for the current-user usage page', async () => {
    setMockUserToken('demo-access-token-user')
    const response = await handleMockRequest({
      method: 'GET',
      url: '/api/users/me/usage/heatmap',
    })

    expectFullYearHeatmap(response?.data)
  })
})
