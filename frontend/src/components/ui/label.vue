<template>
  <label :class="labelClass">
    <slot />
  </label>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { cn } from '@/lib/utils'

interface Props {
  /** Tailwind/Vue class binding accepts strings, arrays, and objects. */
  class?: any
}

const props = defineProps<Props>()

const labelClass = computed(() =>
  cn(
    'text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground peer-disabled:cursor-not-allowed peer-disabled:opacity-70',
    props.class
  )
)
</script>
