# Manual onboarding rig

`scripts/onboarding-e2e` runs one fresh Ubuntu machine per scenario, sequentially.
It consumes a verified release archive, copies bytes through stdin, and installs as
`ada`. It never builds Rust, installs a real provider, logs into an account, or runs
a paid model. Docker instances have two CPUs, 3 GiB RAM and no host bind mounts.
The systemd image uses a privileged container, matching the original experiment.
Docker ada uses UID42420, including when adapting an older equivalent image,
so its user manager does not share the host developer's per-UID inotify quota.
The host kernel limits are unchanged; reports record the actual fixture UID.

Run the pinned old-release baseline:

```sh
scripts/onboarding-e2e baseline --out /var/tmp/onboarding-baseline
```

The default baseline is v0.3.17, rather than a moving latest release. It downloads
the archive and adjacent checksum, records the hash and BUILD.json, then runs
Ubuntu 22.04 and 24.04. An expected old bug is XFAIL; an unexpected improvement is
XPASS and requires reviewing the baseline. FAIL and XPASS return exit 1.

Test an immutable local candidate without another build:

```sh
scripts/onboarding-e2e setup --archive /tmp/candidate.tar.gz \
  --ubuntu 24.04 --out /var/tmp/onboarding-candidate
```

A local archive needs an adjacent `.sha256` file, or `--sha256 HEX`. Its package
directory must contain BUILD.json, bin/st3, bin/st (a relative link) and bin/pty.
Do not copy a host symlink to pty without resolving its executable target. A local
native debug binary may require a newer glibc; package metadata does not establish
compatibility with an older guest. `--strict-release` additionally checks clean
version stderr, release version wording and absence of installed stui.

The setup scenario checks config merge, person and node, automatic self-install,
fresh login PATH, actual user service, doctor, plain-st TUI, idempotence and reboot
behavior. The PTY removes ST_AGENT, answers the device-attributes query, waits for
the working-count frame marker, selects Home/Now via the command palette and
exits with Ctrl+Q. Full terminal output is retained for reviewing the selected
view. No test assumes that Home was the initial view.

Select additional scenarios with repeated `--scenario` and `--ubuntu` flags:

| Scenario | Fixture and assertion |
| --- | --- |
| first-run | Three initial answers in a real PTY; default service yes |
| no-harness | No expert or mission; missing provider diagnosed within eight seconds |
| claude-policy-absent | Native Claude fixture; driver accepts development dialog; channel receipt |
| claude-policy-present | Exact disposable managed fragment; approved plugin route |
| codex | Existing native app-server and TUI fixture; successful message read |
| both | Both providers available; one choice question selecting Claude |
| no-gh, gh-logged-out, gh-broken | Codex fixture; optional GitHub failure does not prevent setup |
| no-manager | Plain container, default service request falls back to detached daemon |
| hostname-localhost, hostname-spaces, hostname-long | First run and reserved/spaced/overlong input rejection |
| existing-store | Seed real state and an unrelated mission run; preserve it on setup |
| linger-on, linger-off | Root-only manager probe after restart, before any test-user login |

Linux kernel hostnames cannot contain spaces; hostname-spaces sets a pretty
hostname and also tests rejection of a spaced node argument. The long kernel
hostname uses the maximum legal 63 characters; a 64-character node is rejected.

Provider scenarios use `scripts/st3-boot-canaries` and the Claude adapter in
`scripts/onboarding-fixtures`. For the harness-and-channel step, add
`--harness-only`: setup still probes providers and answers the choice, but the
runner asserts the exact Selected harness output once, creates its own disposable
native-driver seat, sends a graph message,
requires a successful read of that exact message plus route/consent or app-server
receipts, and stops the seat. It does not depend on the built-in expert or publish
onboarding. The default provider scenarios retain the expert and one-run checks.
The policy-present fixture runs the candidate's `claude-channel install-policy`
helper as root inside the disposable guest and verifies ordinary-user readability.
Optional GitHub probes run as the live test seat, as required by `st gh watch`.
For example:

```sh
scripts/onboarding-e2e --harness-only --scenario codex \
  --archive /tmp/candidate.tar.gz --ubuntu 24.04 --out /tmp/harness-proof
```

The fixtures come from `scripts/st3-boot-canaries` and the adapter in
`scripts/onboarding-fixtures`. The adapter implements the local plugin CLI and
observed consent text; the shared fixture runs real hooks, MCP and message reads.
These prove transport, not provider availability or model judgment. The existing
fixtures read and claim steps; they do not complete the onboarding content.
`--require-wrap-up --mission-timeout 90` requires the graph to show a completed
onboarding run and fails when it does not. Paid content evals belong to the
mission-content step. Do not report a basic stub pass as wrap-up evidence.

Each scenario writes `result.json` and `report.md` under OUT/RELEASE-SCENARIO, with
exact commands, exit codes, separate stdout/stderr and elapsed seconds. Output
directories must be new. `--keep` retains the uniquely named instance for debugging;
otherwise the runner removes its own instance on both success and failure. Root
commands provision a disposable guest and inspect reboot behavior; setup itself
runs as the ordinary user and is never given a password.

An existing equivalent Docker image can be selected with `--image onb-sysd`
(22.04) or `--image onb-sysd2404` (24.04). The rig verifies the actual guest release
and initial absence of development tools and providers. Coordinate the shared
one-container slot with other manual testers; the runner's host lock serializes
its own invocations. Real VM instructions are in [onboarding-vm-runbook.md](onboarding-vm-runbook.md).

For macOS, use `scripts/onboarding-mac-test` or `--backend tart` and follow
[onboarding-mac-runbook.md](onboarding-mac-runbook.md). Its first-run/setup/no-harness
cases use a prepared clean Apple Silicon base, native app/signature/launchd checks
and a GUI-login reboot check. Linux-only and provider-phase options are rejected.
