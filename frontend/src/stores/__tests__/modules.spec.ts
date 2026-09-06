import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

const { getRuntimeStatusMock, getAllStatusMock } = vi.hoisted(() => ({
  getRuntimeStatusMock: vi.fn(),
  getAllStatusMock: vi.fn(),
}))

vi.mock('@/api/modules', () => ({
  modulesApi: {
    getRuntimeStatus: getRuntimeStatusMock,
    getAllStatus: getAllStatusMock,
  },
}))

import { useModuleStore } from '@/stores/modules'

function runtimeStatus(active: boolean) {
  return [{ name: 'wallet', display_name: 'Wallet', active }]
}

describe('module runtime store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    getRuntimeStatusMock.mockReset()
    getAllStatusMock.mockReset()
    vi.useRealTimers()
  })

  it('refreshes runtime status after the short cache TTL', async () => {
    vi.useFakeTimers()
    getRuntimeStatusMock
      .mockResolvedValueOnce(runtimeStatus(true))
      .mockResolvedValueOnce(runtimeStatus(false))
    const store = useModuleStore()

    await store.fetchRuntimeModules()
    await store.fetchRuntimeModules()
    expect(getRuntimeStatusMock).toHaveBeenCalledTimes(1)
    expect(store.isActive('wallet')).toBe(true)

    vi.advanceTimersByTime(30_001)
    await store.fetchRuntimeModules()
    expect(getRuntimeStatusMock).toHaveBeenCalledTimes(2)
    expect(store.isActive('wallet')).toBe(false)
  })

  it('clears role-specific module state when reset is called', async () => {
    getRuntimeStatusMock.mockResolvedValue(runtimeStatus(true))
    const store = useModuleStore()

    await store.fetchRuntimeModules()
    expect(store.runtimeLoaded).toBe(true)
    store.reset()

    expect(store.runtimeLoaded).toBe(false)
    expect(store.modules).toEqual({})
    expect(store.isActive('wallet')).toBe(false)
  })

  it('does not let a request from the previous identity repopulate the store', async () => {
    let resolveOld: (value: ReturnType<typeof runtimeStatus>) => void = () => {}
    getRuntimeStatusMock.mockImplementationOnce(
      () => new Promise(resolve => { resolveOld = resolve })
    )
    const store = useModuleStore()
    const oldRequest = store.fetchRuntimeModules()
    store.reset()
    getRuntimeStatusMock.mockResolvedValueOnce(runtimeStatus(false))
    await store.fetchRuntimeModules()
    resolveOld(runtimeStatus(true))
    await oldRequest

    expect(store.isActive('wallet')).toBe(false)
  })
})
