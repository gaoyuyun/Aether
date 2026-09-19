import { ref, computed, watch } from 'vue'
import type { ProviderSummaryQuery } from '@/api/endpoints'
import { API_FORMAT_ORDER, formatApiFormat } from '@/api/endpoints/types/api-format'
import { useI18n } from '@/i18n'

export interface FilterOption {
  value: string
  label: string
}

export const PROVIDER_PAGE_SIZE_CACHE_KEY = 'provider-management-page-size'
const PROVIDER_PAGE_SIZE_OPTIONS = [10, 20, 50, 100]
const PROVIDER_DEFAULT_PAGE_SIZE = 20

/**
 * 分页组件挂载后会从 localStorage 恢复每页条数并触发 update:pageSize，
 * 这里先读同一份缓存作为初值，避免首屏因 pageSize 变化而重复加载列表与余额。
 */
function readCachedPageSize(): number {
  try {
    const cached = globalThis.localStorage?.getItem(PROVIDER_PAGE_SIZE_CACHE_KEY)
    const parsed = cached ? Number.parseInt(cached, 10) : Number.NaN
    return PROVIDER_PAGE_SIZE_OPTIONS.includes(parsed) ? parsed : PROVIDER_DEFAULT_PAGE_SIZE
  } catch {
    return PROVIDER_DEFAULT_PAGE_SIZE
  }
}

export function useProviderFilters(
  globalModels: () => { id: string; name: string }[],
) {
  const { legacyT } = useI18n()
  // 搜索与筛选
  const searchQuery = ref('')
  const filterStatus = ref('all')
  const filterApiFormat = ref('all')
  const filterModel = ref('all')

  const statusFilters = computed<FilterOption[]>(() => [
    { value: 'all', label: legacyT('全部状态') },
    { value: 'active', label: legacyT('活跃') },
    { value: 'inactive', label: legacyT('停用') },
  ])

  const apiFormatFilters = computed<FilterOption[]>(() => [
    { value: 'all', label: legacyT('全部格式') },
    ...API_FORMAT_ORDER.map(value => ({ value, label: formatApiFormat(value) })),
  ])

  const modelFilters = computed<FilterOption[]>(() => {
    const items = globalModels()
      .map(m => ({ value: m.id, label: m.name }))
      .sort((a, b) => a.label.localeCompare(b.label))
    return [{ value: 'all', label: legacyT('全部模型') }, ...items]
  })

  const hasActiveFilters = computed(() => {
    return (
      searchQuery.value !== '' ||
      filterStatus.value !== 'all' ||
      filterApiFormat.value !== 'all' ||
      filterModel.value !== 'all'
    )
  })

  // 分页
  const currentPage = ref(1)
  const pageSize = ref(readCachedPageSize())
  const total = ref(0)

  // 服务端分页查询参数
  const queryParams = computed<ProviderSummaryQuery>(() => ({
    page: currentPage.value,
    page_size: pageSize.value,
    search: searchQuery.value.trim() || undefined,
    status: filterStatus.value !== 'all' ? filterStatus.value : undefined,
    api_format: filterApiFormat.value !== 'all' ? filterApiFormat.value : undefined,
    model_id: filterModel.value !== 'all' ? filterModel.value : undefined,
  }))

  // 搜索/筛选变化时重置分页到第1页
  watch([searchQuery, filterStatus, filterApiFormat, filterModel], () => {
    currentPage.value = 1
  })

  function resetFilters() {
    searchQuery.value = ''
    filterStatus.value = 'all'
    filterApiFormat.value = 'all'
    filterModel.value = 'all'
  }

  return {
    searchQuery,
    filterStatus,
    filterApiFormat,
    filterModel,
    statusFilters,
    apiFormatFilters,
    modelFilters,
    hasActiveFilters,
    currentPage,
    pageSize,
    total,
    queryParams,
    resetFilters,
  }
}
