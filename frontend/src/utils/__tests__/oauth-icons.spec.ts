import { describe, expect, it } from 'vitest'
import { getOAuthIcon, getOAuthIconUrl } from '../oauth-icons'

describe('OAuth icons', () => {
  it('uses code-owned SVGs for built-in providers', () => {
    expect(getOAuthIcon('github')).toContain('<svg')
    expect(getOAuthIconUrl('github', 'https://example.com/override.png')).toBeNull()
  })

  it('accepts only absolute HTTPS image URLs without credentials', () => {
    expect(getOAuthIconUrl('custom_oidc', 'https://cdn.example.com/icon.png'))
      .toBe('https://cdn.example.com/icon.png')
    expect(getOAuthIconUrl('custom_oidc', 'javascript:alert(1)')).toBeNull()
    expect(getOAuthIconUrl('custom_oidc', 'http://example.com/icon.png')).toBeNull()
    expect(getOAuthIconUrl('custom_oidc', 'https://user:pass@example.com/icon.png')).toBeNull()
    expect(getOAuthIconUrl('custom_oidc', '/relative/icon.png')).toBeNull()
  })
})
