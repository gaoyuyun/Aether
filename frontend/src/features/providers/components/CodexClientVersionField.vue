<template>
  <div
    v-if="isCodexProvider"
    class="space-y-1.5"
  >
    <Label
      :for="inputId"
      class="text-xs"
    >
      Codex 客户端版本（可选）
    </Label>
    <Input
      :id="inputId"
      :model-value="modelValue"
      :disabled="disabled"
      :maxlength="64"
      :aria-describedby="`${inputId}-help`"
      aria-label="Codex 客户端版本"
      placeholder="留空自动选择，例如 0.155.1"
      class="h-9 font-mono"
      autocomplete="off"
      spellcheck="false"
      @update:model-value="emit('update:modelValue', $event)"
    />
    <p
      :id="`${inputId}-help`"
      class="text-xs text-muted-foreground"
    >
      留空使用后台已知版本。设置保存在当前浏览器，用于下次获取模型。
    </p>
  </div>
</template>

<script setup lang="ts">
import { computed, useId } from 'vue'
import { Input, Label } from '@/components/ui'

const props = defineProps<{
  providerType?: string | null
  modelValue: string
  disabled?: boolean
}>()

const emit = defineEmits<{
  'update:modelValue': [value: string]
}>()

const inputId = useId()

const isCodexProvider = computed(() =>
  props.providerType?.trim().toLowerCase() === 'codex',
)
</script>
