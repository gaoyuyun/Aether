import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const source = readFileSync(
  resolve(process.cwd(), 'src/features/providers/components/ProviderDetailDrawer.vue'),
  'utf8',
)

describe('ProviderDetailDrawer Claude Code rate-limit windows', () => {
  it('renders the 5h / 7d windows block for claude_code keys with snapshot windows', () => {
    expect(source).toContain('data-testid="claude-code-quota-windows"')
    expect(source).toMatch(
      /v-if="provider\.provider_type === 'claude_code' && hasClaudeCodeQuotaDisplayData\(key\)"/,
    )
    expect(source).toContain('v-for="window in getClaudeCodeQuotaWindows(key)"')
    // 复用通用进度行，窗口顺序固定 5h → 7d → 7d_oi。
    expect(source).toContain("const order = ['5h', '7d', '7d_oi']")
    expect(source).toContain("case '5h': return '5 小时窗口'")
    expect(source).toContain("case '7d': return '7 天窗口'")
    // 只接受 claude_code 的快照（provider_type 缺省时兼容旧快照）。
    expect(source).toContain("if (providerType && providerType !== 'claude_code') return []")
  })

  it('exposes the passive refresh entry with an explanatory title', () => {
    const block = source.split('data-testid="claude-code-quota-windows"')[1]?.split('<!-- Antigravity')[0]
    expect(block).toBeTruthy()
    expect(block).toContain('refreshable')
    expect(block).toContain('@refresh="handleManualQuotaRefresh(key)"')
    expect(block).toContain('不发上游请求')
  })
})
