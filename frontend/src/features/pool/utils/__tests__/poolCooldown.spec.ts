import { describe, expect, it } from 'vitest'

import {
  buildPoolCooldownPresentation,
  cooldownRemainingSeconds,
  formatCooldownDeadline,
  formatCooldownDuration,
  formatCooldownReason,
  formatCooldownSource,
  poolCooldownPresentationEquals,
  resolveCooldownUntil,
} from '@/features/pool/utils/poolCooldown'

const NOW_MS = Date.UTC(2026, 8, 20, 4, 0, 0)
const NOW_SECS = Math.floor(NOW_MS / 1000)

describe('formatCooldownReason', () => {
  it('maps the fixed reason codes written by the backend', () => {
    expect(formatCooldownReason('rate_limited_429')).toBe('429 限流')
    expect(formatCooldownReason('quota_exhausted_429')).toBe('429 额度耗尽')
    expect(formatCooldownReason('not_found_404')).toBe('404 不存在')
    expect(formatCooldownReason('retry_after_header')).toBe('上游 Retry-After')
    expect(formatCooldownReason('ratelimit_window_5h')).toBe('5 小时窗口耗尽')
    expect(formatCooldownReason('ratelimit_window_7d')).toBe('7 天窗口耗尽')
    expect(formatCooldownReason('google_retry_info')).toBe('Google 重试提示')
    expect(formatCooldownReason('codex_resets_at')).toBe('Codex 额度重置')
    expect(formatCooldownReason('transient_upstream')).toBe('上游瞬时错误')
  })

  it('expands templated reasons', () => {
    expect(formatCooldownReason('backoff_level_0')).toBe('429 退避（第 1 级）')
    expect(formatCooldownReason('backoff_level_3')).toBe('429 退避（第 4 级）')
    expect(formatCooldownReason('transient_upstream_522')).toBe('522 上游瞬时错误')
    expect(formatCooldownReason('rule:review required')).toBe('规则「review required」')
    expect(formatCooldownReason('stream_timeout_x3')).toBe('流超时 ×3')
  })

  it('returns unknown codes unchanged and empty for blank input', () => {
    expect(formatCooldownReason('something_new')).toBe('something_new')
    expect(formatCooldownReason(null)).toBe('')
    expect(formatCooldownReason('  ')).toBe('')
  })

  it('describes hint sources', () => {
    expect(formatCooldownSource('retry_after_header')).toBe('按上游 Retry-After 冷却')
    expect(formatCooldownSource('none')).toBe('上游未给出重试提示，按固定策略冷却')
    expect(formatCooldownSource('custom')).toBe('custom')
  })
})

describe('cooldown deadlines', () => {
  it('formats durations with hours, minutes and seconds', () => {
    expect(formatCooldownDuration(0)).toBe('0s')
    expect(formatCooldownDuration(9)).toBe('9s')
    expect(formatCooldownDuration(125)).toBe('2m 05s')
    expect(formatCooldownDuration(3723)).toBe('1h 02m 03s')
  })

  it('prefers the absolute deadline and only derives from ttl when needed', () => {
    expect(resolveCooldownUntil({ until: NOW_SECS + 120, ttl_seconds: 5 })).toBe(NOW_SECS + 120)
    expect(resolveCooldownUntil({ ttl_seconds: 90, observedAtMs: NOW_MS })).toBe(NOW_SECS + 90)
    expect(resolveCooldownUntil({ until: null, ttl_seconds: null })).toBeNull()
    expect(resolveCooldownUntil({ ttl_seconds: 0 })).toBeNull()
  })

  it('counts down against the local clock and never goes negative', () => {
    expect(cooldownRemainingSeconds(NOW_SECS + 45, NOW_MS)).toBe(45)
    expect(cooldownRemainingSeconds(NOW_SECS - 5, NOW_MS)).toBe(0)
    expect(cooldownRemainingSeconds(null, NOW_MS)).toBe(0)
  })

  it('renders the deadline as local time and adds the date when it is not today', () => {
    const sameDay = formatCooldownDeadline(NOW_SECS + 60, NOW_MS)
    expect(sameDay).toMatch(/^\d{2}:\d{2}:\d{2}$/)
    const nextDay = formatCooldownDeadline(NOW_SECS + 36 * 3600, NOW_MS)
    expect(nextDay).toMatch(/^\d{2}-\d{2} \d{2}:\d{2}$/)
    expect(formatCooldownDeadline(null, NOW_MS)).toBe('')
  })
})

describe('buildPoolCooldownPresentation', () => {
  it('returns null without a reason', () => {
    expect(buildPoolCooldownPresentation({ reason: null, until: NOW_SECS + 10, nowMs: NOW_MS })).toBeNull()
  })

  it('combines reason, source, backoff level and deadline into the title', () => {
    const presentation = buildPoolCooldownPresentation({
      reason: 'backoff_level_1',
      until: NOW_SECS + 60,
      meta: {
        source: 'none',
        backoff_level: 1,
        scope: 'key',
      },
      nowMs: NOW_MS,
    })
    expect(presentation).not.toBeNull()
    expect(presentation?.reasonLabel).toBe('429 退避（第 2 级）')
    expect(presentation?.countdownLabel).toBe('1m 00s')
    expect(presentation?.remainingSeconds).toBe(60)
    expect(presentation?.expired).toBe(false)
    expect(presentation?.title).toContain('来源：上游未给出重试提示，按固定策略冷却')
    expect(presentation?.title).toContain('退避等级：1')
    expect(presentation?.title).toContain('截止：')
  })

  it('surfaces the upstream reset time and model scope for hint-driven cooldowns', () => {
    const presentation = buildPoolCooldownPresentation({
      reason: 'quota_exhausted_429',
      until: NOW_SECS + 3 * 3600,
      meta: {
        source: 'ratelimit_window_5h',
        retry_after_secs: 3 * 3600,
        reset_at: NOW_SECS + 3 * 3600,
        scope: 'key_model',
        model: 'claude-sonnet-4-5',
        rejected_windows: ['5h', 'unified'],
      },
      nowMs: NOW_MS,
    })
    expect(presentation?.reasonLabel).toBe('429 额度耗尽')
    expect(presentation?.title).toContain('来源：按 Anthropic 5 小时窗口重置时刻冷却')
    expect(presentation?.title).toContain('上游要求等待：3h 00m 00s')
    expect(presentation?.title).toContain('作用域：仅模型 claude-sonnet-4-5')
    expect(presentation?.title).toContain('拒绝窗口：5h, unified')
  })

  it('marks expired cooldowns and compares presentations by content', () => {
    const expired = buildPoolCooldownPresentation({
      reason: 'rate_limited_429',
      until: NOW_SECS - 1,
      nowMs: NOW_MS,
    })
    expect(expired?.expired).toBe(true)
    expect(expired?.countdownLabel).toBe('')

    const first = buildPoolCooldownPresentation({ reason: 'rate_limited_429', until: NOW_SECS + 30, nowMs: NOW_MS })
    const second = buildPoolCooldownPresentation({ reason: 'rate_limited_429', until: NOW_SECS + 30, nowMs: NOW_MS })
    const third = buildPoolCooldownPresentation({ reason: 'rate_limited_429', until: NOW_SECS + 30, nowMs: NOW_MS + 1000 })
    expect(poolCooldownPresentationEquals(first, second)).toBe(true)
    expect(poolCooldownPresentationEquals(first, third)).toBe(false)
    expect(poolCooldownPresentationEquals(null, null)).toBe(true)
    expect(poolCooldownPresentationEquals(first, null)).toBe(false)
  })
})
