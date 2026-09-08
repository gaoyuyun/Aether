<template>
  <Dialog
    :model-value="internalOpen"
    :title="legacyT(isEditMode ? '编辑提供商' : '添加提供商')"
    :description="legacyT(isEditMode ? '更新提供商配置。API 端点和密钥需在详情页面单独管理。' : '创建新的提供商配置。创建后可以为其添加 API 端点和密钥。')"
    :icon="isEditMode ? SquarePen : Server"
    size="xl"
    @update:model-value="handleDialogUpdate"
  >
    <form
      class="space-y-5"
      @submit.prevent="handleSubmit"
    >
      <!-- 基本信息 -->
      <div class="space-y-3">
        <h3 class="text-sm font-medium border-b pb-2">
          {{ legacyT('基本信息') }}
        </h3>

        <div class="space-y-1.5">
          <Label for="name">{{ legacyT('名称 *') }}</Label>
          <Input
            id="name"
            v-model="form.name"
            :placeholder="legacyT('例如: OpenAI 主账号')"
          />
        </div>

        <div class="grid grid-cols-2 gap-4">
          <div class="space-y-1.5">
            <Label>{{ legacyT('提供商类型') }}</Label>
            <Select
              v-model="form.provider_type"
              :disabled="isEditMode"
            >
              <SelectTrigger>
                <SelectValue :placeholder="legacyT('请选择')" />
              </SelectTrigger>
              <SelectContent>
                <!-- 新建模式：允许自定义及各反代类型 -->
                <template v-if="!isEditMode">
                  <SelectItem value="custom">
                    {{ legacyT('自定义') }}
                  </SelectItem>
                  <SelectItem value="vertex_ai">
                    Vertex AI
                  </SelectItem>
                  <SelectItem value="claude_code">
                    {{ legacyT('Claude Code（实验性功能）') }}
                  </SelectItem>
                  <SelectItem value="codex">
                    Codex
                  </SelectItem>
                  <SelectItem value="chatgpt_web">
                    ChatGPT Web
                  </SelectItem>
                  <SelectItem value="gemini_cli">
                    Gemini CLI
                  </SelectItem>
                  <SelectItem value="grok">
                    Grok
                  </SelectItem>
                  <SelectItem value="kiro">
                    Kiro
                  </SelectItem>
                  <SelectItem value="windsurf">
                    Windsurf
                  </SelectItem>
                  <SelectItem value="antigravity">
                    Antigravity
                  </SelectItem>
                </template>
                <!-- 编辑模式：显示所有类型（兼容已有数据） -->
                <template v-else>
                  <SelectItem value="custom">
                    {{ legacyT('自定义') }}
                  </SelectItem>
                  <SelectItem value="vertex_ai">
                    Vertex AI
                  </SelectItem>
                  <SelectItem value="claude_code">
                    {{ legacyT('Claude Code（实验性功能）') }}
                  </SelectItem>
                  <SelectItem value="codex">
                    Codex
                  </SelectItem>
                  <SelectItem value="chatgpt_web">
                    ChatGPT Web
                  </SelectItem>
                  <SelectItem value="gemini_cli">
                    Gemini CLI
                  </SelectItem>
                  <SelectItem value="grok">
                    Grok
                  </SelectItem>
                  <SelectItem value="kiro">
                    Kiro
                  </SelectItem>
                  <SelectItem value="windsurf">
                    Windsurf
                  </SelectItem>
                  <SelectItem value="antigravity">
                    Antigravity
                  </SelectItem>
                </template>
              </SelectContent>
            </Select>
            <p
              v-if="!isEditMode && form.provider_type !== 'custom'"
              class="text-xs text-muted-foreground"
            >
              {{ legacyT('反代使用固定端点且不可修改') }}
            </p>
          </div>
          <div class="space-y-1.5">
            <Label for="website">{{ legacyT('主站链接') }}</Label>
            <Input
              id="website"
              v-model="form.website"
              :placeholder="legacyT('https://example.com（可选）')"
            />
          </div>
        </div>
      </div>

      <!-- 请求配置 -->
      <div class="space-y-3">
          <h3 class="text-sm font-medium border-b pb-2">
            {{ legacyT('请求配置') }}
          </h3>
        <div class="grid grid-cols-2 gap-4">
          <div class="space-y-1.5">
            <Label>{{ legacyT('上游成本模式') }}</Label>
            <Select
              v-model="form.billing_type"
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="monthly_quota">
                  {{ legacyT('月卡额度') }}
                </SelectItem>
                <SelectItem value="pay_as_you_go">
                  {{ legacyT('按量付费') }}
                </SelectItem>
                <SelectItem value="free_tier">
                  {{ legacyT('免费套餐') }}
                </SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div class="space-y-1.5">
            <Label>{{ legacyT('最大重试次数') }}</Label>
            <Input
              :model-value="form.max_retries ?? ''"
              type="number"
              min="0"
              max="999"
              :placeholder="legacyT('默认 2')"
              @update:model-value="(v) => form.max_retries = parseNumberInput(v)"
            />
          </div>
        </div>

        <!-- 超时配置 -->
        <div class="grid grid-cols-2 gap-4">
          <div class="space-y-1.5">
            <Label>
              {{ legacyT('流式首字节超时') }}
              <span class="text-xs text-muted-foreground">{{ legacyT('(秒)') }}</span>
            </Label>
            <Input
              :model-value="form.stream_first_byte_timeout ?? ''"
              type="number"
              min="1"
              max="300"
              step="1"
              placeholder="30"
              @update:model-value="(v) => form.stream_first_byte_timeout = parseNumberInput(v)"
            />
          </div>
          <div class="space-y-1.5">
            <Label>
              {{ legacyT('非流式请求超时') }}
              <span class="text-xs text-muted-foreground">{{ legacyT('(秒)') }}</span>
            </Label>
            <Input
              :model-value="form.request_timeout ?? ''"
              type="number"
              min="1"
              max="1200"
              step="1"
              placeholder="300"
              @update:model-value="(v) => form.request_timeout = parseNumberInput(v)"
            />
          </div>
        </div>

        <!-- 提供商内转移限制 -->
        <div class="grid grid-cols-2 gap-2 sm:gap-4">
          <div class="min-w-0 space-y-1.5">
            <Label
              for="max-transfer-count"
              class="whitespace-nowrap text-xs sm:text-sm"
            >
              {{ legacyT('最大转移次数') }}
            </Label>
            <Input
              id="max-transfer-count"
              :model-value="form.max_transfer_count === 0 ? '' : form.max_transfer_count"
              type="number"
              min="0"
              step="1"
              :placeholder="legacyT('0 (不限制)')"
              @update:model-value="(v) => form.max_transfer_count = parseNumberInput(v, { min: 0 }) ?? 0"
            />
          </div>
          <div class="min-w-0 space-y-1.5">
            <Label
              for="max-transfer-timeout-seconds"
              class="whitespace-nowrap text-xs sm:text-sm"
            >
              {{ legacyT('最大转移超时') }}
              <span class="text-xs text-muted-foreground">{{ legacyT('(秒)') }}</span>
            </Label>
            <Input
              id="max-transfer-timeout-seconds"
              :model-value="form.max_transfer_timeout_seconds === 0 ? '' : form.max_transfer_timeout_seconds"
              type="number"
              min="0"
              step="1"
              :placeholder="legacyT('0 (不限制)')"
              @update:model-value="(v) => form.max_transfer_timeout_seconds = parseNumberInput(v, { min: 0 }) ?? 0"
            />
          </div>
        </div>

        <!-- 月卡配置 -->
        <div
          v-if="form.billing_type === 'monthly_quota'"
          class="grid grid-cols-2 gap-4 p-3 border rounded-lg bg-muted/50"
        >
          <div class="space-y-1.5">
            <Label class="text-xs">{{ legacyT('周期额度 (USD)') }}</Label>
            <Input
              :model-value="form.monthly_quota_usd ?? ''"
              type="number"
              step="0.01"
              min="0"
              @update:model-value="(v) => form.monthly_quota_usd = parseNumberInput(v, { allowFloat: true })"
            />
          </div>
          <div class="space-y-1.5">
            <Label class="text-xs">{{ legacyT('总额周期 (天，1=日卡)') }}</Label>
            <Input
              :model-value="form.quota_reset_day ?? ''"
              :disabled="isEditMode && provider?.billing_type === 'monthly_quota'"
              type="number"
              min="1"
              max="30"
              @update:model-value="(v) => form.quota_reset_day = parseNumberInput(v) ?? 30"
            />
          </div>
          <div class="col-span-2 space-y-2">
            <div class="flex items-center justify-between">
              <Label class="text-xs">{{ legacyT('滚动窗口（分钟）') }}</Label>
              <Button
                type="button"
                variant="outline"
                size="sm"
                class="h-8"
                @click="addQuotaWindow"
              >
                + {{ legacyT('添加窗口') }}
              </Button>
            </div>
            <p class="text-xs text-muted-foreground">
              {{ legacyT('窗口时长必须是整分钟，范围为 1 分钟到 30 天；修改时保留当前已用额度。') }}
            </p>
            <div
              v-for="(window, index) in form.quota_windows"
              :key="index"
              class="grid grid-cols-[1fr_1fr_auto] items-end gap-2"
            >
              <div class="space-y-1">
                <Label class="text-xs">{{ legacyT('时长（分钟）') }}</Label>
                <Input
                  :model-value="Math.round(window.duration_secs / 60)"
                  type="number"
                  min="1"
                  max="43200"
                  step="1"
                  @update:model-value="(value) => updateQuotaWindowDuration(index, value)"
                />
              </div>
              <div class="space-y-1">
                <Label class="text-xs">{{ legacyT('窗口额度 (USD)') }}</Label>
                <Input
                  :model-value="window.limit_usd"
                  type="number"
                  min="0"
                  step="0.01"
                  @update:model-value="(value) => updateQuotaWindowLimit(index, value)"
                />
              </div>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                class="h-9 px-2"
                :aria-label="legacyT('删除窗口')"
                @click="removeQuotaWindow(index)"
              >
                ×
              </Button>
            </div>
          </div>
          <div class="space-y-1.5">
            <Label class="text-xs">
              {{ legacyT('订阅开始时间（精确到分）') }} <span class="text-red-500">*</span>
            </Label>
            <Input
              v-model="form.quota_subscription_started_at"
              type="datetime-local"
              step="60"
            />
          </div>
          <p class="text-xs text-muted-foreground">{{ legacyT('订阅开始时间是固定的开通记录。调整周期长度和当前起点，请使用详情中的“调整周期”。') }}</p>
          <div class="space-y-1.5">
            <Label class="text-xs">{{ legacyT('订阅到期时间') }}</Label>
            <Input
              id="quota-expires-at"
              v-model="form.quota_expires_at"
              type="datetime-local"
            />
          </div>
        </div>
      </div>

      <!-- 功能开关 -->
      <div class="space-y-3">
        <h3 class="text-sm font-medium border-b pb-2">
          {{ legacyT('功能开关') }}
        </h3>

        <div class="flex items-center justify-between p-3 border rounded-lg bg-muted/50">
          <div class="space-y-0.5">
            <span class="text-sm font-medium">{{ legacyT('号池调度模式') }}</span>
            <p class="text-xs text-muted-foreground">
              {{ legacyT('启用后该提供商的密钥将由号池统一调度') }}
            </p>
          </div>
          <Switch
            :model-value="form.pool_mode_enabled"
            @update:model-value="(v: boolean) => form.pool_mode_enabled = v"
          />
        </div>

        <div
          v-if="form.provider_type === 'kiro'"
          class="flex items-center justify-between p-3 border rounded-lg bg-muted/50"
        >
          <div class="space-y-0.5">
            <span class="text-sm font-medium">{{ legacyT('模拟缓存模式') }}</span>
            <p class="text-xs text-muted-foreground leading-relaxed">
              {{ legacyT('启用后仅对 Kiro 请求模拟 prompt cache 读写计量。') }}
            </p>
          </div>
          <Switch
            :model-value="form.kiro_simulated_cache_enabled"
            @update:model-value="(v: boolean) => form.kiro_simulated_cache_enabled = v"
          />
        </div>

        <div
          v-if="form.provider_type === 'codex'"
          class="flex items-center justify-between gap-4 p-3 border rounded-lg bg-muted/50"
          data-testid="codex-fingerprint-convergence-setting"
        >
          <div class="space-y-0.5">
            <Label
              for="codex-fingerprint-convergence"
              class="text-sm font-medium"
            >
              {{ legacyT('Codex 指纹收敛') }}
            </Label>
            <p class="text-xs text-muted-foreground leading-relaxed">
              {{ legacyT('统一同一 Codex 身份的设备与会话标识；关闭时保持现有透传行为。') }}
            </p>
          </div>
          <Switch
            id="codex-fingerprint-convergence"
            :model-value="form.codex_fingerprint_convergence_enabled"
            :aria-label="legacyT('Codex 指纹收敛')"
            @update:model-value="(v: boolean) => form.codex_fingerprint_convergence_enabled = v"
          />
        </div>

        <div
          class="flex items-center justify-between p-3 border rounded-lg bg-muted/50"
          data-testid="responses-websocket-setting"
        >
          <div class="space-y-0.5">
            <Label
              for="responses-websocket-enabled"
              class="text-sm font-medium"
            >
              {{ legacyT('Responses WebSocket 模式') }}
            </Label>
            <p class="text-xs text-muted-foreground leading-relaxed">
              {{ legacyT('允许此提供商处理标准 Responses API WebSocket 请求。仅在已验证兼容性后启用。') }}
            </p>
          </div>
          <Switch
            id="responses-websocket-enabled"
            :model-value="form.responses_websocket_enabled"
            :aria-label="legacyT('Responses WebSocket 模式')"
            @update:model-value="(v: boolean) => form.responses_websocket_enabled = v"
          />
        </div>

        <div class="flex items-center justify-between gap-4 p-3 border rounded-lg bg-muted/50">
          <div class="space-y-0.5">
            <span class="text-sm font-medium">{{ legacyT('敏感信息保护') }}</span>
            <p class="text-xs text-muted-foreground leading-relaxed">
              {{ legacyT('请前往模块管理-敏感信息保护中配置详细规则。') }}
            </p>
          </div>
        </div>
      </div>
    </form>

    <template #footer>
      <Button
        type="button"
        variant="outline"
        :disabled="loading"
        @click="handleCancel"
      >
        {{ legacyT('取消') }}
      </Button>
      <Button
        :disabled="loading || !form.name"
        @click="handleSubmit"
      >
        {{ submitLabel }}
      </Button>
    </template>
  </Dialog>
