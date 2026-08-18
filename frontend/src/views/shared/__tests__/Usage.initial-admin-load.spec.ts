import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const source = readFileSync(
  resolve(process.cwd(), 'src/views/shared/Usage.vue'),
  'utf8',
)

describe('admin usage initial loading', () => {
  it('renders records before scheduling non-critical analytics', () => {
    const mountedBlock = source
      .split('onMounted(async () => {')[1]
      ?.split('// 处理时间范围变化')[0]

    expect(mountedBlock).toBeTruthy()
    expect(mountedBlock).toContain('await loadRecords(')
    expect(mountedBlock).toContain('{ loadExactTotal: false }')
    expect(mountedBlock).toContain('scheduleDeferredAnalytics()')
    expect(mountedBlock?.indexOf('await loadRecords('))
      .toBeLessThan(mountedBlock?.indexOf('scheduleDeferredAnalytics()') ?? -1)
    expect(mountedBlock).not.toContain('loadAdminUsers')
  })

  it('uses requestIdleCallback before mounting analytics charts', () => {
    const schedulerBlock = source
      .split('function scheduleDeferredAnalytics()')[1]
      ?.split('// 时间范围选择')[0]

    expect(schedulerBlock).toBeTruthy()
    expect(schedulerBlock).toContain('idleWindow.requestIdleCallback')
    expect(schedulerBlock).toContain('analyticsReady.value = true')
    expect(source).toContain('v-if="statsExpanded && analyticsReady"')
  })

  it('derives the polling set from the backend lifecycle status, not the display status', () => {
    const activeIdsBlock = source
      .split('const activeRequestIds = computed(() => {')[1]
      ?.split('})')[0]

    expect(activeIdsBlock).toBeTruthy()
    expect(activeIdsBlock).toContain('isUsageRecordPollable(record)')
    expect(activeIdsBlock).not.toContain('resolveDisplayRequestStatus')
  })

  it('uses authoritative active snapshots for errors and final-provider facts', () => {
    const pollBlock = source
      .split('async function pollActiveRequests()')[1]
      ?.split('async function discoverActiveRequests()')[0]

    expect(pollBlock).toBeTruthy()
    expect(pollBlock).toContain('const shouldApply = !updateSnapshotIsOlder && newRank >= currentRank')
    expect(pollBlock).toContain('!updateSnapshotIsOlder && currentRank < 2 && updateHasFailureSignal')
    expect(pollBlock).toContain('record.error_message = mergeUsageRecordErrorMessage(')
    expect(pollBlock).toContain('{ authoritative: shouldApply }')
    // A terminal snapshot must be able to clear a status code left by an abandoned candidate.
    expect(pollBlock).toContain('record.status_code = update.status_code ?? undefined')
    // Records the active snapshot omits are left untouched instead of being concluded.
    expect(pollBlock).toContain('if (!record) continue')
    expect(pollBlock).toContain('record.target_model = typeof update.target_model')
    expect(pollBlock).toContain('record.reasoning_effort = typeof update.reasoning_effort')
    expect(pollBlock).toContain('record.service_tier = typeof update.service_tier')
    expect(pollBlock).not.toContain("if ('target_model' in update)")
    expect(pollBlock).not.toContain("if ('reasoning_effort' in update)")
    expect(pollBlock).toContain(
      "if (typeof update.requested_reasoning_effort === 'string' && update.requested_reasoning_effort.trim())",
    )
  })
})
