<template>
  <div
    v-if="isCodexProvider"
    class="space-y-1.5"
  >
    <Label
      for="codex-client-version"
      class="text-xs"
    >
      Codex 客户端版本（可选）
    </Label>
    <Input
      id="codex-client-version"
      :model-value="modelValue"
      placeholder="留空自动选择，例如 0.155.1"
      class="h-9 font-mono"
      autocomplete="off"
      spellcheck="false"
      @update:model-value="emit('update:modelValue', $event)"
    />
    <p class="text-xs text-muted-foreground">
      用于上游 Codex 模型目录查询；填写后点击刷新。
    </p>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Input, Label } from '@/components/ui'

const props = defineProps<{
  providerType?: string | null
  modelValue: string
}>()

const emit = defineEmits<{
  'update:modelValue': [value: string]
}>()

const isCodexProvider = computed(() =>
  props.providerType?.trim().toLowerCase() === 'codex',
)
</script>
