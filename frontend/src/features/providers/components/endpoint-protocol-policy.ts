import { normalizeEndpointApiFormat } from './endpoint-default-paths'

export type FixedUpstreamStreamPolicy = 'force_stream' | 'force_non_stream'

export function isWebSocketEndpointApiFormat(apiFormat: string): boolean {
  const normalizedApiFormat = normalizeEndpointApiFormat(apiFormat)
  return normalizedApiFormat === 'openai:realtime' || normalizedApiFormat === 'codex:live'
}

export function fixedEndpointUpstreamStreamPolicy(
  providerType: string | null | undefined,
  apiFormat: string,
): FixedUpstreamStreamPolicy | null {
  const normalizedApiFormat = normalizeEndpointApiFormat(apiFormat)
  if (normalizedApiFormat === 'openai:search') return 'force_non_stream'
  if (providerType?.trim().toLowerCase() !== 'codex') return null

  switch (normalizedApiFormat) {
    case 'openai:responses':
      return 'force_stream'
    default:
      return null
  }
}
