import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const source = readFileSync(
  resolve(process.cwd(), 'src/features/providers/components/EndpointFormDialog.vue'),
  'utf8',
)

describe('EndpointFormDialog add endpoint format menu', () => {
  it('portals the menu outside the dialog scroll container', () => {
    const addEndpointSection = source.split('<!-- 添加新端点 -->')[1]

    expect(addEndpointSection).toBeDefined()
    expect(addEndpointSection).toContain('<SelectContent :disable-portal="false">')
  })
})
