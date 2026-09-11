import { describe, expect, it } from 'vitest'

import {
  buildUserApiKeyAllowedList,
  buildUserApiKeyListRestriction,
  readUserApiKeyListRestriction,
  retainAvailableAccessValues,
  buildUserApiKeyAllowedProviders,
  buildUserApiKeyMutationPayload,
  formatUserApiKeyAllowedListSummary,
  formatUserApiKeyProvidersSummary,
  normalizeUserApiKeyAllowedList,
  normalizeUserApiKeyAllowedProviders,
  userApiKeyAllowedListsEqual,
  userApiKeyAllowedProvidersEqual,
} from '@/features/api-keys/utils/userKeyPayload'

describe('userKeyPayload', () => {
  it('keeps empty allow and deny rules distinct when the last upstream entry is deleted', () => {
    const values = retainAvailableAccessValues(['removed'], [{ value: 'new' }])
    expect(buildUserApiKeyListRestriction(false, 'allow', values)).toEqual({ allowed: [], denied: null })
    expect(buildUserApiKeyListRestriction(false, 'deny', values)).toEqual({ allowed: null, denied: [] })
    expect(readUserApiKeyListRestriction(null, [])).toEqual({ unrestricted: false, mode: 'deny', values: [] })
    expect(readUserApiKeyListRestriction(null, null)).toEqual({ unrestricted: true, mode: 'allow', values: [] })
  })

  it('writes independent deny rules for providers, endpoints and models and clears the opposite list', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'exclude', providerUnrestricted: false, providerMode: 'deny', allowedProviders: ['p1'],
      apiFormatUnrestricted: false, apiFormatMode: 'deny', allowedApiFormats: ['openai:chat'],
      modelUnrestricted: false, modelMode: 'deny', allowedModels: ['old-model'],
    })).toEqual({
      name: 'exclude', rate_limit: 0,
      allowed_providers: null, denied_providers: ['p1'],
      allowed_api_formats: null, denied_api_formats: ['openai:chat'],
      allowed_models: null, denied_models: ['old-model'],
    })
    expect(buildUserApiKeyListRestriction(false, 'allow', ['p2'])).toEqual({ allowed: ['p2'], denied: null })
  })

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

  it('builds endpoint and model restrictions with the same three-state semantics', () => {
    expect(buildUserApiKeyMutationPayload({
      name: 'scoped-key',
      apiFormatUnrestricted: false,
      allowedApiFormats: ['openai:responses'],
      modelUnrestricted: false,
      allowedModels: [],
    })).toEqual({
      name: 'scoped-key',
      rate_limit: 0,
      allowed_api_formats: ['openai:responses'],
      allowed_models: [],
    })
    expect(buildUserApiKeyAllowedList(true, ['gpt-5'])).toBeNull()
    expect(buildUserApiKeyAllowedList(false, [])).toEqual([])
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

  it('normalizes and compares endpoint/model allowlists as sets', () => {
    expect(normalizeUserApiKeyAllowedList([' gpt-5 ', 'gpt-5', 'claude-sonnet-4'])).toEqual([
      'gpt-5',
      'claude-sonnet-4',
    ])
    expect(userApiKeyAllowedListsEqual(
      ['openai:chat', 'openai:responses'],
      ['openai:responses', 'openai:chat'],
    )).toBe(true)
    expect(userApiKeyAllowedListsEqual(null, [])).toBe(false)
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

  it('formats generic endpoint/model restriction summaries', () => {
    expect(formatUserApiKeyAllowedListSummary(null, '跟随账户可用模型', '模型')).toBe('跟随账户可用模型')
    expect(formatUserApiKeyAllowedListSummary([], '跟随账户可用模型', '模型')).toBe('全部禁用')
    expect(formatUserApiKeyAllowedListSummary(['a', 'b', 'c'], '跟随账户可用模型', '模型')).toBe('3 个模型')
  })
})
