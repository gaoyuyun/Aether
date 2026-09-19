import { ref, onUnmounted } from 'vue'
import type { ProviderWithEndpointsSummary } from '@/api/endpoints'
import {
  batchQueryBalance,
  getArchitectures,
  getBalance,
  type ActionResultResponse,
  type ArchitectureInfo,
  type BalanceLastError,
  type BalanceRefreshState,
} from '@/api/providerOps'
import { formatBalanceExtraFromSchema, type CredentialsSchema } from '@/features/providers/auth-templates/schema-utils'
import type { BalanceExtraItem } from '@/features/providers/auth-templates'
import { formatRelativeTime } from '@/utils/format'
import { log } from '@/utils/logger'

/**
 * 后台刷新的轮询节奏：快速开始、逐步放慢，次数用尽后进入终态而不是无限转圈。
 * 后端把上游查询放在后台，页面只读快照，所以这里只需要等后台任务完成。
 */
export const BALANCE_POLL_DELAYS_MS = [2_000, 4_000, 8_000, 16_000] as const

export interface ProviderBalanceMeta {
  /** 最近一次成功查询的时间，null 表示从未成功 */
  fetchedAt: string | null
  /** 成功值已超过新鲜阈值，或从未成功 */
  stale: boolean
  refreshState: BalanceRefreshState
  /** 退避到期时间，null 表示可随时刷新 */
  nextRetryAt: string | null
  consecutiveFailures: number
  /** 最近一次失败尝试；有值时余额仍显示上次成功的结果 */
  lastError: BalanceLastError | null
  /** 轮询次数用尽仍没有拿到结果 */
  exhausted: boolean
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : null
}

/** 后端仍在后台刷新，或尚无任何快照 */
export function balanceNeedsPolling(result: ActionResultResponse): boolean {
  return result.status === 'pending' || result.refresh_state === 'refreshing'
}

