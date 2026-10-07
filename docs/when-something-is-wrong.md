# When something is wrong

If a populated v0.3.4 store may have unsigned delegation grants, preserve raw state and keys
and follow the [founder signing audit](st3/founder-signing-audit.md) **before** doctor, restart or
export. Doctor can seal pending work; these diagnostics are not the initial read-only capture
for that affected history.

For other stores, start with the smallest question: is the daemon reachable, is the seat ready,
or is the work waiting on a dependency? These checks use the invented garden from
[getting started](getting-started.md):

```sh
st doctor
st service status
st missions show mission-run/garden/first-note/one
st agents show agent/garden/worker
st agents queue agent/garden/worker
```

| What you see | Next check |
| --- | --- |
| Daemon unavailable | `service status` and [startup replay](#startup-replay-and-an-unavailable-api); an active process may still be recovering. |
| Missing `st` or `pty` | [PATH and PTY](#path-pty-and-harness-login); check the daemon user's login-shell PATH too. |
| macOS launch or permission denied | [Signing and permissions](#macos-signing-and-permissions). |
| Seat starting or waiting | Attach to its terminal and finish login, trust, or permission prompts. |
| Seat stopped, failed, or missing from the list | Show its exact ID and history; read its stop reason before starting it. |
| Mission queued or blocked | Its predecessor run, step dependencies, human gate, and seat queue. |
| Message waiting or delivery stale | The recipient's local doctor and delivery assessment; inspect the message receipt. |
| Remote member last seen | Check the encrypted route; a sleeping member is normal and catches up on return. See [replication checks](#replication-and-upgrade-checks). |

To inspect a stopped seat and why it stopped:

```sh
st agents ls --all
st agents show agent/garden/worker --json
st subject history agent/garden/worker --limit 20
st terminals peek agent/garden/worker
```

The last terminal screen can explain a login, crash, or provider limit; the graph says whether the stop was requested, the owner ended, a suspension is active, or the runtime failed. A stopped seat can retain a historical terminal, and a seat with no recorded terminal may have nothing to peek.

## Check usage without starting another turn

```sh
st usage --hours 24 --by agent
st usage --hours 24 --by mission
```

These are observed token totals. Cost is an API-equivalent estimate, and unpriced models are shown as unpriced. Account limits and stop policies can explain why a seat stopped; the seat's reason is more useful than repeatedly restarting it. See [model accounts](st3/accounts.md).

## Recover the cause

Finish a first-run prompt by attaching, then detach with **Ctrl+\\**:

```sh
st terminals attach agent/garden/worker
```

After fixing its dependency, restart a stale running seat; explicitly start an intentionally stopped one. Resume a suspended one instead. [Seat lifecycle](seat-lifecycle.md) gives the commands and their different effects. Retry failed work through the graph after correcting its cause; preserve the failure evidence rather than completing a step just to make the display green.

For a Claude seat with stale delivery, check `st claude-channel status`, then attach and use Claude's `/mcp` menu. If `plugin:st-channel:st` says failed, select it and **Reconnect**. The v0.3.4 rehearsal needed this after some launches; delivery then became `current` and queued work ran. A ready installation does not by itself prove that a seat's channel connected.

If doctor reports an operational contradiction, inspect the bounded repair plan:

```sh
st repair dry-run
```

Apply only the exact reviewed token with the procedure in [operational repair](st3/operational-state/README.md#doctor-and-operational-repair). A changed plan needs a new review; repair appends transitions instead of deleting history.

## What the watchdog does

The runtime watches process liveness, readiness, claim leases, and message delivery; `doctor` reports those facts. A fleet may also run an **operations watchdog mission** that inspects health, groups faults, and routes repairs to an operations agent. That mission is an operating policy, not something every new installation receives automatically. Look for it under Missions, and read its run when it reports a fault; Home should show only what needs your action.

A watcher cannot fix a missing harness login, choose a product preference, or make an expired provider allowance return. Follow the recorded cause, and ask a person through a structured request when only that person can settle it.

See [CLI tour](st3/cli-guided-tour.md), [operational state](st3/operational-state/README.md), and [replication](st3/replication.md) for deeper diagnosis.

## Startup replay and an unavailable API

During daemon startup, `st doctor` reads a local readiness file without waiting for SQLite or
an API listener. It reports `starting`, the current recovery phase, and committed frontier/target
when projection has begun. Full replay also reports its stage and, during base claims, processed
claims/total; those counts describe uncommitted work, not a newer committed frontier. Updates
occur at stage boundaries and every 1,000 base claims. The record changes to `serving` only once
both local API and paired gateway sockets have bound. Serving means the APIs accept requests;
projection health remains a separate doctor check.

`st service status` adds this readiness to the daemon service line, so a service manager's
`active` or `running` state is not mistaken for API readiness. `st doctor --json` includes the
local record as `startup`; if the API is unreachable during recovery it reports a failed
startup check and exits nonzero. Once the API answers, `starting` is informational and the
API's health checks determine the result, even before the gateway has bound.
Command clients that normally announce outage retries return a full-replay phase promptly after
an unsuccessful connection attempt. In ordinary startup phases, they print the phase and keep
waiting under `--daemon-wait`, so a brief restart can finish normally. Long-lived drivers retain
their connect-outage retry behavior. Existing clients still get the
kernel's immediate missing-socket/connection-refused error during recovery and keep their own
retry policy; no early listener accepts requests it cannot yet answer.

The private, rebuildable record is `API_SOCKET.readiness.json`, beside the effective API socket,
with a held `API_SOCKET.readiness.lock` establishing its lifetime. Readers ignore unlocked files,
including a leftover `serving` record after SIGKILL; a restart overwrites the record with
`starting`. These files are local observations, not replicated claims or a client API endpoint.
No client HTTP response schema or API error code changes.

Local startup observation is enabled on Linux. On macOS and other platforms, or when the
observation files or OFD locks are unavailable, startup logs one warning and continues without
a sidecar. Doctor and command clients then use their existing API checks and outage waits.
Only lock-contention errors refuse a second observer; other observation failures never block
startup. Writers and readers resolve socket discovery links and canonicalize the parent
directory, including paths through symlinked directories.


If the frontier or replay counts continue moving, allow recovery to finish. Repeated restarts
can repeat work and make a long replay harder to diagnose. Retain the starting source, target
source, graph size, recovery phase and elapsed time; compare them with the release's impact
notes. On macOS there is no local startup sidecar: use service logs and the existing API checks.
If recovery stops advancing or exits, preserve its bounded error log and report it. Do not
reset state, delete the database or downgrade across a migration to get a reachable API.

## PATH, PTY and harness login

```sh
command -v st st3 stui pty
st --version --json
st doctor
```

Use the bin directory from your chosen archive or Nix route in the daemon user's login-shell
PATH. Open a new terminal after updating your shell profile. Avoid mixing an old archive's
`st` with a new Nix profile or another PTY install. `doctor` checks the daemon's environment;
working commands in your current shell alone do not prove the service sees them. Refresh
service definitions with `st service install` after a changed executable path when they are
manually managed. If Home Manager owns the daemon, update its pinned input and activate that
configuration instead; follow the [Home Manager upgrade](upgrading-st.md#restart-the-services-and-verify).

Log in to the harness as that same OS user, attach to the affected seat and finish login/trust
prompts. Check its declared workspace and [account](st3/accounts.md) rather than copying another
user's credentials. A missing Claude channel needs `st claude-channel status` and the connection
check above. A failed omp/OpenCode admission has exact boundary diagnostics; use
[admission guidance](st3/seat-deploys.md#harness-admission-at-the-next-launch). Attach and inspect
before restarting repeatedly or overriding admission.

## macOS signing and permissions

Verify Python 3 is installed, the configured app path is stable, and the selected signing
identity exists. Release executables are ad-hoc signed, not notarized. Follow macOS's explicit
approval flow for the verified download and run `st service permissions` for the service's
Full Disk Access/Developer Tools guidance. Permissions can need renewed approval after ad-hoc
updates. Do not globally disable Gatekeeper or remove quarantine from unrelated files.
A configured missing or wrong-team signing identity fails instead of falling back; see
[macOS installation](st3/macos-installation.md). The optional voice helper is separate from
ordinary agent and mission use.

## Imported seats and saved sessions

```sh
st import ls --all
st import show SESSION
st agents show agent/import/codex/ID
```

Use actual IDs from discovery; these are placeholders. Check the native session's owning home,
workspace and saved declaration. An older imported Codex seat with explicit `resume` arguments
can time out even after installing a new binary. Follow the [exact declaration repair](seat-lifecycle.md#repair-a-codex-import-created-before-strict-resume);
re-importing does not rewrite an existing seat. Keep exported environment values and transcript
contents private. Inspect delivery state and stop reasons for other stale seats before deciding
whether to resume, start or restart them.

## Replication and upgrade checks

```sh
st replication status
st doctor
```

Compare sources on every member and follow [two-machine checks](two-machines.md#check-what-arrived)
for frontier/digest inspection and a real message receipt. A sleeping member can catch up later;
a mixed-rules fleet cannot finish new checkpoint sealing. Equal replicated history does not prove
equal projections or valid inner claim signatures. Review those doctor checks separately.
Follow [replication recovery](st3/replication.md#recovery); a reset or copying another member's
keys/database is not a normal troubleshooting step.

## Report an install or upgrade problem

Use [Smalltalk GitHub issues](https://github.com/compoundingtech/smalltalk/issues/new/choose)
with the **Install or upgrade problem** template as the single support path. Initial triage is
owned by the Smalltalk distribution agent, which routes
runtime, signing and replication failures to the relevant maintainer. No separate support
account or chat channel is required.

Include the exact installed and responding-daemon sources, OS/architecture, install route,
starting/target release, sanitized doctor and service output, failing command and expected
behavior. If the daemon is unavailable, say that and include its startup phase and elapsed
time instead of waiting for doctor to succeed. Harness problems also need the harness version
and a bounded error excerpt. Preserve timing context; replay observations are not guarantees.

Review output **before posting**. Replace host names, usernames, real paths, seat, person and fleet
IDs, tailnet addresses and repository names with consistent invented values. Remove tokens,
private keys, invite/pairing secrets, bearer credentials, transcript prompts, claim/document
bodies and exported environment values. Keep source hashes, versions, error codes, field names
and relative timing intact. Doctor output, terminal screens and logs are not automatically safe
to publish. Do not upload a database, claim backup, configuration file or full transcript to a
public issue. If an excerpt cannot be made safe, describe the error and say evidence was withheld.
