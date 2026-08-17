<template>
  <Card
    v-if="quota > 0"
    class="p-4"
    data-testid="provider-monthly-quota-card"
  >
    <div class="space-y-3">
      <div class="flex items-center justify-between">
        <h3 class="text-sm font-semibold">
          {{ legacyT('订阅配额') }}
        </h3>
        <Badge
          variant="secondary"
          class="text-xs"
          data-testid="provider-monthly-quota-percent"
        >
          {{ usedPercent.toFixed(1) }}%
        </Badge>
      </div>
      <div class="relative w-full h-2 bg-border rounded-full overflow-hidden">
        <div
          class="absolute left-0 top-0 h-full transition-all duration-300"
          :class="barClass"
          :style="{ width: `${cappedUsedPercent}%` }"
        />
      </div>
      <div class="flex items-center justify-between text-xs">
        <span
          class="font-semibold"
          data-testid="provider-monthly-quota-amount"
        >
          ${{ used.toFixed(2) }} / ${{ quota.toFixed(2) }}
        </span>
        <span
          v-if="resetDay"
          class="text-muted-foreground"
          data-testid="provider-monthly-quota-reset"
        >
          {{ legacyT('每') }} {{ resetDay }} {{ legacyT('天重置') }}
        </span>
      </div>
      <div class="flex items-center justify-between gap-2 text-xs">
        <span
          v-if="pendingResetAt"
          class="text-amber-600 dark:text-amber-400"
          data-testid="provider-monthly-quota-pending-reset"
        >
          {{ legacyT('待重置') }}：{{ formatPendingReset(pendingResetAt) }}
        </span>
        <span v-else />
        <Button
          v-if="providerId"
          type="button"
          variant="outline"
          size="sm"
          class="h-8"
          :disabled="resetting"
          data-testid="provider-monthly-quota-reset-button"
          @click="handleReset"
        >
          <Loader2 v-if="resetting" class="mr-1.5 h-3.5 w-3.5 animate-spin" />
          <RotateCcw v-else class="mr-1.5 h-3.5 w-3.5" />
          {{ legacyT('重置当前周期') }}
        </Button>
      </div>
      <div
        v-if="windows.length > 0"
        class="space-y-1 border-t border-border/60 pt-2 text-xs text-muted-foreground"
        data-testid="provider-quota-windows"
      >
        <div
          v-for="window in windows"
          :key="`${window.duration_secs}-${window.limit_usd}`"
          class="grid grid-cols-[1fr_auto] gap-x-3 gap-y-0.5"
        >
          <span>{{ legacyT('滚动窗口') }} · {{ formatWindowDuration(window.duration_secs) }}</span>
          <span class="tabular-nums">{{ formatWindowUsage(window) }}</span>
          <span
            v-if="window.status && window.status !== 'ready'"
            class="text-amber-600 dark:text-amber-400"
          >
            {{ window.status === 'failed' ? legacyT('统计失败') : legacyT('统计重建中') }}
          </span>
          <span
            v-else-if="window.rolling_start"
            class="text-[11px]"
          >
            {{ legacyT('有效起点') }}：{{ formatPendingReset(window.rolling_start) }}
          </span>
          <span
            v-if="window.status === 'failed' && window.rebuild_error"
            class="col-span-2 text-[11px] text-red-600 dark:text-red-400"
            data-testid="provider-quota-window-error"
          >
            {{ legacyT('失败原因') }}：{{ formatRebuildError(window.rebuild_error) }}
          </span>
        </div>
      </div>
    </div>
  </Card>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import Badge from '@/components/ui/badge.vue'
import Card from '@/components/ui/card.vue'
import Button from '@/components/ui/button.vue'
import { useI18n } from '@/i18n'
import { useConfirm } from '@/composables/useConfirm'
import { useToast } from '@/composables/useToast'
import { getProviderStats, resetProviderQuota } from '@/api/provider-strategy'
import { Loader2, RotateCcw } from 'lucide-vue-next'
import type { ProviderQuotaWindow } from '@/api/endpoints'

