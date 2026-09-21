import { describe, expect, it } from 'vitest'

import { normalizeChatPiiRedactionProviderConfig, normalizePoolAdvancedConfig } from '@/api/endpoints/types'

describe('normalizePoolAdvancedConfig', () => {
  it('keeps object payloads, including empty objects', () => {
    expect(normalizePoolAdvancedConfig({})).toEqual({})
    expect(normalizePoolAdvancedConfig({ rate_limit_cooldown_seconds: 300 })).toEqual({ rate_limit_cooldown_seconds: 300 })
  })

  it('maps legacy boolean payloads to the current object semantics', () => {
    expect(normalizePoolAdvancedConfig(true)).toEqual({})
    expect(normalizePoolAdvancedConfig(false)).toBeNull()
  })

  it('drops unsupported payload shapes', () => {
    expect(normalizePoolAdvancedConfig(null)).toBeNull()
    expect(normalizePoolAdvancedConfig('enabled')).toBeNull()
    expect(normalizePoolAdvancedConfig(['lru'])).toBeNull()
  })
})


describe('normalizeChatPiiRedactionProviderConfig', () => {
  it('defaults unsupported payloads to disabled', () => {
    expect(normalizeChatPiiRedactionProviderConfig(null)).toEqual({ enabled: false })
    expect(normalizeChatPiiRedactionProviderConfig({})).toEqual({ enabled: false })
    expect(normalizeChatPiiRedactionProviderConfig({ enabled: 'yes' })).toEqual({ enabled: false })
  })

  it('passes through enabled state only', () => {
    expect(normalizeChatPiiRedactionProviderConfig({ enabled: true })).toEqual({ enabled: true })
    expect(normalizeChatPiiRedactionProviderConfig({ enabled: false, entities: ['email'] })).toEqual({ enabled: false })
  })
})

describe('normalizeProviderCooldownConfig', () => {
  it('falls back to defaults and coerces partial objects', async () => {
    const { normalizeProviderCooldownConfig, DEFAULT_PROVIDER_COOLDOWN_CONFIG } = await import('../provider')
    expect(normalizeProviderCooldownConfig(undefined)).toEqual(DEFAULT_PROVIDER_COOLDOWN_CONFIG)
    expect(normalizeProviderCooldownConfig(null)).toEqual(DEFAULT_PROVIDER_COOLDOWN_CONFIG)
    expect(normalizeProviderCooldownConfig({ disable: true })).toEqual({
      disable: true,
      transient_error_seconds: 60,
      model_level: false,
    })
    expect(normalizeProviderCooldownConfig({ transient_error_seconds: 15.9, model_level: true })).toEqual({
      disable: false,
      transient_error_seconds: 15,
      model_level: true,
    })
    expect(normalizeProviderCooldownConfig({ transient_error_seconds: -1 }).transient_error_seconds).toBe(60)
  })
})
