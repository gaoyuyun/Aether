<template>
  <Dialog
    :model-value="isOpen"
    title="编辑账号"
    description="修改 OAuth 账号配置"
    :icon="SquarePen"
    size="xl"
    @update:model-value="handleDialogUpdate"
  >
    <form
      class="space-y-3"
      autocomplete="off"
      @submit.prevent="handleSave"
    >
      <!-- 基本信息：账号名称 + 备注 -->
      <div class="grid grid-cols-2 gap-3">
        <div>
          <Label for="name">账号名称 *</Label>
          <Input
            id="name"
            v-model="form.name"
            required
            placeholder="例如：主账号、备用账号"
            maxlength="100"
            autocomplete="off"
          />
        </div>
        <div>
          <Label for="note">备注</Label>
          <Input
            id="note"
            v-model="form.note"
            placeholder="可选的备注信息"
          />
        </div>
      </div>

      <!-- 配置项 -->
      <div class="grid grid-cols-4 gap-3">
        <div>
          <Label
            for="internal_priority"
            class="text-xs"
          >优先级</Label>
          <Input
            id="internal_priority"
            v-model.number="form.internal_priority"
            type="number"
            min="0"
            placeholder="10"
            class="h-8"
          />
          <p class="text-xs text-muted-foreground mt-0.5">
            越小越优先
          </p>
        </div>
        <div>
          <Label
            for="rpm_limit"
            class="text-xs"
          >RPM 限制</Label>
          <Input
            id="rpm_limit"
            :model-value="form.rpm_limit ?? ''"
            type="number"
            min="1"
            max="10000"
            placeholder="自适应"
            class="h-8"
            @update:model-value="(v) => form.rpm_limit = parseNullableNumberInput(v, { min: 1, max: 10000 })"
          />
          <p class="text-xs text-muted-foreground mt-0.5">
            留空自适应
          </p>
        </div>
        <div>
          <Label
            for="concurrent_limit"
            class="text-xs"
          >并发请求上限</Label>
          <Input
            id="concurrent_limit"
            :model-value="form.concurrent_limit ?? ''"
            type="number"
            min="0"
            placeholder="不限制"
            class="h-8"
            @update:model-value="(v) => form.concurrent_limit = parseNullableNumberInput(v, { min: 0 })"
          />
          <p class="text-xs text-muted-foreground mt-0.5">
            留空或 0 表示不限制
          </p>
        </div>
        <div>
          <Label
            for="cache_ttl_minutes"
            class="text-xs"
          >缓存 TTL</Label>
          <Input
            id="cache_ttl_minutes"
            :model-value="form.cache_ttl_minutes ?? ''"
            type="number"
            min="0"
            max="60"
            class="h-8"
            @update:model-value="(v) => form.cache_ttl_minutes = parseNumberInput(v, { min: 0, max: 60 }) ?? 5"
          />
          <p class="text-xs text-muted-foreground mt-0.5">
            分钟，0禁用
          </p>
        </div>
        <div>
          <Label
            for="max_probe_interval_minutes"
            class="text-xs"
          >熔断探测</Label>
          <Input
            id="max_probe_interval_minutes"
            :model-value="form.max_probe_interval_minutes ?? ''"
            type="number"
            min="0"
            max="32"
            placeholder="32"
            class="h-8"
            @update:model-value="(v) => form.max_probe_interval_minutes = parseNumberInput(v, { min: 0, max: 32 }) ?? 32"
          />
          <p class="text-xs text-muted-foreground mt-0.5">
            0-32分钟
          </p>
        </div>
      </div>

      <!-- 推理回放缓存（仅 Codex / Google 系渠道） -->
      <div
        v-if="showReasoningReplayAction"
        class="flex flex-col gap-3 p-3 rounded-lg border border-border/60 bg-muted/30 sm:flex-row sm:items-center sm:justify-between"
        data-testid="reasoning-replay-section"
      >
        <div class="min-w-0 flex-1 space-y-1">
          <Label class="text-sm font-medium">推理回放缓存</Label>
          <p class="text-xs text-muted-foreground leading-relaxed">
            持续提示推理签名无效时，可清除缓存。
          </p>
          <p
            v-if="reasoningReplayClearedCount !== null"
            class="text-xs text-muted-foreground"
          >
            已清除 {{ reasoningReplayClearedCount }} 条本机缓存
          </p>
        </div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          class="shrink-0 self-start whitespace-nowrap sm:self-auto"
          :disabled="clearingReasoningReplay"
          data-testid="clear-reasoning-replay"
          @click="handleClearReasoningReplay"
        >
          {{ clearingReasoningReplay ? '清除中...' : '清除缓存' }}
        </Button>
      </div>

      <!-- Claude Code 设备身份（仅 claude_code；只读摘要 + 重置） -->
      <div
        v-if="showClaudeCodeDeviceSection"
        class="flex flex-col gap-3 p-3 rounded-lg border border-border/60 bg-muted/30 sm:flex-row sm:items-center sm:justify-between"
        data-testid="claude-code-device-section"
      >
        <div class="min-w-0 flex-1 space-y-1">
          <Label class="text-sm font-medium">设备身份</Label>
          <p class="text-xs text-muted-foreground">
            第三方客户端经此 Key 上游时，网关以固定的设备标识与软件版本冒充原生 Claude Code；7 天内只升不降。
          </p>
          <p
            v-if="claudeCodeDeviceProfile"
            class="break-words text-xs text-muted-foreground font-mono"
            data-testid="claude-code-device-summary"
          >
            {{ claudeCodeDeviceProfile.device_id_prefix }}… · CLI {{ claudeCodeDeviceProfile.cli_version }} · SDK {{ claudeCodeDeviceProfile.package_version }} · Node {{ claudeCodeDeviceProfile.runtime_version }} · {{ claudeCodeDeviceProfile.os }}/{{ claudeCodeDeviceProfile.arch }}
          </p>
          <p
            v-else
            class="text-xs text-muted-foreground"
            data-testid="claude-code-device-summary"
          >
            尚未生成（首个第三方请求到达时派生）
          </p>
        </div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          class="shrink-0 self-start whitespace-nowrap sm:self-auto"
          :disabled="resettingClaudeCodeDevice || !claudeCodeDeviceProfile"
          data-testid="reset-claude-code-device"
          @click="handleResetClaudeCodeDevice"
        >
          {{ resettingClaudeCodeDevice ? '重置中...' : '重置设备身份' }}
        </Button>
      </div>

      <!-- 敏感词混淆（仅 claude_code / antigravity；Key 级覆盖供应商词表） -->
      <div
        v-if="showSensitiveWordsSection"
        class="space-y-2 p-3 rounded-lg border border-border/60 bg-muted/30"
        data-testid="sensitive-words-section"
      >
        <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div class="min-w-0 flex-1 space-y-1">
            <Label class="text-sm font-medium">敏感词混淆</Label>
            <p class="text-xs text-muted-foreground">
              默认继承供应商词表；选择「覆盖」后本 Key 只用这里的词表（留空即关闭混淆）。
            </p>
          </div>
          <div
            class="flex shrink-0 items-center gap-3 whitespace-nowrap text-xs"
            role="radiogroup"
            aria-label="敏感词来源"
          >
            <label class="inline-flex items-center gap-1 cursor-pointer">
              <input
                v-model="form.cloak_sensitive_words_mode"
                type="radio"
                value="inherit"
                data-testid="sensitive-words-inherit"
              >
              继承供应商
            </label>
            <label class="inline-flex items-center gap-1 cursor-pointer">
              <input
                v-model="form.cloak_sensitive_words_mode"
                type="radio"
                value="override"
                data-testid="sensitive-words-override"
              >
              覆盖
            </label>
          </div>
        </div>
        <template v-if="form.cloak_sensitive_words_mode === 'override'">
          <Textarea
            id="key-cloak-sensitive-words"
            :model-value="form.cloak_sensitive_words_text"
            class="min-h-[80px] font-mono text-sm"
            :class="{ 'border-destructive': sensitiveWordsError }"
            placeholder="proxy&#10;API"
            :aria-invalid="sensitiveWordsError ? 'true' : undefined"
            data-testid="sensitive-words-textarea"
            @update:model-value="(v: string) => form.cloak_sensitive_words_text = v"
          />
          <p
            v-if="sensitiveWordsError"
            class="text-xs text-destructive"
            data-testid="sensitive-words-error"
          >
            {{ sensitiveWordsError }}
          </p>
          <p
            v-else
            class="text-xs text-muted-foreground"
            data-testid="sensitive-words-summary"
          >
            每行一个词，不区分大小写；已配置 {{ sensitiveWordsParsed.words.length }} / {{ SENSITIVE_WORD_MAX_ENTRIES }}。词表变更会使提示词缓存失效。
          </p>
        </template>
      </div>

      <!-- 传输指纹 profile（P5，仅 claude_code / codex；Key 级覆盖供应商设置） -->
      <div
        v-if="showTransportProfileSection"
        class="space-y-3 p-3 rounded-lg border border-border/60 bg-muted/30"
        data-testid="transport-profile-section"
      >
        <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div class="min-w-0 flex-1 space-y-1">
            <Label
              for="key-transport-profile"
              class="text-sm font-medium"
            >传输指纹</Label>
            <p class="text-xs text-muted-foreground leading-relaxed">
              探测使用已保存的配置。
            </p>
          </div>
          <select
            id="key-transport-profile"
            v-model="form.transport_profile"
            class="h-9 w-full min-w-0 rounded-lg border border-border/60 bg-background px-3 text-sm focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 sm:w-64 sm:shrink-0"
            data-testid="transport-profile-select"
          >
            <option :value="null">
              继承供应商
            </option>
            <option
              v-for="option in transportProfileOptions"
              :key="option"
              :value="option"
            >
              {{ transportProfileLabel(option) }}
            </option>
          </select>
        </div>
        <div class="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 border-t border-border/40 pt-3">
          <p
            class="min-w-0 text-xs text-muted-foreground leading-relaxed"
            data-testid="tls-probe-summary"
            role="status"
          >
            <template v-if="tlsProbe">
              已探测
              <span v-if="tlsProbe.probed_at_unix_secs">· {{ formatProbedAt(tlsProbe.probed_at_unix_secs) }}</span>
            </template>
            <template v-else>
              尚未探测
            </template>
          </p>
          <div class="flex shrink-0 items-center gap-2">
            <Button
              v-if="tlsProbe"
              type="button"
              variant="ghost"
              size="sm"
              class="shrink-0 whitespace-nowrap text-xs text-muted-foreground"
              title="复制完整指纹"
              data-testid="copy-tls-fingerprint"
              @click="handleCopyTlsFingerprint"
            >
              复制结果
            </Button>
            <Button
              type="button"
              variant="outline"
              size="sm"
              class="shrink-0 whitespace-nowrap"
              :disabled="probingTls"
              data-testid="probe-tls-fingerprint"
              @click="handleProbeTlsFingerprint"
            >
              {{ probingTls ? '探测中...' : '探测指纹' }}
            </Button>
          </div>
        </div>
      </div>

      <!-- 自动获取模型 -->
      <div class="space-y-3 py-2 px-3 rounded-md border border-border/60 bg-muted/30">
        <div class="flex items-center justify-between">
          <div class="space-y-0.5">
            <Label class="text-sm font-medium">自动获取上游可用模型</Label>
            <p class="text-xs text-muted-foreground">
              定时更新上游模型, 配合模型映射使用
            </p>
            <p
              v-if="showAutoFetchWarning"
              class="text-xs text-amber-600 dark:text-amber-400"
            >
              {{ autoFetchWarningMessage }}
            </p>
          </div>
          <Switch v-model="form.auto_fetch_models" />
        </div>

        <!-- 模型过滤规则（仅当开启自动获取时显示） -->
        <div
          v-if="form.auto_fetch_models"
          class="space-y-2 pt-2 border-t border-border/40"
        >
          <div class="grid grid-cols-2 gap-3">
            <div>
              <Label class="text-xs">包含规则</Label>
              <Input
                v-model="form.model_include_patterns_text"
                placeholder="gpt-*, claude-*, 留空包含全部"
                class="h-8 text-sm"
              />
            </div>
            <div>
              <Label class="text-xs">排除规则</Label>
              <Input
                v-model="form.model_exclude_patterns_text"
                placeholder="*-preview, *-beta"
                class="h-8 text-sm"
              />
            </div>
          </div>
          <p class="text-xs text-muted-foreground">
            逗号分隔，支持 * ? 通配符，不区分大小写
          </p>
        </div>
      </div>
    </form>

    <template #footer>
      <Button
        variant="outline"
        @click="handleCancel"
      >
        取消
      </Button>
      <Button
        :disabled="saving || !canSave"
        @click="handleSave"
      >
        {{ saving ? '保存中...' : '保存' }}
      </Button>
    </template>
  </Dialog>
