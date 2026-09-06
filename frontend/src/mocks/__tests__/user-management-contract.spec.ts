import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('@/config/demo', () => ({
  isDemoMode: () => true,
  DEMO_ACCOUNTS: {
    admin: { email: 'admin@demo.aether.io', password: 'demo123' },
    user: { email: 'user@demo.aether.io', password: 'demo123' },
  },
}))

import { handleMockRequest, setMockUserToken } from '../handler'

describe('user management demo contracts', () => {
  beforeEach(() => {
    setMockUserToken('demo-access-token-admin')
  })

  it('returns a list-shaped user group response', async () => {
    const response = await handleMockRequest({
      method: 'GET',
      url: '/api/admin/user-groups',
    })

    expect(response?.data).toEqual({
      items: [],
      default_group_id: null,
    })
  })

  it('creates and lists managed keys only for the selected target user', async () => {
    const aliceId = 'demo-user-uuid-0003'
    const bobId = 'demo-user-uuid-0004'
    const created = await handleMockRequest({
      method: 'POST',
      url: `/api/admin/users/${aliceId}/api-keys`,
      data: JSON.stringify({ name: 'Alice inherited key' }),
    })

    expect(created?.data).toMatchObject({
      name: 'Alice inherited key',
      feature_settings: null,
      is_standalone: false,
    })
    const createdData = created?.data as { key?: string } | undefined
    expect(createdData?.key).toMatch(/^sk-ae-demo-/)

    const aliceKeys = await handleMockRequest({
      method: 'GET',
      url: `/api/admin/users/${aliceId}/api-keys`,
    })
    const bobKeys = await handleMockRequest({
      method: 'GET',
      url: `/api/admin/users/${bobId}/api-keys`,
    })

    expect(aliceKeys?.data).toMatchObject({ total: 1 })
    const aliceKeysData = aliceKeys?.data as { api_keys?: unknown[] } | undefined
    expect(aliceKeysData?.api_keys).toHaveLength(1)
    expect(aliceKeysData?.api_keys?.[0]).toMatchObject({ name: 'Alice inherited key' })
    expect(aliceKeysData?.api_keys?.[0]).not.toHaveProperty('key')
    expect(aliceKeysData?.api_keys?.[0]).not.toHaveProperty('fullKey')
    expect(bobKeys?.data).toEqual({ api_keys: [], total: 0 })
  })

  it('persists all user API key access restrictions across create and update', async () => {
    setMockUserToken('demo-access-token-user')
    const created = await handleMockRequest({
      method: 'POST',
      url: '/api/users/me/api-keys',
      data: JSON.stringify({
        name: 'Restricted demo key',
        allowed_providers: ['provider-001'],
        allowed_api_formats: ['openai:chat'],
        allowed_models: ['gpt-5.1'],
      }),
    })
    const createdKey = created?.data as {
      id: string
      allowed_providers: string[] | null
      allowed_api_formats: string[] | null
      allowed_models: string[] | null
    }

    expect(createdKey).toMatchObject({
      allowed_providers: ['provider-001'],
      allowed_api_formats: ['openai:chat'],
      allowed_models: ['gpt-5.1'],
    })

    await handleMockRequest({
      method: 'PUT',
      url: `/api/users/me/api-keys/${createdKey.id}`,
      data: JSON.stringify({
        allowed_providers: [],
        allowed_api_formats: ['openai:responses'],
        allowed_models: ['gpt-5.1-codex'],
      }),
    })
    const listed = await handleMockRequest({
      method: 'GET',
      url: '/api/users/me/api-keys',
    })
    const persisted = (listed?.data as Array<{ id: string }>).find(key => key.id === createdKey.id)

    expect(persisted).toMatchObject({
      allowed_providers: [],
      allowed_api_formats: ['openai:responses'],
      allowed_models: ['gpt-5.1-codex'],
    })
  })
})
