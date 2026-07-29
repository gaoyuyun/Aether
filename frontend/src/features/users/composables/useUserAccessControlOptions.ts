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
let accessControlOptionsLoaded = false
let accessControlOptionsLoadedRevision = -1
let accessControlOptionsRequest: Promise<void> | null = null

export function invalidateUserAccessControlOptions(): void {
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

    const requestRevision = getAccessControlCatalogRevision()
    const request = (async () => {
      const [providerAccessOptions, modelsData] = await Promise.all([
        meApi.getAvailableProviders({ view: 'access-options' }),
        meApi.getAvailableModelOptions({ limit: 1000 }),
      ])
      const formatValues = Array.from(new Set(
        providerAccessOptions.flatMap(provider => (provider.endpoints || [])
          .map(endpoint => String(endpoint.api_format || '').trim())
          .filter(Boolean)),
      )).sort()
      const modelNames = Array.from(new Set(
        (modelsData.models || [])
          .map(model => model.name.trim())
          .filter(Boolean),
      )).sort()

      providers.value = providerAccessOptions
      apiFormats.value = formatValues.map(value => ({ value, label: value }))
      globalModels.value = modelNames.map(name => ({ name }))
      accessControlOptionsLoaded = true
      accessControlOptionsLoadedRevision = requestRevision
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
