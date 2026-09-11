import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  invalidateUserAccessControlOptions,
  useUserAccessControlOptions,
} from '../useUserAccessControlOptions'
import { markAccessControlCatalogChanged } from '@/utils/accessControlCatalog'

const meApiMock = vi.hoisted(() => ({
  getAvailableProviders: vi.fn(),
  getAvailableModelOptions: vi.fn(),
}))

vi.mock('@/api/me', () => ({
  meApi: meApiMock,
}))

afterEach(() => {
  invalidateUserAccessControlOptions()
  vi.clearAllMocks()
})

describe('useUserAccessControlOptions', () => {
  it('loads every model page before replacing the catalog', async () => {
    meApiMock.getAvailableProviders.mockResolvedValue([])
    meApiMock.getAvailableModelOptions
      .mockResolvedValueOnce({ models: [{ name: 'first' }], total: 2 })
      .mockResolvedValueOnce({ models: [{ name: 'last' }], total: 2 })
    const options = useUserAccessControlOptions()
    await options.loadAccessControlOptions()
    expect(meApiMock.getAvailableModelOptions).toHaveBeenLastCalledWith({ limit: 1000, skip: 1 })
    expect(options.modelOptions.value.map(item => item.value)).toEqual(['first', 'last'])
  })

  it('keeps the last complete catalog when a later page fails', async () => {
    meApiMock.getAvailableProviders.mockResolvedValue([])
    meApiMock.getAvailableModelOptions.mockResolvedValueOnce({ models: [{ name: 'keep' }], total: 1 })
    const options = useUserAccessControlOptions()
    await options.loadAccessControlOptions()
    meApiMock.getAvailableModelOptions
      .mockResolvedValueOnce({ models: [{ name: 'partial' }], total: 2 })
      .mockRejectedValueOnce(new Error('network unavailable'))
    await expect(options.loadAccessControlOptions({ force: true })).rejects.toThrow('network unavailable')
    expect(options.modelOptions.value.map(item => item.value)).toEqual(['keep'])
  })

  it('ignores an in-flight catalog after invalidation', async () => {
    let finish!: (value: { models: Array<{ name: string }>; total: number }) => void
    meApiMock.getAvailableProviders.mockResolvedValue([])
    meApiMock.getAvailableModelOptions.mockReturnValueOnce(new Promise(resolve => { finish = resolve }))
    const options = useUserAccessControlOptions()
    const pending = options.loadAccessControlOptions()
    invalidateUserAccessControlOptions()
    finish({ models: [{ name: 'removed' }], total: 1 })
    await expect(pending).rejects.toThrow('访问限制选项已更新')
    expect(options.modelOptions.value).toEqual([])
  })

  it('shares one lightweight catalog load across repeated consumers', async () => {
    meApiMock.getAvailableProviders.mockResolvedValue([
      {
        id: 'provider-2',
        name: 'Zulu',
        endpoints: [{ api_format: 'openai:responses' }],
      },
      {
        id: 'provider-1',
        name: 'Alpha',
        endpoints: [
          { api_format: 'openai:chat' },
          { api_format: 'openai:responses' },
        ],
      },
    ])
    meApiMock.getAvailableModelOptions.mockResolvedValue({
      models: [
        { name: 'gpt-5' },
        { name: 'claude-sonnet-4' },
        { name: 'gpt-5' },
      ],
      total: 3,
    })

    const first = useUserAccessControlOptions()
    const second = useUserAccessControlOptions()
    await Promise.all([
      first.loadAccessControlOptions(),
      second.loadAccessControlOptions(),
    ])
    await first.loadAccessControlOptions()
    markAccessControlCatalogChanged()
    await first.loadAccessControlOptions()

    expect(meApiMock.getAvailableProviders).toHaveBeenCalledTimes(2)
    expect(meApiMock.getAvailableProviders).toHaveBeenCalledWith({ view: 'access-options' })
    expect(meApiMock.getAvailableModelOptions).toHaveBeenCalledTimes(2)
    expect(meApiMock.getAvailableModelOptions).toHaveBeenCalledWith({ limit: 1000 })
    expect(first.providerOptions.value).toEqual([
      { value: 'provider-1', label: 'Alpha' },
      { value: 'provider-2', label: 'Zulu' },
    ])
    expect(first.apiFormatOptions.value).toEqual([
      { value: 'openai:chat', label: 'openai:chat' },
      { value: 'openai:responses', label: 'openai:responses' },
    ])
    expect(first.modelOptions.value).toEqual([
      { value: 'claude-sonnet-4', label: 'claude-sonnet-4' },
      { value: 'gpt-5', label: 'gpt-5' },
    ])
  })

  it('retries after an initial load failure', async () => {
    meApiMock.getAvailableProviders
      .mockRejectedValueOnce(new Error('temporary failure'))
      .mockResolvedValueOnce([])
    meApiMock.getAvailableModelOptions.mockResolvedValue({ models: [], total: 0 })
    const options = useUserAccessControlOptions()

    await expect(options.loadAccessControlOptions()).rejects.toThrow('temporary failure')
    await expect(options.loadAccessControlOptions()).resolves.toBeUndefined()

    expect(meApiMock.getAvailableProviders).toHaveBeenCalledTimes(2)
  })
})
