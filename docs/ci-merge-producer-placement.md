Owned producer placement: bounded capacity assessment, 2026-10-15

Decision: no added runner slots or NixOS switch (Factory0c116102/57ccc506). First source change targets merge_group linux-test-build only; PR producers remain a later unit after measured before/after. Use the same runner census, choose a host with all six records online, <=3 busy and at least two eligible idle priority workers; otherwise Namespace. Advisory reads are not reservations. Current three concurrent merge groups give a planning ceiling of three added merge producers across12 slots (25%); this does not bound existing PR producers and is not a new global semaphore. Activation still needs Factory's word.

| Held cohort / scenario | Count / occupied job-minutes | Median / p90 whole-job occupancy |
|---|---:|---:|
| Exact event-joined successful OWNED merge producers, selected Oct9–15 cache |197 /1805.95 |9.083 /10.35 min |
| Successful Namespace merge producers in same selected cache |100 /1305.02 central min |12.808 /16.267 min |
| All successful owned producers in seven-day cache (PR+merge+other) |484 |8.817 /10.683 min |
| Three overlapping producers at owned cohort durations (illustration) |3 slots;27.25 total runner-min at median,31.05 at p90 |~9.08 /10.35 min simultaneous occupancy |

No measured saving yet. The historical100 Namespace producers/1305.02 minutes describe a candidate workload, not an expected reduction or controlled speedup. Eligibility at the new <=3-busy gate, execution time on a different host/cache and cancellation/rebuild changes are unknown. Hosted light jobs do not move, so this proposal directly targets Namespace elapsed job-minutes only. Production effects on runner wait, merges/hour and minutes/merge remain unmeasured.

The seven-day terminal cache contains22662 unique jobs but NO created_at or contemporaneous six-runner busy census per interval. It cannot identify exactly which Namespace jobs queued while all owned slots were busy, estimate a nine/ten-slot counterfactual, or provide complete daily totals/merge denominators. Selective coverage is uneven. Runtime sampler evidence (Factory0c116, Operations custody) covers98min: ci1 allsixbusy~9%, fiveplus~15%, typical2busy; these are observations, not a week. Worker/load/memory values are unaffected by the PSI correction; v1 fields named *_psi10 were actually avg60. CPU idle-core estimates from load alone are not measurements; corrected /proc/stat deltas are pending. Six slots with typical two busy favor trying routing before adding slots.

Current owned_queue_age_max_s=0 is a snapshot; merge_group_p90_wait_s=455 is a24h percentile over BOTH pools. Their difference alone does NOT prove the historical wait was on Namespace. The producer pilot and per-pool cached intervals can test the hypothesis. No stage gets a capacity grant from that inference.

Registration if slots were ever added: source hosts/ci1/modules/runners.nix generates distinct system users, state/cache directories and ephemeral systemd services from general/merge counts. A reviewed host config change, qualified switch and startup would be needed; configure uses organization https://github.com/compoundingtech, existing group ci1(id3), tokenType=access, tokenFile /var/lib/ci1/runner-token or /var/lib/ci2/runner-token. Those are operator-managed credential paths, not permission to read or copy the secret; exact PAT human owner is not held here. Operations/admin owns installation, service registration uses the installed token. This registers GitHub organization inventory; it is not a merge-queue ruleset edit or Namespace quota increase. Factory has explicitly declined that change. The approved producer-routing source changes neither registrations nor token/group policy and is not capacity lowering under Factory57/Nathan2980; no runtime activation follows from this document.

Live limits remain important: repository host-module default MemoryMax=14G per job, regardless of92GB whole-host RAM. No memory-limit/parallelism change is proposed, and owned placement is not an OOM cure claim. At nine14G maxima the configured possible sum would be126G versus92GB host RAM (not allocated memory), another reason idle snapshots cannot justify extra slots.

Before activation, freeze a genuine existing-reader census/SLO baseline and exact eligible/selected producer rows; after activation compare owned producer occupancy, owned-label queue ages/starts, central job-minutes and merges/hour against that baseline using the SAME reader, separating merge_group and PR. If owned queue/start wait, merge-group CONSUMER start wait, or merge-group p90 start wait PER POOL increases, or merges/hour degrades relative to the frozen baseline, set the source toggle to off for Namespace-only FUTURE merge producers; do not cancel running work or retry CI. No new poller/GitHub loop. Source defaults unset/inert; PR-surface extension needs its own total-producer bound and a second reviewed PR only after step1 results.

Evidence: build-memory-comparison@a33693b5 and cached-rows@301d43c2; seven-day-central-minutes-held@d9ae52cf (partial lower bounds through09:50); Factory0c116/57ccc sampler/scope decision. All calculations reused local rows; no new GitHub or host acquisition.

Source toggle: `CI_MERGE_TEST_BUILD_OWNED` is unset by default. `on` enables
merge-group producer admission; `off` forces Namespace for future producers;
other nonempty values fail open to Namespace. A failed/skipped picker also uses
Namespace while the toggle is set. Required test consumers keep the old picker
output. The read-only runner token and existing single group-3 census are reused.
An ordinary PR hosted run with the toggle unset qualifies syntax/inactive paths,
not active placement, headroom, savings or the per-unit memory budget.

The two-eligible-idle margin reduces concurrent-picker contention but does not
reserve either worker. Three simultaneous groups can still choose the same host
before any assignment is visible, and GitHub does not move a queued producer to
Namespace automatically. A queued producer is an explicit remaining activation
risk; the off switch affects future admissions, not already queued/running jobs.
Consumer starvation is included in the baseline and rollback observation above.
Configured CI_MERGE_NAMESPACE_PROFILE is preserved by both picker fallback and
the failed/skipped-picker runs-on expression. Factory message/fbdf38c0 explicitly records that it weighed st2#2980 and that
this moves our producers to owned capacity without changing the shared order.
Activation still requires a separate word after merge, a frozen baseline and
Operations' variable deployment; this source merge sets nothing.
