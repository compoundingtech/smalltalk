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
