# Provider Quota Windows

Provider subscription quotas have a cycle total (`monthly_quota_usd` plus
`quota_reset_day`) and may also define one or more fixed-period spending windows. Window definitions
are stored in `providers.config.quota_windows`:

```json
[
  {"duration_secs": 86400, "limit_usd": 5},
  {"duration_secs": 604800, "limit_usd": 20}
]
```

The admin provider form exposes daily, weekly, and custom-duration windows. A one-day cycle
(`quota_reset_day: 1`) is the daily-card configuration; a longer cycle is a monthly/custom card.
Fixed windows are aligned to the subscription start: `duration_secs: 86400` resets every 24 hours
from that instant, and `duration_secs: 604800` resets every seven days from that instant. They are
not aligned to calendar-day or calendar-week boundaries.

The admin form accepts the subscription start in the browser's local time with minute precision.
Changing it for an existing subscription starts a fresh live quota epoch: current total and window
counters are cleared, while historical request and cost records remain unchanged.

The cycle total and every configured window are enforced together. Once any limit is reached,
candidate selection marks the provider as `provider_quota_blocked`, so it is excluded from polling
until the window or cycle rolls over. The API accepts at most eight distinct window durations;
durations must be positive and bounded, while limits must be finite and non-negative.

Quota enforcement uses `actual_total_cost_usd`. Settlement asynchronously folds completed requests
into one bounded counter row per provider and duration; candidate selection reads only those current
rows and never scans the usage history. Total quota snapshots are cached for five seconds and window
counters for one second, with cache misses combined into batched queries. This keeps the SQLite hot
path bounded on small hosts. Because counters are asynchronous, an already in-flight request or a
request completed just before a flush may briefly exceed a local window; upstream quota responses
remain the final enforcement signal.

All persisted Unix timestamps used by quota settlement and usage filtering are Unix seconds. The
legacy `usage.created_at_unix_ms` column name is retained for database compatibility; adapters must
pass its value through without converting it to milliseconds or dividing it again. Admin API date
fields should be exposed as RFC 3339 strings, while internal storage and arithmetic remain Unix
seconds.

Changing a provider between subscription and pay-as-you-go starts a new live quota epoch and clears
the live counter. Late settlement events from the previous epoch and pay-as-you-go events are ignored
by subscription counters. Historical usage/audit records are not deleted. The regular request cost
(`total_cost_usd`) is still stored and shown for subscription traffic, so expense statistics do not
turn into zero-valued requests. Switching back to a subscription therefore does not charge old
pay-as-you-go traffic against the new quota.