</template>

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import {
  Dialog,
  Button,
  Input,
  Label,
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
  Switch,
} from '@/components/ui'
import { Server, SquarePen } from 'lucide-vue-next'
import { useToast } from '@/composables/useToast'
import { useFormDialog } from '@/composables/useFormDialog'
import { useI18n } from '@/i18n'
import {
  createProvider,
  normalizePoolAdvancedConfig,
  updateProvider,
  type ProviderQuotaWindow,
  type ProviderType,
  type ProviderWithEndpointsSummary,
} from '@/api/endpoints'
import { parseApiError } from '@/utils/errorParser'
import { parseNumberInput } from '@/utils/form'
import { dateTimeLocalToRfc3339, formatDateTimeLocalInput } from '@/utils/date'

const props = defineProps<{
  modelValue: boolean
  provider?: ProviderWithEndpointsSummary | null  // 编辑模式时传入
  maxPriority?: number  // 当前已有的最大优先级值
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
  'providerCreated': []
  'providerUpdated': [provider: ProviderWithEndpointsSummary]
}>()

const { success, error: showError } = useToast()
const { legacyT } = useI18n()
const loading = ref(false)

// 内部状态
const internalOpen = computed(() => props.modelValue)

// 计算新建时的默认优先级
const defaultPriority = computed(() => {
  if (props.maxPriority != null) {
    return Math.min(props.maxPriority + 10, 10000)
  }
  return 100
})

