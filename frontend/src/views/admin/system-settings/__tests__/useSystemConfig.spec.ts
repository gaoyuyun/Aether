import { beforeEach, describe, expect, it, vi } from 'vitest'

const { getAllSystemConfigsMock, updateSystemConfigMock } = vi.hoisted(() => ({
  getAllSystemConfigsMock: vi.fn(),
  updateSystemConfigMock: vi.fn(),
}))

vi.mock('@/api/admin', () => ({
  adminApi: {
    getAllSystemConfigs: getAllSystemConfigsMock,
    updateSystemConfig: updateSystemConfigMock,
    getSystemVersion: vi.fn(),
  },
}))

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({
    success: vi.fn(),
    error: vi.fn(),
  }),
}))

vi.mock('@/composables/useSiteInfo', () => ({
  useSiteInfo: () => ({
    refreshSiteInfo: vi.fn(),
  }),
}))

vi.mock('@/utils/logger', () => ({
  log: {
    error: vi.fn(),
  },
}))

import { useSystemConfig } from '../composables/useSystemConfig'

describe('useSystemConfig', () => {
  beforeEach(() => {
    getAllSystemConfigsMock.mockReset()
    updateSystemConfigMock.mockReset()
  })

  it('loads config keys in one request and keeps change detection disabled until the baseline is ready', async () => {
    let resolveConfigs: (value: Array<{ key: string, value: unknown, is_set?: boolean }>) => void = () => {}
    getAllSystemConfigsMock.mockImplementation(() => new Promise((resolve) => {
      resolveConfigs = resolve
    }))

    const state = useSystemConfig()
    const loadPromise = state.loadSystemConfig()

    expect(getAllSystemConfigsMock).toHaveBeenCalledTimes(1)
    expect(getAllSystemConfigsMock).toHaveBeenCalledWith({ cacheTtlMs: 30_000 })

    state.systemConfig.value.request_record_level = 'headers'
    expect(state.systemConfigLoading.value).toBe(true)
    expect(state.hasLogConfigChanges.value).toBe(false)

    resolveConfigs([
      { key: 'request_record_level', value: 'basic' },
      { key: 'proxy_node_metrics_cleanup_batch_size', value: 5000 },
    ])
    await loadPromise

    expect(state.systemConfigLoading.value).toBe(false)
    expect(state.systemConfig.value.request_record_level).toBe('basic')
    expect(state.hasLogConfigChanges.value).toBe(false)

    state.systemConfig.value.request_record_level = 'full'
    expect(state.hasLogConfigChanges.value).toBe(true)
  })

  it('loads and saves the standard text sync heartbeat flag as a basic config item', async () => {
    getAllSystemConfigsMock.mockResolvedValue([
      { key: 'enable_standard_text_sync_heartbeat', value: false },
    ])
    updateSystemConfigMock.mockResolvedValue({})

    const state = useSystemConfig()
    await state.loadSystemConfig()

    expect(state.systemConfig.value.enable_standard_text_sync_heartbeat).toBe(false)
    state.systemConfig.value.enable_standard_text_sync_heartbeat = true
    expect(state.hasBasicConfigChanges.value).toBe(true)

    await state.saveBasicConfig()

    expect(updateSystemConfigMock).toHaveBeenCalledWith(
      'enable_standard_text_sync_heartbeat',
      true,
      '标准文本非流式心跳开关：开启后外层 HTTP 状态固定为 200，上游失败写入响应体'
    )
    expect(state.hasBasicConfigChanges.value).toBe(false)
  })

  it('keeps Cyber failover disabled by default and saves the enabled state', async () => {
    getAllSystemConfigsMock.mockResolvedValue([])
    updateSystemConfigMock.mockResolvedValue({})

    const state = useSystemConfig()
    await state.loadSystemConfig()

    expect(state.systemConfig.value.cyber_continue_failover).toBe(false)
    state.systemConfig.value.cyber_continue_failover = true
    expect(state.hasBasicConfigChanges.value).toBe(true)

    await state.saveBasicConfig()

    expect(updateSystemConfigMock).toHaveBeenCalledWith(
      'cyber_continue_failover',
      true,
      'Cyber继续转移开关：开启后在响应内容开始前将Cyber Policy错误按普通错误继续故障转移，可能增加首字等待时间'
    )
    expect(state.hasBasicConfigChanges.value).toBe(false)
  })

  it('defaults provider visibility on and saves it with the basic settings', async () => {
    getAllSystemConfigsMock.mockResolvedValue([])
    updateSystemConfigMock.mockResolvedValue({})

    const state = useSystemConfig()
    await state.loadSystemConfig()

    expect(state.systemConfig.value.show_provider_in_user_usage).toBe(true)
    state.systemConfig.value.show_provider_in_user_usage = false
    expect(state.hasBasicConfigChanges.value).toBe(true)

    await state.saveBasicConfig()

    expect(updateSystemConfigMock).toHaveBeenCalledWith(
      'show_provider_in_user_usage',
      false,
      '是否允许普通用户在自己的使用记录中查看 Provider'
    )
    expect(state.hasBasicConfigChanges.value).toBe(false)
  })

  it('uses backend-compatible defaults when config rows have not been persisted yet', async () => {
    getAllSystemConfigsMock.mockResolvedValue([])

    const state = useSystemConfig()
    await state.loadSystemConfig()

    expect(state.systemConfig.value.request_record_level).toBe('basic')
    expect(state.systemConfig.value).not.toHaveProperty('max_request_body_size')
    expect(state.systemConfig.value).not.toHaveProperty('max_response_body_size')
  })

  it('loads and saves the extra trusted Fake-IP DNS hosts with proxy settings', async () => {
    getAllSystemConfigsMock.mockResolvedValue([
      { key: 'system_proxy_node_id', value: 'node-1' },
      { key: 'execution_extra_trusted_dns_hosts', value: ['custom.example.com'] },
    ])
    updateSystemConfigMock.mockResolvedValue(undefined)

    const state = useSystemConfig()
    await state.loadSystemConfig()

    expect(state.extraTrustedDnsHostsStr.value).toBe('custom.example.com')
    state.extraTrustedDnsHostsStr.value = 'api.example.com\nmodels.example.com'
    expect(state.systemConfig.value.execution_extra_trusted_dns_hosts).toEqual([
      'api.example.com',
      'models.example.com',
    ])

    await state.saveProxyConfig()

    expect(updateSystemConfigMock).toHaveBeenCalledWith(
      'execution_extra_trusted_dns_hosts',
      ['api.example.com', 'models.example.com'],
      '执行运行时额外可信 Fake-IP 域名'
    )
    expect(state.hasProxyConfigChanges.value).toBe(false)
  })
})
