export interface ActiveRequestDiscoverySnapshot {
  activeRequestIds: Iterable<string>
  knownRecordIds: Iterable<string>
  discoveredActiveRequestMissCounts: Iterable<readonly [string, number]>
}

export interface ActiveRequestDiscoveryResult {
  retainedDiscoveredActiveRequestMissCounts: Array<[string, number]>
  unseenActiveRequestIds: string[]
}

export const ACTIVE_REQUEST_DISCOVERY_MISS_LIMIT = 3

export function reconcileActiveRequestDiscovery(
  snapshot: ActiveRequestDiscoverySnapshot
): ActiveRequestDiscoveryResult {
  const knownRecordIds = new Set(snapshot.knownRecordIds)
  const activeRequestIds: string[] = []
  const activeRequestIdSet = new Set<string>()

  for (const id of snapshot.activeRequestIds) {
    if (!id || activeRequestIdSet.has(id)) continue
    activeRequestIdSet.add(id)
    activeRequestIds.push(id)
  }

  const retainedDiscoveredActiveRequestMissCounts: Array<[string, number]> = []
  const retainedDiscoveredSet = new Set<string>()

  for (const [id, previousMissCount] of snapshot.discoveredActiveRequestMissCounts) {
    if (!id || retainedDiscoveredSet.has(id)) continue
    if (knownRecordIds.has(id)) continue

    // A discovery snapshot can briefly omit an in-flight request because of paging or replica
    // lag. Keep it through a small grace window, but eventually release completed requests that
    // cannot enter the currently filtered/paged table so discovery polling can cool down again.
    const missCount = activeRequestIdSet.has(id)
      ? 0
      : Math.max(0, previousMissCount) + 1
    if (missCount >= ACTIVE_REQUEST_DISCOVERY_MISS_LIMIT) continue

    retainedDiscoveredSet.add(id)
    retainedDiscoveredActiveRequestMissCounts.push([id, missCount])
  }

  const unseenActiveRequestIds = activeRequestIds.filter(
    id => !knownRecordIds.has(id) && !retainedDiscoveredSet.has(id)
  )

  return {
    retainedDiscoveredActiveRequestMissCounts,
    unseenActiveRequestIds
  }
}