</template>

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { Dialog, Button, Input, Label, Switch, Textarea } from '@/components/ui'
import { SquarePen } from 'lucide-vue-next'
import { useToast } from '@/composables/useToast'
import { useClipboard } from '@/composables/useClipboard'
import { useConfirm } from '@/composables/useConfirm'
import { useFormDialog } from '@/composables/useFormDialog'
import { parseApiError } from '@/utils/errorParser'
import { parseNumberInput, parseNullableNumberInput } from '@/utils/form'
import {
  probeProviderKeyTlsFingerprint,
  resetProviderKeyClaudeCodeDevice,
  updateProviderKey,
  type EndpointAPIKey,
  type EndpointAPIKeyUpdate,
} from '@/api/endpoints'
import {
  extractProviderWriteWarnings,
  SENSITIVE_WORD_MAX_CHARS,
  SENSITIVE_WORD_MAX_ENTRIES,
  SENSITIVE_WORD_MIN_CHARS,
  normalizeSensitiveWordList,
  normalizeTlsProbeSummary,
  normalizeTransportProfile,
  parseSensitiveWordListText,
  providerTypeSupportsSensitiveWords,
  providerTypeSupportsTransportProfile,
  transportProfileLabel,
  transportProfileOptionsForProviderType,
  type ClaudeCodeDeviceProfileSummary,
  type TlsProbeSummary,
  type TransportProfileId,
} from '@/api/endpoints/types/provider'
import { clearReasoningReplayCache } from '@/api/endpoints/pool'

