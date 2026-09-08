import { describe, expect, it } from 'vitest'

import { safePaymentTargetUrl } from '../paymentUrl'

describe('safePaymentTargetUrl', () => {
  it('allows absolute HTTPS targets', () => {
    expect(safePaymentTargetUrl('https://pay.example/submit?order=1')).toBe(
      'https://pay.example/submit?order=1',
    )
    expect(safePaymentTargetUrl(' HTTPS://pay.example/submit ')).toBe(
      'https://pay.example/submit',
    )
  })

  it.each([
    'javascript:alert(1)',
    'data:text/html,attack',
    'http://pay.example/submit',
    '//pay.example/submit',
    'https://user:secret@pay.example/submit',
    'https://pay.example/submit#fragment',
    '/\\pay.example/submit',
    '/payment/continue?order=1',
    '../payment/continue?order=1',
    'submit.php',
  ])('rejects an unsafe payment target: %s', (value) => {
    expect(safePaymentTargetUrl(value)).toBeNull()
  })
})
