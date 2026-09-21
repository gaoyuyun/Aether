import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import { normalizeClaudeCodeCloakMode } from '@/api/endpoints/types/provider'

function readSource(path: string): string {
  return readFileSync(resolve(process.cwd(), path), 'utf8')
}

describe('ProviderFormDialog Claude Code cloak mode', () => {
  it('renders the selector only for claude_code and submits the mode', () => {
    const source = readSource('src/features/providers/components/ProviderFormDialog.vue')

    expect(source).toContain('客户端伪装模式')
    expect(source).toMatch(
      /<div(?=[^>]*v-if="form\.provider_type === 'claude_code'")(?=[^>]*data-testid="claude-code-cloak-mode-setting")[^>]*>/,
    )
    expect(source).toContain('id="claude-code-cloak-mode"')
    for (const mode of ['auto', 'always', 'off']) {
      expect(source).toContain(`<SelectItem value="${mode}">`)
    }
    expect(source).toContain('claude_code_cloak_mode: normalizeClaudeCodeCloakMode(props.provider.claude_code_cloak_mode)')
    expect(source).toContain("? { claude_code_cloak_mode: form.value.claude_code_cloak_mode }")
    // 切换到非 claude_code 类型时回到默认值，避免脏值随表单提交。
    expect(source).toContain("if (form.value.provider_type !== 'claude_code') {\n    form.value.claude_code_cloak_mode = DEFAULT_CLAUDE_CODE_CLOAK_MODE")
  })

  it('normalizes unknown cloak modes back to auto', () => {
    expect(normalizeClaudeCodeCloakMode('always')).toBe('always')
    expect(normalizeClaudeCodeCloakMode(' OFF ')).toBe('off')
    expect(normalizeClaudeCodeCloakMode('bogus')).toBe('auto')
    expect(normalizeClaudeCodeCloakMode(null)).toBe('auto')
  })
})