const submitLabel = computed(() => {
  if (loading.value) {
    return legacyT(isEditMode.value ? '保存中...' : '创建中...')
  }
  return legacyT(isEditMode.value ? '保存' : '创建')
})

// 表单数据
const form = ref({
  name: '',
  provider_type: 'custom' as ProviderType,
  description: '',
  website: '',
  // 计费配置
  billing_type: 'pay_as_you_go' as 'monthly_quota' | 'pay_as_you_go' | 'free_tier',
  monthly_quota_usd: undefined as number | undefined,
  quota_reset_day: 30,
  quota_subscription_started_at: '',  // 订阅开始时间
  quota_expires_at: '',
  quota_windows: [] as ProviderQuotaWindow[],
  provider_priority: 100,
  // 状态配置
  is_active: true,
  rate_limit: undefined as number | undefined,
  concurrent_limit: undefined as number | undefined,
  // 请求配置
  max_retries: undefined as number | undefined,
  max_transfer_count: 0,
  max_transfer_timeout_seconds: 0,
  // 超时配置（秒）
  stream_first_byte_timeout: undefined as number | undefined,
  request_timeout: undefined as number | undefined,
  // 号池模式
  pool_mode_enabled: false,
  // Codex 专属配置
  codex_fingerprint_convergence_enabled: false,
  // Kiro 专属配置
  kiro_simulated_cache_enabled: false,
  // Responses WebSocket 配置
  responses_websocket_enabled: false,
})
const initialQuotaLastResetAt = ref<string | undefined>(undefined)