const props = defineProps<{
  open: boolean
  editingKey: EndpointAPIKey | null
  /** 所属渠道类型；只有会跨格式回放推理签名的渠道才显示「清除推理回放缓存」。 */
  providerType?: string | null
}>()

const emit = defineEmits<{
  close: []
  saved: [key: EndpointAPIKey]
}>()

const { success, error: showError, warning } = useToast()
const { confirmWarning } = useConfirm()
const { copyToClipboard } = useClipboard()

// 显示自动获取模型警告：编辑模式下，原本未启用但现在启用，且已有 allowed_models
const showAutoFetchWarning = computed(() => {
  if (!props.editingKey) return false
  // 原本已启用，不需要警告
  if (props.editingKey.auto_fetch_models) return false
  // 现在未启用，不需要警告
  if (!form.value.auto_fetch_models) return false
  // 检查是否有已配置的模型权限
  const allowedModels = props.editingKey.allowed_models
  if (!allowedModels) return false
  if (Array.isArray(allowedModels) && allowedModels.length === 0) return false
  if (typeof allowedModels === 'object' && Object.keys(allowedModels).length === 0) return false
  return true
})

const autoFetchWarningMessage = computed(() => {
  if (!showAutoFetchWarning.value || !props.editingKey?.allowed_models) return ''
  const models = Array.isArray(props.editingKey.allowed_models)
    ? props.editingKey.allowed_models
    : []
  if (models.length === 0) return ''
  return `当前 Key 模型权限存在以下模型：${models.map(model => `“${model}”`).join('、')}，开启自动获取后将被覆盖`
})

