import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, type App } from 'vue'
import type { ActionResultResponse } from '@/api/providerOps'
import { BALANCE_POLL_DELAYS_MS, useProviderBalance } from '../useProviderBalance'

const api = vi.hoisted(() => ({
  batchQueryBalance: vi.fn<() => Promise<Record<string, ActionResultResponse>>>(),
  getBalance: vi.fn<() => Promise<ActionResultResponse>>(),
  getArchitectures: vi.fn().mockResolvedValue([]),
}))

vi.mock('@/api/providerOps', () => api)

let app: App | undefined
let root: HTMLDivElement

function mountBalance() {
  let balance!: ReturnType<typeof useProviderBalance>
  root = document.createElement('div')
  app = createApp({
    setup() {
      balance = useProviderBalance()
      return () => null
    },
  })
  app.mount(root)
  return balance
}

function result(status: ActionResultResponse['status'], available: number): ActionResultResponse {
  return {
    status,
    action_type: 'query_balance',
    data: { total_available: available, currency: 'USD', extra: {} },
    message: null,
    executed_at: '2026-09-07T00:00:00Z',
    response_time_ms: 0,
    cache_ttl_seconds: 0,
  }
}

const providers = [{ id: 'provider-1', ops_configured: true }]

beforeEach(() => {
  vi.useFakeTimers()
  api.batchQueryBalance.mockReset()
  api.getBalance.mockReset()
})

afterEach(() => {
  app?.unmount()
  app = undefined
  root?.remove()
  vi.useRealTimers()
})

