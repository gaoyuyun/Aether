let catalogRevision = 0

export function getAccessControlCatalogRevision(): number {
  return catalogRevision
}

export function markAccessControlCatalogChanged(): void {
  catalogRevision += 1
}
