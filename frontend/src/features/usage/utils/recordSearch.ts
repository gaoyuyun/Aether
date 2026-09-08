import type { UsageRecord } from '../types'

type SearchableUsageRecord = Pick<
  UsageRecord,
  | 'model'
  | 'target_model'
  | 'provider'
  | 'api_key'
  | 'api_key_name'
  | 'provider_key_name'
>

function normalizedSearchValues(record: SearchableUsageRecord): string[] {
  return [
    record.model,
    record.target_model,
    record.provider,
    record.api_key?.id,
    record.api_key?.name,
    record.api_key?.display,
    record.api_key_name,
    record.provider_key_name,
  ]
    .filter((value): value is string => typeof value === 'string' && value.trim().length > 0)
    .map(value => value.toLocaleLowerCase())
}

export function matchesUsageRecordSearch(
  record: SearchableUsageRecord,
  search: string,
): boolean {
  const keywords = search
    .trim()
    .toLocaleLowerCase()
    .split(/\s+/)
    .filter(Boolean)
  if (keywords.length === 0) return true

  const values = normalizedSearchValues(record)
  return keywords.every(keyword => values.some(value => value.includes(keyword)))
}
