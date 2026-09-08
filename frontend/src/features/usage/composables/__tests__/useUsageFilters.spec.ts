import { describe, expect, it } from 'vitest'
import { ref } from 'vue'

import type { UsageRecord } from '../../types'
import { useUsageFilters } from '../useUsageFilters'

function buildRecord(
  id: string,
  isWebSocket: boolean,
  isStream = true,
): UsageRecord {
  return {
    id,
    model: 'gpt-5',
    input_tokens: 1,
    output_tokens: 1,
    total_tokens: 2,
    cost: 0.01,
    is_stream: isStream,
    is_websocket: isWebSocket,
    created_at: '2026-08-21T00:00:00Z',
  }
}

describe('useUsageFilters', () => {
  it('filters WebSocket records independently from streaming HTTP records', () => {
    const records = ref([
      buildRecord('responses-websocket', true),
      buildRecord('streaming-http', false),
    ])
    const filters = useUsageFilters({ allRecords: records })

    filters.handleFilterStatusChange('websocket')

    expect(filters.filteredRecords.value.map(record => record.id)).toEqual([
      'responses-websocket',
    ])
  })

  it('keeps WebSocket records out of standard and streaming HTTP filters', () => {
    const records = ref([
      buildRecord('responses-websocket', true, true),
      buildRecord('realtime-websocket', true, false),
      buildRecord('streaming-http', false, true),
      buildRecord('standard-http', false, false),
    ])
    const filters = useUsageFilters({ allRecords: records })

    filters.handleFilterStatusChange('stream')
    expect(filters.filteredRecords.value.map(record => record.id)).toEqual([
      'streaming-http',
    ])

    filters.handleFilterStatusChange('standard')
    expect(filters.filteredRecords.value.map(record => record.id)).toEqual([
      'standard-http',
    ])
  })
})
