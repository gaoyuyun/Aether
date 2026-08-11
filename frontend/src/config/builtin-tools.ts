import { AlertTriangle, BellRing, KeyRound, Mail, Megaphone, Package, Puzzle, Shield, Wallet } from 'lucide-vue-next'
import type { LucideIcon } from 'lucide-vue-next'
import type { ModuleStatus } from '@/api/modules'

export interface BuiltinTool {
  name: string
  description: string
  href: string
  icon: LucideIcon
  order: number
  module?: ModuleStatus
}

const STATIC_BUILTIN_TOOLS: BuiltinTool[] = [
  {
    name: '邮件配置',
    description: '配置 SMTP 邮件服务，管理邮件模板和发送设置',
    href: '/admin/email',
    icon: Mail,
    order: 80,
  },
  {
    name: 'IP 安全',
    description: '管理 IP 黑白名单，控制系统访问权限',
    href: '/admin/ip-security',
    icon: Shield,
    order: 90,
  },
  {
    name: '审计日志',
    description: '查看系统操作日志，追踪安全事件与变更记录',
    href: '/admin/audit-logs',
    icon: AlertTriangle,
    order: 100,
  },
]

const moduleIconMap: Record<string, LucideIcon> = {
  BellRing,
  KeyRound,
  Megaphone,
  Package,
  Wallet,
}

export function buildBuiltinTools(modules: ModuleStatus[]): BuiltinTool[] {
  const moduleTools = modules
    .filter(module => module.kind === 'builtin' && module.admin_route)
    .map(module => ({
      name: module.display_name,
      description: module.description,
      href: module.admin_route ?? '',
      icon: moduleIconMap[module.admin_menu_icon ?? ''] ?? Puzzle,
      order: module.admin_menu_order,
      module,
    }))

  return [...moduleTools, ...STATIC_BUILTIN_TOOLS]
    .sort((a, b) => a.order - b.order || a.name.localeCompare(b.name, 'zh-Hans'))
}

/** href → display name mapping for breadcrumbs */
export const BUILTIN_TOOL_BREADCRUMBS: Record<string, string> = Object.fromEntries(
  STATIC_BUILTIN_TOOLS.map(tool => [tool.href, tool.name])
)