const props = withDefaults(defineProps<{
  used?: number | null
  quota?: number | null
  resetDay?: number | null
  providerId?: string | null
  pendingResetAt?: string | null
  windows?: ProviderQuotaWindow[]
}>(), {
  used: 0,
  quota: 0,
  resetDay: null,
  providerId: null,
  pendingResetAt: null,
  windows: () => [],
})

const { legacyT } = useI18n()
const { confirm } = useConfirm()
const { success, error: showError } = useToast()
const liveUsed = ref<number | null>(null)
const liveWindows = ref<ProviderQuotaWindow[] | null>(null)
const livePendingResetAt = ref<string | null>(null)
const resetting = ref(false)

const used = computed(() => Number.isFinite(Number(liveUsed.value ?? props.used)) ? Number(liveUsed.value ?? props.used) : 0)
const quota = computed(() => Number.isFinite(Number(props.quota)) ? Number(props.quota) : 0)
const windows = computed(() => liveWindows.value ?? props.windows ?? [])
const pendingResetAt = computed(() => livePendingResetAt.value ?? props.pendingResetAt ?? null)
const usedPercent = computed(() => quota.value > 0 ? (used.value / quota.value) * 100 : 0)
const cappedUsedPercent = computed(() => Math.min(Math.max(usedPercent.value, 0), 100))
const barClass = computed(() => {
  const ratio = quota.value > 0 ? used.value / quota.value : 0
  if (ratio >= 0.9) return 'bg-red-500'
  if (ratio >= 0.7) return 'bg-yellow-500'
  return 'bg-green-500'
})

function formatWindowDuration(durationSecs: number): string {
  if (durationSecs % 604_800 === 0) return `${durationSecs / 604_800} ${legacyT('周')}`
  if (durationSecs % 86_400 === 0) return `${durationSecs / 86_400} ${legacyT('日')}`
  if (durationSecs % 3_600 === 0) return `${durationSecs / 3_600} ${legacyT('小时')}`
  return `${durationSecs} ${legacyT('秒')}`
}

function formatWindowUsage(window: ProviderQuotaWindow): string {
  const limit = `$${window.limit_usd.toFixed(2)}`
  if (window.status !== 'ready' || window.used_usd == null) return limit
  return `$${window.used_usd.toFixed(2)} / ${limit}`
}

function formatRebuildError(error: string): string {
  if (error === 'quota cost is unavailable for a dispatched monthly request') {
    return legacyT('请求已发送，但无法计算月卡费用；请检查该模型的价格配置')
  }
  return error
}

function formatPendingReset(value: string): string {
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString()
}

async function loadQuotaStats() {
  if (!props.providerId) return
  try {
    const stats = await getProviderStats(props.providerId, 24)
    const billing = stats?.billing_info
    if (billing) {
      liveUsed.value = Number.isFinite(Number(billing.monthly_used_usd)) ? Number(billing.monthly_used_usd) : null
      livePendingResetAt.value = billing.pending_quota_reset_at ?? null
      liveWindows.value = Array.isArray(billing.quota_windows) ? billing.quota_windows : null
    }
  } catch {
    // The summary remains useful if the optional stats endpoint is unavailable.
  }
}

async function handleReset() {
  if (!props.providerId || resetting.value) return
  const confirmed = await confirm({
    title: legacyT('重置当前额度周期'),
    message: legacyT('将安排在下一整分钟生效，历史 usage 不会删除。确认继续吗？'),
    confirmText: legacyT('确认重置'),
    variant: 'warning',
  })
  if (!confirmed) return
  resetting.value = true
  try {
    const result = await resetProviderQuota(props.providerId)
    livePendingResetAt.value = result?.effective_at ?? null
    await loadQuotaStats()
    success(legacyT('额度周期重置已安排'))
  } catch (error) {
    showError(legacyT('额度周期重置失败'))
  } finally {
    resetting.value = false
  }
}

watch(() => props.providerId, () => {
  liveUsed.value = null
  liveWindows.value = null
  livePendingResetAt.value = null
  void loadQuotaStats()
}, { immediate: true })
</script>
