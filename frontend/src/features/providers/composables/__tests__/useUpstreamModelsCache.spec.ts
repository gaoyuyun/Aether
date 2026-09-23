import { beforeEach, describe, expect, it, vi } from 'vitest'

const adminApiMocks = vi.hoisted(() => ({
  queryProviderModels: vi.fn(),
  queryProviderModelsForKeys: vi.fn(),
}))

vi.mock('@/api/admin', () => ({ adminApi: adminApiMocks }))

import { useUpstreamModelsCache } from '../useUpstreamModelsCache'

function response(modelId: string) {
  return {
    success: true,
    data: { models: [{ id: modelId }] },
    provider: { id: 'provider-1', name: 'Provider', display_name: 'Provider' },
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

describe('useUpstreamModelsCache', () => {
  beforeEach(() => {
    adminApiMocks.queryProviderModels.mockReset()
    adminApiMocks.queryProviderModelsForKeys.mockReset()
  })

  it('deduplicates equivalent multi-key model requests', async () => {
    const request = deferred<ReturnType<typeof response>>()
    adminApiMocks.queryProviderModelsForKeys.mockReturnValue(request.promise)
    const { fetchModelsForKeys } = useUpstreamModelsCache()

    const first = fetchModelsForKeys('provider-1', ['key-b', 'key-a', 'key-a'])
    const second = fetchModelsForKeys('provider-1', ['key-a', 'key-b'])
    expect(adminApiMocks.queryProviderModelsForKeys).toHaveBeenCalledTimes(1)
    expect(adminApiMocks.queryProviderModelsForKeys).toHaveBeenCalledWith(
      'provider-1',
      ['key-a', 'key-b'],
      false,
      undefined,
    )

    request.resolve(response('gpt-5.6-sol'))
    await expect(first).resolves.toMatchObject({ models: [{ id: 'gpt-5.6-sol' }] })
    await expect(second).resolves.toMatchObject({ models: [{ id: 'gpt-5.6-sol' }] })
  })

  it('keeps the loading state owned by the latest forced request', async () => {
    const firstRequest = deferred<ReturnType<typeof response>>()
    const forcedRequest = deferred<ReturnType<typeof response>>()
    adminApiMocks.queryProviderModels
      .mockReturnValueOnce(firstRequest.promise)
      .mockReturnValueOnce(forcedRequest.promise)
    const { fetchModels, isLoading } = useUpstreamModelsCache()

    const first = fetchModels('provider-1', 'key-a')
    const forced = fetchModels('provider-1', 'key-a', true)
    expect(isLoading('provider-1', 'key-a')).toBe(true)

    firstRequest.resolve(response('gpt-5.6-sol'))
    await first
    expect(isLoading('provider-1', 'key-a')).toBe(true)

    forcedRequest.resolve(response('gpt-5.6-luna'))
    await forced
    expect(isLoading('provider-1', 'key-a')).toBe(false)
  })

  it('isolates concurrent catalogs and loading state by client version', async () => {
    const oldRequest = deferred<ReturnType<typeof response>>()
    const newRequest = deferred<ReturnType<typeof response>>()
    adminApiMocks.queryProviderModels
      .mockReturnValueOnce(oldRequest.promise)
      .mockReturnValueOnce(newRequest.promise)
    const { fetchModels, isLoading } = useUpstreamModelsCache()

    const oldCatalog = fetchModels('provider-versions', 'key-a', false, '0.153.4')
    const newCatalog = fetchModels('provider-versions', 'key-a', false, '0.200.0')
    const sameCatalog = fetchModels('provider-versions', 'key-a', false, ' 0.200.0 ')
    expect(adminApiMocks.queryProviderModels).toHaveBeenCalledTimes(2)
    expect(isLoading('provider-versions', 'key-a', '0.200.0')).toBe(true)

    oldRequest.resolve(response('gpt-old'))
    await expect(oldCatalog).resolves.toMatchObject({ models: [{ id: 'gpt-old' }] })
    expect(isLoading('provider-versions', 'key-a', '0.153.4')).toBe(false)
    expect(isLoading('provider-versions', 'key-a', '0.200.0')).toBe(true)

    newRequest.resolve(response('gpt-6-sol'))
    await expect(newCatalog).resolves.toMatchObject({ models: [{ id: 'gpt-6-sol' }] })
    await expect(sameCatalog).resolves.toMatchObject({ models: [{ id: 'gpt-6-sol' }] })
    expect(isLoading('provider-versions', 'key-a', '0.200.0')).toBe(false)
  })

  it('keeps batch requests for different client versions separate', async () => {
    const older = deferred<ReturnType<typeof response>>()
    const newer = deferred<ReturnType<typeof response>>()
    adminApiMocks.queryProviderModelsForKeys
      .mockReturnValueOnce(older.promise)
      .mockReturnValueOnce(newer.promise)
    const { fetchModelsForKeys } = useUpstreamModelsCache()

    const first = fetchModelsForKeys('provider-batch', ['key-a', 'key-b'], false, '0.153.4')
    const second = fetchModelsForKeys('provider-batch', ['key-b', 'key-a'], false, '0.200.0')
    const same = fetchModelsForKeys('provider-batch', ['key-a', 'key-b'], false, ' 0.200.0 ')
    expect(adminApiMocks.queryProviderModelsForKeys).toHaveBeenCalledTimes(2)
    expect(adminApiMocks.queryProviderModelsForKeys).toHaveBeenLastCalledWith(
      'provider-batch', ['key-a', 'key-b'], false, '0.200.0',
    )
    older.resolve(response('gpt-old'))
    newer.resolve(response('gpt-6-sol'))
    await expect(first).resolves.toMatchObject({ models: [{ id: 'gpt-old' }] })
    await expect(second).resolves.toMatchObject({ models: [{ id: 'gpt-6-sol' }] })
    await expect(same).resolves.toMatchObject({ models: [{ id: 'gpt-6-sol' }] })
  })
})
