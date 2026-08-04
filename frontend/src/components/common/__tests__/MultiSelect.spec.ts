import { afterEach, describe, expect, it } from 'vitest'
import { createApp, nextTick, type App } from 'vue'

import MultiSelect from '../MultiSelect.vue'

describe('MultiSelect', () => {
  let app: App<Element> | null = null

  afterEach(() => {
    app?.unmount()
    app = null
    document.body.innerHTML = ''
  })

  it('opens upward and keeps its options scrollable near the viewport bottom', async () => {
    Object.defineProperty(window, 'innerHeight', { configurable: true, value: 640 })
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 900 })

    const root = document.createElement('div')
    document.body.appendChild(root)
    app = createApp(MultiSelect, {
      modelValue: [],
      options: Array.from({ length: 20 }, (_, index) => ({
        value: `option-${index}`,
        label: `Option ${index}`,
      })),
      teleport: true,
    })
    app.mount(root)

    const trigger = root.querySelector('button') as HTMLButtonElement
    trigger.getBoundingClientRect = () => ({
      bottom: 560,
      height: 40,
      left: 120,
      right: 360,
      top: 520,
      width: 240,
      x: 120,
      y: 520,
      toJSON: () => ({}),
    })

    trigger.click()
    await nextTick()
    await nextTick()

    const dropdown = Array.from(document.body.querySelectorAll<HTMLElement>('div'))
      .find(element => element.style.bottom === '124px')

    expect(dropdown).toBeDefined()
    expect(dropdown?.style.maxHeight).toBe('304px')
    expect(dropdown?.querySelector('.overflow-y-auto')).not.toBeNull()
  })
})
