# Pinned baseline evidence, 2026-10-08

Candidate: v0.3.17, source f4cec96e414fee2a780910730b019fe6bdbddc1d.
Archive SHA256: d30869ac1251dbeb30862bc3f5c74db4a0ccfe385110e43c5c474aa385dcddb2.
BUILD.json is retained in both JSON receipts. This old archive includes its bundled
runtime; it does not establish compatibility of a new native host binary.

Both runs used one fresh systemd container at a time, user ada on studio, two CPUs,
3 GiB RAM and no host mounts. Neither had gh, developer tools or a provider. Docker
restart was used for the container boot check. Each test container was removed.

| Run | PASS | XFAIL | FAIL / XPASS | Exit |
| --- | ---: | ---: | ---: | ---: |
| Ubuntu 22.04, systemd 249 | 9 | 10 | 0 | 0 |
| Ubuntu 24.04, systemd 255 | 9 | 9 | 0 | 0 |

The 22.04 run additionally verifies the requested guest release. Its first attempt
misclassified a missing-harness error because a glibc loader warning contained
"not found". The classifier now removes those lines and rejects generic spawn
errors; the complete corrected 22.04 run is the retained receipt. On 24.04 the
recorded wait reaches the 15-second bound without a missing-provider diagnosis.
On 22.04 the output contains the unsupported --expand-environment=no option.

Expected failures reproduce plain st printing help in a PTY, absent setup, manually
required person config, daemon-down text without setup guidance, irrelevant doctor
warnings, missing provider diagnosis, non-idle outside-seat MCP, no daemon before
login after restart, release version wording, and the additional weak-version
loader warning on 22.04. Baseline 24's combined diagnostic label mentions both
possible old seat failures; its exact command output shows the silent wait.

Commands (the archive was downloaded once and checksum verified by the runner):

```sh
scripts/onboarding-e2e baseline --ubuntu 22.04 --image onb-sysd \
  --archive /tmp/smalltalk-x86_64-unknown-linux-gnu.tar.gz --out /tmp/baseline-22
scripts/onboarding-e2e baseline --ubuntu 24.04 --image onb-sysd2404 \
  --archive /tmp/smalltalk-x86_64-unknown-linux-gnu.tar.gz --out /tmp/baseline-24
python3 scripts/onboarding-fixtures/test_rig.py -v
```

JSON files retain every exact guest command, separate stdout/stderr, exit code and
elapsed time. Text files retain the runner output. Five local checks passed for
archive integrity/traversal, the CI VM guard, run identity and consent-before-start.
Incus CLI absence and lack of read/write /dev/kvm access were separately confirmed;
the runner exits 2 before creating output or a guest when those prerequisites are
absent. Real Incus/Tart boot, polkit and SSH login remain untested. Candidate
provider/mission scenarios are not claimed by these baseline receipts. See
docs/onboarding-vm-runbook.md for the manual next pass.

## Composed candidate setup on Ubuntu 24.04

The core2959b4d518c11db95f638dea780577e5f9aaa616 archive (SHA256
ebacde8508d9e5c6510f1b45766716ed8300d67d34bd21bb994286e750da533c) passed
all **18 setup assertions**, exit0, with the embedded clean24 image recipe built
by the runner. setup-core24.txt is its exact runner output; setup-core24-summary.json
records the checks and composition. The source path of the host pty is omitted
from this public summary. Full exact commands/stdout/stderr and unmodified BUILD.json
are retained privately in:

- doc/fleet/smalltalk/onboarding/2026-10-08d/setup-core24-report@635bf7606ae9b5c41db5f14130c6387c03e8172b201d612282a13c67a82394b6
- doc/fleet/smalltalk/onboarding/2026-10-08d/setup-core24-receipt@6aa08885f472f877b8f15853da3ae70c593a02a0b2c9c938021ec56d222626f1

This proves config merge, person and node, self-install/link/pty, service, fresh
login PATH, doctor without development tools, visible Home/Now, Ctrl+Q/restoration,
idempotence, the refused-linger command and manager-down behavior before user login
after a container restart. The guest was removed. No harness/expert/mission-content,
release-version, stock22 native compatibility or real-VM pass is implied. The
candidate is a proof-only dev composition with no bundled loader/libc; native22
is known to fail before main and remains install-story work.

## Harness-only driver split

