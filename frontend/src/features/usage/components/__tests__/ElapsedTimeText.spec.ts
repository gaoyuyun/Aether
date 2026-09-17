import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, h, nextTick, reactive, type App } from 'vue'
import ElapsedTimeText from '../ElapsedTimeText.vue'

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

function mountElapsedTimeText(props: Record<string, unknown>) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp({
    render: () => h(ElapsedTimeText, props),
  })
  app.mount(root)
  mountedApps.push({ app, root })
  return root
}

function mountReactiveElapsedTimeText(initialProps: Record<string, unknown>) {
  const props = reactive(initialProps)
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp({
    render: () => h(ElapsedTimeText, { ...props }),
  })
  app.mount(root)
  mountedApps.push({ app, root })
  return { props, root }
}

afterEach(() => {
  vi.useRealTimers()
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

describe('ElapsedTimeText', () => {
  it('uses active response timing from response_time_updated_at instead of stale created_at', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-06-08T12:00:10.000Z'))

    const root = mountElapsedTimeText({
      status: 'streaming',
      createdAt: '2026-06-08T11:59:00Z',
      responseTimeUpdatedAt: '2026-06-08T12:00:06Z',
      responseTimeMs: 1500,
    })
    await nextTick()

    expect(root.textContent).toBe('5.50s')
  })

  it('falls back to created_at when active timing has not reached the backend yet', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-06-08T12:00:10.000Z'))

    const root = mountElapsedTimeText({
      status: 'pending',
      createdAt: '2026-06-08T12:00:06Z',
      responseTimeUpdatedAt: null,
      responseTimeMs: null,
    })
    await nextTick()

    expect(root.textContent).toBe('4.00s')
  })

  it('anchors the live total on the request accepted clock and ignores candidate-level snapshots', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-09-17T12:00:30.000Z'))

    // created_at is truncated to the second and the response clock belongs to a candidate that
    // started 25s into the request (after an earlier attempt failed); neither may drive the total.
    const { props, root } = mountReactiveElapsedTimeText({
      status: 'streaming',
      createdAt: '2026-09-17T12:00:00Z',
      responseTimeUpdatedAt: '2026-09-17T12:00:27Z',
      responseTimeMs: 1500,
      requestAcceptedAtUnixMs: Date.parse('2026-09-17T12:00:00.250Z'),
    })
    await nextTick()
    expect(root.textContent).toBe('29.75s')

    // A later candidate snapshot changes nothing: the accepted clock keeps counting.
    props.responseTimeUpdatedAt = '2026-09-17T12:00:29Z'
    props.responseTimeMs = 800
    await nextTick()
    expect(root.textContent).toBe('29.75s')

    vi.advanceTimersByTime(500)
    await nextTick()
    const advanced = Number.parseFloat(root.textContent ?? '')
    expect(advanced).toBeGreaterThanOrEqual(30.24)
    expect(advanced).toBeLessThanOrEqual(30.25)
  })

  it('falls back to the legacy anchors when the accepted clock is unknown', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-09-17T12:00:10.000Z'))

    const root = mountElapsedTimeText({
      status: 'pending',
      createdAt: '2026-09-17T12:00:06Z',
      responseTimeUpdatedAt: null,
      responseTimeMs: null,
      requestAcceptedAtUnixMs: null,
    })
    await nextTick()

    expect(root.textContent).toBe('4.00s')
  })

  it('does not pause or move total time backwards when the first-byte clock arrives', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-07-17T12:00:06.250Z'))

    const { props, root } = mountReactiveElapsedTimeText({
      status: 'pending',
      createdAt: '2026-07-17T12:00:00Z',
      responseTimeUpdatedAt: null,
      responseTimeMs: null,
    })
    await nextTick()
    expect(root.textContent).toBe('6.25s')

    // The first-byte snapshot implies 5.85s at the same instant because its
    // timestamp is truncated to seconds. The visible clock must stay continuous.
    props.status = 'streaming'
    props.responseTimeUpdatedAt = '2026-07-17T12:00:06Z'
    props.responseTimeMs = 5600
    await nextTick()
    expect(root.textContent).toBe('6.25s')

    vi.advanceTimersByTime(500)
    await nextTick()
    expect(Number.parseFloat(root.textContent ?? '')).toBeGreaterThanOrEqual(6.74)
  })
})
