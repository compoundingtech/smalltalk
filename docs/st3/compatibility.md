# Smalltalk 0.x compatibility

For your own fleet, install the **same verified release source on every member** and read the
release's **Upgrade impact** before restart. A patch tag is a publication label, not a promise
of compatible databases, mixed builds or safe rollback. Check exact sources using `BUILD.json`,
`RELEASE.json`, `st --version --json` and the daemon's `machine_version` in `st doctor --json`.
Downloading or installing an archive does not replace a running daemon.

## Upgrade starting points and limits

These are the current documented evidence boundaries, as of 2026-10-07. Use the
[upgrade procedure](../upgrading-st.md) and every intervening release's impact notes when your
starting source is older than the previous published release.

| Starting state | Guidance and evidence |
| --- | --- |
| New empty installation | Use [getting started](../getting-started.md). Initial distribution scope is Linux x86_64 and macOS Apple Silicon, one fleet per person. Archive CI checks packaging and CLI/TUI startup; a clean machine with a real harness login needs a separate onboarding rehearsal. |
| v0.3.15, source `122fefd6` → v0.3.16, source `5c0a151e` | The [v0.3.16 report](https://github.com/compoundingtech/smalltalk/releases/tag/v0.3.16) classifies the full source interval. Schema identity remains 16 and checkpoint rules remain 11, but startup DDL, client behavior and manual steps still change. This establishes classified upgrade guidance, not a completed isolated populated-fleet rehearsal or a guaranteed interruption time. |
| Earlier st3 source | Read all intervening notes, including intermediate migrations and manual repairs. Rehearse a populated copy before rollout; no blanket upgrade or downgrade guarantee covers every earlier 0.x source. |
| Populated v0.3.4 with possible unsigned delegation grants | Preserve raw SQLite state and keys **before doctor, restart or export**. Follow the [founder signing audit](founder-signing-audit.md). Its bounded candidate rehearsal proved recovery of preserved unsealed work; already-sealed unsigned history remains affected. It is not exact v0.3.5 archive acceptance. |
| st2 state | This is a different generation. Use its explicit migration workflow and [st2 guide](../../README.st2.md); do not treat installing an st3 archive as an in-place database upgrade. |

Prefer roll-forward when the old binary cannot read the current database, claim vocabulary or
live driver resume format. Database migrations are forward-only unless a downgrade is explicitly
implemented and verified. A saved archive or the macOS installer's file rollback does not undo
a migration. A [claim restore](backups.md) rebuilds into the restoring build's schema under a
fresh writer; it is not a downgrade or permission to overwrite a running member.

## Independent compatibility contracts

| Contract | What to check |
| --- | --- |
| SQLite schema and startup DDL | Read all transitions in the source interval. A numeric schema identity alone does not describe every startup index or projection change. |
| Checkpoint rules | All participating members must agree on rules before new sealing. Mixed builds can replicate signed history while sealing stalls or projections differ. A rules bump alone does not establish a full-replay trigger. See [checkpoints](checkpoints.md). |
| Claim registry and replication | Admitted history, known claim vocabulary, projection digests and signature verification are separate checks. Equal replication digests alone do not establish claim-signature validity. See [replication](replication.md). |
| Client API | `st3.client.v0`, projection versions and capability versions are distinct from release tags and database schema. Discover capabilities first; unknown required capability versions stop the client, while unknown optional capabilities may be ignored. See the [client contract](client-v0/README.md#discovery-lists-and-details). |
| Native harness and driver resume | Provider APIs, managed extensions and saved resume state have their own contracts. See [seats across deploys](seat-deploys.md); rolling back across the push-delivery boundary can require coordinated seat restarts. |

Use CLI and the terminal UI from the same release source. Current client version scope is:

| Client | Version/evidence boundary |
| --- | --- |
| CLI and the terminal UI | v0.3.16 archives embed source `5c0a151e`; native release CI checks both tools. This is not a cross-version client guarantee. |
| TypeScript `@smalltalk/st3-client` | Repository-private package version `0.1.0`; CI contract/schema tests run against its source revision. The package number does not identify the daemon source. |
| Swift `St3Client` / iOS app | In-tree client source and client-v0 capability contract; no separate released client-version compatibility matrix is claimed. Fresh-device app onboarding is separate evidence. |

The Swift and TypeScript clients share the client-v0 contract; optional features are negotiated
with the responding daemon.
Matching API labels do not promise every feature on an older daemon. For example, native content
blocks require an upgraded serving/owner member and client opt-in; older clients keep the legacy
shape. Consult the release notes before using new client features.

## Harness support and test coverage

Install and log in to the harness as the OS user who runs the daemon. Record its actual version
when diagnosing an issue; Smalltalk does not publish a universal minimum-version promise for
all future vendor releases. No exhaustive real-provider version matrix is established for
v0.3.16. The rows below describe tested native contracts, not acceptance of every vendor version.

| Harness | Existing coverage and launch boundary |
| --- | --- |
| Claude Code | Managed channel, hook and transcript contracts are covered by boot/delivery regressions. Install the owned channel and verify its connection, then complete real login/trust prompts. |
| Codex | App-server, native thread binding, import and resume contracts are covered by boot/delivery regressions. Saved imports made with authored `resume` arguments need the [declaration repair](../seat-lifecycle.md#repair-a-codex-import-created-before-strict-resume). |
| pi and omp | Managed extension, mailbox and resume contracts have boot/delivery regressions. An unseen omp build is measured in a disposable native admission probe before launch; a failed probe refuses launch and reports the boundary. |
| OpenCode | Loopback API, mailbox and resume contracts have boot/delivery regressions. An unseen build is measured before launch; a failed native contract disables delivery and retains queued mail. |

The [boot canaries](../../scripts/st3-boot-canaries/README.md) use real Smalltalk processes with
stand-in providers; they do not prove real vendor login, model turns or every installed vendor
version. Native admission probes identify the exact installed build, and their exceptions expire
when the build identity changes. Follow [admission diagnosis](seat-deploys.md#harness-admission-at-the-next-launch)
rather than bypassing a failed boundary without understanding it.

Historical cross-build tests pin exact sources in
[the fleet baseline](../../.github/fleet-compat-baseline.json) and
[the messaging baseline](../../.github/messaging-compat-baseline.json). Those fixtures prove their
specified histories and messaging contracts, not arbitrary mixed fleets or downgrade support.
The friend-ready idle and delivery gates remain separate evidence; packaging or documentation
checks do not replace their continuous observations.

## Platform verification

| Route | Requirements and current evidence |
| --- | --- |
| Linux x86_64 archive | glibc 2.35+ (for example Ubuntu 22.04), Bash or Zsh, CA certificates, `curl`, `tar`, a checksum tool and Git for the walkthrough. Persistent service setup needs a systemd user manager; enable lingering for work after logout. Archives include the pinned PTY runtime and need neither Rust nor Nix. Native release CI verifies extraction, installation, exact source and CLI/TUI smoke tests. |
| Apple Silicon archive | macOS 15+, `curl`, `tar`, `shasum`, Git and Python 3. Installer creates `SmallTalk.app`, with ad-hoc signing unless explicitly configured otherwise. LaunchAgent permissions and login remain user setup. Optional voice-helper builds need Xcode's macOS 26 SDK; voice is not an onboarding prerequisite. |
| Nix profile or Home Manager | A Nix installation with `nix-command` and `flakes` enabled. It supplies build dependencies and PTY; pin the same source across members. Home Manager owns service paths when that route is selected. Harness login and host permissions remain separate. |

A **fresh Mac onboarding rehearsal remains unverified**. The separate macOS workspace CI is
[manually disabled](https://github.com/compoundingtech/smalltalk/actions/workflows/macos.yml)
as checked on 2026-10-07. Native Apple Silicon release builds and archive smoke checks still run;
they do not establish a clean user's signing, permissions, harness login and first mission.
The approved clean Linux, Apple Silicon and Nix rehearsals must retain exact sources and actual
prerequisites before this status changes. [Report an install or upgrade problem](../when-something-is-wrong.md#report-an-install-or-upgrade-problem).
