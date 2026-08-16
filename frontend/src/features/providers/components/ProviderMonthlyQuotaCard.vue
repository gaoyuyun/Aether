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
      <div
        v-if="windows.length > 0"
        class="space-y-1 border-t border-border/60 pt-2 text-xs text-muted-foreground"
        data-testid="provider-quota-windows"
      >
        <div
          v-for="window in windows"
          :key="`${window.duration_secs}-${window.limit_usd}`"
          class="flex items-center justify-between"
        >
          <span>{{ legacyT('窗口限额') }} · {{ formatWindowDuration(window.duration_secs) }}</span>
          <span class="tabular-nums">≤ ${{ window.limit_usd.toFixed(2) }}</span>
        </div>
      </div>
    </div>
  </Card>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import Badge from '@/components/ui/badge.vue'
import Card from '@/components/ui/card.vue'
import { useI18n } from '@/i18n'
import type { ProviderQuotaWindow } from '@/api/endpoints'

const props = withDefaults(defineProps<{
  used?: number | null
  quota?: number | null
  resetDay?: number | null
  windows?: ProviderQuotaWindow[]
}>(), {
  used: 0,
  quota: 0,
  resetDay: null,
  windows: () => [],
})

const { legacyT } = useI18n()

const used = computed(() => Number.isFinite(Number(props.used)) ? Number(props.used) : 0)
const quota = computed(() => Number.isFinite(Number(props.quota)) ? Number(props.quota) : 0)
const windows = computed(() => props.windows ?? [])
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
</script>
