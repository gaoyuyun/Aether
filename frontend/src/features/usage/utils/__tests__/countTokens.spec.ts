import { describe, expect, it } from 'vitest'
import { isUsageCountTokensRequest } from '../countTokens'

describe('token counting usage records', () => {
  it.each([
    { request_type: 'count_tokens' },
    { request_type: 'chat', request_path: '/v1/messages/count_tokens' },
    { request_path: '/v1/messages/count_token/' },
    { request_path_and_query: '/v1/messages/count_tokens?beta=true' },
    { request_path: '/v1/messages', request_path_and_query: '/v1/messages/count_tokens/?beta=true' },
  ])('recognizes token counting records from their type or original path: %j', record => {
    expect(isUsageCountTokensRequest(record)).toBe(true)
  })

  it.each([
    {},
    { request_type: null, request_path: null, request_path_and_query: null },
    { request_type: 'chat', request_path: '/v1/messages' },
    { request_path_and_query: '/v1/messages?redirect=/v1/messages/count_tokens' },
    { request_path: '/v1/messages/count_tokens_extra' },
  ])('keeps unrelated requests visible: %j', record => {
    expect(isUsageCountTokensRequest(record)).toBe(false)
  })
})
