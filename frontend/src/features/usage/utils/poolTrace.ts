import type { CandidateRecord } from '@/api/requestTrace'

export const TIMELINE_STATUS: CandidateRecord['status'][] = [
  'success',
  'failed',
  'skipped',
  'cancelled',
  'pending',
  'streaming',
  'available',
  'unused',
  'stream_interrupted',
]

const PROVIDER_TYPE_LIKE_NAMES = new Set<string>([
  'codex',
  'kiro',
  'antigravity',
  'claude_code',
  'claude code',
  'gemini_cli',
  'gemini cli',
  'oauth',
  'api_key',
  'api key',
])

const toInt = (value: unknown, defaultValue = 0): number => {
  const num = Number(value)
  return Number.isFinite(num) ? Math.trunc(num) : defaultValue
}

export const makeAttemptKey = (candidateIndex: number, retryIndex: number): string => {
  return `${candidateIndex}:${retryIndex}`
}

export const isPoolParticipatedCandidate = (candidate: CandidateRecord): boolean => {
  return TIMELINE_STATUS.includes(candidate.status)
}

export const isAttemptedCandidate = (
  candidate: Pick<CandidateRecord, 'status' | 'started_at'>,
): boolean => {
  switch (candidate.status) {
    case 'streaming':
    case 'success':
    case 'failed':
    case 'cancelled':
    case 'stream_interrupted':
      return true
    case 'pending':
      return Boolean(candidate.started_at)
    case 'available':
    case 'unused':
    case 'skipped':
    default:
      return false
  }
}

export function buildPoolGroupVisibleAttempts(
  attempts: CandidateRecord[],
): CandidateRecord[] {
  return attempts.filter(isPoolParticipatedCandidate)
}

export const parseTimelineStatus = (value: unknown): CandidateRecord['status'] | null => {
  if (typeof value !== 'string') return null
  const normalized = value.trim().toLowerCase()
  if ((TIMELINE_STATUS as string[]).includes(normalized)) {
    return normalized as CandidateRecord['status']
  }
  return null
}

export const extractPoolGroupId = (
  candidate: Pick<CandidateRecord, 'extra_data' | 'provider_id'>,
): string | null => {
  const extra = candidate.extra_data
  if (!extra || typeof extra !== 'object' || Array.isArray(extra)) return null
  const raw = extra as Record<string, unknown>

  for (const key of ['pool_group_id', 'candidate_group_id']) {
    const value = raw[key]
    if (typeof value === 'string') {
      const text = value.trim()
      if (text) return text
    }
  }

  const routingTrace = raw.routing_trace
  if (routingTrace && typeof routingTrace === 'object' && !Array.isArray(routingTrace)) {
    const poolExpansion = (routingTrace as Record<string, unknown>).pool_expansion
    if (Array.isArray(poolExpansion)) {
      for (const item of poolExpansion) {
        if (!item || typeof item !== 'object' || Array.isArray(item)) continue
        const value = (item as Record<string, unknown>).pool_group_id
        if (typeof value !== 'string') continue
        const text = value.trim()
        if (text) return text
      }
    }
  }

  if (raw.pool_key_index !== undefined && raw.pool_key_index !== null) {
    const providerId = String(candidate.provider_id || '').trim()
    if (providerId) return providerId
  }

  return null
}

export function buildPoolParticipatedCandidates(
  rawTimeline: CandidateRecord[],
  attempts: unknown,
  requestId?: string | null,
): CandidateRecord[] {
  const fromTrace = rawTimeline.filter(
    candidate => extractPoolGroupId(candidate) !== null && isPoolParticipatedCandidate(candidate),
  )
  const fromAudit = buildPoolAttemptCandidatesFromAudit(rawTimeline, attempts, requestId)

  const traceKeys = new Set(
    fromTrace.map(candidate => makeAttemptKey(candidate.candidate_index, candidate.retry_index)),
  )
  const merged = [...fromTrace]
  for (const candidate of fromAudit) {
    const key = makeAttemptKey(candidate.candidate_index, candidate.retry_index)
    if (!traceKeys.has(key)) {
      merged.push(candidate)
      traceKeys.add(key)
    }
  }

  // Some runtime and historical usage snapshots omit pool metadata. Attach
  // them only when the provider has a single unambiguous pool group.
  const groupsByProvider = new Map<string, Set<string>>()
  for (const candidate of merged) {
    const providerId = candidate.provider_id?.trim()
    const groupId = extractPoolGroupId(candidate)
    if (!providerId || !groupId) continue
    const groups = groupsByProvider.get(providerId) ?? new Set<string>()
    groups.add(groupId)
    groupsByProvider.set(providerId, groups)
  }
  for (const candidate of rawTimeline) {
    const key = makeAttemptKey(candidate.candidate_index, candidate.retry_index)
    if (traceKeys.has(key)) continue
    const groups = groupsByProvider.get(candidate.provider_id?.trim() ?? '')
    if (groups?.size !== 1) continue
    merged.push({
      ...candidate,
      extra_data: { ...candidate.extra_data, pool_group_id: [...groups][0] },
    })
    traceKeys.add(key)
  }

  return merged.sort((a, b) => {
    if (a.candidate_index !== b.candidate_index) {
      return a.candidate_index - b.candidate_index
    }
    return a.retry_index - b.retry_index
  })
}

