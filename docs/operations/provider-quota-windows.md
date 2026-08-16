# Provider Quota Windows

Provider subscription quotas combine a cycle total (`monthly_quota_usd`) with zero or more rolling
spending windows stored in `providers.config.quota_windows`:

```json
[
  {"duration_secs": 18000, "limit_usd": 5},
  {"duration_secs": 604800, "limit_usd": 20}
]
```

Each duration must be a whole number of minutes, from 60 seconds through 30 days. At most eight
distinct durations may be configured. The cycle length (`quota_reset_day`) is also limited to 1-30
days. A window covers `[max(quota_epoch_start, clock_minute - duration), clock_minute)`: it is a true
rolling range inside the current quota epoch, not a fixed bucket aligned to the subscription start.

Quota epochs, dispatch attribution, buckets, and window boundaries use whole Unix minutes. Usage in
the current unfinished minute enters the rolling total after the minute finalizer runs. Together with
the one-second counter cache, this means natural expiry and newly completed usage are intentionally
eventually consistent at minute precision. Candidate selection reads only bounded counter rows; it
never scans request history or minute buckets.

## Cost Semantics

The provider billing type describes upstream cost, independently of whether a downstream user is
charged:

- `monthly_quota` records `provider_quota_cost_usd` against the cycle total and rolling windows.
- `pay_as_you_go` records provider cost but produces no subscription quota delta.
- `free_tier` has zero provider and quota cost, while `user_billable_cost_usd` can remain positive.

User billing continues to use the existing wallet, plan, and skip-billing policies. A provider being
`free_tier` must not be presented as making the user's request free. This implementation does not add
a new user charge multiplier or sales-policy precedence.

Provider billing type, quota epoch, dispatch minute, pricing version, and pricing snapshot are saved
at upstream dispatch and reused at settlement. A later provider configuration change cannot reclassify
the request. Failed, cancelled, and partially completed upstream attempts still consume subscription
quota when a cost can be calculated. An attempt with neither measurable usage nor a minimum cost is
marked pending/failed and makes the affected quota state fail closed instead of silently counting zero.
Provider quota accounting is independent of successful wallet settlement.

## Counters And Maintenance

All durations reuse `provider_quota_usage_buckets`, keyed by provider, quota epoch, and dispatch
minute. `provider_quota_window_counters` stores only the current aggregate for each configured
duration, including `rolling_start`, `accounted_until`, and `status`.

The delta flusher adds completed monthly-quota deltas to the shared minute bucket. Every minute the
finalizer incorporates ended minutes, then cleanup subtracts buckets that left each rolling range.
Adding or re-adding a duration rebuilds it from the current epoch's shared buckets. While a required
counter is missing, `rebuilding`, or `failed`, the scheduler treats that provider as quota-blocked.

Old epoch buckets are not deleted by the reset transaction. They remain available for late deltas and
diagnostics, and are removed separately only after the relevant outbox watermark and in-flight attempt
barrier are safe. Schema migration preserves the previous fixed-window table as a legacy copy until
the rolling counters have been validated.

## Reset And Configuration Changes

Automatic reset checks run every minute. The quota read/dispatch path also attempts any due reset, so
a delayed worker cannot continue using an expired epoch. If the due reset transaction fails, the
provider remains fail closed. `quota_expires_at` also blocks the provider after expiry.

Manual reset uses:

```http
POST /api/admin/provider-strategy/providers/{provider_id}/quota/reset
```

The legacy `DELETE .../quota` route has the same scheduling semantics during compatibility. A request
records `pending_quota_reset_at` for the next whole minute and returns that `effective_at`. Reset
changes only live quota state. Historical usage, settlement, and audit records remain intact.

Changing a limit preserves the epoch and current usage. Adding, changing, or removing a duration does
not reset other windows. Switching among `monthly_quota`, `pay_as_you_go`, and `free_tier` does not
create or reset an epoch; switching back resumes the retained epoch and only dispatches originally
made under `monthly_quota` count toward it.

The admin stats response returns every configured window with `used_usd`, `rolling_start`,
`accounted_until`, `quota_epoch_start`, and `status`, plus `pending_quota_reset_at`. The provider form
edits the complete window array rather than only its first entry.
