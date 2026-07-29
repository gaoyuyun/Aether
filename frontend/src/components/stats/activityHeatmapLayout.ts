const IDEAL_CELL_SIZE = 6
const MIN_CELL_GAP = 1
const MAX_CELL_GAP = 4
const MIN_HEATMAP_BODY_HEIGHT = 160
export const ACTIVITY_HEATMAP_MONTH_HEADER_HEIGHT = 15
const MONTH_HEADER_MARGIN = 12
const MONTH_HEADER_BLOCK_HEIGHT = ACTIVITY_HEATMAP_MONTH_HEADER_HEIGHT + MONTH_HEADER_MARGIN
const DAY_ROW_COUNT = 7
export const ACTIVITY_HEATMAP_ROW_GAP = 4

export interface ActivityHeatmapLayout {
  cellSize: number
  cellGap: number
}

export function calculateActivityHeatmapLayout(
  availableWidth: number,
  columnCount: number,
): ActivityHeatmapLayout {
  if (availableWidth <= 0 || columnCount <= 0) {
    return {
      cellSize: IDEAL_CELL_SIZE,
      cellGap: MAX_CELL_GAP,
    }
  }

  if (columnCount === 1) {
    return {
      cellSize: availableWidth,
      cellGap: 0,
    }
  }

  const gapCount = columnCount - 1
  const gapForIdealCells = (availableWidth - columnCount * IDEAL_CELL_SIZE) / gapCount
  const cellGap = Math.min(MAX_CELL_GAP, Math.max(MIN_CELL_GAP, gapForIdealCells))
  const cellSize = Math.max((availableWidth - gapCount * cellGap) / columnCount, 0)

  return { cellSize, cellGap }
}

export function calculateActivityHeatmapBodyHeight(cellWidth: number): number {
  const dayGridHeight = DAY_ROW_COUNT * cellWidth
    + (DAY_ROW_COUNT - 1) * ACTIVITY_HEATMAP_ROW_GAP

  return Math.max(
    MIN_HEATMAP_BODY_HEIGHT,
    MONTH_HEADER_BLOCK_HEIGHT + dayGridHeight,
  )
}
