import { Mail, Shield, AlertTriangle, BellRing, KeyRound, Megaphone, Package, Wallet } from 'lucide-vue-next'
import type { LucideIcon } from 'lucide-vue-next'

export interface BuiltinTool {
  name: string
  description: string
  href: string
  icon: LucideIcon
  moduleName?: string
}

export const BUILTIN_TOOLS: BuiltinTool[] = [
  {
    name: '独立密钥',
    description: '管理不绑定用户账号的独立 API 密钥；关闭后全部停止鉴权，但保留相关数据',
    href: '/admin/keys',
    icon: KeyRound,
    moduleName: 'standalone_keys',
  },
  {
    name: '钱包管理',
    description: '管理用户钱包、充值和额度结算；关闭后用户按无限额度运行',
    href: '/admin/wallets',
    icon: Wallet,
    moduleName: 'wallet',
  },
  {
    name: '套餐管理',
    description: '配置每日额度和会员权益套餐；需要先启用钱包管理',
    href: '/admin/billing-plans',
    icon: Package,
    moduleName: 'billing_plans',
  },
  {
    name: '邮件配置',
    description: '配置 SMTP 邮件服务，管理邮件模板和发送设置',
    href: '/admin/email',
    icon: Mail,
  },
  {
    name: '通知服务',
    description: '管理通知项、模板和推送服务策略',
    href: '/admin/notification-service',
    icon: BellRing,
  },
  {
    name: '公告管理',
    description: '发布和管理系统公告，并控制仪表盘公告展示',
    href: '/admin/announcements',
    icon: Megaphone,
    moduleName: 'announcements',
  },
  {
    name: 'IP 安全',
    description: '管理 IP 黑白名单，控制系统访问权限',
    href: '/admin/ip-security',
    icon: Shield,
  },
  {
    name: '审计日志',
    description: '查看系统操作日志，追踪安全事件与变更记录',
    href: '/admin/audit-logs',
    icon: AlertTriangle,
  },
]

/** href → display name mapping for breadcrumbs */
export const BUILTIN_TOOL_BREADCRUMBS: Record<string, string> = Object.fromEntries(
  BUILTIN_TOOLS.map(t => [t.href, t.name])
)
