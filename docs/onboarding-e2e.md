# Manual onboarding rig

`scripts/onboarding-e2e` runs one fresh Ubuntu machine per scenario, sequentially.
It consumes a verified release archive, copies bytes through stdin, and installs as
`ada`. It never builds Rust, installs a real provider, logs into an account, or runs
a paid model. Docker instances have two CPUs and no host bind mounts. Most use
3 GiB RAM; the small-machine cases use enforced 1 GiB and 512 MiB limits.
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
| small-machine | Actual 1 GiB container limit; automatic cache selection, live daemon, doctor and restart persistence |
| small-machine-512m | Actual 512 MiB limit forces a smaller cache even with the current 2 MiB default |

Linux kernel hostnames cannot contain spaces; hostname-spaces sets a pretty
hostname and also tests rejection of a spaced node argument. The long kernel
hostname uses the maximum legal 63 characters; a 64-character node is rejected.

The small-machine cases require Docker and positive `reader_cache_default_kib`
and `reader_cache_retained_readers` fields in the pinned BUILD.json, taken from
the exact compiled source. The rig verifies the real guest cgroup limit and
derives the expected target from a quarter of RAM divided by retained readers,
bounded by the compiled default. No synthetic `/proc/meminfo`, cache environment
injection or host mount is used. With 128 retained readers, 1 GiB permits 2048 KiB
per reader, so the current 2048 KiB default needs no override; a larger default
must reduce. The 512 MiB control permits 1024 KiB and requires a strict reduction.
Ordinary `setup` on the default 3 GiB Docker machine must persist no override.

When a reduction is selected, the exact `Read cache: N KiB per reader.` summary,
main service environment, actual daemon `/proc/PID/environ` and live doctor
reader-memory target must agree. When the default fits, the summary and both
environment overrides must be absent. The daemon must serve its configured
machine through the API and doctor must have no failed checks. An explicit
service restart must create a different daemon PID with the same cache choice;
ordinary repeated setup must preserve it. Each report records the memory limit.

```sh
scripts/onboarding-e2e --scenario setup --scenario small-machine \
  --scenario small-machine-512m --ubuntu 24.04 --image onb-sysd2404 \
  --archive /tmp/cache-candidate.tar.gz --sha256 HEX --out /tmp/cache-proof
```

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

The full expert phase adds two focused scenarios:

- `expert-lifecycle`: a fresh Codex first-run waits for the focused `Message Expert`
  composer hint with `type or click` before navigating Home. It requires a reachable
  built-in expert and the exact graph wake read, stops the expert, explicitly
  cancels `mission-run/st/onboarding`, and repeats ordinary setup twice. The expert
  must stay stopped and non-actionable in history with one historical run. `setup --onboarding` must
  then restore it with a different observed incarnation and add exactly one UUID
  run alongside cancelled history; the restored expert must read a new exact wake.
- `claude-pre-channel`: `setup --harness claude --claude-channel false` must preserve
  the selected Claude harness, print its inline development route and start the
  built-in expert without the user plugin. The original incarnation must read a
  wake through `server:st3`; user `claude-channel install --no-policy` followed by
  `agents restart` must produce a different reachable incarnation using the
  installed development plugin, accept its consent, and read a new exact wake.

These cases require the expert-seat candidate and reject `--harness-only`.
Historical run counts use `missions show` overview fields, including cancelled
runs; the CLI returns the run itself when exactly one historical run exists.
Stopped seats can retain historical runtime/reachability fields; their stopped,
historical/non-actionable projection is authoritative. The Claude adapter records `ST3_INCARNATION` on every receipt; an old process's
read cannot satisfy the post-restart assertion. The native Claude fixture respects
`CLAUDE_CONFIG_DIR` so the managed transcript is available for resume. Fixtures
read/claim only; a plumbing pass does not prove later gate content or wrap-up.

Restart can return exit2 with the exact restarted-and-waiting-for-input message
before channel consent finishes. The pre-channel case accepts only that recognized
nonzero outcome, then still requires a different current healthy incarnation, its
exact post-install wake read, the installed plugin argv and its consent receipt.
This proves channel recovery; the early restart CLI wait classification remains.

```sh
scripts/onboarding-e2e --scenario expert-lifecycle --scenario claude-pre-channel \
  --scenario claude-policy-absent --scenario claude-policy-present --scenario codex \
  --ubuntu 24.04 --image onb-sysd2404 --archive /tmp/expert-candidate.tar.gz \
  --sha256 HEX --out /tmp/expert-proof
```
