import { computed, ref } from 'vue'
import { meApi, type AvailableProvider } from '@/api/me'
import {
  getAccessControlCatalogRevision,
  markAccessControlCatalogChanged,
} from '@/utils/accessControlCatalog'

interface AccessControlModelOption {
  name: string
}

interface AccessControlApiFormatOption {
  value: string
  label: string
}

const providers = ref<AvailableProvider[]>([])
const globalModels = ref<AccessControlModelOption[]>([])
const apiFormats = ref<AccessControlApiFormatOption[]>([])
let accessControlOptionsGeneration = 0
let accessControlOptionsLoaded = false
let accessControlOptionsLoadedRevision = -1
let accessControlOptionsRequest: Promise<void> | null = null

export function invalidateUserAccessControlOptions(): void {
  accessControlOptionsGeneration += 1
  markAccessControlCatalogChanged()
  accessControlOptionsLoaded = false
  accessControlOptionsLoadedRevision = -1
  accessControlOptionsRequest = null
  providers.value = []
  globalModels.value = []
  apiFormats.value = []
}

export function useUserAccessControlOptions() {
  const providerOptions = computed(() =>
    providers.value
      .map((provider) => ({
        value: provider.id,
        label: provider.name?.trim() || provider.id,
      }))
      .sort((left, right) => left.label.localeCompare(right.label)),
  )
  const apiFormatOptions = computed(() =>
    apiFormats.value.map((format) => ({
      value: format.value,
      label: format.label,
    })),
  )
  const modelOptions = computed(() =>
    globalModels.value.map((model) => ({
      value: model.name,
      label: model.name,
    })),
  )

  async function loadAccessControlOptions(options: { force?: boolean } = {}): Promise<void> {
    if (accessControlOptionsRequest) return accessControlOptionsRequest
    if (
      !options.force
      && accessControlOptionsLoaded
      && accessControlOptionsLoadedRevision === getAccessControlCatalogRevision()
    ) return

    const requestGeneration = accessControlOptionsGeneration
    const request = (async () => {
      while (true) {
        const requestRevision = getAccessControlCatalogRevision()
        const [providerAccessOptions, modelsData] = await Promise.all([
          meApi.getAvailableProviders({ view: 'access-options' }),
          meApi.getAvailableModelOptions({ limit: 1000 }),
        ])
        const models = [...(modelsData.models || [])]
        while (models.length < modelsData.total) {
          const page = await meApi.getAvailableModelOptions({ limit: 1000, skip: models.length })
          if (page.models.length === 0) throw new Error('模型列表加载不完整，请重试')
          models.push(...page.models)
        }
        if (requestGeneration !== accessControlOptionsGeneration) throw new Error('访问限制选项已更新，请重试')
        // An upstream mutation during loading invalidates every page from that request.
        if (requestRevision !== getAccessControlCatalogRevision()) continue
        const formatValues = Array.from(new Set(
          providerAccessOptions.flatMap(provider => (provider.endpoints || [])
            .map(endpoint => String(endpoint.api_format || '').trim())
            .filter(Boolean)),
        )).sort()
        const modelNames = Array.from(new Set(models.map(model => model.name.trim()).filter(Boolean))).sort()
        providers.value = providerAccessOptions
        apiFormats.value = formatValues.map(value => ({ value, label: value }))
        globalModels.value = modelNames.map(name => ({ name }))
        accessControlOptionsLoaded = true
        accessControlOptionsLoadedRevision = requestRevision
        return
      }
    })()
    accessControlOptionsRequest = request
    try {
      await request
    } finally {
      if (accessControlOptionsRequest === request) {
        accessControlOptionsRequest = null
      }
    }
  }

  return {
    providers,
    globalModels,
    apiFormats,
    providerOptions,
    apiFormatOptions,
    modelOptions,
    loadAccessControlOptions,
  }
}
