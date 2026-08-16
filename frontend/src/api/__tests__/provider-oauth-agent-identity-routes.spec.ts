import { beforeEach, describe, expect, it, vi } from 'vitest'

const { getMock, postMock } = vi.hoisted(() => ({
  getMock: vi.fn(),
  postMock: vi.fn(),
}))

vi.mock('@/api/client', () => ({
  default: {
    get: getMock,
    post: postMock,
  },
}))

import {
  authorizeProviderWithCookie,
  getProviderCookieAuthorizeTaskStatus,
  getBatchImportOAuthTaskStatus,
  importProviderRefreshToken,
  providerOAuthExpiryToUnixSeconds,
  startProviderCookieAuthorizeTask,
  startBatchImportOAuthTask,
} from '@/api/endpoints/provider_oauth'

const CLAUDE_COOKIE_AUTHORIZE_TIMEOUT_MS = 4 * 60 * 1000

describe('Provider OAuth management routes', () => {
  beforeEach(() => {
    getMock.mockReset()
    postMock.mockReset()
    getMock.mockResolvedValue({ data: {} })
    postMock.mockResolvedValue({ data: {} })
  })

  it('converts RFC3339 response expiries only at the internal runtime boundary', () => {
    expect(providerOAuthExpiryToUnixSeconds('2100-01-01T00:00:00Z')).toBe(4_102_444_800)
    expect(providerOAuthExpiryToUnixSeconds('invalid')).toBeNull()
    expect(providerOAuthExpiryToUnixSeconds(null)).toBeNull()
  })

  it('routes one Agent Identity JSON through the provider-oauth permission surface', async () => {
    const credentials = JSON.stringify({
      auth_mode: 'agentIdentity',
      agent_runtime_id: 'runtime-1',
      agent_private_key: 'private-key',
      task_id: 'task-1',
    })

    await startBatchImportOAuthTask('provider-codex', credentials, 'proxy-1')

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/agent-identity-import/tasks',
      { credentials, proxy_node_id: 'proxy-1' },
    )
  })

  it('routes Agent Identity arrays through the dedicated import surface', async () => {
    const credentials = JSON.stringify([
      {
        auth_mode: 'agentIdentity',
        agent_runtime_id: 'runtime-1',
        agent_private_key: 'private-key-1',
      },
      {
        auth_mode: 'agentIdentity',
        agent_runtime_id: 'runtime-2',
        agent_private_key: 'private-key-2',
      },
    ])

    await startBatchImportOAuthTask('provider-codex', credentials)

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/agent-identity-import/tasks',
      { credentials, proxy_node_id: undefined },
    )
  })

  it('routes sub2api Agent Identity exports through the dedicated import surface', async () => {
    const credentials = JSON.stringify({
      type: 'sub2api-data',
      accounts: [{
        platform: 'openai',
        credentials: {
          auth_mode: 'agentIdentity',
          agent_runtime_id: 'runtime-1',
          agent_private_key: 'private-key',
        },
      }],
    })

    await startBatchImportOAuthTask('provider-codex', credentials)

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/agent-identity-import/tasks',
      { credentials, proxy_node_id: undefined },
    )
  })

  it('routes mixed Agent Identity credentials to the dedicated surface for rejection', async () => {
    const credentials = JSON.stringify([
      { refresh_token: 'ordinary-refresh-token' },
      {
        auth_mode: 'agentIdentity',
        agent_runtime_id: 'runtime-1',
        agent_private_key: 'private-key',
      },
    ])

    await startBatchImportOAuthTask('provider-codex', credentials)

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/agent-identity-import/tasks',
      { credentials, proxy_node_id: undefined },
    )
  })

  it('keeps ordinary batch credentials on the pool permission surface', async () => {
    await startBatchImportOAuthTask('provider-codex', 'refresh-token')

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/batch-import/tasks',
      { credentials: 'refresh-token', proxy_node_id: undefined },
    )
  })

  it('polls Agent Identity tasks through the matching dedicated status route', async () => {
    await getBatchImportOAuthTaskStatus(
      'provider-codex',
      'agent-identity-task-1',
    )

    expect(getMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/agent-identity-import/tasks/agent-identity-task-1',
    )
  })

  it('keeps create requests on the single registration endpoint', async () => {
    await importProviderRefreshToken('provider-codex', {
      access_token: 'access-token',
      create_agent_identity: true,
    })

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-codex/import-refresh-token',
      {
        access_token: 'access-token',
        create_agent_identity: true,
      },
    )
  })

  it('allows the sequential Claude Cookie exchange to outlive the global timeout', async () => {
    const payload = {
      cookie: 'sessionKey=claude-session-key',
      proxy_node_id: 'proxy-1',
    }

    await authorizeProviderWithCookie('provider-claude', payload)

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-claude/cookie-authorize',
      payload,
      { timeout: CLAUDE_COOKIE_AUTHORIZE_TIMEOUT_MS },
    )
  })

  it('starts and polls Claude Cookie batches through the dedicated task routes', async () => {
    const payload = {
      cookies: ['sessionKey=claude-1', 'sessionKey=claude-2'],
      proxy_node_id: 'proxy-1',
    }

    await startProviderCookieAuthorizeTask('provider-claude', payload)
    await getProviderCookieAuthorizeTaskStatus('provider-claude', 'claude-cookie-task-1')

    expect(postMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-claude/cookie-authorize/tasks',
      payload,
    )
    expect(getMock).toHaveBeenCalledWith(
      '/api/admin/provider-oauth/providers/provider-claude/cookie-authorize/tasks/claude-cookie-task-1',
    )
  })
})
