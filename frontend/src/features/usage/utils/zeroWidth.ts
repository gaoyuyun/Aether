/**
 * 零宽字符（U+200B）工具。
 *
 * 后端的敏感词混淆会在匹配词的第一个字符后插入 U+200B，肉眼看不见。
 * 管理端展示时必须把它变成可见占位符，复制时要告知数量并提供“去除后复制”。
 */

export const ZERO_WIDTH_SPACE = '\u200B'
export const ZERO_WIDTH_PLACEHOLDER = '⟨ZWSP⟩'
export const ZERO_WIDTH_PLACEHOLDER_TITLE = '零宽空格 U+200B（敏感词混淆插入，实际请求体中存在）'

const ZERO_WIDTH_GLOBAL = /\u200B/g

export function countZeroWidth(text: string): number {
  if (!text) return 0
  let count = 0
  for (let index = 0; index < text.length; index += 1) {
    if (text.charCodeAt(index) === 0x200b) count += 1
  }
  return count
}

export function stripZeroWidth(text: string): string {
  return text.replace(ZERO_WIDTH_GLOBAL, '')
}

export function hasZeroWidth(text: string): boolean {
  return text.includes(ZERO_WIDTH_SPACE)
}

export type ZeroWidthSegment =
  | { kind: 'text', text: string }
  | { kind: 'zwsp' }

/** 把文本按 U+200B 切成可渲染的片段；没有零宽字符时只有一个 text 片段。 */
export function splitZeroWidthSegments(text: string): ZeroWidthSegment[] {
  if (!hasZeroWidth(text)) return [{ kind: 'text', text }]
  const segments: ZeroWidthSegment[] = []
  let start = 0
  for (let index = 0; index < text.length; index += 1) {
    if (text.charCodeAt(index) !== 0x200b) continue
    if (index > start) segments.push({ kind: 'text', text: text.slice(start, index) })
    segments.push({ kind: 'zwsp' })
    start = index + 1
  }
  if (start < text.length) segments.push({ kind: 'text', text: text.slice(start) })
  return segments
}

/** 已转义的 HTML 文本中把 U+200B 替换成可见占位符（带 tooltip）。 */
export function markZeroWidthHtml(escapedHtml: string): string {
  if (!hasZeroWidth(escapedHtml)) return escapedHtml
  return escapedHtml.replace(
    ZERO_WIDTH_GLOBAL,
    `<span class="zwsp-marker" title="${ZERO_WIDTH_PLACEHOLDER_TITLE}" data-zwsp="1">${ZERO_WIDTH_PLACEHOLDER}</span>`,
  )
}

export function formatZeroWidthCopyNotice(count: number): string {
  return `内容含 ${count} 个零宽字符（敏感词混淆），已按原始字节复制；可改用“去除零宽字符后复制”`
}

export interface SensitiveWordObfuscationReport {
  applied: boolean
  replaced: number
  fields: string[]
}

/** 从请求详情的 metadata（report_context 落库投影）解析混淆报告。 */
export function resolveSensitiveWordObfuscation(
  metadata: Record<string, unknown> | null | undefined,
): SensitiveWordObfuscationReport | null {
  const raw = metadata?.sensitive_words_obfuscation
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null
  const record = raw as Record<string, unknown>
  const applied = record.applied === true
  const replacedRaw = Number(record.replaced)
  const replaced = Number.isFinite(replacedRaw) && replacedRaw >= 0 ? Math.floor(replacedRaw) : 0
  const fields = Array.isArray(record.fields)
    ? record.fields.filter((item): item is string => typeof item === 'string' && item.trim().length > 0)
    : []
  if (!applied && replaced === 0 && fields.length === 0) return null
  return { applied, replaced, fields }
}
