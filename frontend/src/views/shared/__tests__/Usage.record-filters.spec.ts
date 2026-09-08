import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const source = readFileSync(
  resolve(process.cwd(), 'src/views/shared/Usage.vue'),
  'utf8',
)

function functionBlock(name: string, nextName: string): string {
  return source.split(`async function ${name}`)[1]?.split(`async function ${nextName}`)[0] ?? ''
}

describe('usage record server filters', () => {
  it('uses server pagination for normal-user API format and transport filters', () => {
    expect(source).toContain('shouldUseServerUserRecordFilters({')
    expect(source).toContain('!isAdminPage.value && !userUsesServerRecordFilters.value')

    const apiFormatHandler = functionBlock('handleFilterApiFormatChange', 'handleFilterStatusChange')
    const statusHandler = functionBlock('handleFilterStatusChange', 'handleFilterClientFamilyChange')
    expect(apiFormatHandler).toContain('isAdminPage.value || userUsesServerRecordFilters.value')
    expect(apiFormatHandler).toContain('await loadRecords(')
    expect(statusHandler).toContain('isAdminPage.value || userUsesServerRecordFilters.value')
    expect(statusHandler).toContain('await loadRecords(')
  })

  it('keeps filtered normal-user pagination and refreshes on the server', () => {
    const pageHandler = functionBlock('handlePageChange', 'handlePageSizeChange')
    const pageSizeHandler = functionBlock('handlePageSizeChange', 'handleFilterSearchChange')
    const refreshHandler = functionBlock('refreshData', 'handleManualRefresh')

    expect(pageHandler).toContain('isAdminPage.value || userUsesServerRecordFilters.value')
    expect(pageSizeHandler).toContain('isAdminPage.value || userUsesServerRecordFilters.value')
    expect(refreshHandler).toContain('isAdminPage.value || userUsesServerRecordFilters.value')
    expect(refreshHandler).toContain('await loadRecords(')
  })

  it('keeps retry and fallback filters local because the user API does not accept them', () => {
    expect(source).toContain('isUserLocalOnlyRecordStatus(filterStatus.value)')
    expect(source).toContain('matchesUsageRecordSearch(record, filterSearch.value)')

    const statusHandler = functionBlock('handleFilterStatusChange', 'handleFilterClientFamilyChange')
    expect(statusHandler).toContain('await loadStats(timeRange.value)')
  })
})