export function buildPoolAttemptCandidatesFromAudit(
  rawTimeline: CandidateRecord[],
  attempts: unknown,
  requestId?: string | null,
): CandidateRecord[] {
  if (!Array.isArray(attempts) || attempts.length === 0) return []

  const providerNameById = new Map<string, string>()
  for (const candidate of rawTimeline) {
    const providerId = String(candidate.provider_id || '').trim()
    const providerName = String(candidate.provider_name || '').trim()
    if (!providerId || !providerName) continue
    if (!providerNameById.has(providerId)) {
      providerNameById.set(providerId, providerName)
    }
  }

  const traceMap = new Map<string, CandidateRecord>()
  for (const candidate of rawTimeline) {
    traceMap.set(makeAttemptKey(candidate.candidate_index, candidate.retry_index), candidate)
  }

  return attempts
    .map((item, index) => {
      if (!item || typeof item !== 'object' || Array.isArray(item)) return null
      const raw = item as Record<string, unknown>
      const candidateIndex = toInt(raw.candidate_index, index)
      const retryIndex = toInt(raw.retry_index, 0)
      const key = makeAttemptKey(candidateIndex, retryIndex)
      const fromTrace = traceMap.get(key)
      const parsedStatus = parseTimelineStatus(raw.status)

      if (!fromTrace && parsedStatus === null) {
        return null
      }

      const merged: CandidateRecord = fromTrace
        ? { ...fromTrace }
        : {
            id: `pool-${requestId || 'unknown'}-${candidateIndex}-${retryIndex}-${index}`,
            request_id: requestId || '',
            candidate_index: candidateIndex,
            retry_index: retryIndex,
            provider_id: undefined,
            provider_name: undefined,
            endpoint_id: undefined,
            key_id: undefined,
            key_name: undefined,
            status: 'failed',
            is_cached: false,
            created_at: new Date(0).toISOString(),
          }

      // The execution trace owns the attempt result; the scheduling audit is
      // a fallback for missing attempts and may predate completion.
      if (!fromTrace && parsedStatus !== null) {
        merged.status = parsedStatus
      }
      if (!merged.provider_id && typeof raw.provider_id === 'string') merged.provider_id = raw.provider_id
      if (!merged.provider_name && typeof raw.provider_name === 'string') merged.provider_name = raw.provider_name
      if (!merged.endpoint_id && typeof raw.endpoint_id === 'string') merged.endpoint_id = raw.endpoint_id
      if (!merged.key_id && typeof raw.key_id === 'string') merged.key_id = raw.key_id
      if (!merged.key_name && typeof raw.key_name === 'string') merged.key_name = raw.key_name
      if (!fromTrace && typeof raw.status_code === 'number') merged.status_code = raw.status_code
      if (!fromTrace && typeof raw.error_type === 'string') merged.error_type = raw.error_type
      const rawPoolGroupId = typeof raw.pool_group_id === 'string' ? raw.pool_group_id.trim() : ''
      const finalPoolGroupId = extractPoolGroupId(merged) || rawPoolGroupId || merged.provider_id?.trim()
      if (finalPoolGroupId) {
        merged.extra_data = {
          ...(merged.extra_data || {}),
          pool_group_id: finalPoolGroupId,
        }
      }

      const mergedProviderId = String(merged.provider_id || '').trim()
      if (mergedProviderId) {
        const inferredProviderName = providerNameById.get(mergedProviderId)
        const currentProviderName = String(merged.provider_name || '').trim()
        if (
          inferredProviderName
          && (
            !currentProviderName
            || PROVIDER_TYPE_LIKE_NAMES.has(currentProviderName.toLowerCase())
          )
        ) {
          merged.provider_name = inferredProviderName
        }
      }

      return isPoolParticipatedCandidate(merged) ? merged : null
    })
    .filter((item): item is CandidateRecord => item !== null)
}