// 表单是否可以保存
const canSave = computed(() => {
  // 必须填写名称
  if (!form.value.name.trim()) return false
  if (showSensitiveWordsSection.value && sensitiveWordsError.value) return false
  return true
})

const isOpen = computed(() => props.open)
const saving = ref(false)

// 推理回放缓存：只有会跨格式回放签名的渠道类型才展示清理入口。
const REASONING_REPLAY_PROVIDER_TYPES = new Set(['codex', 'antigravity', 'gemini_cli', 'vertex_ai'])
const clearingReasoningReplay = ref(false)
const reasoningReplayClearedCount = ref<number | null>(null)
const showReasoningReplayAction = computed(() => {
  if (!props.editingKey) return false
  const providerType = String(props.providerType ?? '').trim().toLowerCase()
  return REASONING_REPLAY_PROVIDER_TYPES.has(providerType)
})

async function handleClearReasoningReplay() {
  if (!props.editingKey) return
  const confirmed = await confirmWarning(
    '清除后，该账号下所有会话缓存的推理签名都会丢失，下一轮跨格式工具调用将退回占位符。确定继续？',
    '清除推理回放缓存',
  )
  if (!confirmed) return
  clearingReasoningReplay.value = true
  try {
    const result = await clearReasoningReplayCache(props.editingKey.provider_id, props.editingKey.id)
    reasoningReplayClearedCount.value = result.cleared
    success(result.message, '成功')
  } catch (err: unknown) {
    showError(parseApiError(err, '清除失败'), '错误')
  } finally {
    clearingReasoningReplay.value = false
  }
}

