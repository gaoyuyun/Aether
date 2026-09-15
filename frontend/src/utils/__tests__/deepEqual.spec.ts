import { describe, expect, it } from 'vitest'
import { reactive } from 'vue'

import { deepEqual } from '@/utils/deepEqual'

describe('deepEqual', () => {
  it('compares primitives with Object.is semantics', () => {
    expect(deepEqual(1, 1)).toBe(true)
    expect(deepEqual('a', 'a')).toBe(true)
    expect(deepEqual(NaN, NaN)).toBe(true)
    expect(deepEqual(null, null)).toBe(true)
    expect(deepEqual(undefined, null)).toBe(false)
    expect(deepEqual(1, '1')).toBe(false)
    expect(deepEqual(0, false)).toBe(false)
  })

  it('treats objects as equal regardless of key order and compares nested values', () => {
    const left = { a: 1, b: { c: [1, { d: 'x' }], e: null } }
    const right = { b: { e: null, c: [1, { d: 'x' }] }, a: 1 }
    expect(deepEqual(left, right)).toBe(true)
    expect(deepEqual(left, { ...right, b: { ...right.b, c: [1, { d: 'y' }] } })).toBe(false)
  })

  it('rejects missing or extra keys, type mismatches and reordered arrays', () => {
    expect(deepEqual({ a: 1 }, { a: 1, b: undefined })).toBe(false)
    expect(deepEqual({ a: 1, b: 2 }, { a: 1 })).toBe(false)
    expect(deepEqual({}, [])).toBe(false)
    expect(deepEqual([], {})).toBe(false)
    expect(deepEqual(null, {})).toBe(false)
    expect(deepEqual([1, 2], [2, 1])).toBe(false)
    expect(deepEqual([1, 2], [1, 2, 3])).toBe(false)
  })

  it('only considers non-plain objects equal by reference', () => {
    const date = new Date(0)
    expect(deepEqual(date, date)).toBe(true)
    expect(deepEqual(new Date(0), new Date(0))).toBe(false)
    expect(deepEqual({ value: new Map() }, { value: new Map() })).toBe(false)
    expect(deepEqual(Object.create(null), {})).toBe(true)
  })

  it('sees through Vue reactive proxies', () => {
    const raw = { candidates: [{ extra_data: { image_progress: { phase: 'streaming', count: 3 } } }] }
    const proxy = reactive({ candidates: [{ extra_data: { image_progress: { phase: 'streaming', count: 3 } } }] })
    expect(deepEqual(proxy, raw)).toBe(true)
    expect(deepEqual(proxy.candidates[0], { extra_data: { image_progress: { phase: 'streaming', count: 4 } } })).toBe(false)
  })
})
