import type { UsageRecord } from '../types'

const COUNT_TOKENS_OPERATION = 'count_tokens'
const COUNT_TOKENS_PATHS = ['/v1/messages/count_tokens', '/v1/messages/count_token']

function isCountTokensOperation(value: string | null | undefined): boolean {
  return value?.trim().toLowerCase() === COUNT_TOKENS_OPERATION
}

/**
 * 判断一条使用记录是否为 Claude Token 计数请求。
 *
 * 必须同时看三个信号，因为没有一个能覆盖全部记录，这与后端四个存储实现的过滤谓词口径一致：
 * - request_type 只对"网关开始携带已规划 API 操作"之后写入的记录有效；更早的记录会写成 chat，
 *   因为 Token 计数与普通对话共用 claude:messages 契约，请求体也只是一个普通消息列表。
 * - 请求路径在网关本地拒绝请求时是空的，因为压根没构造过上游请求。
 * - route_kind 是控制面始终会解析出来的信号。
 */
export function isUsageCountTokensRequest(record: Pick<UsageRecord, 'request_type' | 'request_path' | 'request_path_and_query' | 'route_kind'>): boolean {
  if (isCountTokensOperation(record.request_type) || isCountTokensOperation(record.route_kind)) return true

  return [record.request_path, record.request_path_and_query].some(path => {
    const pathname = path?.split('?')[0]?.replace(/\/+$/, '')
    return COUNT_TOKENS_PATHS.includes(pathname ?? '')
  })
}
