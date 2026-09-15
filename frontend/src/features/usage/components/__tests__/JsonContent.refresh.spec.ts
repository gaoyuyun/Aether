import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, ref, shallowRef, type App } from 'vue'
import JsonContent from '../RequestDetailDrawer/JsonContent.vue'

const mountedApps: Array<{ app: App, root: HTMLElement }> = []
afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

function buildValues(version: string) {
  return Array.from({ length: 600 }, (_value, index) => `value-${index}-${version}`)
}

function mountJson(initialData: unknown) {
  const data = shallowRef(initialData)
  const expandDepth = ref(999)
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(defineComponent({
    setup: () => () => h(JsonContent, { data: data.value, expandDepth: expandDepth.value, isDark: false, viewMode: 'formatted', emptyMessage: '无数据' }),
  }))
  app.mount(root)
  mountedApps.push({ app, root })
  return { root, data, expandDepth }
}

function viewport(root: HTMLElement) {
  return root.querySelector<HTMLElement>('.virtual-body-scroll')!
}

async function scrollTo(root: HTMLElement, top: number, chunkIndex: number) {
  const element = viewport(root)
  element.scrollTop = top
  element.dispatchEvent(new Event('scroll'))
  await vi.waitFor(() => expect(root.querySelector(`[data-body-chunk="${chunkIndex}"] .json-line`)).not.toBeNull())
  return element
}

function foldButton(root: HTMLElement, key: string) {
  return [...root.querySelectorAll('.json-line')]
    .find(line => line.querySelector('.token-key')?.textContent === JSON.stringify(key))!
    .querySelector<HTMLButtonElement>('button')!
}

describe('JsonContent refresh behaviour under polling', () => {
  it('ignores a new object with identical content and keeps the viewer, scroll position and fold state', async () => {
    const build = () => ({ first: { a: 1, b: 2 }, headers: buildValues('a') })
    const { root, data } = mountJson(build())
    await vi.waitFor(() => expect(root.querySelector('.json-line')).not.toBeNull())
    foldButton(root, 'first').click()
    await vi.waitFor(() => expect(foldButton(root, 'first').getAttribute('aria-expanded')).toBe('false'))
    const element = await scrollTo(root, 3000, 3)
    const renderedBefore = root.querySelector('[data-body-chunk="3"] .json-line')!

    data.value = build()
    await nextTick()
    await nextTick()

    expect(viewport(root)).toBe(element)
    expect(element.scrollTop).toBe(3000)
    expect(root.querySelector('[data-body-chunk="3"] .json-line')).toBe(renderedBefore)

    await scrollTo(root, 0, 0)
    await vi.waitFor(() => expect(foldButton(root, 'first').getAttribute('aria-expanded')).toBe('false'))
    expect(root.textContent).not.toContain('"a": 1')
  })

  it('reloads changed content in place without losing the scroll position', async () => {
    const { root, data } = mountJson({ headers: buildValues('a') })
    await vi.waitFor(() => expect(root.querySelector('.json-line')).not.toBeNull())
    const element = await scrollTo(root, 3000, 3)
    expect(root.textContent).toContain('value-150-a')

    data.value = { headers: buildValues('b') }
    await vi.waitFor(() => expect(root.textContent).toContain('value-150-b'))

    expect(viewport(root)).toBe(element)
    expect(element.scrollTop).toBe(3000)
    expect(root.textContent).not.toContain('value-150-a')
  })

  it('still rebuilds from the top when the expand depth changes', async () => {
    const { root, expandDepth } = mountJson({ headers: buildValues('a') })
    await vi.waitFor(() => expect(root.querySelector('.json-line')).not.toBeNull())
    const element = await scrollTo(root, 3000, 3)

    expandDepth.value = 0
    await vi.waitFor(() => expect(viewport(root)).not.toBe(element))
    await vi.waitFor(() => expect(root.querySelectorAll('.json-line')).toHaveLength(3))
    expect(viewport(root).scrollTop).toBe(0)
  })
})
