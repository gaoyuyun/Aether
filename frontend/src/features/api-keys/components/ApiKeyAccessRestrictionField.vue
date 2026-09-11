<template>
  <div class="space-y-2">
    <div class="flex items-center justify-between gap-3">
      <Label class="text-sm font-medium">{{ legacyT(label) }}</Label>
      <div class="flex items-center gap-2">
        <span class="text-xs text-muted-foreground">{{ legacyT('跟随用户') }}</span>
        <Switch
          :model-value="unrestricted"
          :data-testid="testId ? `${testId}-unrestricted` : undefined"
          @update:model-value="$emit('update:unrestricted', $event)"
        />
      </div>
    </div>
    <Select
      :model-value="mode"
      :disabled="unrestricted || loading"
      @update:model-value="$emit('update:mode', $event === 'deny' ? 'deny' : 'allow')"
    >
      <SelectTrigger
        :data-testid="testId ? `${testId}-mode` : undefined"
        :aria-label="`${legacyT(label)}${legacyT('限制方式')}`"
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="allow">
          {{ legacyT('允许') }}
        </SelectItem>
        <SelectItem value="deny">
          {{ legacyT('拒绝') }}
        </SelectItem>
      </SelectContent>
    </Select>
    <MultiSelect
      :model-value="visibleValues"
      :options="options"
      :search-threshold="0"
      teleport
      :disabled="unrestricted || loading"
      :placeholder="legacyT(unrestricted ? '跟随用户权限' : mode === 'deny' ? '未选择（不排除任何项目）' : '未选择（全部禁用）')"
      :empty-text="legacyT('暂无可用选项')"
      :no-results-text="legacyT('未找到匹配项')"
      :search-placeholder="legacyT('搜索...')"
      :data-testid="testId"
      @update:model-value="$emit('update:modelValue', $event)"
    />
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Label, Select, SelectContent, SelectItem, SelectTrigger, SelectValue, Switch } from '@/components/ui'
import { MultiSelect } from '@/components/common'
import { useI18n } from '@/i18n'
import { retainAvailableAccessValues, type AccessRestrictionMode } from '@/features/api-keys/utils/userKeyPayload'

const props = defineProps<{
  label: string
  modelValue: string[]
  mode: AccessRestrictionMode
  unrestricted: boolean
  options: Array<{ value: string; label: string }>
  loading?: boolean
  testId?: string
}>()
defineEmits<{
  'update:modelValue': [value: string[]]
  'update:mode': [value: AccessRestrictionMode]
  'update:unrestricted': [value: boolean]
}>()
const { legacyT } = useI18n()
const visibleValues = computed(() => props.loading ? [] : retainAvailableAccessValues(props.modelValue, props.options))
</script>
