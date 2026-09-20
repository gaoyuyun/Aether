import { describe, expect, it } from 'vitest'
import { isUsageCountTokensRequest } from '../countTokens'

describe('token counting usage records', () => {
  it.each([
    { request_type: 'count_tokens' },
    { request_type: 'chat', request_path: '/v1/messages/count_tokens' },
    { request_path: '/v1/messages/count_token/' },
    { request_path_and_query: '/v1/messages/count_tokens?beta=true' },
    { request_path: '/v1/messages', request_path_and_query: '/v1/messages/count_tokens/?beta=true' },
    // 网关本地拒绝的请求没有上游请求路径，且旧写入侧会把类型记成 chat，只剩 route_kind 可用
    { request_type: 'chat', route_kind: 'count_tokens' },
    { route_kind: ' COUNT_TOKENS ' },
  ])('recognizes token counting records from their type, route kind or original path: %j', record => {
    expect(isUsageCountTokensRequest(record)).toBe(true)
  })

  it.each([
    {},
    { request_type: null, request_path: null, request_path_and_query: null },
    { request_type: 'chat', request_path: '/v1/messages' },
    { request_path_and_query: '/v1/messages?redirect=/v1/messages/count_tokens' },
    { request_path: '/v1/messages/count_tokens_extra' },
    { request_type: 'chat', route_kind: 'messages' },
    { route_kind: null },
  ])('keeps unrelated requests visible: %j', record => {
    expect(isUsageCountTokensRequest(record)).toBe(false)
  })
})
