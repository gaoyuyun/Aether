import { describe, expect, it } from 'vitest'
import { createSSRApp, h } from 'vue'
import { renderToString } from '@vue/server-renderer'

import type { ProviderMappingPreviewResponse, ProviderWithEndpointsSummary } from '@/api/endpoints'
import ModelMappingTab from '../provider-tabs/ModelMappingTab.vue'

const provider: ProviderWithEndpointsSummary = {
  id: 'provider-demo',
  name: 'Demo Provider',
  provider_type: 'custom',
  is_active: true,
  active_keys: 0,
  api_formats: [],
  provider_priority: 0,
  keep_priority_on_conversion: false,
  enable_format_conversion: true,
  total_endpoints: 0,
  active_endpoints: 0,
  total_keys: 0,
  total_models: 0,
  active_models: 0,
  global_model_ids: [],
  avg_health_score: null,
  unhealthy_endpoints: 0,
  endpoint_health_details: [],
  ops_configured: false,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}

describe('ModelMappingTab response contracts', () => {
  it('keeps the module visible when a legacy or malformed preview reaches the component', async () => {
    const props: InstanceType<typeof ModelMappingTab>['$props'] = {
        provider,
        models: [],
        endpoints: [],
        providerKeys: [],
      loading: false,
    }
    Reflect.set(props, 'mappingPreview', {
          message: '演示模式：该接口暂未模拟',
          demo_mode: true,
    })
    const app = createSSRApp({
      render: () => h(ModelMappingTab, props),
    })

    const html = await renderToString(app)

    expect(html).toContain('模型映射')
    expect(html).toContain('暂无模型映射')
  })
})
