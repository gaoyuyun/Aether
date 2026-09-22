import { ref, watch } from 'vue'

const CODEX_CLIENT_VERSION_STORAGE_KEY = 'aether:codex-client-version'

function readStoredVersion(): string {
  if (typeof window === 'undefined') return ''
  try {
    return window.localStorage.getItem(CODEX_CLIENT_VERSION_STORAGE_KEY)?.trim() || ''
  } catch {
    return ''
  }
}

const codexClientVersion = ref(readStoredVersion())

if (typeof window !== 'undefined') {
  watch(codexClientVersion, (value) => {
    try {
      const normalized = value.trim()
      if (normalized) {
        window.localStorage.setItem(CODEX_CLIENT_VERSION_STORAGE_KEY, normalized)
      } else {
        window.localStorage.removeItem(CODEX_CLIENT_VERSION_STORAGE_KEY)
      }
    } catch {
      // Storage can be unavailable in private browsing or restricted frames.
    }
  })
}

export function useCodexClientVersion() {
  return codexClientVersion
}
