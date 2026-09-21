import { describe, expect, it } from 'vitest'

import {
  countZeroWidth,
  formatZeroWidthCopyNotice,
  markZeroWidthHtml,
  resolveSensitiveWordObfuscation,
  splitZeroWidthSegments,
  stripZeroWidth,
  ZERO_WIDTH_PLACEHOLDER,
} from '../zeroWidth'

const ZW = '\u200B'

describe('zero width helpers', () => {
  it('counts, strips and detects U+200B without touching other characters', () => {
    const text = `p${ZW}roxy a${ZW}pi 你${ZW}好🙂`
    expect(countZeroWidth(text)).toBe(3)
    expect(stripZeroWidth(text)).toBe('proxy api 你好🙂')
    expect(countZeroWidth('plain')).toBe(0)
    expect(stripZeroWidth('plain')).toBe('plain')
  })

  it('splits text into renderable segments with one marker per zero width character', () => {
    expect(splitZeroWidthSegments('plain')).toEqual([{ kind: 'text', text: 'plain' }])
    expect(splitZeroWidthSegments(`p${ZW}roxy${ZW}`)).toEqual([
      { kind: 'text', text: 'p' },
      { kind: 'zwsp' },
      { kind: 'text', text: 'roxy' },
      { kind: 'zwsp' },
    ])
  })

  it('marks escaped html with a visible placeholder carrying a tooltip', () => {
    const html = markZeroWidthHtml(`&quot;p${ZW}roxy&quot;`)
    expect(html).toContain(ZERO_WIDTH_PLACEHOLDER)
    expect(html).toContain('data-zwsp="1"')
    expect(html).toContain('title="')
    expect(html).not.toContain(ZW)
    expect(markZeroWidthHtml('clean')).toBe('clean')
  })

  it('formats the copy notice with the count', () => {
    expect(formatZeroWidthCopyNotice(3)).toContain('3 个零宽字符')
  })

  it('parses the obfuscation report from request metadata', () => {
    expect(resolveSensitiveWordObfuscation(null)).toBeNull()
    expect(resolveSensitiveWordObfuscation({})).toBeNull()
    expect(resolveSensitiveWordObfuscation({ sensitive_words_obfuscation: { applied: false, replaced: 0, fields: [] } })).toBeNull()
    expect(resolveSensitiveWordObfuscation({
      sensitive_words_obfuscation: { applied: true, replaced: '2', fields: ['system[0]', 7, ' '] },
    })).toEqual({ applied: true, replaced: 2, fields: ['system[0]'] })
  })
})