// 重置表单
function resetForm() {
  initialQuotaLastResetAt.value = undefined
  form.value = {
    name: '',
    provider_type: 'custom',
    description: '',
    website: '',
    billing_type: 'pay_as_you_go',
    monthly_quota_usd: undefined,
    quota_reset_day: 30,
    quota_subscription_started_at: '',
    quota_expires_at: '',
    quota_windows: [],
    provider_priority: defaultPriority.value,
    is_active: true,
    rate_limit: undefined,
    concurrent_limit: undefined,
    // 请求配置
    max_retries: undefined,
    max_transfer_count: 0,
    max_transfer_timeout_seconds: 0,
    // 超时配置
    stream_first_byte_timeout: undefined,
    request_timeout: undefined,
    // 号池模式
    pool_mode_enabled: false,
    // Codex 专属配置
    codex_fingerprint_convergence_enabled: false,
    // Kiro 专属配置
    kiro_simulated_cache_enabled: false,
    // Responses WebSocket 配置
    responses_websocket_enabled: false,
  }
}

// 加载提供商数据（编辑模式）
function loadProviderData() {
  if (!props.provider) return
  const poolAdvanced = normalizePoolAdvancedConfig(props.provider.pool_advanced)

  form.value = {
    name: props.provider.name,
    provider_type: props.provider.provider_type || 'custom',
    description: props.provider.description || '',
    website: props.provider.website || '',
    billing_type: (props.provider.billing_type as 'monthly_quota' | 'pay_as_you_go' | 'free_tier') || 'pay_as_you_go',
    monthly_quota_usd: props.provider.monthly_quota_usd ?? undefined,
    quota_reset_day: props.provider.quota_reset_day || 30,
    quota_subscription_started_at: formatDateTimeLocalInput(props.provider.quota_subscription_started_at ?? props.provider.quota_last_reset_at),
    quota_expires_at: formatDateTimeLocalInput(props.provider.quota_expires_at),
    quota_windows: (props.provider.quota_windows ?? []).map(window => ({ ...window })),
    provider_priority: props.provider.provider_priority || 999,
    is_active: props.provider.is_active,
    rate_limit: undefined,
    concurrent_limit: undefined,
    // 请求配置
    max_retries: props.provider.max_retries ?? undefined,
    max_transfer_count: props.provider.max_transfer_count ?? 0,
    max_transfer_timeout_seconds: props.provider.max_transfer_timeout_seconds ?? 0,
    // 超时配置
    stream_first_byte_timeout: props.provider.stream_first_byte_timeout ?? undefined,
    request_timeout: props.provider.request_timeout ?? undefined,
    // 号池模式
    pool_mode_enabled: poolAdvanced !== null,
    // Codex 专属配置
    codex_fingerprint_convergence_enabled: props.provider.codex_fingerprint_convergence_enabled ?? false,
    // Kiro 专属配置
    kiro_simulated_cache_enabled: props.provider.kiro_simulated_cache_enabled ?? false,
    // Responses WebSocket 配置
    responses_websocket_enabled: props.provider.responses_websocket_enabled ?? false,
  }
  initialQuotaLastResetAt.value = dateTimeLocalToRfc3339(form.value.quota_subscription_started_at)
}

