import client from '../client'
import { buildCacheKey, cachedRequest, dedupedRequest } from '@/utils/cache'
import { markAccessControlCatalogChanged } from '@/utils/accessControlCatalog'
import type {
  GlobalModelCreate,
  GlobalModelUpdate,
  GlobalModelResponse,
  GlobalModelWithStats,
  GlobalModelListResponse,
  ModelCatalogProviderDetail,
  ModelRoutingPreviewResponse,
} from './types'

// 重新导出路由相关类型供外部使用
export type {
  RoutingKeyInfo,
  RoutingEndpointInfo,
  RoutingModelMapping,
  RoutingProviderInfo,
  ModelRoutingPreviewResponse,
} from './types'

/**
 * 获取 GlobalModel 列表
 */
interface GlobalModelListOptions {
  cacheTtlMs?: number
}

export async function getGlobalModels(params?: {
  skip?: number
  limit?: number
  is_active?: boolean
  search?: string
}, options: GlobalModelListOptions = {}): Promise<GlobalModelListResponse> {
  const cacheTtlMs = options.cacheTtlMs ?? 0
  const key = buildCacheKey('global-models:list', params as Record<string, unknown> | undefined)
  return cachedRequest(
    key,
    async () => {
      const response = await client.get('/api/admin/models/global', { params })
      return response.data
    },
    cacheTtlMs,
  )
}

/**
 * 获取单个 GlobalModel 详情
 */
export async function getGlobalModel(id: string): Promise<GlobalModelWithStats> {
  return dedupedRequest(`global-models:detail:${id}`, async () => {
    const response = await client.get(`/api/admin/models/global/${id}`)
    return response.data
  })
}

/**
 * 创建 GlobalModel
 */
export async function createGlobalModel(data: GlobalModelCreate): Promise<GlobalModelResponse> {
  const response = await client.post('/api/admin/models/global', data)
  markAccessControlCatalogChanged()
  return response.data
}

/**
 * 更新 GlobalModel
 */
export async function updateGlobalModel(
  id: string,
  data: GlobalModelUpdate
): Promise<GlobalModelResponse> {
  const response = await client.patch(`/api/admin/models/global/${id}`, data)
  markAccessControlCatalogChanged()
  return response.data
}

/**
 * 删除 GlobalModel
 */
export async function deleteGlobalModel(
  id: string,
  force: boolean = false
): Promise<void> {
  await client.delete(`/api/admin/models/global/${id}`, { params: { force } })
  markAccessControlCatalogChanged()
}

/**
 * 批量删除 GlobalModel
 */
export async function batchDeleteGlobalModels(
  ids: string[]
): Promise<{ success_count: number; failed: Array<{ id: string; error: string }> }> {
  const response = await client.post('/api/admin/models/global/batch-delete', { ids })
  markAccessControlCatalogChanged()
  return response.data
}

/**
 * 批量为 GlobalModel 添加关联提供商
 */
export async function batchAssignToProviders(
  globalModelId: string,
  data: {
    provider_ids: string[]
    create_models: boolean
  }
): Promise<{
  success: Array<{
    provider_id: string
    provider_name: string
    model_id?: string
  }>
  errors: Array<{
    provider_id: string
    error: string
  }>
}> {
  const response = await client.post(
    `/api/admin/models/global/${globalModelId}/assign-to-providers`,
    data
  )
  return response.data
}

/**
 * 获取 GlobalModel 的所有关联提供商（包括非活跃的）
 */
export async function getGlobalModelProviders(globalModelId: string): Promise<{
  providers: ModelCatalogProviderDetail[]
  total: number
}> {
  return dedupedRequest(`global-models:providers:${globalModelId}`, async () => {
    const response = await client.get(
      `/api/admin/models/global/${globalModelId}/providers`
    )
    return response.data
  })
}

/**
 * 获取 GlobalModel 的请求链路预览
 */
export async function getGlobalModelRoutingPreview(
  globalModelId: string
): Promise<ModelRoutingPreviewResponse> {
  const response = await client.get(
    `/api/admin/models/global/${globalModelId}/routing`
  )
  return response.data
}
