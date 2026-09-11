import { ref } from 'vue'

const catalogRevision = ref(0)
const catalogStorageKey = 'aether-access-control-catalog-revision'

export function getAccessControlCatalogRevision(): number {
  return catalogRevision.value
}

export function markAccessControlCatalogChanged(): void {
  catalogRevision.value += 1
  if (typeof window !== 'undefined') {
    try { window.localStorage.setItem(catalogStorageKey, `${Date.now()}-${Math.random()}`) } catch { /* Storage may be disabled. */ }
  }
}

if (typeof window !== 'undefined') {
  window.addEventListener('storage', event => {
    if (event.key === catalogStorageKey) catalogRevision.value += 1
  })
}
