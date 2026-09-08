import { describe, expect, it } from 'vitest'

import {
  fixedEndpointUpstreamStreamPolicy,
  isWebSocketEndpointApiFormat,
} from '../endpoint-protocol-policy'

describe('fixedEndpointUpstreamStreamPolicy', () => {
  it('uses the fixed Codex transport mode for each protocol', () => {
    expect(fixedEndpointUpstreamStreamPolicy('codex', 'openai:responses')).toBe('force_stream')
    expect(fixedEndpointUpstreamStreamPolicy('codex', 'openai:search')).toBe('force_non_stream')
  })

  it('applies Search synchronization by format and keeps other formats configurable', () => {
    expect(fixedEndpointUpstreamStreamPolicy('custom', 'openai:search')).toBe('force_non_stream')
    expect(fixedEndpointUpstreamStreamPolicy('codex', 'openai:responses:compact')).toBeNull()
  })

  it('identifies formats whose transport is WebSocket rather than HTTP streaming', () => {
    expect(isWebSocketEndpointApiFormat('openai:realtime')).toBe(true)
    expect(isWebSocketEndpointApiFormat(' CODEX:LIVE ')).toBe(true)
    expect(isWebSocketEndpointApiFormat('openai:responses')).toBe(false)
  })
})