describe('provider balance refresh', () => {
  it('does not overwrite a newer refresh with an older pending retry', async () => {
    const balance = mountBalance()
    let resolveRetry!: (value: Record<string, ActionResultResponse>) => void
    api.batchQueryBalance
      .mockResolvedValueOnce({ 'provider-1': result('pending', 0) })
      .mockImplementationOnce(() => new Promise(resolve => { resolveRetry = resolve }))
      .mockResolvedValueOnce({ 'provider-1': result('success', 20) })

    await balance.loadBalances(providers)
    await vi.advanceTimersByTimeAsync(BALANCE_POLL_DELAYS_MS[0])
    await balance.loadBalances(providers)
    resolveRetry({ 'provider-1': result('success', 10) })
    await Promise.resolve()

    expect(balance.getProviderBalance('provider-1')).toEqual({ available: 20, currency: 'USD' })
  })

  it('ignores a pending response after unmount', async () => {
    const balance = mountBalance()
    let resolveLoad!: (value: Record<string, ActionResultResponse>) => void
    api.batchQueryBalance.mockImplementationOnce(() => new Promise(resolve => { resolveLoad = resolve }))
    const loading = balance.loadBalances(providers)
    app?.unmount()
    app = undefined
    resolveLoad({ 'provider-1': result('pending', 10) })
    await loading

    expect(balance.balanceCache.value).toEqual({})
    expect(vi.getTimerCount()).toBe(0)
  })

  it('preserves zero balances and false check-in results', async () => {
    const balance = mountBalance()
    api.batchQueryBalance.mockResolvedValueOnce({
      'provider-1': {
        ...result('success', 0),
        data: {
          total_available: 0,
          currency: 'USD',
          extra: { balance: 0, points: 0, checkin_success: false, checkin_message: 'try again' },
        },
      },
    })
    await balance.loadBalances(providers)

    expect(balance.getProviderBalanceBreakdown('provider-1')).toEqual({ balance: 0, points: 0, currency: 'USD' })
    expect(balance.getProviderCheckin('provider-1')).toEqual({ success: false, message: 'try again' })
  })

  it('polls with a bounded schedule and ends in a terminal state instead of spinning forever', async () => {
    const balance = mountBalance()
    api.batchQueryBalance.mockResolvedValue({ 'provider-1': { ...result('pending', 0), refresh_state: 'refreshing' } })

    await balance.loadBalances(providers)
    expect(balance.isBalanceLoading('provider-1')).toBe(true)

    for (const delay of BALANCE_POLL_DELAYS_MS) {
      await vi.advanceTimersByTimeAsync(delay)
    }

    expect(api.batchQueryBalance).toHaveBeenCalledTimes(1 + BALANCE_POLL_DELAYS_MS.length)
    expect(vi.getTimerCount()).toBe(0)
    expect(balance.isBalanceLoading('provider-1')).toBe(false)
    expect(balance.getProviderBalanceError('provider-1')).toBeNull()
    expect(balance.getProviderBalanceMeta('provider-1')?.exhausted).toBe(true)
  })

  it('stops polling as soon as the background refresh finishes', async () => {
    const balance = mountBalance()
    api.batchQueryBalance
      .mockResolvedValueOnce({ 'provider-1': result('pending', 0) })
      .mockResolvedValueOnce({ 'provider-1': { ...result('success', 7), refresh_state: 'idle' } })

    await balance.loadBalances(providers)
    await vi.advanceTimersByTimeAsync(BALANCE_POLL_DELAYS_MS[0])

    expect(balance.getProviderBalance('provider-1')).toEqual({ available: 7, currency: 'USD' })
    expect(api.batchQueryBalance).toHaveBeenCalledTimes(2)
    expect(vi.getTimerCount()).toBe(0)
  })

  it('keeps the last value while the upstream fails and exposes the failure meta', async () => {
    const balance = mountBalance()
    api.batchQueryBalance.mockResolvedValueOnce({
      'provider-1': {
        ...result('success', 12),
        fetched_at: '2026-09-17T08:00:00Z',
        stale: true,
        refresh_state: 'idle',
        next_retry_at: '2026-09-19T07:30:00Z',
        consecutive_failures: 3,
        last_error: { status: 'network_error', message: '请求超时', at: '2026-09-19T07:08:00Z' },
      },
    })
    await balance.loadBalances(providers)

    expect(balance.getProviderBalance('provider-1')).toEqual({ available: 12, currency: 'USD' })
    expect(balance.getProviderBalanceError('provider-1')).toBeNull()
    expect(balance.isBalanceLoading('provider-1')).toBe(false)
    expect(balance.getProviderBalanceMeta('provider-1')).toEqual({
      fetchedAt: '2026-09-17T08:00:00Z',
      stale: true,
      refreshState: 'idle',
      nextRetryAt: '2026-09-19T07:30:00Z',
      consecutiveFailures: 3,
      lastError: { status: 'network_error', message: '请求超时', at: '2026-09-19T07:08:00Z' },
      exhausted: false,
    })
    expect(vi.getTimerCount()).toBe(0)
  })

  it('reports a failure without any previous value as an error, not as loading', async () => {
    const balance = mountBalance()
    api.batchQueryBalance.mockResolvedValueOnce({
      'provider-1': {
        ...result('auth_failed', 0),
        data: null,
        message: '认证失败，请检查凭据配置',
        fetched_at: null,
        stale: true,
        refresh_state: 'idle',
      },
    })
    await balance.loadBalances(providers)

    expect(balance.isBalanceLoading('provider-1')).toBe(false)
    expect(balance.getProviderBalance('provider-1')).toBeNull()
    expect(balance.getProviderBalanceError('provider-1')).toEqual({ status: 'auth_failed', message: '认证失败，请检查凭据配置' })
    expect(vi.getTimerCount()).toBe(0)
  })

  it('polls a manual refresh until the background job finishes', async () => {
    const balance = mountBalance()
    api.batchQueryBalance
      .mockResolvedValueOnce({ 'provider-1': { ...result('success', 1), refresh_state: 'idle' } })
      .mockResolvedValueOnce({ 'provider-1': { ...result('success', 2), refresh_state: 'idle' } })
    api.getBalance.mockResolvedValueOnce({ ...result('success', 1), refresh_state: 'refreshing' })

    await balance.loadBalances(providers)
    expect(balance.isBalanceRefreshing('provider-1')).toBe(false)

    await balance.refreshProviderBalance('provider-1')
    expect(api.getBalance).toHaveBeenCalledWith('provider-1', true)
    expect(balance.isBalanceRefreshing('provider-1')).toBe(true)

    await vi.advanceTimersByTimeAsync(BALANCE_POLL_DELAYS_MS[0])
    expect(balance.getProviderBalance('provider-1')).toEqual({ available: 2, currency: 'USD' })
    expect(balance.isBalanceRefreshing('provider-1')).toBe(false)
    expect(vi.getTimerCount()).toBe(0)
  })

  it('does not cancel page-wide polling when a single provider is reloaded', async () => {
    const balance = mountBalance()
    api.batchQueryBalance
      .mockResolvedValueOnce({ 'provider-1': result('pending', 0), 'provider-2': result('pending', 0) })
      .mockResolvedValueOnce({ 'provider-2': { ...result('success', 5), refresh_state: 'idle' } })
      .mockResolvedValueOnce({ 'provider-1': { ...result('success', 3), refresh_state: 'idle' }, 'provider-2': { ...result('success', 5), refresh_state: 'idle' } })

    await balance.loadBalances([...providers, { id: 'provider-2', ops_configured: true }])
    await balance.loadBalances([{ id: 'provider-2', ops_configured: true }], false)
    await vi.advanceTimersByTimeAsync(BALANCE_POLL_DELAYS_MS[0])

    expect(balance.getProviderBalance('provider-1')).toEqual({ available: 3, currency: 'USD' })
    expect(balance.getProviderBalance('provider-2')).toEqual({ available: 5, currency: 'USD' })
  })
})
