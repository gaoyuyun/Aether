import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

function readSource(path: string): string {
  return readFileSync(resolve(process.cwd(), path), 'utf8')
}

describe('ProviderFormDialog cooldown policy group', () => {
  it('renders the three cooldown controls for every provider type and submits them', () => {
    const source = readSource('src/features/providers/components/ProviderFormDialog.vue')

    expect(source).toContain('冷却策略')
    expect(source).toMatch(
      /<div(?=[^>]*data-testid="cooldown-policy-setting")(?![^>]*\bv-if=)[^>]*>[\s\S]{0,400}冷却策略/,
    )
    expect(source).toContain('id="cooldown-disable"')
    expect(source).toContain('id="cooldown-transient-error-seconds"')
    expect(source).toContain('id="cooldown-model-level"')
    expect(source).toContain('cooldown: normalizeProviderCooldownConfig(props.provider.cooldown)')
    expect(source).toContain('disable: form.value.cooldown.disable')
    expect(source).toContain('transient_error_seconds: form.value.cooldown.transient_error_seconds')
    expect(source).toContain('model_level: form.value.cooldown.model_level')
  })
})