// Claude Code 设备身份：只读摘要 + 重置（P2.7）。
const resettingClaudeCodeDevice = ref(false)
const claudeCodeDeviceProfileOverride = ref<ClaudeCodeDeviceProfileSummary | null | undefined>(undefined)
const showClaudeCodeDeviceSection = computed(() => {
  if (!props.editingKey) return false
  return String(props.providerType ?? '').trim().toLowerCase() === 'claude_code'
})
const claudeCodeDeviceProfile = computed<ClaudeCodeDeviceProfileSummary | null>(() => {
  if (claudeCodeDeviceProfileOverride.value !== undefined) return claudeCodeDeviceProfileOverride.value
  return props.editingKey?.claude_code_device_profile ?? null
})

async function handleResetClaudeCodeDevice() {
  if (!props.editingKey) return
  const confirmed = await confirmWarning(
    '重置后该 Key 会派生新的设备标识，上游会把后续请求视为一台新设备。确定继续？',
    '重置设备身份',
  )
  if (!confirmed) return
  resettingClaudeCodeDevice.value = true
  try {
    const result = await resetProviderKeyClaudeCodeDevice(props.editingKey.id)
    if (result.reset) {
      claudeCodeDeviceProfileOverride.value = null
    }
    success(result.message, '成功')
  } catch (err: unknown) {
    showError(parseApiError(err, '重置失败'), '错误')
  } finally {
    resettingClaudeCodeDevice.value = false
  }
}

// 敏感词混淆（P6 F2）：Key 级覆盖三态。inherit → 发 null（删除覆盖）；override → 发数组（空数组 = 关闭）。
type SensitiveWordsMode = 'inherit' | 'override'
const showSensitiveWordsSection = computed(() => {
  if (!props.editingKey) return false
  return providerTypeSupportsSensitiveWords(props.providerType)
})
const sensitiveWordsParsed = computed(() => parseSensitiveWordListText(form.value.cloak_sensitive_words_text))
const sensitiveWordsError = computed<string | null>(() => {
  if (form.value.cloak_sensitive_words_mode !== 'override') return null
  const parsed = sensitiveWordsParsed.value
  if (parsed.zeroWidth.length > 0) return `敏感词不能包含零宽字符：${parsed.zeroWidth.join('、')}`
  if (parsed.tooShort.length > 0) {
    return `以下敏感词过短，每个词至少 ${SENSITIVE_WORD_MIN_CHARS} 个字符：${parsed.tooShort.join('、')}`
  }
  if (parsed.tooLong.length > 0) {
    return `以下敏感词过长，每个词最多 ${SENSITIVE_WORD_MAX_CHARS} 个字符`
  }
  if (parsed.tooMany) return `敏感词最多 ${SENSITIVE_WORD_MAX_ENTRIES} 条，当前 ${parsed.words.length}`
  return null
})

