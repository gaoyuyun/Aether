/**
 * 提供商策略管理 API 客户端
 */

import apiClient from './client';
import type { ProviderQuotaWindow } from './endpoints';

const API_BASE = '/api/admin/provider-strategy';

export interface ProviderBillingConfig {
  billing_type: 'monthly_quota' | 'pay_as_you_go' | 'free_tier';
  monthly_quota_usd?: number;
  quota_reset_day?: number;
  quota_last_reset_at?: string;  // 当前周期开始时间
  quota_expires_at?: string;
  quota_windows?: Array<{ duration_secs: number; limit_usd: number }>;
  rpm_limit?: number | null;
  cache_ttl_minutes?: number;  // 0表示不支持缓存，>0表示支持缓存并设置TTL(分钟)
  provider_priority?: number;
}

export interface ProviderQuotaBillingInfo {
  monthly_used_usd?: number | null
  monthly_quota_usd?: number | null
  pending_quota_reset_at?: string | null
  quota_windows?: ProviderQuotaWindow[] | null
  quota_subscription_started_at?: string | null
  quota_cycle_start_at?: string | null
  quota_last_reset_at?: string | null
  quota_next_reset_at?: string | null
  quota_expires_at?: string | null
  quota_reset_day?: number | null
  status?: 'active' | 'expired' | 'not_started' | 'disabled' | 'exhausted' | 'accounting_pending'
}
export interface ProviderStatsResponse { billing_info?: ProviderQuotaBillingInfo | null }
export interface ProviderQuotaResetRequest {
  mode: 'cycle' | 'usage_only'
  effective_at?: string
  cycle_days?: number
  reset_usage?: boolean
}
export interface ProviderQuotaResetResponse {
  effective_at?: string | null
  pending?: boolean
}

/**
 * 更新提供商计费配置
 */
export async function updateProviderBilling(
  providerId: string,
  config: ProviderBillingConfig
): Promise<ProviderStatsResponse> {
  const response = await apiClient.put(`${API_BASE}/providers/${providerId}/billing`, config);
  return response.data as ProviderStatsResponse;
}

/**
 * 获取提供商使用统计
 */
export async function getProviderStats(providerId: string, hours: number = 24): Promise<ProviderStatsResponse> {
  const response = await apiClient.get(`${API_BASE}/providers/${providerId}/stats`, {
    params: { hours }
  });
  return response.data as ProviderStatsResponse;
}

/**
 * 重置提供商月卡额度
 */
export async function resetProviderQuota(providerId: string, operation: ProviderQuotaResetRequest = { mode: 'cycle' }): Promise<ProviderQuotaResetResponse> {
  const response = await apiClient.post(`${API_BASE}/providers/${providerId}/quota/reset`, operation);
  return response.data as ProviderQuotaResetResponse;
}

/**
 * 获取所有可用的负载均衡策略
 */
export async function listAvailableStrategies() {
  const response = await apiClient.get(`${API_BASE}/strategies`);
  return response.data;
}
