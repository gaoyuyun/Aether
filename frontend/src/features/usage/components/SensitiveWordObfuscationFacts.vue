<template>
  <div
    class="text-xs"
    data-testid="sensitive-words-obfuscation-facts"
  >
    <button
      type="button"
      class="inline-flex items-center gap-1.5 rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 font-medium text-amber-700 transition-colors hover:bg-amber-500/20 dark:text-amber-300"
      :aria-expanded="expanded"
      :title="expanded ? '收起字段路径' : '点击查看被混淆的字段路径'"
      data-testid="sensitive-words-obfuscation-badge"
      @click="expanded = !expanded"
    >
      <EyeOff class="h-3 w-3" />
      <span>已应用敏感词混淆 {{ report.replaced }} 处</span>
      <ChevronRight
        class="h-3 w-3 transition-transform"
        :class="{ 'rotate-90': expanded }"
      />
    </button>
    <ul
      v-if="expanded"
      class="mt-1.5 space-y-0.5 font-mono text-[11px] text-muted-foreground"
      data-testid="sensitive-words-obfuscation-fields"
    >
      <li
        v-for="field in report.fields"
        :key="field"
      >
        {{ field }}
      </li>
      <li
        v-if="report.fields.length === 0"
        class="italic"
      >
        未记录字段路径
      </li>
    </ul>
    <p
      v-if="expanded"
      class="mt-1 text-[11px] text-muted-foreground"
    >
      请求体里的 U+200B 会显示为 {{ ZERO_WIDTH_PLACEHOLDER }}；复制正文或 cURL 时默认保留原始字节。
    </p>
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { ChevronRight, EyeOff } from 'lucide-vue-next'
import { ZERO_WIDTH_PLACEHOLDER, type SensitiveWordObfuscationReport } from '../utils/zeroWidth'

defineProps<{
  report: SensitiveWordObfuscationReport
}>()

const expanded = ref(false)
</script>