// 传输指纹 profile（P5）：Key 级覆盖三态。null → 发 null（回到供应商 / 系统默认）；字符串 → 覆盖。
const showTransportProfileSection = computed(() => {
  if (!props.editingKey) return false
  return providerTypeSupportsTransportProfile(props.providerType)
})
const transportProfileOptions = computed(() => transportProfileOptionsForProviderType(props.providerType))
const probingTls = ref(false)
const tlsProbeOverride = ref<TlsProbeSummary | null | undefined>(undefined)
const tlsProbe = computed<TlsProbeSummary | null>(() => {
  if (tlsProbeOverride.value !== undefined) return tlsProbeOverride.value
  return normalizeTlsProbeSummary(props.editingKey?.tls_probe)
})

function formatProbedAt(unixSecs: number | null | undefined): string {
  if (!unixSecs) return '-'
  return new Date(unixSecs * 1000).toLocaleString()
}

async function handleCopyTlsFingerprint() {
  if (!tlsProbe.value) return
  await copyToClipboard(JSON.stringify(tlsProbe.value, null, 2))
}

async function handleProbeTlsFingerprint() {
  if (!props.editingKey) return
  probingTls.value = true
  try {
    const result = await probeProviderKeyTlsFingerprint(props.editingKey.id)
    tlsProbeOverride.value = normalizeTlsProbeSummary(result.probe)
    success(result.message, '成功')
  } catch (err: unknown) {
    showError(parseApiError(err, '探测失败'), '错误')
  } finally {
    probingTls.value = false
  }
}

const form = ref({
  name: '',
  internal_priority: 10,
  rpm_limit: undefined as number | null | undefined,
  concurrent_limit: undefined as number | null | undefined,
  cache_ttl_minutes: 5,
  max_probe_interval_minutes: 32,
  note: '',
  auto_fetch_models: false,
  model_include_patterns_text: '',
  model_exclude_patterns_text: '',
  cloak_sensitive_words_mode: 'inherit' as SensitiveWordsMode,
  cloak_sensitive_words_text: '',
  transport_profile: null as TransportProfileId | null,
})

// ---------------------------------------------------------------------------
// Dirty 状态跟踪：通过快照比较判断表单是否被修改
// ---------------------------------------------------------------------------
const formSnapshot = ref('')

function takeSnapshot() {
  formSnapshot.value = JSON.stringify(form.value)
}

const isDirty = computed(() => {
  if (!formSnapshot.value) return false
  return JSON.stringify(form.value) !== formSnapshot.value
})

// 对话框关闭时清除快照（快照在 loadKeyData 中拍摄）
watch(isOpen, (val) => {
  if (!val) {
    formSnapshot.value = ''
    claudeCodeDeviceProfileOverride.value = undefined
    tlsProbeOverride.value = undefined
  }
})

// 重置表单
function resetForm() {
  form.value = {
    name: '',
    internal_priority: 10,
    rpm_limit: undefined,
    concurrent_limit: undefined,
    cache_ttl_minutes: 5,
    max_probe_interval_minutes: 32,
    note: '',
    auto_fetch_models: false,
    model_include_patterns_text: '',
    model_exclude_patterns_text: '',
    cloak_sensitive_words_mode: 'inherit',
    cloak_sensitive_words_text: '',
    transport_profile: null,
  }
  formSnapshot.value = ''
}