function addQuotaWindow() {
  if (form.value.quota_windows.length >= 8) return
  form.value.quota_windows.push({ duration_secs: 86_400, limit_usd: 0 })
}

function removeQuotaWindow(index: number) {
  form.value.quota_windows.splice(index, 1)
}

function updateQuotaWindowDuration(index: number, value: string | number | null | undefined) {
  const minutes = parseNumberInput(value, { min: 1, max: 43_200 }) ?? 1
  form.value.quota_windows[index].duration_secs = Math.round(minutes) * 60
}

function updateQuotaWindowLimit(index: number, value: string | number | null | undefined) {
  form.value.quota_windows[index].limit_usd = parseNumberInput(value, { allowFloat: true, min: 0 }) ?? 0
}

// 使用 useFormDialog 统一处理对话框逻辑
const { isEditMode, handleDialogUpdate, handleCancel } = useFormDialog({
  isOpen: () => props.modelValue,
  entity: () => props.provider,
  isLoading: loading,
  onClose: () => emit('update:modelValue', false),
  loadData: loadProviderData,
  resetForm,
})

// 新建模式下切换 provider_type 时不自动开启号池模式
watch(() => form.value.provider_type, () => {
  if (!isEditMode.value) {
    form.value.pool_mode_enabled = false
  }
  if (form.value.provider_type !== 'kiro') {
    form.value.kiro_simulated_cache_enabled = false
  }
  if (form.value.provider_type !== 'codex') {
    form.value.codex_fingerprint_convergence_enabled = false
  }
})

