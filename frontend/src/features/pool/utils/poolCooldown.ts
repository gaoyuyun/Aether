/**
 * 号池冷却展示工具：原因码文案映射、绝对截止时刻与倒计时。
 *
 * 后端（`handlers/admin/provider/pool/cooldown.rs`）写进冷却 KV 的原因码分三类：
 * - 固定状态码：`rate_limited_429` / `quota_exhausted_429` / `forbidden_403` …
 * - 无提示 429 的退避阶梯：`backoff_level_N`
 * - 上游提示来源（`cooldown_meta.source`）：`retry_after_header` / `ratelimit_window_5h` …
 *
 * 倒计时按「服务端返回的绝对截止时刻 + 本地时钟」计算，不靠响应里的 TTL 递减，
 * 这样多次轮询之间数字连续，也不用引用比较做变化检测（见记忆「视图层变化检测约定」）。
 */

export interface PoolCooldownMeta {
  action?: string | null
  reason?: string | null
  ttl_seconds?: number | null
  effective_ttl_seconds?: number | null
  until?: number | null
  backoff_level?: number | null
  source?: string | null
  scope?: 'key' | 'key_model' | string | null
  retry_after_secs?: number | null
  reset_at?: number | null
  quota_exhausted?: boolean | null
  rejected_windows?: string[] | null
  extended_existing?: boolean | null
  decided_at?: number | null
  model?: string | null
  [key: string]: unknown
}

export interface PoolModelCooldown {
  model: string
  reason: string
  ttl_seconds: number
  until?: number | null
  meta?: PoolCooldownMeta | null
}

const COOLDOWN_REASON_MAP: Record<string, string> = {
  rate_limited_429: '429 限流',
  quota_exhausted_429: '429 额度耗尽',
  forbidden_403: '403 禁止',
  overloaded_529: '529 过载',
  auth_failed_401: '401 认证失败',
  payment_required_402: '402 欠费',
  not_found_404: '404 不存在',
  server_error_500: '500 错误',
  request_timeout_408: '408 超时',
  conflict_409: '409 冲突',
  locked_423: '423 锁定',
  too_early_425: '425 Too Early',
  bad_gateway_502: '502 网关错误',
  service_unavailable_503: '503 服务不可用',
  gateway_timeout_504: '504 网关超时',
  retry_after_header: '上游 Retry-After',
  ratelimit_window_5h: '5 小时窗口耗尽',
  ratelimit_window_7d: '7 天窗口耗尽',
  ratelimit_window: '上游限流窗口',
  google_retry_info: 'Google 重试提示',
  codex_resets_at: 'Codex 额度重置',
  grok_text: 'Grok 等待提示',
  transient_upstream: '上游瞬时错误',
}

const COOLDOWN_SOURCE_MAP: Record<string, string> = {
  retry_after_header: '按上游 Retry-After 冷却',
  ratelimit_window_5h: '按 Anthropic 5 小时窗口重置时刻冷却',
  ratelimit_window_7d: '按 Anthropic 7 天窗口重置时刻冷却',
  ratelimit_window: '按 Anthropic 限流窗口重置时刻冷却',
  google_retry_info: '按 Google RetryInfo / 配额重置时刻冷却',
  codex_resets_at: '按 Codex usage_limit_reached.resets_at 冷却',
  grok_text: '按 Grok 错误文案中的等待时长冷却',
  none: '上游未给出重试提示，按固定策略冷却',
}

/**
 * 原因码转文案。`backoff_level_N` 与 `transient_upstream_5xx` 是带参数的模板，
 * 其余查表；未知原因码原样返回。
 */
export function formatCooldownReason(reason: string | null | undefined): string {
  const code = String(reason ?? '').trim()
  if (!code) return ''
  const backoff = /^backoff_level_(\d+)$/.exec(code)
  if (backoff) {
    const level = Number(backoff[1])
    return `429 退避（第 ${level + 1} 级）`
  }
  const transient = /^transient_upstream_(\d{3})$/.exec(code)
  if (transient) {
    return `${transient[1]} 上游瞬时错误`
  }
  if (code.startsWith('rule:')) {
    return `规则「${code.slice('rule:'.length)}」`
  }
  const streamTimeout = /^stream_timeout_x(\d+)$/.exec(code)
  if (streamTimeout) {
    return `流超时 ×${streamTimeout[1]}`
  }
  return COOLDOWN_REASON_MAP[code] ?? code
}

export function formatCooldownSource(source: string | null | undefined): string {
  const code = String(source ?? '').trim()
  if (!code) return ''
  return COOLDOWN_SOURCE_MAP[code] ?? code
}