// 加载密钥数据
function loadKeyData() {
  if (!props.editingKey) return
  form.value = {
    name: props.editingKey.name,
    internal_priority: props.editingKey.internal_priority ?? 10,
    rpm_limit: props.editingKey.rpm_limit ?? undefined,
    concurrent_limit: props.editingKey.concurrent_limit ?? undefined,
    cache_ttl_minutes: props.editingKey.cache_ttl_minutes ?? 5,
    max_probe_interval_minutes: props.editingKey.max_probe_interval_minutes ?? 32,
    note: props.editingKey.note || '',
    auto_fetch_models: props.editingKey.auto_fetch_models ?? false,
    model_include_patterns_text: (props.editingKey.model_include_patterns || []).join(', '),
    model_exclude_patterns_text: (props.editingKey.model_exclude_patterns || []).join(', '),
    cloak_sensitive_words_mode: Array.isArray(props.editingKey.cloak_sensitive_words) ? 'override' : 'inherit',
    cloak_sensitive_words_text: normalizeSensitiveWordList(props.editingKey.cloak_sensitive_words).join('\n'),
    transport_profile: normalizeTransportProfile(props.editingKey.transport_profile),
  }
  // 数据加载完成后拍快照，作为 dirty 判断的基准
  takeSnapshot()
}

// 使用 useFormDialog 统一处理对话框逻辑
const {
  handleDialogUpdate: _baseHandleDialogUpdate,
  handleCancel: _baseHandleCancel,
} = useFormDialog({
  isOpen: () => props.open,
  entity: () => props.editingKey,
  isLoading: saving,
  onClose: () => emit('close'),
  loadData: loadKeyData,
  resetForm,
})

// 包装关闭逻辑：有未保存更改时弹出确认
async function handleDialogUpdate(value: boolean) {
  if (!value && isDirty.value) {
    const confirmed = await confirmWarning('有未保存的更改，确定要关闭吗？', '放弃更改')
    if (!confirmed) return
  }
  _baseHandleDialogUpdate(value)
}

async function handleCancel() {
  if (isDirty.value) {
    const confirmed = await confirmWarning('有未保存的更改，确定要关闭吗？', '放弃更改')
    if (!confirmed) return
  }
  _baseHandleCancel()
}

// 将逗号分隔的文本解析为数组（去空、去重）
function parsePatternText(text: string): string[] {
  if (!text.trim()) return []
  const patterns = text
    .split(',')
    .map(s => s.trim())
    .filter(s => s.length > 0)
  return [...new Set(patterns)]
}

async function handleSave() {
  if (!props.editingKey) {
    showError('无法保存：缺少账号信息', '错误')
    return
  }

  saving.value = true
  try {
    const shouldClearAllowedModels = !!props.editingKey.auto_fetch_models && !form.value.auto_fetch_models
    const updateData: EndpointAPIKeyUpdate = {
      name: form.value.name,
      internal_priority: form.value.internal_priority,
      rpm_limit: form.value.rpm_limit,
      concurrent_limit: form.value.concurrent_limit,
      cache_ttl_minutes: form.value.cache_ttl_minutes,
      max_probe_interval_minutes: form.value.max_probe_interval_minutes,
      note: form.value.note,
      allowed_models: shouldClearAllowedModels ? null : undefined,
      auto_fetch_models: form.value.auto_fetch_models,
      model_include_patterns: parsePatternText(form.value.model_include_patterns_text),
      model_exclude_patterns: parsePatternText(form.value.model_exclude_patterns_text)
    }
    if (showSensitiveWordsSection.value) {
      if (sensitiveWordsError.value) {
        showError(sensitiveWordsError.value, '验证失败')
        return
      }
      // 三态：继承 → null（删除覆盖）；覆盖 → 数组（空数组 = 关闭混淆）
      updateData.cloak_sensitive_words = form.value.cloak_sensitive_words_mode === 'override'
        ? sensitiveWordsParsed.value.words
        : null
    }
    if (showTransportProfileSection.value) {
      // 三态：null → 回到供应商 / 系统默认；字符串 → Key 级覆盖
      updateData.transport_profile = form.value.transport_profile
    }

    const updatedKey = await updateProviderKey(props.editingKey.id, updateData)
    success('账号已更新', '成功')
    for (const message of extractProviderWriteWarnings(updatedKey)) warning(message, '提示')
    emit('saved', updatedKey)
    emit('close')
  } catch (err: unknown) {
    const errorMessage = parseApiError(err, '保存失败')
    showError(errorMessage, '错误')
  } finally {
    saving.value = false
  }
}
</script>
