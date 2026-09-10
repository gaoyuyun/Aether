import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, reactive, type App } from 'vue'
import RoutingProfiles from '../RoutingProfiles.vue'
import { createEmptyRoutingGroupConfig, getModelScheduling, savePerModelRoutingConfig } from '@/features/routing/utils/routingPolicy'
import type { RoutingGroupRecord, RoutingGroupUpdateRequest } from '@/api/routing-profiles'

const routingApi = vi.hoisted(() => ({
  listRoutingGroups: vi.fn(),
  updateRoutingGroup: vi.fn(),
  createRoutingGroup: vi.fn(),
  deleteRoutingGroup: vi.fn(),
}))
const toast = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }))
const route = reactive({ name: 'RoutingProfileDetail', params: { groupId: 'strategy-a' } })

vi.mock('@/api/routing-profiles', () => routingApi)
vi.mock('@/api/global-models', () => ({ getGlobalModels: vi.fn().mockResolvedValue({ models: [] }) }))
vi.mock('@/composables/useToast', () => ({ useToast: () => toast }))
vi.mock('vue-router', () => ({ useRoute: () => route, useRouter: () => ({ replace: vi.fn(), push: vi.fn() }) }))
vi.mock('@/utils/logger', () => ({ log: { error: vi.fn(), warn: vi.fn() } }))
vi.mock('@/features/routing/components', async () => ({
  RoutingFailoverPolicyEditor: (await import('@/features/routing/components/RoutingFailoverPolicyEditor.vue')).default,
  RoutingPriorityPolicyEditor: { render: () => null },
}))

const mounted: Array<{ app: App, root: HTMLElement }> = []

