import { describe, expect, it } from 'vitest'

import { reconcileActiveRequestDiscovery } from '../activeRequestDiscovery'

describe('reconcileActiveRequestDiscovery', () => {
  it('returns unseen active request ids while retaining still-pending discoveries', () => {
    const result = reconcileActiveRequestDiscovery({
      activeRequestIds: ['req-new', 'req-retained', 'req-new'],
      knownRecordIds: ['req-known'],
      discoveredActiveRequestMissCounts: [
        ['req-retained', 1],
        ['req-briefly-omitted', 0]
      ]
    })

    expect(result).toEqual({
      retainedDiscoveredActiveRequestMissCounts: [
        ['req-retained', 0],
        ['req-briefly-omitted', 1]
      ],
      unseenActiveRequestIds: ['req-new']
    })
  })

  it('drops discovered ids once they are known in the table', () => {
    const result = reconcileActiveRequestDiscovery({
      activeRequestIds: ['req-known', 'req-fresh'],
      knownRecordIds: ['req-known'],
      discoveredActiveRequestMissCounts: [['req-known', 0]]
    })

    expect(result).toEqual({
      retainedDiscoveredActiveRequestMissCounts: [],
      unseenActiveRequestIds: ['req-fresh']
    })
  })

  it('does not conclude anything about a record the active snapshot briefly omits', () => {
    // A request missing from one active snapshot (paging, a lagging replica, a filtered window)
    // must stay discovered so polling remains hot until the table can load its durable row.
    const result = reconcileActiveRequestDiscovery({
      activeRequestIds: [],
      knownRecordIds: [],
      discoveredActiveRequestMissCounts: [['req-briefly-omitted', 0]]
    })

    expect(result).toEqual({
      retainedDiscoveredActiveRequestMissCounts: [['req-briefly-omitted', 1]],
      unseenActiveRequestIds: []
    })
  })

  it('drops a discovery after three consecutive active snapshot misses', () => {
    let discoveredActiveRequestMissCounts: Array<[string, number]> = [['req-completed', 0]]

    for (let miss = 1; miss <= 3; miss += 1) {
      const result = reconcileActiveRequestDiscovery({
        activeRequestIds: [],
        knownRecordIds: [],
        discoveredActiveRequestMissCounts
      })
      discoveredActiveRequestMissCounts = result.retainedDiscoveredActiveRequestMissCounts

      expect(discoveredActiveRequestMissCounts).toEqual(
        miss < 3 ? [['req-completed', miss]] : []
      )
    }
  })

  it('resets the miss count when a discovered request reappears', () => {
    const result = reconcileActiveRequestDiscovery({
      activeRequestIds: ['req-lagged'],
      knownRecordIds: [],
      discoveredActiveRequestMissCounts: [['req-lagged', 2]]
    })

    expect(result).toEqual({
      retainedDiscoveredActiveRequestMissCounts: [['req-lagged', 0]],
      unseenActiveRequestIds: []
    })
  })

  it('returns no unseen ids when every active request is already known or retained', () => {
    const result = reconcileActiveRequestDiscovery({
      activeRequestIds: ['req-known', 'req-retained'],
      knownRecordIds: ['req-known'],
      discoveredActiveRequestMissCounts: [['req-retained', 0]]
    })

    expect(result).toEqual({
      retainedDiscoveredActiveRequestMissCounts: [['req-retained', 0]],
      unseenActiveRequestIds: []
    })
  })
})