The foundation archive also passed **23 native Codex checks**, exit0, with a
disposable explicitly created seat and a matching successful read of the exact
graph wake, app-server/TUI handshake, outside-seat MCP and seat stop. Reboot was
explicitly skipped for this driver validation; the earlier setup proof includes it.
native-codex-split.txt retains exact runner output. This validation preceded the
new Selected harness assertion and proves driver plumbing, not the next candidate's
setup detection. The latest --harness-only requires Selected harness exactly once
and Claude setup/driver route and consent checks; default expert assertions remain.

Two preceding attempts failed during container boot, before any setup or driver
check. Retained startup diagnostics showed user manager Error24/Too many open
files; the host developer UID had125 readable inotify instances against limit128.
Assigning Docker ada UID42420 resolved the failure. The runner changes only its
disposable guest UID, including when adapting old images, and keeps host kernel
limits unchanged. All failed and successful containers were removed. Incus and
external transports use the guest's actual UID. Six guard/consent checks now pass,
including packaged selectors in approved, development split and development joined
forms; their stdout is in rig-checks-harness.txt.

## Composed harness/channel candidate on Ubuntu 24.04

Source48871974cae9ce31e2dd4e4578bf056c986e7f08, harness implementation
63f0b36215bc4dca159ea17045914d51ee5fe872, archive SHA256
e5fc412e12db59c74791ca6d467039a334e8642ce2eea3c8f39df49ac7e31554.
The eight final receipts total **214 PASS, zero FAIL**: no-harness23,
Claude development28, Claude approved28, Codex26, both28, and each of
no-gh/logged-out-gh/broken-gh27. Each includes the container restart check.

The initial matrix exited1 with five failed assertions in four scenarios.
The rig's sorted JSON policy differed from the candidate's exact managed bytes;
its GitHub watch probe used a person actor despite requiring a running agent seat.
The policy fixture now uses the candidate root helper only inside its disposable
guest and verifies ordinary-user readability. GitHub probes use the live fixture
seat. Only those four affected scenarios were rerun, exit0. Initial failed receipts
remain in harness24-initial.txt and pinned documents; harness24-corrected.txt records
the bounded reruns. harness24-summary.json lists every initial/corrected full
report and JSON document pin, final checks and counts, and archive/source pins.

All runs used fresh native Ubuntu24 systemd containers with two CPUs, 3 GiB RAM,
UID42420 and no host mounts; all were removed. The approved/development actual
seat routes, consent, exact graph wake reads, one multiple-provider prompt,
none-installed behavior and clear optional GitHub errors passed. GitHub probes
returned authentication errors without anonymous requests. No bundled runtime,
real model, expert/content completion, final release or Ubuntu22 compatibility
is claimed. Incus remains a separate person-owned manual test.

## Expert plumbing on native Ubuntu 24.04

The expert candidate source78c3e6fae05b4e373a54abe4002779613cdc2282 includes
expert855d2250ea76de82fd814d6730215040c8f5bba0 and the final one-shot UI adapter
9dbb0f07c60106c27f5325897748c6c36e206409. Archive SHA256:
2a157a02875134f3ce9059be97fe92a329b4ad551b228b9fc1c33697ed91e679.
The corrected runner a97adc1f1649181b82012fcd232f5072e83f3c30 passed all
**139 assertions**, exit0:

| Scenario | PASS |
| --- | ---: |
| Expert first-run focus, stop/cancel and explicit rerun | 32 |
| Claude before channel install, then restart recovery | 32 |
| Claude development plugin | 26 |
| Claude approved plugin | 26 |
| Codex expert | 23 |

The lifecycle receipt captures the focused Expert composer before navigation,
the exact expert wake read, explicit stop and terminal cancellation, two ordinary
setups preserving the stopped/non-actionable expert and one cancelled historical
run, then one explicit UUID run and a different observed expert incarnation that
reads a new wake. The Claude recovery receipt starts the actual built-in expert
through inline `server:st3` with no user plugin. An ordinary-user channel install
without policy and agent restart switch it to the installed development plugin;
the different current incarnation resumes its managed transcript, accepts consent
and reads the exact distinct post-install wake. The public JSON records both
incarnations and their successful read receipts.

