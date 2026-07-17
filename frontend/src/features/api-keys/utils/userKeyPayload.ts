export interface UserApiKeyMutationPayload {
  name: string
  rate_limit: number
  concurrent_limit?: number
  allowed_providers?: string[] | null
}

interface BuildUserApiKeyMutationPayloadInput {
  name: string
  rate_limit?: number
  concurrent_limit?: number
  providerUnrestricted?: boolean
  allowedProviders?: string[]
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
    payload.allowed_providers = buildUserApiKeyAllowedProviders(
      input.providerUnrestricted,
      input.allowedProviders ?? [],
    )
  }

  return payload
}

/** `null` = inherit the account's full provider allowance; list = key-level subset. */
export function buildUserApiKeyAllowedProviders(
  providerUnrestricted: boolean,
  allowedProviders: string[],
): string[] | null {
  if (providerUnrestricted) {
    return null
  }
  return [...allowedProviders]
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
