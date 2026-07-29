import type { RouteLocationNormalized } from 'vue-router'
import type { useModuleStore } from '@/stores/modules'
import { log } from '@/utils/logger'

function requiredModuleName(to: RouteLocationNormalized): string | undefined {
  return to.matched.find(record => record.meta.module)?.meta.module as string | undefined
}

/**
 * Re-evaluates an already open user module route after a successful runtime status refresh.
 */
export function inactiveModuleRouteRedirect(
  to: RouteLocationNormalized,
  moduleStore: ReturnType<typeof useModuleStore>
): string | null {
  if (!to.path.startsWith('/dashboard/') || !moduleStore.runtimeLoaded) {
    return null
  }
  const moduleName = requiredModuleName(to)
  return moduleName && !moduleStore.isActive(moduleName) ? '/dashboard' : null
}

/**
 * 检查非管理端路由的模块激活状态。
 * @returns 重定向路径，或 null 表示通过
 */
export async function checkModuleAccess(
  to: RouteLocationNormalized,
  moduleStore: ReturnType<typeof useModuleStore>
): Promise<string | null> {
  // 检查路由链中是否有模块要求
  const moduleName = requiredModuleName(to)
  if (!moduleName) {
    return null
  }

  // Store 内部使用短 TTL；每次进入模块路由都校验是否需要刷新。
  try {
    await moduleStore.fetchRuntimeModules()
  } catch (error) {
    // fail-close: 获取模块状态失败时拒绝访问
    log.warn('Failed to fetch modules status, denying access', { error })
    return '/dashboard'
  }

  // 用户侧需要检查模块是否激活（active），而不仅仅是可用（available）
  if (!moduleStore.isActive(moduleName)) {
    log.warn(`Module ${moduleName} is not active, redirecting to user dashboard`)
    return '/dashboard'
  }

  return null
}
