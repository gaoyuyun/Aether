import { describe, expect, it } from 'vitest'

import {
  isUserLocalOnlyRecordStatus,
  shouldUseServerUserRecordFilters,
} from '../recordFilterPolicy'

describe('normal-user usage record filter policy', () => {
  it.each(['has_retry', 'has_fallback'] as const)(
    'keeps %s local even when combined with server-supported filters',
    (status) => {
      expect(isUserLocalOnlyRecordStatus(status)).toBe(true)
      expect(shouldUseServerUserRecordFilters({
        search: 'production',
        apiFormat: 'codex:live',
        status,
      })).toBe(false)
    },
  )

  it.each([
    { search: '', apiFormat: '__all__', status: 'websocket' as const },
    { search: '', apiFormat: 'codex:live', status: '__all__' as const },
    { search: 'production', apiFormat: '__all__', status: '__all__' as const },
    { search: 'production', apiFormat: 'codex:live', status: 'websocket' as const },
  ])('uses server pagination for supported filter combination %#', (filters) => {
    expect(shouldUseServerUserRecordFilters(filters)).toBe(true)
  })

  it('keeps the unfiltered normal-user list on its existing local pagination path', () => {
    expect(shouldUseServerUserRecordFilters({
      search: '   ',
      apiFormat: '__all__',
      status: '__all__',
    })).toBe(false)
  })
})