function group(id: string, scope: 'unified' | 'per_model' = 'unified'): RoutingGroupRecord {
  const config = createEmptyRoutingGroupConfig()
  return {
    id,
    name: id,
    enabled: true,
    is_system_default: false,
    sort_order: 0,
    config_json: scope === 'per_model' ? savePerModelRoutingConfig(config, 'model-a') : config,
    version: 1,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
}

async function flush() {
  await nextTick()
  await new Promise(resolve => setTimeout(resolve, 0))
  await nextTick()
}

async function mountPage(groups = [group('strategy-a'), group('strategy-b')]) {
  routingApi.listRoutingGroups.mockResolvedValue({ items: groups, total: groups.length })
  routingApi.updateRoutingGroup.mockImplementation(async (id: string, payload: RoutingGroupUpdateRequest) => ({
    ...groups.find(entry => entry.id === id),
    ...payload,
    version: 2,
  }))
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(RoutingProfiles)
  app.mount(root)
  mounted.push({ app, root })
  await flush()
  return root
}

function element<T extends HTMLElement>(root: HTMLElement, selector: string): T {
  const found = root.querySelector<T>(selector)
  if (!found) throw new Error(`Missing element: ${selector}`)
  return found
}

function button(root: HTMLElement, label: string): HTMLButtonElement {
  return element(root, `button[aria-label="${label}"]`)
}

async function input(root: HTMLElement, label: string, value: string) {
  const field = element<HTMLInputElement | HTMLTextAreaElement>(root, `[aria-label="${label}"]`)
  field.value = value
  field.dispatchEvent(new Event('input', { bubbles: true }))
  await nextTick()
}

async function editJson(root: HTMLElement, section: string, value: string) {
  button(root, `切到${section} JSON`).click()
  await nextTick()
  await input(root, `${section} JSON`, value)
}

beforeEach(() => {
  vi.clearAllMocks()
  route.name = 'RoutingProfileDetail'
  route.params.groupId = 'strategy-a'
})

afterEach(() => {
  for (const { app, root } of mounted.splice(0)) {
    app.unmount()
    root.remove()
  }
})

describe('RoutingProfiles failover persistence', () => {
  it.each(['unified', 'per_model'] as const)('enables Save for JSON-only edits and persists both sections together (%s)', async (scope) => {
    const root = await mountPage([group('strategy-a', scope)])
    expect(button(root, '保存').disabled).toBe(true)
    await editJson(root, '成功转移规则', '[{"pattern":"(?i)capacity"}]')
    await editJson(root, '错误终止规则', '[{"status_codes":[400,413]}]')
    expect(button(root, '保存').disabled).toBe(false)
    button(root, '保存').click()
    await flush()
    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(1)
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy.failover_rules).toEqual({
      success_failover_patterns: [{ pattern: '(?i)capacity', status_codes: [] }],
      error_stop_patterns: [{ pattern: '', status_codes: [400, 413] }],
    })
    expect(toast.error).not.toHaveBeenCalled()
    expect(button(root, '保存').disabled).toBe(true)
  })

  it('does not submit partial JSON drafts when either section is invalid', async () => {
    const root = await mountPage()
    await editJson(root, '成功转移规则', '[{"pattern":"capacity"}]')
    await editJson(root, '错误终止规则', '{')
    button(root, '保存').click()
    await flush()
    expect(routingApi.updateRoutingGroup).not.toHaveBeenCalled()
    expect(root.querySelector('[role="alert"]')).not.toBeNull()
    await input(root, '错误终止规则 JSON', '[{"status_codes":[429]}]')
    button(root, '保存').click()
    await flush()
    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(1)
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy.failover_rules.success_failover_patterns).toHaveLength(1)
  })

  it('discards local rule drafts when navigating to another strategy', async () => {
    const root = await mountPage()
    await editJson(root, '成功转移规则', '[{"pattern":"only-strategy-a"}]')
    route.params.groupId = 'strategy-b'
    await flush()
    expect(root.querySelector('textarea[aria-label="成功转移规则 JSON"]')).toBeNull()
    expect(button(root, '保存').disabled).toBe(true)
    await input(root, '全局最大转移次数', '3')
    button(root, '保存').click()
    await flush()
    expect(routingApi.updateRoutingGroup.mock.calls[0][0]).toBe('strategy-b')
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy.failover_rules.success_failover_patterns).toEqual([])
  })

  it('preserves global system and failover edits while saving an independently edited model', async () => {
    const strategy = group('strategy-a')
    strategy.config_json = savePerModelRoutingConfig(strategy.config_json, 'model-a')
    const root = await mountPage([strategy])
    const configured = [...root.querySelectorAll<HTMLButtonElement>('button')].find(control => control.textContent?.trim() === '已配置')
    configured?.click()
    await nextTick()
    const model = [...root.querySelectorAll<HTMLButtonElement>('button')]
      .find(control => control.textContent?.replace(/\s+/g, '') === 'model-amodel-a')
    if (!model) throw new Error('Missing configured model')
    model.click()
    await nextTick()
    await input(root, '全局最大转移时间', '60')
    expect(button(root, '保存').disabled).toBe(false)
    expect(element<HTMLButtonElement>(root, 'button[title="保存到草稿"]').disabled).toBe(true)
    const loadBalance = [...root.querySelectorAll<HTMLButtonElement>('button')].find(control => control.textContent?.trim() === '负载均衡')
    if (!loadBalance) throw new Error('Missing model scheduling control')
    loadBalance.click()
    await nextTick()
    button(root, 'CF保持心跳').click()
    await nextTick()
    await input(root, '错误重试次数', '4')
    await input(root, '全局最大转移次数', '5')
    button(root, '添加错误终止规则').click()
    await nextTick()
    await input(root, '终止规则 1 状态码', '429')
    expect(button(root, '保存').disabled).toBe(true)
    element<HTMLButtonElement>(root, 'button[title="保存到草稿"]').click()
    await nextTick()
    expect(button(root, '保存').disabled).toBe(false)
    button(root, '保存').click()
    await flush()
    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(1)
    const saved = routingApi.updateRoutingGroup.mock.calls[0][1].config_json
    expect(saved.default_policy.enable_cf_heartbeat).toBe(true)
    expect(saved.default_policy.sticky_key_attempts).toBe(4)
    expect(saved.default_policy.max_transfer_count).toBe(5)
    expect(saved.default_policy.max_transfer_timeout_seconds).toBe(60)
    expect(saved.default_policy.failover_rules.error_stop_patterns).toEqual([{ pattern: '', status_codes: [429] }])
    expect(getModelScheduling(saved, 'model-a').scheduling_mode).toBe('load_balance')
    expect(toast.error).not.toHaveBeenCalled()
  })
})

