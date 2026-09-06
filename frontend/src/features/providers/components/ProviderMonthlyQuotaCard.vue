<template>
  <Card class="p-4" data-testid="provider-monthly-quota-card">
    <div class="space-y-3">
      <div class="flex flex-wrap items-center justify-between gap-2">
        <h3 class="text-sm font-semibold">{{ legacyT('订阅配额') }}</h3>
        <div class="flex items-center gap-2">
          <Badge variant="secondary" class="text-xs">{{ statusText }}</Badge>
          <Badge v-if="quota !== null" variant="secondary" class="text-xs" data-testid="provider-monthly-quota-percent">{{ usedPercent.toFixed(1) }}%</Badge>
          <Button v-if="providerId" variant="ghost" size="sm" :disabled="loading" :aria-label="legacyT('刷新配额')" data-testid="provider-quota-refresh" @click="loadQuotaStats">
            <RotateCcw class="h-3.5 w-3.5" :class="{ 'animate-spin': loading }" />
          </Button>
        </div>
      </div>
      <div v-if="quota !== null" class="relative h-2 w-full overflow-hidden rounded-full bg-border">
        <div class="absolute left-0 top-0 h-full transition-all duration-300" :class="barClass" :style="{ width: `${Math.min(Math.max(usedPercent, 0), 100)}%` }" />
      </div>
      <div class="flex flex-wrap items-center justify-between gap-2 text-xs">
        <span class="font-semibold" data-testid="provider-monthly-quota-amount">{{ amount(used) }} / {{ quota === null ? legacyT('未设总额度') : amount(quota) }}</span>
        <span v-if="resetDays" class="text-muted-foreground" data-testid="provider-monthly-quota-reset">{{ legacyT('每') }} {{ resetDays }} {{ legacyT('天重置') }}</span>
      </div>
      <dl class="grid gap-1 text-xs text-muted-foreground">
        <div v-for="item in timeRows" :key="item.label" class="flex flex-wrap justify-between gap-x-2">
          <dt>{{ legacyT(item.label) }}</dt><dd>{{ formatTime(item.value) }}</dd>
        </div>
      </dl>
      <p v-if="loadError" role="alert" class="text-xs text-red-600" data-testid="provider-quota-load-error">{{ loadError }} {{ legacyT('可点击刷新重试，当前展示可能尚未更新。') }}</p>
      <p v-else-if="loading" class="text-xs text-muted-foreground">{{ legacyT('正在加载配额统计…') }}</p>
      <div class="flex flex-wrap items-center justify-between gap-2 text-xs">
        <span v-if="pendingResetAt" class="text-amber-600" data-testid="provider-monthly-quota-pending-reset">{{ legacyT('待调整') }}：{{ formatTime(pendingResetAt) }}</span>
        <span v-else />
        <div v-if="providerId" class="flex gap-2">
          <Button variant="outline" size="sm" :disabled="resetting" data-testid="provider-monthly-quota-reset-button" @click="openReset(false)">{{ legacyT('手动重置') }}</Button>
          <Button variant="outline" size="sm" :disabled="resetting" @click="openReset(true)">{{ legacyT('调整周期') }}</Button>
        </div>
      </div>
      <div v-if="windows.length" class="space-y-3 border-t border-border/60 pt-3 text-xs" data-testid="provider-quota-windows">
        <div v-for="window in windows" :key="window.duration_secs" class="space-y-1" :data-duration="window.duration_secs">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <span>{{ legacyT('滚动窗口') }} · {{ formatDuration(window.duration_secs) }}</span>
            <span class="tabular-nums">{{ window.status === 'ready' ? amount(window.used_usd) : legacyT('待统计') }} / {{ amount(window.limit_usd) }}</span>
          </div>
          <div class="flex flex-wrap items-center justify-between gap-2 text-muted-foreground">
            <span>{{ window.status === 'failed' ? legacyT('统计失败') : window.status === 'rebuilding' ? legacyT('统计重建中') : window.status === 'ready' && window.used_usd != null ? legacyT('已就绪') : legacyT('待统计') }}</span>
            <Button v-if="providerId" variant="ghost" size="sm" class="h-6 text-xs" :disabled="loading" @click="loadQuotaStats">{{ legacyT('刷新') }}</Button>
          </div>
          <p v-if="window.accounted_until" class="text-muted-foreground">{{ legacyT('统计至') }}：{{ formatTime(window.accounted_until) }}</p>
          <p v-if="window.status === 'failed' && window.rebuild_error" class="break-words text-red-600" data-testid="provider-quota-window-error">{{ legacyT('失败原因') }}：{{ formatRebuildError(window.rebuild_error) }}</p>
        </div>
      </div>
      <p v-else class="text-xs text-muted-foreground">{{ legacyT('未配置滚动窗口') }}</p>
    </div>
  </Card>
  <Dialog v-model="resetOpen" :title="legacyT(adjustingCycle ? '调整周期' : '手动重置额度')" size="md">
    <form class="space-y-4" @submit.prevent="submitReset">
      <fieldset v-if="!adjustingCycle" class="space-y-3">
        <legend class="mb-2 text-sm">{{ legacyT('选择重置方式') }}</legend>
        <label class="flex items-start gap-2 text-sm"><input v-model="resetMode" type="radio" value="usage_only" name="quota-reset-mode"><span>{{ legacyT('仅重置用量，保留当前周期起点和下次自然重置时间') }}</span></label>
        <label class="flex items-start gap-2 text-sm"><input v-model="resetMode" type="radio" value="cycle" name="quota-reset-mode"><span>{{ legacyT('重置周期起点并清零用量，从生效时间重新计算周期') }}</span></label>
      </fieldset>
      <label class="block space-y-1 text-sm"><span>{{ legacyT('生效时间（精确到分）') }}</span><Input v-model="effectiveAt" type="datetime-local" step="60" required /></label>
      <template v-if="adjustingCycle">
        <label class="block space-y-1 text-sm"><span>{{ legacyT('周期长度（天）') }}</span><Input v-model.number="cycleDays" type="number" min="1" max="30" required /></label>
        <label class="flex items-center gap-2 text-sm"><input v-model="clearUsage" type="checkbox">{{ legacyT('生效时同时清零用量') }}</label>
        <p class="text-xs text-muted-foreground">{{ legacyT('当前周期起点将改为所选生效时间。未勾选清零时，已用额度继续保留。') }}</p>
      </template>
      <p class="text-xs text-muted-foreground">{{ legacyT('订阅开始记录和到期时间保持不变。历史用量保留；生效前已发出的请求仍按原记账周期结算。') }}</p>
      <p v-if="resetError" role="alert" class="text-xs text-red-600">{{ resetError }}</p>
      <div class="flex justify-end gap-2"><Button type="button" variant="outline" @click="resetOpen = false">{{ legacyT('取消') }}</Button><Button type="submit" :disabled="resetting"><Loader2 v-if="resetting" class="mr-1 h-4 w-4 animate-spin" />{{ legacyT('确认安排') }}</Button></div>
    </form>
  </Dialog>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { Badge, Button, Card, Dialog, Input } from '@/components/ui'
