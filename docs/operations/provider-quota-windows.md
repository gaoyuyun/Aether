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
quota when a cost can be calculated. New attempts reserve an estimated provider cost atomically with their dispatch candidate. Normal
in-flight attempts do not disable the provider. A terminal attempt without measurable usage keeps
only its own reservation as `uncertain`; it does not silently count as zero or disable all other
requests. Legacy unresolved attempts without reservations still fail closed until recovery.
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

Historical migration is a resumable, provider-scoped maintenance task. It captures a quota-delta
sequence high-water mark, scans the current epoch in bounded dispatch-time batches, and persists its
cursor and row counts after every batch. Deltas at or below that cutover are marked as absorbed by the
backfill; later deltas remain owned by the normal flusher. For legacy candidates without the new
dispatch snapshot, their persisted attempt `started_at` is the only dispatch-time fallback. Missing
dispatch, billing-type, or cost evidence is reported as an unknown row and keeps the provider fail
closed for operator review. While a backfill is pending, running, or failed, quota reads and upstream
dispatch do not treat an uninitialized counter as zero usage.

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

## Concurrent Admission And Reservations

The HTTP/SSE final dispatch transaction locks the provider briefly and checks each applicable limit against
settled cost plus unflushed actual-cost deltas plus outstanding reservations. It inserts the
candidate, its dispatch snapshot, and `provider_quota_reservations` together. Network execution
starts after commit. MySQL uses READ COMMITTED for reservation-bearing candidate transactions so
concurrent waiters see the latest committed reservations after obtaining the provider lock.
PostgreSQL uses NO KEY UPDATE to avoid upgrading candidate foreign-key locks into deadlocks.
The final check uses the current clock after locking, so a request prepared before a minute
boundary still sees reservations committed in the new minute.

Rolling admission includes the current minute's processed buckets as well as ready outbox deltas.
It also adjusts overdue minute counters to the requested rolling range. The existing minute-based
statistics remain unchanged; their lag cannot create a dispatch-admission gap. Reconciliation
replaces a reservation with actual cost in one transaction, before the asynchronous counter flush.
Repeated terminal events and late Pending writes cannot reserve again or apply the same cost twice.

Reservations belong to an upstream candidate/attempt, not just the public request ID. Retries keep
the public request ID while accounting for each actual upstream attempt independently. A refused
reservation skips the whole provider for that request, avoiding pointless same-key retries. If no
candidate remains, the request terminates with a normal availability error, without orphan Pending
usage waiting for the ten-minute cleaner.

Provider configuration accepts an optional `quota_reservation` object:

```json
{
  "quota_reservation": {
    "minimum_usd": 0.01,
    "fallback_usd": 0.5,
    "output_tokens": 4096,
    "safety_multiplier": 1.25
  }
}
```

These are the defaults. The estimate uses the dispatch pricing snapshot, request input size and
an output budget; high/xhigh/max/ultra reasoning doubles that budget. A smaller explicit output
limit is respected in the estimate. The original request and its output limit are not modified.
The fallback applies when a meaningful input or cost estimate is unavailable. The configured
minimum and any known per-request charge are lower bounds. These estimates are availability-oriented,
not a guarantee that actual upstream cost cannot exceed the quota. An actual overrun is recorded in
full and prevents further paid dispatches when the remaining budget is exhausted.

Dedicated `openai:search` reserves zero. Its subscription activation/expiry and provider enabled
state are checked, while token-spend limits and unrelated accounting fences do not block it.

Interrupted or unmeasurable attempts keep their bounded estimate pending reconciliation. Time
alone never releases a live reservation. Durable request finalization permits recovery to mark an
interrupted reservation uncertain; a later actual settlement replaces it. All amounts remain tied
to the dispatch quota epoch and dispatch time, so old-period reservations cannot consume a new
period and rolling reservations age out consistently with usage attribution.

Validation entry points:

- `cargo test -p aether-billing reservation::tests`
- `cargo test -p aether-gateway --test provider_quota_reservations`
- `cargo test -p aether-gateway --test monthly_quota_failover`
- The ignored PostgreSQL/MySQL reservation tests require disposable `AETHER_TEST_POSTGRES_URL` /
  `AETHER_TEST_MYSQL_URL` databases; run with `--include-ignored` to verify concurrent last-budget
  admission, idempotency, outbox hand-off, current-minute rolling cost, unknown-usage recovery,
  zero-cost admission and quota-epoch transitions on the server databases.

`provider_quota_reservation_rejected` logs the request ID, provider ID and admission reason
(`cycle_reservation_insufficient`, `window_reservation_insufficient`, or an availability reason).
It is a candidate skip within the existing request, not a new request or an internal 500.
