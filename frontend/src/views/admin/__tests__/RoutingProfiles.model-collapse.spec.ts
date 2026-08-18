import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, nextTick, type App } from 'vue'

import {
  createEmptyModelPolicy,
  createEmptyRoutingGroupConfig,
  upsertModelPolicy,
} from '@/features/routing/utils/routingPolicy'
import RoutingProfiles from '../RoutingProfiles.vue'

const apiMocks = vi.hoisted(() => ({
  listRoutingGroups: vi.fn(),
  getGlobalModels: vi.fn(),
}))

const routerMocks = vi.hoisted(() => ({
  route: {
    name: 'RoutingProfileDetail',
    params: { groupId: 'group-a' },
  },
}))

vi.mock('vue-router', () => ({
  useRoute: () => routerMocks.route,
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
}))

vi.mock('@/api/routing-profiles', () => ({
  listRoutingGroups: apiMocks.listRoutingGroups,
  createRoutingGroup: vi.fn(),
  updateRoutingGroup: vi.fn(),
  deleteRoutingGroup: vi.fn(),
}))

vi.mock('@/api/global-models', () => ({
  getGlobalModels: apiMocks.getGlobalModels,
}))

vi.mock('@/composables/useToast', () => ({
  useToast: () => ({ success: vi.fn(), error: vi.fn() }),
}))

vi.mock('@/utils/logger', () => ({ log: { error: vi.fn() } }))

vi.mock('@/components/layout', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    PageContainer: defineComponent({
      setup(_, { slots }) {
        return () => h('main', slots.default?.())
      },
    }),
  }
})

vi.mock('@/components/ui', async () => {
  const { defineComponent, h } = await import('vue')
  const component = (tag: string) => defineComponent({
    inheritAttrs: false,
    setup(_, { attrs, slots }) {
      return () => h(tag, attrs, slots.default?.())
    },
  })
  return {
    Badge: component('span'),
    Button: component('button'),
    Card: component('section'),
    Input: component('input'),
    Textarea: component('textarea'),
    Table: component('table'),
    TableBody: component('tbody'),
    TableCell: component('td'),
    TableHead: component('th'),
    TableHeader: component('thead'),
    TableRow: component('tr'),
    TableCard: component('section'),
  }
})

vi.mock('@/components/ui/dropdown-menu', async () => {
  const { defineComponent, h } = await import('vue')
  const component = (tag: string) => defineComponent({
    setup(_, { slots }) {
      return () => h(tag, slots.default?.())
    },
  })
  return {
    DropdownMenu: component('div'),
    DropdownMenuTrigger: component('div'),
    DropdownMenuContent: component('div'),
    DropdownMenuItem: component('button'),
  }
})

vi.mock('@/components/common', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    AlertDialog: defineComponent({
      setup(_, { slots }) {
        return () => h('div', slots.default?.())
      },
    }),
  }
})

vi.mock('@/features/routing/components', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    RoutingPriorityPolicyEditor: defineComponent({
      setup() {
        return () => h('div', { 'data-testid': 'model-policy-editor' })
      },
    }),
  }
})

let app: App | undefined
let root: HTMLElement | undefined

function modelConfig(model: string) {
  return upsertModelPolicy(createEmptyRoutingGroupConfig(), createEmptyModelPolicy(model))
}

async function flushPromises() {
  await Promise.resolve()
  await Promise.resolve()
  await nextTick()
}

function clickButton(label: string) {
  const button = [...root!.querySelectorAll('button')]
    .find(item => item.textContent?.trim() === label)
  expect(button).toBeTruthy()
  button!.click()
}

beforeEach(() => {
  vi.clearAllMocks()
  routerMocks.route.name = 'RoutingProfileDetail'
  routerMocks.route.params.groupId = 'group-a'
  apiMocks.listRoutingGroups.mockResolvedValue({
    items: [{
      id: 'group-a',
      name: '按模型调度',
      description: null,
      enabled: true,
      is_system_default: false,
      config_json: modelConfig('gpt-5'),
      version: 1,
      updated_at: '2026-01-01T00:00:00Z',
    }],
  })
  apiMocks.getGlobalModels.mockResolvedValue({
    models: [{ name: 'gpt-5', display_name: 'GPT 5' }],
  })
})

afterEach(() => {
  app?.unmount()
  root?.remove()
  app = undefined
  root = undefined
})

describe('RoutingProfiles per-model editor', () => {
  it('keeps configured model menus collapsed when opening a strategy', async () => {
    root = document.createElement('div')
    document.body.appendChild(root)
    app = createApp(RoutingProfiles)
    app.mount(root)
    await flushPromises()

    clickButton('已配置')
    await nextTick()

    expect(root.querySelector('[data-testid="model-policy-editor"]')).toBeNull()

    clickButton('GPT 5gpt-5')
    await nextTick()

    expect(root.querySelector('[data-testid="model-policy-editor"]')).not.toBeNull()
  })
})
