import { beforeEach, describe, expect, it, vi } from 'vitest'

const apiMocks = vi.hoisted(() => ({
  get: vi.fn(),
  put: vi.fn(),
  delete: vi.fn(),
}))

vi.mock('@/api/client', () => ({
  default: { get: apiMocks.get, put: apiMocks.put, delete: apiMocks.delete },
}))

import {
  clearModelsDevCache,
  getExternalModelsAccessConfig,
  getModelsDevList,
  refreshModelsDevList,
  updateExternalModelsAccessConfig,
} from '@/api/models-dev'

beforeEach(() => {
  clearModelsDevCache()
  localStorage.clear()
  apiMocks.get.mockReset()
  apiMocks.put.mockReset()
  apiMocks.delete.mockReset()
  apiMocks.delete.mockResolvedValue({ data: { cleared: true } })
})

describe('external models access config', () => {
  it('reads the configured proxy node', async () => {
    apiMocks.get.mockResolvedValue({ data: { proxy_node_id: 'proxy-1' } })

    await expect(getExternalModelsAccessConfig()).resolves.toEqual({
      proxy_node_id: 'proxy-1',
    })
    expect(apiMocks.get).toHaveBeenCalledWith('/api/admin/models/external/config')
  })

  it.each([
    ['a proxy node', 'proxy-1'],
    ['direct access', null],
  ] as const)('updates %s and clears the browser cache', async (_label, proxyNodeId) => {
    localStorage.setItem('models_dev_cache', JSON.stringify({ timestamp: Date.now(), data: {} }))
    apiMocks.put.mockResolvedValue({
      data: { proxy_node_id: proxyNodeId, cache_cleared: true },
    })

    await expect(updateExternalModelsAccessConfig(proxyNodeId)).resolves.toEqual({
      proxy_node_id: proxyNodeId,
      cache_cleared: true,
    })
    expect(apiMocks.put).toHaveBeenCalledWith(
      '/api/admin/models/external/config',
      { proxy_node_id: proxyNodeId },
    )
    expect(localStorage.getItem('models_dev_cache')).toBeNull()
  })
})

describe('getModelsDevList', () => {
  it('uses current modalities and experimental mode pricing while keeping legacy fallbacks', async () => {
    apiMocks.get.mockResolvedValue({
      data: {
        openai: {
          id: 'openai',
          name: 'OpenAI',
          official: true,
          models: {
            'gpt-test': {
              id: 'gpt-test',
              name: 'GPT Test',
              input: ['text'],
              output: ['text'],
              modalities: {
                input: ['text', 'image'],
                output: ['text', 'image'],
              },
              cost: {
                input: 2,
                output: 4,
                tiers: [{
                  input: 4,
                  output: 8,
                  tier: { type: 'context', size: 100_000 },
                }],
              },
              experimental: {
                modes: {
                  fast: {
                    cost: { input: 4, output: 8 },
                    provider: { body: { service_tier: 'priority' } },
                  },
                },
              },
              reasoning: true,
              reasoning_options: [
                { type: 'effort', values: ['low', 'medium', 'high'] },
                { type: 'toggle' },
              ],
            },
            legacy: {
              id: 'legacy',
              name: 'Legacy',
              input: ['text', 'image'],
              output: ['text'],
              cost: { input: 1, output: 2 },
            },
            'audio-priced': {
              id: 'audio-priced',
              name: 'Audio Priced',
              cost: { input: 1, output: 2, input_audio: 4 },
            },
          },
        },
      },
    })

    const models = await getModelsDevList()
    const current = models.find(model => model.modelId === 'gpt-test')
    const legacy = models.find(model => model.modelId === 'legacy')
    const audioPriced = models.find(model => model.modelId === 'audio-priced')

    expect(current).toMatchObject({
      supportsVision: true,
      inputModalities: ['text', 'image'],
      outputModalities: ['text', 'image'],
      tieredPricing: {
        processing_tiers: { priority: { price_multiplier: 2 } },
      },
      supportsReasoning: true,
      reasoningLevels: ['low', 'medium', 'high'],
    })
    expect(legacy).toMatchObject({
      supportsVision: true,
      inputModalities: ['text', 'image'],
      outputModalities: ['text'],
    })
    expect(audioPriced).toMatchObject({
      inputPrice: 1,
      outputPrice: 2,
      pricingUnsupportedFields: ['input_audio'],
    })
    expect(audioPriced?.tieredPricing).toBeUndefined()
  })

  it('clears the gateway cache before rebuilding the online model list', async () => {
    apiMocks.get.mockResolvedValue({
      data: {
        openai: {
          name: 'OpenAI',
          official: true,
          models: {
            'gpt-test': {
              id: 'gpt-test',
              name: 'GPT Test',
              cost: { input: 2, output: 4 },
            },
          },
        },
      },
    })

    await getModelsDevList(false)
    await refreshModelsDevList(false)

    expect(apiMocks.delete).toHaveBeenCalledOnce()
    expect(apiMocks.delete).toHaveBeenCalledWith('/api/admin/models/external/cache')
    expect(apiMocks.get).toHaveBeenCalledTimes(2)
  })
})