The initial runner9db380173b9e4949aff29927a59c8203ff90d386 matrix exited1
with eight assertion failures across the five cases. Four assumptions were
corrected after examining native receipts and confirming the CLI/projection
contracts with the owner: a single historical run is unwrapped by mission show;
stopped history retains runtime/reachability fields while non-actionable; inline
stdout explains a development channel without printing its exact driver flag;
and restart can return exit2 with its explicit restarted-and-waiting message
before automatic consent finishes. Only that exact nonzero restart outcome is
accepted, followed by the new current incarnation's exact wake read, plugin argv
and consent. The early CLI wait classification remains a limitation. The product
archive was unchanged; all five affected cases were rerun once.

expert24-initial.txt retains the initial failures; expert24-corrected.txt retains
the final runner output. expert24-summary.json pins all twenty immutable full
report/JSON documents, source and packaged binary hashes, checks and limits.
Immutable summary: doc/fleet/smalltalk/onboarding/2026-10-08d/expert24-summary@61849d62ff6dac0577a2d9987f71d742d1629c7516d5c7276db332bd75bdee7a.
Full proof: doc/fleet/smalltalk/onboarding/2026-10-08d/expert-seat-proof@4349e6712105264194ae6590fea8a4fb0a7a7e48425c870aa6ef2142ce21b455.
Twelve local guard/focus/resume/history/restart checks passed and are retained in
rig-checks-expert-corrected.txt. The shared Tart branch also passes eight local
Mac guards and the same twelve rig checks, with no native Mac claim.

All ten owned initial/corrected native24 containers were removed and the shared
slot was released. Each used two CPUs, 3 GiB RAM, UID42420 and no host mounts or
supplied runtime. This proves expert/channel plumbing with native protocol
stand-ins; it does not prove paid-model semantics, finalized gate content,
wrap-up, stock22 expert behavior, macOS/TCC or a real VM.


## Small-machine cache sizing on native Ubuntu 24.04

Production proof source97190ca64120057138e1b1bc84b0e49bf753f822 composes the
foundation with setup-memory sourcefac628632ab97226f0d3358d9f4081f1b1fef372
(PR1893). Archive SHA256:
05d74c1e350b46b60115fc46e5107d2b877bbecb0f8b665c91cbdb6a62646e00.
BUILD.json pins the actual 2048 KiB default and128 retained readers. Final native
receipts total **71 PASS, zero FAIL**:

| Scenario | RAM limit | Selected cache | Persisted override | PASS |
| --- | --- | --- | --- | ---: |
| setup | 3 GiB | 2048 KiB | absent | 20 |
| small-machine | 1 GiB | 2048 KiB | absent | 25 |
| small-machine-512m | 512 MiB | 1024 KiB | 1024 | 26 |

The rig verifies actual guest cgroup memory.max without changing /proc/meminfo
or injecting a cache setting. The1 GiB case's compiled default already fits;
the512 MiB control proves a strict reduction. Main service unit environment,
actual daemon PID environment and live doctor reader target agree initially,
after service restart and after repeated setup. Both small daemons serve the
configured machine API and ordinary doctor has no failed checks. Service restart
creates a different daemon PID. The3 GiB control persists no override.

The initial source987cb0d26faa6f90e875b039fa14f875c49559a0 recorded two failed
API assertions because the rig invoked `st machines ls` instead of `st machines`.
The corrected runner changed only that command; the identical product archive
passed both affected cases on a single bounded rerun. Initial failures remain in
cache24-initial.txt and their pinned full receipts. cache24-corrected.txt retains
the rerun output; cache24-summary.json pins all ten full report/JSON receipts,
source and packaged binary hashes, resource limits and observed cache settings.
Immutable summary: doc/fleet/smalltalk/onboarding/2026-10-08d/cache24-summary@f50a7e47a12014a79f5e7b2da3471897050a556da45be5577c2015e5cdcab2e4.

All five initial/corrected containers were verified removed and the shared slot
released. Each used two CPUs, ada UID42420, stock guest libc/native loader and no
host mounts. An earlier two-container pilot independently confirmed the real
Docker and guest1 GiB/512 MiB caps with active systemd user managers; both pilot
containers were removed. Fifteen local rig checks and eight Tart guard checks
pass; the public repository and effective-merge impact checks pass. No provider,
paid model, expert/content, release, stock22, nativeMac/TCC or realVM proof is
claimed. Page-cache planning targets do not bound total RSS or concurrent bursts.