describe.each(['unified', 'per_model'] as const)('RoutingProfiles global settings (%s)', (scope) => {
  it.each([
    { label: '格式转换保持优先级', field: 'keep_priority_on_conversion', value: true },
    { label: 'Cyber继续转移', field: 'cyber_continue_failover', value: true },
    { label: 'CF保持心跳', field: 'enable_cf_heartbeat', value: true },
    { label: '取消请求立即打断', field: 'cancel_on_client_disconnect', value: true },
    { label: '错误重试次数', field: 'sticky_key_attempts', value: 4 },
    { label: '全局最大转移次数', field: 'max_transfer_count', value: 5 },
    { label: '全局最大转移时间', field: 'max_transfer_timeout_seconds', value: 60 },
  ] as const)('saves $label without requiring a model save', async ({ label, field, value }) => {
    const strategy = group('strategy-a', scope)
    const root = await mountPage([strategy])
    const initialValue = strategy.config_json.default_policy[field]
    const setValue = async (nextValue: boolean | number) => {
      if (typeof nextValue === 'boolean') {
        button(root, label).click()
        await nextTick()
      } else {
        await input(root, label, String(nextValue))
      }
    }

    expect(button(root, '保存').disabled).toBe(true)
    await setValue(value)
    expect(button(root, '保存').disabled).toBe(false)
    expect(root.querySelector('button[title="保存到草稿"]')).toBeNull()

    await setValue(initialValue)
    expect(button(root, '保存').disabled).toBe(true)
    await setValue(value)
    button(root, '保存').click()
    await flush()

    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(1)
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy[field]).toBe(value)
    expect(button(root, '保存').disabled).toBe(true)
    expect(toast.error).not.toHaveBeenCalled()
  })

  it('saves system options together with added, edited and removed failover form rules', async () => {
    const root = await mountPage([group('strategy-a', scope)])
    button(root, '取消请求立即打断').click()
    await nextTick()
    button(root, '添加成功转移规则').click()
    await nextTick()
    await input(root, '成功转移规则 1 正则', '(?i)capacity')
    button(root, '添加错误终止规则').click()
    await nextTick()
    await input(root, '终止规则 1 状态码', '400, 413')
    expect(button(root, '保存').disabled).toBe(false)
    button(root, '保存').click()
    await flush()

    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(1)
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy.cancel_on_client_disconnect).toBe(true)
    expect(routingApi.updateRoutingGroup.mock.calls[0][1].config_json.default_policy.failover_rules).toEqual({
      success_failover_patterns: [{ pattern: '(?i)capacity', status_codes: [] }],
      error_stop_patterns: [{ pattern: '', status_codes: [400, 413] }],
    })
    expect(button(root, '保存').disabled).toBe(true)

    await input(root, '成功转移规则 1 正则', '(?i)overloaded')
    button(root, '删除错误终止规则 1').click()
    await nextTick()
    expect(button(root, '保存').disabled).toBe(false)
    button(root, '保存').click()
    await flush()

    expect(routingApi.updateRoutingGroup).toHaveBeenCalledTimes(2)
    expect(routingApi.updateRoutingGroup.mock.calls[1][1].config_json.default_policy.failover_rules).toEqual({
      success_failover_patterns: [{ pattern: '(?i)overloaded', status_codes: [] }],
      error_stop_patterns: [],
    })
    expect(button(root, '保存').disabled).toBe(true)
    expect(toast.error).not.toHaveBeenCalled()
  })
})
