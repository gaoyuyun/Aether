import { describe, expect, it, vi } from 'vitest'
import { checkModuleAccess, inactiveModuleRouteRedirect } from '../moduleGuard'

function moduleRoute(name: string, path = '/dashboard/wallet') {
  return {
    path,
    matched: [{ meta: { module: name } }],
  }
}

describe('module route guard', () => {
  it('loads public runtime status before allowing an active user module', async () => {
    const store = {
      runtimeLoaded: false,
      fetchRuntimeModules: vi.fn(async () => {
        store.runtimeLoaded = true
      }),
      isActive: vi.fn((name: string) => name === 'wallet'),
    }

    const redirect = await checkModuleAccess(moduleRoute('wallet') as never, store as never)

    expect(redirect).toBeNull()
    expect(store.fetchRuntimeModules).toHaveBeenCalledOnce()
    expect(store.isActive).toHaveBeenCalledWith('wallet')
  })

  it('fails closed when public runtime status cannot be loaded', async () => {
    const store = {
      runtimeLoaded: false,
      fetchRuntimeModules: vi.fn().mockRejectedValue(new Error('offline')),
      isActive: vi.fn(),
    }

    const redirect = await checkModuleAccess(moduleRoute('billing_plans') as never, store as never)

    expect(redirect).toBe('/dashboard')
    expect(store.isActive).not.toHaveBeenCalled()
  })

  it('lets the store revalidate an already loaded runtime status', async () => {
    const store = {
      runtimeLoaded: true,
      fetchRuntimeModules: vi.fn(async () => undefined),
      isActive: vi.fn(() => true),
    }

    const redirect = await checkModuleAccess(moduleRoute('wallet') as never, store as never)

    expect(redirect).toBeNull()
    expect(store.fetchRuntimeModules).toHaveBeenCalledOnce()
  })

  it('redirects an open user module page after the refreshed status becomes inactive', () => {
    const store = {
      runtimeLoaded: true,
      isActive: vi.fn(() => false),
    }

    const redirect = inactiveModuleRouteRedirect(
      moduleRoute('wallet') as never,
      store as never
    )

    expect(redirect).toBe('/dashboard')
    expect(store.isActive).toHaveBeenCalledWith('wallet')
  })

  it('keeps the current page until runtime status has loaded successfully', () => {
    const store = {
      runtimeLoaded: false,
      isActive: vi.fn(() => false),
    }

    const redirect = inactiveModuleRouteRedirect(
      moduleRoute('billing_plans', '/dashboard/billing') as never,
      store as never
    )

    expect(redirect).toBeNull()
    expect(store.isActive).not.toHaveBeenCalled()
  })

  it('does not redirect admin module configuration routes when a module is inactive', () => {
    const store = {
      runtimeLoaded: true,
      isActive: vi.fn(() => false),
    }

    const redirect = inactiveModuleRouteRedirect(
      moduleRoute('wallet', '/admin/modules/wallet') as never,
      store as never
    )

    expect(redirect).toBeNull()
    expect(store.isActive).not.toHaveBeenCalled()
  })
})