/** 秒数 → `1h 02m 03s` / `2m 05s` / `9s`。 */
export function formatCooldownDuration(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds))
  if (total <= 0) return '0s'
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m ${String(s).padStart(2, '0')}s`
  if (m > 0) return `${m}m ${String(s).padStart(2, '0')}s`
  return `${s}s`
}

/**
 * 解析冷却截止时刻（Unix 秒）。优先服务端给的绝对时刻；只有 TTL 时，
 * 用 `observedAtMs`（本次响应到达的本地时间）推算，避免每次轮询都重新“起算”。
 */
export function resolveCooldownUntil(input: {
  until?: number | null
  ttl_seconds?: number | null
  observedAtMs?: number
}): number | null {
  if (typeof input.until === 'number' && Number.isFinite(input.until) && input.until > 0) {
    return Math.floor(input.until)
  }
  const ttl = input.ttl_seconds
  if (typeof ttl === 'number' && Number.isFinite(ttl) && ttl > 0) {
    const base = input.observedAtMs ?? Date.now()
    return Math.floor(base / 1000) + Math.floor(ttl)
  }
  return null
}

export function cooldownRemainingSeconds(untilUnixSecs: number | null | undefined, nowMs: number): number {
  if (typeof untilUnixSecs !== 'number' || !Number.isFinite(untilUnixSecs)) return 0
  return Math.max(0, untilUnixSecs - Math.floor(nowMs / 1000))
}

export function formatCooldownDeadline(untilUnixSecs: number | null | undefined, nowMs: number): string {
  if (typeof untilUnixSecs !== 'number' || !Number.isFinite(untilUnixSecs)) return ''
  const deadline = new Date(untilUnixSecs * 1000)
  const now = new Date(nowMs)
  const sameDay = deadline.getFullYear() === now.getFullYear()
    && deadline.getMonth() === now.getMonth()
    && deadline.getDate() === now.getDate()
  const hh = String(deadline.getHours()).padStart(2, '0')
  const mm = String(deadline.getMinutes()).padStart(2, '0')
  const ss = String(deadline.getSeconds()).padStart(2, '0')
  if (sameDay) return `${hh}:${mm}:${ss}`
  const month = String(deadline.getMonth() + 1).padStart(2, '0')
  const day = String(deadline.getDate()).padStart(2, '0')
  return `${month}-${day} ${hh}:${mm}`
}

export interface PoolCooldownPresentation {
  /** 原因文案，例如「429 限流」。 */
  reasonLabel: string
  /** 绝对截止时刻（本地时区）。 */
  deadlineLabel: string
  /** 倒计时，例如「2m 05s」；已过期为空串。 */
  countdownLabel: string
  remainingSeconds: number
  /** 悬浮提示：来源、退避等级、上游重置时刻等。 */
  title: string
  /** 冷却已到期（截止时刻已过，等待下一次轮询刷新）。 */
  expired: boolean
}

/**
 * 组合出一份可直接渲染的冷却展示。纯函数：同样的输入产生同样的输出，
 * 组件按字段内容判断是否需要更新。
 */
export function buildPoolCooldownPresentation(input: {
  reason: string | null | undefined
  until: number | null | undefined
  meta?: PoolCooldownMeta | null
  nowMs: number
}): PoolCooldownPresentation | null {
  const reason = String(input.reason ?? '').trim()
  if (!reason) return null
  const remainingSeconds = cooldownRemainingSeconds(input.until, input.nowMs)
  const lines: string[] = []
  const reasonLabel = formatCooldownReason(reason)
  lines.push(`原因：${reasonLabel}`)
  const meta = input.meta ?? null
  if (meta?.source) {
    lines.push(`来源：${formatCooldownSource(meta.source)}`)
  }
  if (typeof meta?.backoff_level === 'number') {
    lines.push(`退避等级：${meta.backoff_level}`)
  }
  if (typeof meta?.retry_after_secs === 'number') {
    lines.push(`上游要求等待：${formatCooldownDuration(meta.retry_after_secs)}`)
  }
  if (typeof meta?.reset_at === 'number' && meta.reset_at > 0) {
    lines.push(`上游重置时刻：${formatCooldownDeadline(meta.reset_at, input.nowMs)}`)
  }
  if (meta?.scope === 'key_model' && meta.model) {
    lines.push(`作用域：仅模型 ${meta.model}`)
  }
  if (meta?.extended_existing) {
    lines.push('本次失败未缩短已有冷却')
  }
  if (Array.isArray(meta?.rejected_windows) && meta.rejected_windows.length > 0) {
    lines.push(`拒绝窗口：${meta.rejected_windows.join(', ')}`)
  }
  const deadlineLabel = formatCooldownDeadline(input.until, input.nowMs)
  if (deadlineLabel) {
    lines.push(`截止：${deadlineLabel}`)
  }
  return {
    reasonLabel,
    deadlineLabel,
    countdownLabel: remainingSeconds > 0 ? formatCooldownDuration(remainingSeconds) : '',
    remainingSeconds,
    title: lines.join('\n'),
    expired: input.until != null && remainingSeconds === 0,
  }
}

/** 两份展示内容是否一致；用于避免无变化的重渲染。 */
export function poolCooldownPresentationEquals(
  left: PoolCooldownPresentation | null,
  right: PoolCooldownPresentation | null,
): boolean {
  if (left === right) return true
  if (!left || !right) return false
  return left.reasonLabel === right.reasonLabel
    && left.deadlineLabel === right.deadlineLabel
    && left.countdownLabel === right.countdownLabel
    && left.remainingSeconds === right.remainingSeconds
    && left.title === right.title
    && left.expired === right.expired
}
