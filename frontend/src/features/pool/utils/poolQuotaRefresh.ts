import type { PoolKeyDetail } from '@/api/endpoints/pool'
import type { RefreshQuotaResult } from '@/api/endpoints/keys'

export function mergePoolKeyQuotaSnapshots(
  keys: PoolKeyDetail[],
  results: RefreshQuotaResult['results'],
): PoolKeyDetail[] {
  const quotaByKeyId = new Map<string, NonNullable<RefreshQuotaResult['results'][number]['quota_snapshot']>>()
  for (const result of results) {
    if (result.quota_snapshot) {
      quotaByKeyId.set(result.key_id, result.quota_snapshot)
    }
  }
  if (quotaByKeyId.size === 0) return keys

  return keys.map((key) => {
    const quotaSnapshot = quotaByKeyId.get(key.key_id)
    if (!quotaSnapshot) return key
    return {
      ...key,
      quota_updated_at: quotaSnapshot.updated_at ?? quotaSnapshot.observed_at ?? key.quota_updated_at ?? null,
      status_snapshot: {
        oauth: key.status_snapshot?.oauth ?? { code: 'none' },
        account: key.status_snapshot?.account ?? { code: 'ok', blocked: false },
        quota: quotaSnapshot,
      },
    }
  })
}
