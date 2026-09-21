import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, shallowRef, type App } from 'vue'

import JsonContent from '../RequestDetailDrawer/JsonContent.vue'
import BlockRenderer from '../RequestDetailDrawer/BlockRenderer.vue'
import SensitiveWordObfuscationFacts from '../SensitiveWordObfuscationFacts.vue'
import { BodyDocumentEngine } from '../../utils/body-document-engine'
import type { BodyDocument } from '../../utils/body-document'
import type { BodyJsonOptions } from '../../utils/body-document-protocol'
import { ZERO_WIDTH_PLACEHOLDER } from '../../utils/zeroWidth'
import { JSON_TEXT_CHUNK_SIZE } from '../../utils/json-viewer'

const ZW = '\u200B'
const apps: Array<{ app: App, root: HTMLElement }> = []
afterEach(() => { for (const { app, root } of apps.splice(0)) { app.unmount(); root.remove() } })

function mount(render: () => ReturnType<typeof h>) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(defineComponent({ setup: () => render }))
  app.mount(root)
  apps.push({ app, root })
  return root
}

describe('zero width characters are visible in body views (F3)', () => {
  it('renders U+200B as a placeholder inside JSON string tokens', async () => {
    const root = mount(() => h(JsonContent, {
      data: { system: `You are a p${ZW}roxy for the A${ZW}PI.` }, expandDepth: 999, isDark: false, viewMode: 'formatted', emptyMessage: '无数据',
    }))
    await vi.waitFor(() => expect(root.querySelectorAll('.zwsp-marker')).toHaveLength(2))
    const marker = root.querySelector<HTMLElement>('.zwsp-marker')!
    expect(marker.textContent).toBe(ZERO_WIDTH_PLACEHOLDER)
    expect(marker.title).toContain('U+200B')
    expect(root.querySelector('.line-content')?.textContent).not.toContain(ZW)
  })

  it('renders U+200B in raw text chunks and keeps each marker whole across worker chunk boundaries', async () => {
    // 把零宽字符放在 Worker 字符串分段的边界前后，断言每个标记完整渲染、数量不变。
    const filler = 'x'.repeat(JSON_TEXT_CHUNK_SIZE - 1)
    const text = `${filler}${ZW}${filler}${ZW}tail`
    const engine = new BodyDocumentEngine({ text })
    const bodyDocument = shallowRef({ json: vi.fn(async (options: BodyJsonOptions) => engine.json(options)) } as unknown as BodyDocument)
    const root = mount(() => h(JsonContent, {
      data: null, bodyDocument: bodyDocument.value, expandDepth: 999, isDark: false, viewMode: 'formatted', emptyMessage: '无数据',
    }))
    await vi.waitFor(() => expect(root.querySelectorAll('.zwsp-marker')).toHaveLength(2))
    for (const marker of root.querySelectorAll<HTMLElement>('.zwsp-marker')) {
      expect(marker.textContent).toBe(ZERO_WIDTH_PLACEHOLDER)
    }
    expect(root.textContent).not.toContain(ZW)
    expect(root.textContent).toContain('tail')

    const rawRoot = mount(() => h(JsonContent, {
      data: `raw p${ZW}roxy text`, expandDepth: 999, isDark: false, viewMode: 'raw', emptyMessage: '无数据',
    }))
    await vi.waitFor(() => expect(rawRoot.querySelectorAll('.zwsp-marker')).toHaveLength(1))
    expect(rawRoot.textContent).toContain(`raw p${ZERO_WIDTH_PLACEHOLDER}roxy text`)
  })

  it('renders U+200B as a placeholder in conversation blocks', () => {
    const root = mount(() => h(BlockRenderer, { blocks: [
      { type: 'message', role: 'system', content: [{ type: 'text', content: `You are a p${ZW}roxy.` }] },
      { type: 'tool_use', toolName: 'search', input: `{"q":"p${ZW}roxy"}` },
      { type: 'label', label: 'model', value: `cl${ZW}aude` },
    ] }))
    const markers = root.querySelectorAll<HTMLElement>('.zwsp-marker')
    expect(markers).toHaveLength(3)
    expect(markers[0].title).toContain('U+200B')
    expect(root.textContent).not.toContain(ZW)
    expect(root.textContent).toContain(`p${ZERO_WIDTH_PLACEHOLDER}roxy.`)
  })
})

describe('sensitive word obfuscation facts (F4)', () => {
  it('shows the count badge and lists field paths on click', async () => {
    const root = mount(() => h(SensitiveWordObfuscationFacts, {
      report: { applied: true, replaced: 3, fields: ['system[1]', 'messages[0].content[0]'] },
    }))
    const badge = root.querySelector<HTMLButtonElement>('[data-testid="sensitive-words-obfuscation-badge"]')!
    expect(badge.textContent).toContain('已应用敏感词混淆 3 处')
    expect(root.querySelector('[data-testid="sensitive-words-obfuscation-fields"]')).toBeNull()
    badge.click()
    await vi.waitFor(() => expect(root.querySelector('[data-testid="sensitive-words-obfuscation-fields"]')).not.toBeNull())
    const items = [...root.querySelectorAll('[data-testid="sensitive-words-obfuscation-fields"] li')].map(node => node.textContent?.trim())
    expect(items).toEqual(['system[1]', 'messages[0].content[0]'])
  })
})

describe('copy keeps raw bytes unless stripping is requested (F5, engine)', () => {
  it('copies zero width characters by default and strips them on request', () => {
    const value = { system: `p${ZW}roxy`, messages: [{ role: 'user', content: `use the A${ZW}PI` }] }
    const engine = new BodyDocumentEngine(value)
    expect(engine.copy()).toContain(ZW)
    expect(JSON.parse(engine.copy())).toEqual(value)
    const stripped = engine.copy(undefined, { stripZeroWidth: true })
    expect(stripped).not.toContain(ZW)
    expect(JSON.parse(stripped)).toEqual({ system: 'proxy', messages: [{ role: 'user', content: 'use the API' }] })
    const conversation = engine.copy({ kind: 'request', apiFormat: 'claude:messages' })
    expect(conversation).toContain(ZW)
    expect(engine.copy({ kind: 'request', apiFormat: 'claude:messages' }, { stripZeroWidth: true })).not.toContain(ZW)
  })
})
