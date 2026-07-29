import { describe, expect, it } from 'vitest'
import {
  calculateActivityHeatmapBodyHeight,
  calculateActivityHeatmapLayout,
} from '../activityHeatmapLayout'

describe('calculateActivityHeatmapLayout', () => {
  it('reduces the gap to keep a full year visible in a narrow card', () => {
    const columnCount = 53
    const availableWidth = 450
    const layout = calculateActivityHeatmapLayout(availableWidth, columnCount)

    expect(layout.cellSize).toBeCloseTo(6)
    expect(layout.cellGap).toBeLessThan(4)
    expect(
      columnCount * layout.cellSize + (columnCount - 1) * layout.cellGap,
    ).toBeCloseTo(availableWidth)
  })

  it('shrinks cells only after the compact gap can no longer preserve their ideal size', () => {
    const columnCount = 53
    const availableWidth = 320
    const layout = calculateActivityHeatmapLayout(availableWidth, columnCount)

    expect(layout.cellGap).toBe(1)
    expect(layout.cellSize).toBeLessThan(6)
    expect(
      columnCount * layout.cellSize + (columnCount - 1) * layout.cellGap,
    ).toBeCloseTo(availableWidth)
  })

  it('keeps the normal desktop gap when enough width is available', () => {
    const layout = calculateActivityHeatmapLayout(600, 53)

    expect(layout.cellGap).toBe(4)
  })

  it('fills the chart height on narrow cards and preserves square cells on wide cards', () => {
    expect(calculateActivityHeatmapBodyHeight(6)).toBe(160)
    expect(calculateActivityHeatmapBodyHeight(18)).toBe(177)
  })
})