import { useI18n } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { getProviderStats, resetProviderQuota, type ProviderQuotaBillingInfo } from '@/api/provider-strategy'
import { Loader2, RotateCcw } from 'lucide-vue-next'
import type { ProviderQuotaWindow } from '@/api/endpoints'

const props = withDefaults(defineProps<{
  used?: number | null; quota?: number | null; resetDay?: number | null
  providerId?: string | null; pendingResetAt?: string | null; windows?: ProviderQuotaWindow[]
  active?: boolean; subscriptionStart?: string | null; cycleStart?: string | null
  nextResetAt?: string | null; expiresAt?: string | null
}>(), { used: 0, quota: null, resetDay: null, providerId: null, pendingResetAt: null,
  windows: () => [], active: true, subscriptionStart: null, cycleStart: null, nextResetAt: null, expiresAt: null })
const { legacyT } = useI18n()
const { success } = useToast()
const live = ref<ProviderQuotaBillingInfo | null>(null)
const loading = ref(false)
const loadError = ref('')
const clock = ref(Date.now())
let generation = 0
let refreshTimer: ReturnType<typeof setTimeout> | undefined
const used = computed(() => live.value ? live.value.monthly_used_usd : props.used)
const quota = computed(() => live.value?.monthly_quota_usd ?? props.quota)
const resetDays = computed(() => live.value?.quota_reset_day ?? props.resetDay)
const windows = computed(() => [...(live.value?.quota_windows ?? props.windows)].sort((a, b) => a.duration_secs - b.duration_secs))
const pendingResetAt = computed(() => live.value ? live.value.pending_quota_reset_at : props.pendingResetAt)
const usedPercent = computed(() => quota.value != null && quota.value > 0 ? ((used.value ?? 0) / quota.value) * 100 : 0)
const barClass = computed(() => usedPercent.value >= 90 ? 'bg-red-500' : usedPercent.value >= 70 ? 'bg-yellow-500' : 'bg-green-500')
const expiry = computed(() => live.value ? live.value.quota_expires_at : props.expiresAt)
const subscription = computed(() => live.value?.quota_subscription_started_at ?? props.subscriptionStart)
const statusText = computed(() => {
  if (expiry.value && Date.parse(expiry.value) <= clock.value) return legacyT('已过期')
  if (subscription.value && Date.parse(subscription.value) > clock.value) return legacyT('未开始')
  const labels: Record<string, string> = { expired: '已过期', not_started: '未开始', disabled: '已停用', exhausted: '额度耗尽', accounting_pending: '待结算或统计维护', active: '可用' }
  return legacyT(labels[live.value?.status ?? ''] ?? (quota.value !== null && (used.value ?? 0) >= quota.value ? '额度耗尽' : '配额概览'))
})
const timeRows = computed(() => [
  { label: '订阅开始', value: subscription.value },
  { label: '当前周期起点', value: live.value?.quota_cycle_start_at ?? props.cycleStart },
  { label: '下次自然重置', value: live.value?.quota_next_reset_at ?? props.nextResetAt },
  { label: '订阅到期', value: expiry.value },
].filter((item): item is { label: string; value: string } => !!item.value))
function amount(value?: number | null) { return value != null && Number.isFinite(value) ? `$${value.toFixed(2)}` : legacyT('待统计') }
function formatTime(value: string) { const date = new Date(value); return Number.isNaN(date.getTime()) ? value : date.toLocaleString() }
function formatDuration(seconds: number) {
  if (seconds % 604800 === 0) return `${seconds / 604800} ${legacyT('周')}`
  if (seconds % 86400 === 0) return `${seconds / 86400} ${legacyT('日')}`
  if (seconds % 3600 === 0) return `${seconds / 3600} ${legacyT('小时')}`
  return `${seconds / 60} ${legacyT('分钟')}`
}
function formatRebuildError(error: string) { return error === 'quota cost is unavailable for a dispatched monthly request' ? legacyT('有已分发请求的费用尚未完成核算，请刷新查看恢复状态') : error }
async function loadQuotaStats() {
  clearTimeout(refreshTimer)
  if (!props.providerId || !props.active) return
  const id = props.providerId
  const request = ++generation
  loading.value = true
  loadError.value = ''
  try {
    const stats = await getProviderStats(id, 24)
    if (request !== generation) return
    if (!stats.billing_info) throw new Error('missing quota statistics')
    live.value = stats.billing_info
  } catch {
    if (request === generation) loadError.value = legacyT('配额统计加载失败。')
  } finally {
    if (request === generation) {
      loading.value = false
      clock.value = Date.now()
      if (props.active) {
        const remaining = pendingResetAt.value ? Date.parse(pendingResetAt.value) - Date.now() + 1000 : 15000
        refreshTimer = setTimeout(() => { void loadQuotaStats() }, Math.max(1000, Math.min(15000, remaining)))
      }
    }
  }
}
const resetOpen = ref(false)
const adjustingCycle = ref(false)
const resetMode = ref<'cycle' | 'usage_only'>('usage_only')
const effectiveAt = ref('')
const cycleDays = ref(30)
const clearUsage = ref(false)
const resetting = ref(false)
const resetError = ref('')
function openReset(adjust: boolean) {
  adjustingCycle.value = adjust
  resetMode.value = 'usage_only'
  cycleDays.value = resetDays.value ?? 30
  clearUsage.value = false
  const next = new Date((Math.floor(Date.now() / 60000) + 1) * 60000)
  effectiveAt.value = new Date(next.getTime() - next.getTimezoneOffset() * 60000).toISOString().slice(0, 16)
  resetError.value = ''
  resetOpen.value = true
}
async function submitReset() {
  if (!props.providerId || resetting.value) return
  const date = new Date(effectiveAt.value)
  if (!Number.isFinite(date.getTime()) || date.getTime() <= Date.now()) { resetError.value = legacyT('请选择下一整分钟或更晚的生效时间'); return }
  const providerId = props.providerId
  resetting.value = true
  try {
    const result = await resetProviderQuota(providerId, { mode: adjustingCycle.value ? 'cycle' : resetMode.value,
      effective_at: date.toISOString(), reset_usage: adjustingCycle.value ? clearUsage.value : true,
      ...(adjustingCycle.value ? { cycle_days: cycleDays.value } : {}) })
    if (props.providerId !== providerId) return
    if (live.value) live.value.pending_quota_reset_at = result.effective_at
    resetOpen.value = false
    success(legacyT('额度调整已安排'))
    await loadQuotaStats()
  } catch { resetError.value = legacyT('额度调整失败，请刷新后重试') }
  finally { resetting.value = false }
}
watch(() => [props.providerId, props.active, props.windows, props.used, props.quota, props.resetDay, props.subscriptionStart, props.expiresAt], () => {
  ++generation
  clearTimeout(refreshTimer)
  live.value = null
  loadError.value = ''
  loading.value = false
  resetOpen.value = false
  void loadQuotaStats()
}, { immediate: true, deep: true })
onBeforeUnmount(() => { ++generation; clearTimeout(refreshTimer) })
</script>