// 提交表单
const handleSubmit = async () => {
  // 月卡类型必须设置订阅开始时间
  if (form.value.billing_type === 'monthly_quota' && !form.value.quota_subscription_started_at) {
    showError(legacyT('月卡类型必须设置订阅开始时间'), legacyT('验证失败'))
    return
  }

  const quotaLastResetAt = dateTimeLocalToRfc3339(form.value.quota_subscription_started_at)
  if (form.value.billing_type === 'monthly_quota' && !quotaLastResetAt) {
    showError(legacyT('订阅开始时间必须是合法时间'), legacyT('验证失败'))
    return
  }
  const quotaExpiresAt = dateTimeLocalToRfc3339(form.value.quota_expires_at)
  if (form.value.quota_expires_at && !quotaExpiresAt) {
    showError(legacyT('过期时间必须是合法时间'), legacyT('验证失败'))
    return
  }

  loading.value = true
  try {
    const currentPoolAdvanced = normalizePoolAdvancedConfig(props.provider?.pool_advanced)
    const quotaLastResetAtChanged = !isEditMode.value
      || quotaLastResetAt !== initialQuotaLastResetAt.value
    const basePayload = {
      name: form.value.name,
      provider_type: form.value.provider_type,
      description: form.value.description || undefined,
      website: form.value.website || undefined,
      billing_type: form.value.billing_type,
      monthly_quota_usd: form.value.monthly_quota_usd,
      quota_reset_day: form.value.quota_reset_day,
      ...(quotaLastResetAtChanged ? { quota_subscription_started_at: quotaLastResetAt } : {}),
      // 编辑时清空过期时间需显式发送 null，后端只有收到 null 才会清除已保存的值
      quota_expires_at: quotaExpiresAt ?? (isEditMode.value ? null : undefined),
      // Leave the saved window policy intact while a provider is temporarily pay-as-you-go;
      // switching back to a subscription can then resume the same policy.
      quota_windows: form.value.billing_type === 'monthly_quota'
        ? form.value.quota_windows.map(window => ({ ...window }))
        : undefined,
      responses_websocket_enabled: form.value.responses_websocket_enabled,
      is_active: form.value.is_active,
      // 请求配置
      max_retries: form.value.max_retries ?? undefined,
      max_transfer_count: form.value.max_transfer_count,
      max_transfer_timeout_seconds: form.value.max_transfer_timeout_seconds,
      // 超时配置（null 表示清除，使用全局配置）
      stream_first_byte_timeout: form.value.stream_first_byte_timeout ?? null,
      request_timeout: form.value.request_timeout ?? null,
      pool_advanced: form.value.pool_mode_enabled
        ? (currentPoolAdvanced ?? {})
        : null,
      ...(form.value.provider_type === 'codex'
        ? {
            codex_fingerprint_convergence_enabled: form.value.codex_fingerprint_convergence_enabled,
          }
        : {}),
      ...(form.value.provider_type === 'kiro'
        ? {
            config: {
              kiro: {
                simulated_cache_enabled: form.value.kiro_simulated_cache_enabled,
              },
            },
          }
        : {}),
    }

    if (isEditMode.value && props.provider) {
      // 更新提供商
      const updated = await updateProvider(props.provider.id, {
        ...basePayload,
        provider_priority: form.value.provider_priority,
      })
      success(legacyT('提供商更新成功'))
      emit('providerUpdated', updated)
    } else {
      // 创建提供商（优先级由后端自动置顶）
      await createProvider(basePayload)
      success(legacyT('提供商已创建，请继续添加端点和密钥，或在优先级管理中调整顺序'), legacyT('创建成功'))
      emit('providerCreated')
    }

    emit('update:modelValue', false)
  } catch (error: unknown) {
    const action = isEditMode.value ? '更新' : '创建'
    showError(parseApiError(error, legacyT(`${action}提供商失败`)), legacyT(`${action}失败`))
  } finally {
    loading.value = false
  }
}
</script>
