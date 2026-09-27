# st3 idle stability sample — 2026-09-23

The samples below used the installed, pre-fix daemons on Hetz and Silber. Process CPU is the change in cumulative process time divided by wall time; values above 100% mean more than one core. RSS is the observed range, not a heap measurement. Graph writes are changes in the client snapshot store index. Context switches are a wakeup proxy, not a count of application timer firings. Both hosts were sampled through their ordinary local API; Silber commands ran through Fabric.

| Window | Host | Main daemon CPU | Main RSS | Context switches | Graph writes | Received envelopes |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| No graph writes, ~5–7 s | Hetz | ~29% | 977–978 MiB | 187 (~28/s) | 0 | Not sampled |
| No graph writes, ~5–7 s | Silber | ~235% | ~756 MiB | 445,783 (~90k/s, macOS `top` CSW) | 0 | Not sampled |
| Active graph, ~12–15 s | Hetz | ~43% | 1010–1014 MiB | 300 (~26/s) | 2 | 2 |
| Active graph, ~12–15 s | Silber | ~147% | 606–635 MiB | 920,000 (~60k/s, macOS `top` CSW) | 4 | 4 |

The replication workers consumed under one second of CPU in each active window. At the end of that window, the last successful signed exchange was about 10 seconds old on Hetz and 8 seconds old on Silber. Both peer statuses were `up`, with no recorded last error and no pending replication records. The main and worker PIDs stayed stable throughout sampling; Hetz systemd reported zero restarts. One transient Fabric `exec` connection reset during sampling recovered on retry while both signed replication peers remained `up`. The samples therefore show live graph activity and idle main-daemon cost, not evidence of a sustained peer reconnect loop. Silber's high no-write CPU warrants a separate focused profiler pass; this sample does not establish its cause.

## Demonstrated unnecessary work and validation

Before this change, each outbound signed POST built a new `reqwest::Client`, so neither the second phase of an exchange nor later wakeups could reuse its connection pool. The signed exchange regression uses the same live TCP test server: two exchanges with the retained client use **one** source port; the next exchange with a fresh client increases the count to **two**. The worker now owns one client per configured peer, retaining the 3-second connect and 120-second request deadlines. This is deterministic before/after evidence for outbound dials; the installed daemons were not restarted, so it is not a production after measurement.

The `actual_cache` previously retained entries for every historical subject queried, even after all entries became unusable at the next store index. The cache regression places two entries at one index, advances the index, and verifies the stale entry is removed when the new value is read. The cache now retains only entries from the current queried index. The replication snapshot cache already replaces its prior snapshot on a miss, and the store read pool is fixed at four connections; neither showed the same unbounded retention pattern in this code review.

Focused validation: `cargo test -p st3 peer::tests`, `cargo test -p st3 the_current_actual_cache_follows_the_store_index`, `cargo fmt --all -- --check`, and `git diff --check` passed. Production CPU, RSS, and context-switch after samples require integration and a controlled rollout of the new binary; no live improvement is claimed here.
