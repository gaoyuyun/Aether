import { describe, expect, it } from 'vitest'

import type { UsageRecord } from '../../types'
import { matchesUsageRecordSearch } from '../recordSearch'

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: 'usage-1',
    model: 'gpt-5.6-sol',
    provider: 'OpenAI',
    api_key: {
      id: 'key-1',
      name: 'Production Key',
      display: 'Production Key',
    },
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    cost: 0,
    is_stream: false,
    created_at: '2026-08-21T00:00:00Z',
    ...overrides,
  }
}

describe('matchesUsageRecordSearch', () => {
  it('matches normal-user key and model search terms case-insensitively', () => {
    const usage = record()

    expect(matchesUsageRecordSearch(usage, 'production')).toBe(true)
    expect(matchesUsageRecordSearch(usage, 'GPT-5.6')).toBe(true)
    expect(matchesUsageRecordSearch(usage, 'production gpt-5.6')).toBe(true)
  })

  it('requires every whitespace-delimited search term to match the record', () => {
    const usage = record()

    expect(matchesUsageRecordSearch(usage, 'production missing')).toBe(false)
    expect(matchesUsageRecordSearch(usage, '   ')).toBe(true)
  })
})