export function useProviderBalance() {
  // 余额快照缓存 {providerId: ActionResultResponse}，失败状态也保留以便展示原因
  const balanceCache = ref<Record<string, ActionResultResponse>>({})
  // 轮询用尽仍未拿到结果的 provider
  const exhaustedProviderIds = ref<Record<string, true>>({})
  // 手动刷新进行中的 provider
  const manualRefreshingIds = ref<Record<string, true>>({})
  // 全量加载版本号：新一轮全量加载会作废之前所有轮询
  let balanceLoadVersion = 0

  // 追踪待处理的定时器，用于组件卸载时清理
  const pendingTimers = new Set<ReturnType<typeof setTimeout>>()

  // 架构 schema 缓存（用于 balance extra 格式化）
  const architectureSchemas = ref<Record<string, CredentialsSchema>>({})
  const architectureSchemasLoaded = ref(false)

  // 用于触发倒计时/相对时间更新的响应式计数器
  const tickCounter = ref(0)
  let tickInterval: ReturnType<typeof setInterval> | null = null

  function startTick() {
    if (tickInterval) return
    tickInterval = setInterval(() => {
      tickCounter.value++
    }, 1000)
  }

  function stopTick() {
    if (tickInterval) {
      clearInterval(tickInterval)
      tickInterval = null
    }
  }

  /** 加载架构 schema 缓存 */
  async function loadArchitectureSchemas() {
    if (architectureSchemasLoaded.value) return
    try {
      const archs: ArchitectureInfo[] = await getArchitectures()
      const schemas: Record<string, CredentialsSchema> = {}
      for (const arch of archs) {
        if (arch.credentials_schema) {
          schemas[arch.architecture_id] = arch.credentials_schema
        }
      }
      architectureSchemas.value = schemas
      architectureSchemasLoaded.value = true
    } catch {
      // 加载失败不影响主流程
    }
  }

  function storeResults(results: Record<string, ActionResultResponse>) {
    for (const [providerId, result] of Object.entries(results)) {
      balanceCache.value[providerId] = result
      delete exhaustedProviderIds.value[providerId]
    }
  }

  function pollingIds(results: Record<string, ActionResultResponse>): string[] {
    return Object.entries(results)
      .filter(([, result]) => balanceNeedsPolling(result))
      .map(([providerId]) => providerId)
  }

  /**
   * 加载余额快照（使用批量接口）。
   * fullReload=false 只补充给定 provider，不会打断页面上其他 provider 的轮询。
   */
  async function loadBalances(providers: Pick<ProviderWithEndpointsSummary, 'id' | 'ops_configured'>[], fullReload = true) {
    if (fullReload) {
      balanceCache.value = {}
      exhaustedProviderIds.value = {}
    }
    const currentVersion = fullReload ? ++balanceLoadVersion : balanceLoadVersion
    try {
      const opsProviderIds = providers.filter(p => p.ops_configured).map(p => p.id)
      if (opsProviderIds.length === 0) return

      const results = await batchQueryBalance(opsProviderIds)

      // 检查是否有新的全量加载已经开始，如果有则丢弃当前结果
      if (currentVersion !== balanceLoadVersion) return

      storeResults(results)
      schedulePoll(pollingIds(results), currentVersion, 0)
    } catch (e) {
      log.warn('[loadBalances] 加载余额数据失败', e)
    }
  }

  function schedulePoll(providerIds: string[], loadVersion: number, attempt: number) {
    if (providerIds.length === 0) return
    if (attempt >= BALANCE_POLL_DELAYS_MS.length) {
      for (const providerId of providerIds) {
        exhaustedProviderIds.value[providerId] = true
      }
      return
    }
    const timerId = setTimeout(() => {
      pendingTimers.delete(timerId)
      if (loadVersion !== balanceLoadVersion) return
      void pollBalances(providerIds, loadVersion, attempt)
    }, BALANCE_POLL_DELAYS_MS[attempt])
    pendingTimers.add(timerId)
  }

  // 轮询后台刷新中的余额
  async function pollBalances(providerIds: string[], loadVersion: number, attempt: number) {
    try {
      const results = await batchQueryBalance(providerIds)
      if (loadVersion !== balanceLoadVersion) return
      storeResults(results)
      schedulePoll(pollingIds(results), loadVersion, attempt + 1)
    } catch (e) {
      log.warn('[pollBalances] 轮询余额失败', e)
    }
  }

  /** 手动触发一次后台刷新（忽略退避），然后轮询直到后台任务结束 */
  async function refreshProviderBalance(providerId: string) {
    if (manualRefreshingIds.value[providerId]) return
    manualRefreshingIds.value[providerId] = true
    const loadVersion = balanceLoadVersion
    try {
      const result = await getBalance(providerId, true)
      if (loadVersion !== balanceLoadVersion) return
      storeResults({ [providerId]: result })
      if (balanceNeedsPolling(result)) {
        schedulePoll([providerId], loadVersion, 0)
      }
    } catch (e) {
      log.warn('[refreshProviderBalance] 刷新余额失败', e)
    } finally {
      delete manualRefreshingIds.value[providerId]
    }
  }

  /**
   * 类型守卫：检查是否为 BalanceInfo（简化版）
   */
  function isBalanceInfo(data: unknown): data is { total_available: number | null; currency: string } {
    if (typeof data !== 'object' || data === null) return false
    if (!('total_available' in data) || !('currency' in data)) return false
    const d = data as Record<string, unknown>
    if (d.total_available !== null && typeof d.total_available !== 'number') return false
    if (typeof d.currency !== 'string') return false
    return true
  }

  // 获取 provider 的余额显示
  function getProviderBalance(providerId: string): { available: number | null; currency: string } | null {
    const result = balanceCache.value[providerId]
    // auth_expired 时余额数据仍有效（只是签到 Cookie 失效）
    if (!result || (result.status !== 'success' && result.status !== 'auth_expired') || !result.data) {
      return null
    }
    if (!isBalanceInfo(result.data)) {
      return null
    }
    return {
      available: result.data.total_available,
      currency: result.data.currency || 'USD',
    }
  }

  // 获取 provider 余额明细（balance + points 分开显示）
  function getProviderBalanceBreakdown(providerId: string): { balance: number; points: number; currency: string } | null {
    const result = balanceCache.value[providerId]
    if (!result || (result.status !== 'success' && result.status !== 'auth_expired') || !result.data) {
      return null
    }
    const data = result.data as Record<string, unknown>
    const extra = asRecord(data.extra)
    if (!extra || typeof extra.balance !== 'number' || typeof extra.points !== 'number') {
      return null
    }
    return {
      balance: extra.balance,
      points: extra.points,
      currency: typeof data.currency === 'string' && data.currency ? data.currency : 'USD',
    }
  }

  // 获取 provider 余额查询的错误状态（没有任何可用历史值时）
  function getProviderBalanceError(providerId: string): { status: string; message: string } | null {
    const result = balanceCache.value[providerId]
    if (!result) {
      return null
    }
    // pending 状态不是错误，正在加载中
    if (result.status === 'pending') {
      return null
    }
    // 认证失败或过期
    if (result.status === 'auth_failed' || result.status === 'auth_expired') {
      return {
        status: result.status,
        message: result.message || '认证失败',
      }
    }
    // 其他错误
    if (result.status !== 'success') {
      return {
        status: result.status,
        message: result.message || '查询失败',
      }
    }
    return null
  }

  // 检查余额是否正在首次加载（尚无任何快照）
  function isBalanceLoading(providerId: string): boolean {
    const result = balanceCache.value[providerId]
    return result?.status === 'pending' && exhaustedProviderIds.value[providerId] !== true
  }

  // 已有快照但后台正在刷新
  function isBalanceRefreshing(providerId: string): boolean {
    const result = balanceCache.value[providerId]
    return manualRefreshingIds.value[providerId] === true || result?.refresh_state === 'refreshing'
  }

  // 获取快照元数据：新鲜度、最近失败、退避
  function getProviderBalanceMeta(providerId: string): ProviderBalanceMeta | null {
    const result = balanceCache.value[providerId]
    if (!result) return null
    return {
      fetchedAt: result.fetched_at ?? null,
      stale: result.stale ?? false,
      refreshState: result.refresh_state ?? 'idle',
      nextRetryAt: result.next_retry_at ?? null,
      consecutiveFailures: result.consecutive_failures ?? 0,
      lastError: result.last_error ?? null,
      exhausted: exhaustedProviderIds.value[providerId] === true,
    }
  }

  // 获取 provider 的签到信息（从 extra 字段）
  function getProviderCheckin(providerId: string): { success: boolean | null; message: string } | null {
    const result = balanceCache.value[providerId]
    if (!result || result.status !== 'success' || !result.data) {
      return null
    }
    const data = result.data as Record<string, unknown>
    const extra = asRecord(data.extra)
    if (!extra || (extra.checkin_success !== null && typeof extra.checkin_success !== 'boolean')) {
      return null
    }
    return {
      success: typeof extra.checkin_success === 'boolean' ? extra.checkin_success : null,
      message: typeof extra.checkin_message === 'string' ? extra.checkin_message : '',
    }
  }

  // 获取 provider 的 Cookie 失效状态（从 extra 字段）
  function getProviderCookieExpired(providerId: string): { expired: boolean; message: string } | null {
    const result = balanceCache.value[providerId]
    if (!result || !result.data) {
      return null
    }
    if (result.status !== 'success' && result.status !== 'auth_expired') {
      return null
    }
    const data = result.data as Record<string, unknown>
    const extra = asRecord(data.extra)
    if (!extra || !extra.cookie_expired) {
      return null
    }
    return {
      expired: true,
      message: typeof extra.cookie_expired_message === 'string' && extra.cookie_expired_message
        ? extra.cookie_expired_message
        : 'Cookie 已失效',
    }
  }

  // 格式化余额显示
  function formatBalanceDisplay(balance: { available: number | null; currency: string } | null): string {
    if (!balance || balance.available == null) {
      return '-'
    }
    const symbol = balance.currency === 'USD' ? '$' : balance.currency
    return `${symbol}${balance.available.toFixed(2)}`
  }

  // 格式化重置倒计时（从 Unix 时间戳）
  function formatResetCountdown(resetsAt: number): string {
    // 依赖 tickCounter 触发响应式更新
    void tickCounter.value

    const now = Date.now() / 1000
    const diff = resetsAt - now

    if (diff <= 0) return '即将重置'

    const totalHours = Math.floor(diff / 3600)
    const minutes = Math.floor((diff % 3600) / 60)
    const seconds = Math.floor(diff % 60)

    const pad = (n: number) => n.toString().padStart(2, '0')

    if (totalHours > 0) {
      return `${totalHours}:${pad(minutes)}:${pad(seconds)}`
    }
    return `${minutes}:${pad(seconds)}`
  }

  // 把成功获取时间格式化成本地化的相对时间（“3 分钟前”）
  function formatBalanceFetchedAt(fetchedAt: string): string {
    // 依赖 tickCounter 触发响应式更新
    void tickCounter.value
    const fetchedAtMs = new Date(fetchedAt).getTime()
    if (!Number.isFinite(fetchedAtMs)) return fetchedAt
    const diffSeconds = Math.max(0, Math.round((Date.now() - fetchedAtMs) / 1000))
    if (diffSeconds < 60) return formatRelativeTime(-diffSeconds, 'second')
    if (diffSeconds < 3600) return formatRelativeTime(-Math.floor(diffSeconds / 60), 'minute')
    if (diffSeconds < 86400) return formatRelativeTime(-Math.floor(diffSeconds / 3600), 'hour')
    return formatRelativeTime(-Math.floor(diffSeconds / 86400), 'day')
  }

  // 获取 provider 余额的额外信息（如窗口限额）
  function getProviderBalanceExtra(providerId: string, architectureId?: string): BalanceExtraItem[] {
    if (!architectureId) return []

    const result = balanceCache.value[providerId]
    // auth_expired 时余额数据仍有效（只是签到 Cookie 失效）
    if (!result || (result.status !== 'success' && result.status !== 'auth_expired') || !result.data) {
      return []
    }

    const data = result.data as Record<string, unknown>
    const extra = asRecord(data.extra)
    if (!extra) return []

    // 从 schema 缓存中获取格式化配置
    const schema = architectureSchemas.value[architectureId]
    if (!schema) return []

    return formatBalanceExtraFromSchema(schema, extra)
  }

  // 配额已用颜色（根据使用比例）
  function getQuotaUsedColorClass(provider: ProviderWithEndpointsSummary): string {
    const used = provider.monthly_used_usd ?? 0
    const quota = provider.monthly_quota_usd ?? 0
    if (quota <= 0) return 'text-foreground'
    const ratio = used / quota
    if (ratio >= 0.9) return 'text-red-600 dark:text-red-400'
    if (ratio >= 0.7) return 'text-amber-600 dark:text-amber-400'
    return 'text-foreground'
  }

  // 组件卸载时清理
  function cleanup() {
    balanceLoadVersion++
    stopTick()
    pendingTimers.forEach(clearTimeout)
    pendingTimers.clear()
  }

  onUnmounted(cleanup)

  return {
    balanceCache,
    loadArchitectureSchemas,
    loadBalances,
    refreshProviderBalance,
    getProviderBalance,
    getProviderBalanceBreakdown,
    getProviderBalanceError,
    getProviderBalanceMeta,
    isBalanceLoading,
    isBalanceRefreshing,
    getProviderCheckin,
    getProviderCookieExpired,
    formatBalanceDisplay,
    formatBalanceFetchedAt,
    formatResetCountdown,
    getProviderBalanceExtra,
    getQuotaUsedColorClass,
    tickCounter,
    startTick,
    stopTick,
  }
}
