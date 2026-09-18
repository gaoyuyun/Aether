import type { UsageRecord } from '../types'

export function isUsageCountTokensRequest(record: Pick<UsageRecord, 'request_type' | 'request_path' | 'request_path_and_query'>): boolean {
  if (record.request_type === 'count_tokens') return true

  return [record.request_path, record.request_path_and_query].some(path => {
    const pathname = path?.split('?')[0]?.replace(/\/+$/, '')
    return pathname === '/v1/messages/count_tokens' || pathname === '/v1/messages/count_token'
  })
}
