# Friend-ready delivery gate

The live cross-host monitor sends an original message with a unique token every five minutes, alternating Hetz→Silber and Silber→Hetz. It waits for one exact linked reply, archives the receipt, and raises actionable attention after three consecutive failures in one direction. The monitor runs as a user service with linger enabled; its append-only event log and per-direction state live under the local `st3/message-soak` state directory.

Either local or Fabric-mediated `conversations send` can commit before its CLI
response is lost or malformed. In both directions the monitor first searches
the recipient's durable mailbox for exactly one request with its unique token,
then requires the exact linked reply. Zero or multiple matching requests do
not become a success. This is recovery from an ambiguous client result, not a
claim that a timed-out send succeeded.

`scripts/st3-message-soak-report` is the release gate. With no overrides, it passes only when the service is active, neither direction has an unresolved failure, and the preceding 72 hours contain at least 400 successful exact receipts in **each** direction, no recorded failure, no evidence gap over 15 minutes, and a current receipt within 15 minutes. The first complete-failure-logging monitor start must predate the window. Test-only environment overrides allow short synthetic fixtures; release evidence must use the defaults. A failed report is a release hold, not permission to edit the log or reset state.

This is an operational smoke gate, not a mathematical proof of 99.999999% availability. That target needs longer production observation, explicit SLO accounting, and failure-injection coverage. A passing 72-hour gate supports a minimally friend-ready trial; the continuous monitor and attention escalation remain enabled afterward.

The separate `st3-idle-soak` service samples Hetz once per minute; a launchd agent samples Silber locally at the same cadence. Each host keeps its own append-only JSONL log, so a transient Fabric control-plane outage cannot erase otherwise valid Silber evidence. `ST3_IDLE_REPORT_REMOTE=1` makes the report fetch Silber's log through Fabric and fail closed if it cannot; release reports must set this flag. Each row records cumulative daemon CPU time, RSS, context switches, graph index, latest material event index, and replication status, pending records, received envelopes, and last successful exchange time. Local sampling has a 30-second deadline; failures and peer-down states are recorded rather than silently skipped. Compare CPU and wakeup deltas only across same-PID rows with an unchanged material event index. The four excluded event kinds are automatic `harness.usage`, `harness.observed`, `work.renewed`, and `replication.heartbeat`; operational message, runtime, and transport changes still disqualify an interval. If over 200 graph events arrive between samples, the interval is not classified quiet because the bounded activity page cannot prove no material change. A restart begins a new RSS/CPU series. A 24-hour flat-RSS claim requires a full post-rollout window with no missing or failing samples and a stable PID. The current status API does not expose exact TCP dial counts, so retained-client connection reuse is supported by its focused TCP test, not by an invented live dial metric.

`scripts/st3-idle-soak-report` applies that 24-hour gate mechanically. It requires at least 1,200 samples per host, no gap over three minutes, no failed/late peer sample or restart, and at least 60 quiet material-event intervals lasting at least 45 seconds per host. Short manual samples cannot create quiet-window evidence. It reports median quiet CPU and context-switch rates. The provisional absolute quiet CPU limits are 15% on Hetz and 50% on Silber; the last-hour mean RSS may not exceed the first-hour mean by more than 64 MiB or 10%, whichever is larger. These limits are deliberately stricter than the measured pre-fix idle samples, but they do not replace a matched before/after measurement or an exact dial counter. A failed report holds release and triggers investigation, not threshold editing to make the result green.

The gate does not replace graph gate evidence for individual missions: work completion still requires its declared products and gates. Preserve the report output and service journal as release evidence, and do not mark a mission complete on a `sent` message alone.

`st3-friend-ready-gate-watch.timer` runs every 15 minutes on Hetz. Its host-local, untracked environment file pins the first local Silber idle sample and the complete-logging delivery monitor start. Before each due time it records `waiting`; afterward it checks both unmodified default reports, sends a durable normalized message to the claimed st3 operator on each pass/fail transition, and sends a combined release-review wake only when both pass. Once both windows are due it starts one exact final-audit mission run, keyed by both window starts, whether either report passes or fails. The run is claimable work with mechanical gates for both unmodified reports; a message alone is never treated as work. The watcher checks the exact run before starting and after an ambiguous start response, then retries on its next timer invocation if no run exists. It keys notifications by window start as well as result, so a reset window cannot silently inherit an old failure notice or audit run. It persists notification state after successful send so a failed send is retried and a duplicate delivery cannot silently erase a gate transition. The reports, not the watcher state, remain the release authority.

The watcher service has a ten-minute start budget: three independent remote
reads may each consume their two-minute Fabric timeout during an outage, and
the remaining time permits fault notification and audit-queue retry. Its
15-minute timer continues to retry a failed invocation.

During the idle window the watcher also runs `st3-idle-soak-preflight` against
only samples since the pinned final-build start. This separate integrity check
alerts early if a host's evidence is missing or stale, a sample or peer is bad,
the daemon PID changes, or a sample gap exceeds three minutes. It never grants
release or relaxes the 24-hour CPU/RSS gate. A failed alert send is retried on
the next timer run; the alert is deduplicated for the current start marker.

After two hours, the watcher also evaluates the post-rollout quiet CPU median
when each host has at least 60 quiet intervals. It sends a deduplicated early
risk notice if Hetz exceeds 15% or Silber exceeds 50%. This is a warning to
investigate, not a final gate result; the complete 24-hour report remains the
release authority. Missing remote evidence remains an integrity fault under
the preflight rather than a passing CPU check.
