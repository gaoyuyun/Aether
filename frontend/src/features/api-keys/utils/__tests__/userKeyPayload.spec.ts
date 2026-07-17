import { describe, expect, it } from 'vitest'

import {
  buildUserApiKeyAllowedProviders,
  buildUserApiKeyMutationPayload,
  formatUserApiKeyProvidersSummary,
  normalizeUserApiKeyAllowedProviders,
  userApiKeyAllowedProvidersEqual,
} from '@/features/api-keys/utils/userKeyPayload'

describe('userKeyPayload', () => {
  it('omits concurrent_limit when the field is left blank', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'writer-key',
      rate_limit: 30,
      concurrent_limit: undefined,
    })).toEqual({
      name: 'writer-key',
      rate_limit: 30,
    })
  })

  it('keeps explicit unlimited concurrent_limit values', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'writer-key',
      rate_limit: undefined,
      concurrent_limit: 0,
    })).toEqual({
      name: 'writer-key',
      rate_limit: 0,
      concurrent_limit: 0,
    })
  })

  it('keeps positive concurrent_limit values', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'writer-key',
      rate_limit: 15,
      concurrent_limit: 4,
    })).toEqual({
      name: 'writer-key',
      rate_limit: 15,
      concurrent_limit: 4,
    })
  })

  it('includes allowed_providers when provider restriction is configured', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'writer-key',
      rate_limit: 10,
      providerUnrestricted: false,
      allowedProviders: ['provider-openai'],
    })).toEqual({
      name: 'writer-key',
      rate_limit: 10,
      allowed_providers: ['provider-openai'],
    })

    expect(buildUserApiKeyMutationPayload({
      name: 'writer-key',
      rate_limit: 10,
      providerUnrestricted: true,
      allowedProviders: ['provider-openai'],
    })).toEqual({
      name: 'writer-key',
      rate_limit: 10,
      allowed_providers: null,
    })
  })

  it('builds inherit vs subset allowed_providers values', () => {
    expect(buildUserApiKeyAllowedProviders(true, ['provider-a'])).toBeNull()
    expect(buildUserApiKeyAllowedProviders(false, ['provider-a', 'provider-b'])).toEqual([
      'provider-a',
      'provider-b',
    ])
    expect(buildUserApiKeyAllowedProviders(false, [])).toEqual([])
  })

  it('normalizes string and object provider entries', () => {
    expect(normalizeUserApiKeyAllowedProviders(null)).toBeNull()
    expect(normalizeUserApiKeyAllowedProviders([' provider-a ', 'provider-a', 'provider-b'])).toEqual([
      'provider-a',
      'provider-b',
    ])
    expect(normalizeUserApiKeyAllowedProviders([
      { provider_id: 'provider-a', priority: 1, weight: 1, enabled: true },
      { provider_id: 'provider-b' },
    ])).toEqual(['provider-a', 'provider-b'])
  })

  it('compares provider allowlists as sets while preserving null inheritance', () => {
    expect(userApiKeyAllowedProvidersEqual(null, undefined)).toBe(true)
    expect(userApiKeyAllowedProvidersEqual(null, [])).toBe(false)
    expect(userApiKeyAllowedProvidersEqual(
      ['provider-a', 'provider-b'],
      ['provider-b', 'provider-a'],
    )).toBe(true)
    expect(userApiKeyAllowedProvidersEqual(['provider-a'], ['provider-b'])).toBe(false)
  })

  it('formats provider restriction summaries for the list UI', () => {
    expect(formatUserApiKeyProvidersSummary(null)).toBe('跟随账户可用提供商')
    expect(formatUserApiKeyProvidersSummary([])).toBe('全部禁用')
    expect(formatUserApiKeyProvidersSummary(
      ['provider-a', 'provider-b'],
      { 'provider-a': 'OpenAI', 'provider-b': 'Claude' },
    )).toBe('OpenAI、Claude')
    expect(formatUserApiKeyProvidersSummary(
      ['a', 'b', 'c'],
      new Map([['a', 'A'], ['b', 'B'], ['c', 'C']]),
    )).toBe('3 个提供商')
  })
})
