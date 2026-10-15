# Local GitHub observer request meter and admission

The response meter previously counted only completed resource-provider HTTP
responses in memory. A failed send or restart could lose usage. This change
reserves configured daemon GitHub HTTP requests before local reqwest or the
existing authorized RPC runs. `STATE/github-admission.json` retains the last
hour's reservations; doctor reads them as
`github-requests/admitted/CALLER/RESOURCE` even before a new request after restart.

**Default: meter only, no quota enforcement.** The optional DAEMON setting
`[github].observer_requests_per_hour = 1000` enables the Factory allocation, per
resource and per caller, with a shared aggregate of1000 rather than1000 for each
observer. This is a local config setting, not a GitHub setting. Values1..1000 can narrow the Factory ceiling; raising it requires a new policy
review. No live config is changed. Existing production observer usage is UNKNOWN in this source review.
Before setting a cap, the owner must measure current per-resource request rates
including pagination at the5-minute interval and confirm quota/freshness headroom.
Meter-only I/O failures are visible gaps and do not stop polling; enforced
observer admissions fail closed on an unproven durable write. Unscoped commands
remain metered and uncapped; a broken meter must not block merge writes.

GLOBAL-VIEW: no. No other machine, distributed lock or shared-token reservation
is consulted. The existing repository observer and shared per-URL cache remain
the only poller. A local per-process mutex serializes this daemon's journal. The
caller is the reconciler's task-local `spend_as` subject, never ST_AGENT or an HTTP
header. Unenrolled direct comment/merge-gate traffic is `unattributed`; authentic
per-agent write admission remains a separate obligation.

Core, GraphQL and search use separate request counters (including GitHub
Enterprise `/api/graphql` and `/api/v3/search` path shapes without relaxing any
existing transport-domain policy). These are NOT GraphQL points, search-provider
or secondary-limit guarantees. Failed sends and conditional304s remain reserved.
Refusal adds no reservation and returns the existing typed ProviderRateLimit.

Counters are coalesced per minute/caller/resource, including unscoped traffic:
a100000-call unscoped flood in one minute occupies one row and cannot consume
the observer quota or fill its row allowance. Whole-minute expiry is conservative
by at most a minute, so doctor reports a61-minute upper window. Small backward
clock steps preserve counts and clamp future timestamps to now; timestamps more
than a day ahead remain invalid. A cold read never silently resets bad data.

Publication writes an exclusive private temporary, fsyncs, renames atomically and
syncs the parent before send. A directory-sync failure invalidates memory so the
next attempt rereads any committed reservation. Configuration removes stale
private regular temporaries in a bounded scan, without following symlinks; UUID
names avoid pid-reuse collisions. Doctor try-locks rather than waiting for disk
sync, and limits output to127caller/resource groups plus an aggregated other row
and any fault line. Expected fsync latency has NOT been measured; minute buckets
reduce file size, but there is still a durable publication per admitted request.

Conditional REST behavior is unchanged: valid retained ETags produce
If-None-Match on revalidation and a304 preserves full bodies/page links. First
reads and endpoints without ETags cannot supply a valid conditional header.
GraphQL has no ETag revalidation. Its shared repository minimum is now5minutes
(was60seconds), settled remains15minutes, explicit refresh remains explicit.
This visibly affects st gh watch wakes and observer-backed PR/check/queue UI
freshness, up to5minutes while settling. Curator's source page says live merge
and ci-passed gates fetch current data and enqueue/merge are direct writes; this
patch does not replace those live gates with cached PR facts. Consumer coverage
must still be qualified on the deployed build. No automatic skip or stale-head
approval follows from the cache or this meter.

Rust controls are written and have not been run locally:
restart/expiry and aggregate limits, separate resource counting, refusal without
spending, persistence without a response, corrupt/gross-future state, symlink
refusal, unscoped flood, tolerated skew, meter-only policy, enforced failure vs
unscoped availability, stale temporary cleanup, and doctor contention. The new
module parses with rustfmt and the diff passes whitespace checks. Existing
resource tests cover shared lists and200/304/head-change behavior. Exact-head
Intake review, ordinary hosted qualification and later deployment evidence are
required; no live admission or savings claim is made here.
