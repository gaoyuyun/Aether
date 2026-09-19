<template>
  <div class="flex items-center justify-between mb-1">
    <span class="text-[10px] text-muted-foreground">
      {{ title }}
    </span>
    <div class="flex items-center gap-1">
      <span
        v-if="updatedText"
        class="text-[9px] text-muted-foreground/70"
        data-testid="provider-quota-header-updated"
      >
        {{ updatedText }}
      </span>
      <Button
        v-if="refreshable"
        variant="ghost"
        size="icon"
        class="h-4 w-4 shrink-0 text-muted-foreground/70 hover:text-foreground"
        :disabled="loading"
        :title="refreshTitle || legacyT('刷新额度')"
        :aria-label="refreshTitle || legacyT('刷新额度')"
        data-testid="provider-quota-header-refresh"
        @click.stop="$emit('refresh')"
      >
        <RefreshCw
          class="w-2.5 h-2.5"
          :class="{ 'animate-spin': loading }"
        />
      </Button>
      <RefreshCw
        v-else-if="loading"
        class="w-3 h-3 text-muted-foreground/70 animate-spin"
        data-testid="provider-quota-header-loading"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { RefreshCw } from 'lucide-vue-next'
import Button from '@/components/ui/button.vue'
import { useI18n } from '@/i18n'

withDefaults(defineProps<{
  title: string
  loading?: boolean
  updatedText?: string | null
  refreshable?: boolean
  refreshTitle?: string | null
}>(), {
  loading: false,
  updatedText: null,
  refreshable: false,
  refreshTitle: null,
})

defineEmits<{
  (e: 'refresh'): void
}>()

const { legacyT } = useI18n()
</script>
