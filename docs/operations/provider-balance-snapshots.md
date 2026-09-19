# Provider Balance Snapshots

The admin provider list shows each provider's upstream balance ("余额监控"). The page never waits
for an upstream: it reads a per-provider snapshot, and a background refresher keeps the snapshot
current. One unreachable provider therefore cannot blank the whole column or hold a request open
until the browser gives up.

## Snapshot Model

`provider_ops_balance_snapshots` stores one row per provider with a provider ops configuration:

| Column | Meaning |
|---|---|
| `payload_json` | Projected result of the most recent **successful** query (`success` / `auth_expired`). Kept across failed attempts. |
| `last_success_at` | When `payload_json` was fetched. Drives the `stale` flag. |
| `last_attempt_at`, `last_status`, `last_error` | The most recent attempt, successful or not. `last_error` is the gateway's own classification (never upstream text) and is capped at 200 characters. |
| `consecutive_failures` | Failure streak; reset to 0 on success. |
| `next_refresh_at` | Backoff deadline. Non-forced refreshes skip the provider until it passes. |

Deployments without a catalog database keep the same structure in the runtime KV store under
`provider_ops:balance:<provider_id>` for seven days. Saving or deleting a provider's ops
configuration deletes its snapshot; the next page load or monitor tick queries the new credentials.

## Refresh Pipeline

Every trigger goes through the same enqueue function and returns immediately:

- **Page load** (`POST /api/admin/provider-ops/batch/balance`): snapshots missing, failed, or older
  than 5 minutes are queued.
- **Scheduled monitor** (`maintenance.provider.balance_monitor`, every 60 s): every active provider
  with ops configured is kept fresher than 10 minutes.
- **Quota alert**: reads the snapshot instead of querying the upstream itself and queues a refresh
  when the snapshot is older than the alert's `fetch_interval_seconds`.
- **Manual refresh** (`GET .../balance?refresh=true` or the refresh button): forced, ignores backoff.

Refresh jobs are deduplicated per provider per gateway instance, limited to four concurrent
upstream queries, and guarded by a 90-second runtime lock so only one instance in a cluster
refreshes a given provider. Balance queries use a 5 s connect / 10 s total timeout; interactive
"verify credentials" calls keep the 30 s default.

Failures back off exponentially from 60 s to 30 min. `auth_failed` waits 15 min because credentials
do not fix themselves; `not_configured`, `not_supported` and `parse_error` wait the full 30 min.
Each attempt logs `provider_ops_balance_refreshed` or `provider_ops_balance_refresh_failed` with the
provider, architecture, trigger, status and elapsed time.

## API Shape

Balance responses keep the historical `status` / `data` fields (describing the last successful
value) and add snapshot metadata:

```json
{
  "status": "success",
  "data": { "total_available": 12.34, "currency": "USD" },
  "fetched_at": "2026-09-17T08:00:00Z",
  "stale": true,
  "refresh_state": "refreshing",
  "next_retry_at": null,
  "consecutive_failures": 3,
  "last_error": { "status": "network_error", "message": "请求超时", "at": "2026-09-19T07:08:00Z" }
}
```

Only a provider that has never succeeded and has no failure recorded returns `pending`. A provider
that failed before ever succeeding returns its failure status directly so the UI can show the reason
instead of spinning. `refresh_state` is `refreshing` while a job is queued or running on the serving
instance; clients poll (2 s, 4 s, 8 s, 16 s) until it returns to `idle` and then stop.

`GET .../balance?refresh=false` on a provider without any snapshot still queries the upstream in the
request and records the result; `POST .../balance` always does. Both use the short balance
timeouts.
