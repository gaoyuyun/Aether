<template>
  <div class="flex flex-col gap-2">
    <template
      v-for="(block, index) in blocks"
      :key="index"
    >
      <!-- 文本块 -->
      <pre
        v-if="block.type === 'text'"
        class="m-0 text-sm leading-relaxed whitespace-pre-wrap break-words"
        :class="block.className"
      ><ZeroWidthText :text="block.content" /></pre>

      <!-- 可折叠块 -->
      <details
        v-else-if="block.type === 'collapsible'"
        class="group cursor-pointer bg-muted/30 rounded-lg px-3 py-2"
        :class="block.className"
        :open="block.defaultOpen"
      >
        <summary class="flex items-center gap-2 list-none select-none [&::-webkit-details-marker]:hidden">
          <ChevronRight class="w-4 h-4 text-muted-foreground transition-transform duration-200 group-open:rotate-90" />
          <span class="text-xs text-muted-foreground">{{ block.title }}</span>
        </summary>
        <div class="mt-2 p-3 bg-muted/50 rounded-lg">
          <BlockRenderer :blocks="block.content" />
        </div>
      </details>

      <!-- 代码块 -->
      <div
        v-else-if="block.type === 'code'"
        class="bg-muted/50 rounded-lg overflow-hidden"
      >
        <div
          v-if="block.language"
          class="px-3 py-2 text-xs font-medium bg-muted/50 text-muted-foreground border-b border-border"
        >
          {{ block.language }}
        </div>
        <pre
          class="m-0 p-3 font-mono text-xs max-h-[300px] overflow-auto whitespace-pre-wrap break-words"
          :style="block.maxHeight ? { maxHeight: `${block.maxHeight}px` } : {}"
        ><ZeroWidthText :text="block.code" /></pre>
      </div>

      <!-- 徽章块 -->
      <Badge
        v-else-if="block.type === 'badge'"
        :variant="block.variant || 'secondary'"
        class="w-fit"
      >
        {{ block.label }}
      </Badge>

      <!-- 图片块 -->
      <div
        v-else-if="block.type === 'image'"
        class="bg-muted/30 p-3 rounded-lg"
      >
        <img
          v-if="block.src"
          :src="block.src"
          :alt="block.alt || '图片'"
          class="max-w-full max-h-[400px] rounded-lg"
        >
        <div
          v-else
          class="flex items-center gap-2 text-muted-foreground text-xs"
        >
          <ImageIcon class="w-4 h-4" />
          <span>{{ block.mimeType || block.alt || '图片' }}</span>
        </div>
      </div>

      <!-- 错误块 -->
      <div
        v-else-if="block.type === 'error'"
        class="flex items-center gap-2 p-3 bg-destructive/10 border border-destructive/20 rounded-lg text-destructive text-sm"
      >
        <AlertCircle class="w-4 h-4 shrink-0" />
        <span>{{ block.message }}</span>
        <span
          v-if="block.code"
          class="font-mono text-xs opacity-80"
        >{{ block.code }}</span>
      </div>

      <!-- 容器块 -->
      <div
        v-else-if="block.type === 'container'"
        class="flex flex-col gap-2"
        :class="block.className"
      >
        <div
          v-if="block.header"
          class="flex items-center gap-2"
        >
          <BlockRenderer :blocks="block.header" />
        </div>
        <BlockRenderer :blocks="block.children" />
      </div>

      <!-- 消息块 -->
      <div
        v-else-if="block.type === 'message'"
        class="rounded-lg overflow-hidden"
        :class="messageBlockClasses[block.role] || messageBlockClasses.default"
      >
        <div
          class="flex items-center gap-2 px-3 py-2 text-xs font-medium"
          :class="messageHeaderClasses[block.role] || messageHeaderClasses.default"
        >
          <component
            :is="getRoleIcon(block.role)"
            class="w-4 h-4"
          />
          <span>{{ block.roleLabel || getRoleLabel(block.role) }}</span>
          <template v-if="block.badges">
            <Badge
              v-for="(badge, badgeIndex) in block.badges"
              :key="badgeIndex"
              :variant="badge.variant || 'secondary'"
            >
              {{ badge.label }}
            </Badge>
          </template>
        </div>
        <div class="px-3 pb-3 text-sm leading-relaxed">
          <BlockRenderer :blocks="block.content" />
        </div>
      </div>

      <!-- 工具调用块 -->
      <div
        v-else-if="block.type === 'tool_use'"
        class="bg-muted/30 rounded-lg overflow-hidden"
      >
        <div class="flex items-center gap-2 px-3 py-2 text-xs font-medium text-muted-foreground">
          <Wrench class="w-4 h-4" />
          <span>{{ block.toolName }}</span>
          <span
            v-if="block.toolId"
            class="font-mono text-xs opacity-60"
          >{{ block.toolId }}</span>
        </div>
        <pre class="m-0 p-3 bg-muted/50 font-mono text-xs max-h-[200px] overflow-y-auto whitespace-pre-wrap break-words"><ZeroWidthText :text="block.input" /></pre>
      </div>

      <!-- 工具结果块 -->
      <div
        v-else-if="block.type === 'tool_result'"
        class="rounded-lg overflow-hidden"
        :class="block.isError ? 'bg-destructive/10 border border-destructive/20' : 'bg-muted/30'"
      >
        <div class="flex items-center gap-2 px-3 py-2 text-xs font-medium text-muted-foreground">
          <FileText class="w-4 h-4" />
          <span>工具结果</span>
          <Badge
            v-if="block.isError"
            variant="destructive"
          >
            错误
          </Badge>
        </div>
        <pre class="m-0 p-3 bg-muted/50 font-mono text-xs max-h-[200px] overflow-y-auto whitespace-pre-wrap break-words"><ZeroWidthText :text="block.content" /></pre>
      </div>

      <!-- 分隔符块 -->
      <hr
        v-else-if="block.type === 'divider'"
        class="border-0 border-t border-border my-2"
      >

      <!-- 标签块 -->
      <div
        v-else-if="block.type === 'label'"
        class="flex gap-2 text-sm"
      >
        <span class="text-muted-foreground">{{ block.label }}:</span>
        <span :class="block.mono ? 'font-mono' : ''"><ZeroWidthText :text="block.value" /></span>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
import { User, Bot, Settings, Wrench, AlertCircle, ChevronRight, FileText, Image as ImageIcon } from 'lucide-vue-next'
import Badge from '@/components/ui/badge.vue'
import ZeroWidthText from './ZeroWidthText.vue'
import type { RenderBlock } from '../../conversation'

defineProps<{
  blocks: RenderBlock[]
}>()

const messageBlockClasses: Record<string, string> = {
  user: 'bg-primary/[0.08] border border-primary/20',
  assistant: 'bg-muted/50 border border-border',
  system: 'bg-muted/30 border border-dashed border-border',
  tool: 'bg-muted/30 border border-border',
  default: 'bg-muted/30 border border-border',
}

const messageHeaderClasses: Record<string, string> = {
  user: 'text-primary',
  assistant: 'text-foreground',
  system: 'text-muted-foreground',
  tool: 'text-muted-foreground',
  default: 'text-foreground',
}

const getRoleIcon = (role: string) => {
  switch (role) {
    case 'user': return User
    case 'assistant': return Bot
    case 'system': return Settings
    case 'tool': return Wrench
    default: return User
  }
}

const getRoleLabel = (role: string) => {
  switch (role) {
    case 'user': return 'User'
    case 'assistant': return 'Assistant'
    case 'system': return 'System'
    case 'tool': return 'Tool'
    default: return role
  }
}
</script>
