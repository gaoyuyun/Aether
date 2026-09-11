export type AccessRestrictionMode = 'allow' | 'deny'

/** Null inherits; an explicit empty allowlist denies all; an empty denylist excludes nothing. */
export function buildUserApiKeyListRestriction(unrestricted: boolean, mode: AccessRestrictionMode, values: string[]) {
  return {
    allowed: unrestricted || mode === 'deny' ? null : [...values],
    denied: unrestricted || mode === 'allow' ? null : [...values],
  }
}

export function readUserApiKeyListRestriction(allowed: unknown, denied: unknown) {
  const allowedValues = normalizeUserApiKeyAllowedList(allowed)
  const deniedValues = normalizeUserApiKeyAllowedList(denied)
  return {
    unrestricted: allowedValues == null && deniedValues == null,
    mode: (deniedValues != null ? 'deny' : 'allow') as AccessRestrictionMode,
    values: deniedValues ?? allowedValues ?? [],
  }
}

export function retainAvailableAccessValues(values: string[], options: Array<{ value: string }>): string[] {
  const available = new Set(options.map(option => option.value))
  return values.filter(value => available.has(value))
}

export interface UserApiKeyMutationPayload {
  name: string
  rate_limit: number
  concurrent_limit?: number
  allowed_providers?: string[] | null
  allowed_api_formats?: string[] | null
  allowed_models?: string[] | null
  denied_providers?: string[] | null
  denied_api_formats?: string[] | null
  denied_models?: string[] | null
}

interface BuildUserApiKeyMutationPayloadInput {
  name: string
  rate_limit?: number
  concurrent_limit?: number
  providerUnrestricted?: boolean
  providerMode?: AccessRestrictionMode
  allowedProviders?: string[]
  apiFormatUnrestricted?: boolean
  apiFormatMode?: AccessRestrictionMode
  allowedApiFormats?: string[]
  modelUnrestricted?: boolean
  modelMode?: AccessRestrictionMode
  allowedModels?: string[]
}

export function buildUserApiKeyMutationPayload(
  input: BuildUserApiKeyMutationPayloadInput,
): UserApiKeyMutationPayload {
  const payload: UserApiKeyMutationPayload = {
    name: input.name,
    rate_limit: input.rate_limit ?? 0,
    ...(input.concurrent_limit === undefined ? {} : { concurrent_limit: input.concurrent_limit }),
  }

  if (input.providerUnrestricted !== undefined) {
    const restriction = buildUserApiKeyListRestriction(input.providerUnrestricted, input.providerMode ?? 'allow', input.allowedProviders ?? [])
    payload.allowed_providers = restriction.allowed
    if (input.providerMode !== undefined) payload.denied_providers = restriction.denied
  }
  if (input.apiFormatUnrestricted !== undefined) {
    const restriction = buildUserApiKeyListRestriction(input.apiFormatUnrestricted, input.apiFormatMode ?? 'allow', input.allowedApiFormats ?? [])
    payload.allowed_api_formats = restriction.allowed
    if (input.apiFormatMode !== undefined) payload.denied_api_formats = restriction.denied
  }
  if (input.modelUnrestricted !== undefined) {
    const restriction = buildUserApiKeyListRestriction(input.modelUnrestricted, input.modelMode ?? 'allow', input.allowedModels ?? [])
    payload.allowed_models = restriction.allowed
    if (input.modelMode !== undefined) payload.denied_models = restriction.denied
  }

  return payload
}

/** `null` = inherit the account's full provider allowance; list = key-level subset. */
export function buildUserApiKeyAllowedProviders(
  providerUnrestricted: boolean,
  allowedProviders: string[],
): string[] | null {
  return buildUserApiKeyAllowedList(providerUnrestricted, allowedProviders)
}

export function buildUserApiKeyAllowedList(
  unrestricted: boolean,
  allowedValues: string[],
): string[] | null {
  if (unrestricted) {
    return null
  }
  return [...allowedValues]
}

export function normalizeUserApiKeyAllowedList(value: unknown): string[] | null {
  if (value == null) return null
  if (!Array.isArray(value)) return null
  const normalized: string[] = []
  const seen = new Set<string>()
  for (const item of value) {
    const text = typeof item === 'string' ? item.trim() : ''
    if (!text || seen.has(text)) continue
    seen.add(text)
    normalized.push(text)
  }
  return normalized
}

export function normalizeUserApiKeyAllowedProviders(value: unknown): string[] | null {
  if (value == null) {
    return null
  }
  if (!Array.isArray(value)) {
    return null
  }

  const ids: string[] = []
  const seen = new Set<string>()
  for (const item of value) {
    let id = ''
    if (typeof item === 'string') {
      id = item.trim()
    } else if (item && typeof item === 'object' && 'provider_id' in item) {
      id = String((item as { provider_id?: unknown }).provider_id ?? '').trim()
    }
    if (!id || seen.has(id)) {
      continue
    }
    seen.add(id)
    ids.push(id)
  }
  return ids
}

export function userApiKeyAllowedProvidersEqual(
  left: unknown,
  right: unknown,
): boolean {
  const normalizedLeft = normalizeUserApiKeyAllowedProviders(left)
  const normalizedRight = normalizeUserApiKeyAllowedProviders(right)
  if (normalizedLeft == null || normalizedRight == null) {
    return normalizedLeft == null && normalizedRight == null
  }
  if (normalizedLeft.length !== normalizedRight.length) {
    return false
  }
  const rightIds = new Set(normalizedRight)
  return normalizedLeft.every(id => rightIds.has(id))
}

export function userApiKeyAllowedListsEqual(left: unknown, right: unknown): boolean {
  const normalizedLeft = normalizeUserApiKeyAllowedList(left)
  const normalizedRight = normalizeUserApiKeyAllowedList(right)
  if (normalizedLeft == null || normalizedRight == null) {
    return normalizedLeft == null && normalizedRight == null
  }
  if (normalizedLeft.length !== normalizedRight.length) return false
  const rightValues = new Set(normalizedRight)
  return normalizedLeft.every(value => rightValues.has(value))
}

export function formatUserApiKeyAllowedListSummary(
  values: string[] | null | undefined,
  unrestrictedLabel: string,
  itemLabel: string,
): string {
  if (values == null) return unrestrictedLabel
  if (values.length === 0) return '全部禁用'
  if (values.length <= 2) return values.join('、')
  return `${values.length} 个${itemLabel}`
}

export function formatUserApiKeyProvidersSummary(
  allowedProviders: string[] | null | undefined,
  providerNameById?: Map<string, string> | Record<string, string>,
): string {
  if (allowedProviders == null) {
    return '跟随账户可用提供商'
  }
  if (allowedProviders.length === 0) {
    return '全部禁用'
  }

  const resolveName = (id: string): string => {
    if (providerNameById instanceof Map) {
      return providerNameById.get(id) || id
    }
    if (providerNameById) {
      return providerNameById[id] || id
    }
    return id
  }

  if (allowedProviders.length <= 2) {
    return allowedProviders.map(resolveName).join('、')
  }
  return `${allowedProviders.length} 个提供商`
}
