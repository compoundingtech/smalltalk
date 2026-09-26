# st3 constructed spawn environment design

This document records a design. It does not change runtime behavior.

## Outcome

Every new member receives one constructed environment.

The runtime never copies the daemon environment into a member.

The same contract applies to exec members, PTY members, native drivers, and manual PTY restarts.

A runtime restart reproduces the same environment from durable inputs.

## Current gap

The current exec runtime adds declared values to `Command`. It does not clear inherited values first.

The current PTY runtime passes repeated `--env` values. The `pty` launcher and member still inherit other values.

The st2 launcher has the same inheritance. It removes only `NO_COLOR` from managed agents.

Declaration expansion also falls back to the daemon environment for an unresolved variable.

These paths let an unrelated `PATH` entry or provider marker enter every child process.

An old `pty` can also ignore `--unset-env`. A successful command therefore does not prove that a removal persisted.

## Environment model

The runtime builds the final environment from four explicit layers.

```text
account base
  + selected host profile
  + agent and task declarations
  + runtime-owned values
  = final member environment
```

The daemon process environment is not a fifth layer.

Later layers replace earlier values. Runtime-owned values replace every conflicting authored value.

Variable expansion reads only values from completed lower layers. An unknown variable is an error.

This rule keeps `PATH "/workspace/bin:$PATH"` useful without reading the daemon's `PATH`.

### Account base

The account base contains only account facts that the runtime derives from the operating system.

| Name | Source | Rule |
| --- | --- | --- |
| `HOME` | The effective user account | The path must be absolute and owned by the effective user. |
| `USER` | The effective user account | The value never comes from the daemon environment. |
| `LOGNAME` | The effective user account | The value equals `USER`. |
| `SHELL` | The effective user account | The path must be absolute. |

The account base does not contain credentials, tool settings, provider state, or process-tree markers.

### Selected host profile

Each host selects one durable spawn profile. The profile is configuration, not captured process state.

The profile supplies the host's approved `PATH`. It can also supply locale and temporary-directory values.

The service installer records the selected profile. Foreground and one-shot runs load the same profile.

The active executable directory is added to the selected `PATH`. It does not come from ambient `PATH`.

Linux derives `XDG_RUNTIME_DIR` from the effective user and verifies its owner before use.

macOS uses its validated per-user temporary directory. Other hosts need an explicit profile value.

The initial profile should support these optional names:

- `PATH`
- `LANG`
- `LC_ALL`
- `LC_CTYPE`
- `TMPDIR`
- `XDG_RUNTIME_DIR`

No other host value enters a member unless the profile names it.

### Authored values

An agent environment overlays the selected host profile. A task environment overlays its agent environment.

Sensitive capabilities require an explicit authored value or a future typed secret binding.

The default environment does not include these examples:

- `SSH_AUTH_SOCK`
- `GPG_AGENT_INFO`
- cloud credentials
- package registry tokens
- `CODEX_HOME`
- `CLAUDE_CONFIG_DIR`
- `NIX_PATH`
- pager and editor settings

File-based provider configuration remains available through the derived `HOME` value.

An agent that needs an SSH or GPG socket must request it explicitly.

### Runtime-owned values

The runtime adds values that identify the exact member and its control plane.

Examples include `ST_AGENT`, `ST3_ENDPOINT`, `ST3_DRIVER_STATE_DIR`, `PTY_ROOT`, and the resolved workspace.

PTY members receive a runtime-owned `TERM`. Exec members receive no terminal identity unless they declare one.

Provider drivers add only their documented values. They never forward existing `CLAUDE_CODE_*` or `CODEX_*` markers.

The working directory sets `PWD` through process launch. The runtime does not copy an ambient `PWD`.

## Execution boundary

The shared implementation belongs in `st-runtime`.

It should expose a typed `SpawnEnvironment` with the final map and per-name provenance.

The builder validates names, rejects NUL bytes, resolves expansion, and protects runtime-owned names.

st2 and st3 must use the same builder and the same PTY protocol.

### Exec members

The exec runtime calls `env_clear()` before it applies the final map.

It resolves launcher programs before clearing the environment. Program lookup must not depend on a cleared ambient `PATH`.

The isolation wrapper receives a minimal control environment. The target receives only the final member environment.

The wrapper API must keep these two environments separate.

Linux control values can include the verified `XDG_RUNTIME_DIR` needed by `systemd-run`.

### PTY members

The PTY protocol needs a persisted replace mode, such as `pty run --clear-env`.

Replace mode means that the member starts with an empty environment before stored `--env` entries apply.

The PTY metadata must retain replace mode. A later `pty restart` must not inherit the restart caller's environment.

Repeated `--unset-env` options cannot provide this guarantee. A later caller can introduce a previously unseen name.

The st2 PTY adapter and the st3 PTY runtime must both require replace mode.

## PTY capability proof

The daemon must prove replace mode by behavior before it launches a managed PTY.

A version string or a successful exit is not sufficient. An old binary can ignore an unknown option.

The proof uses a temporary PTY registry and a random poison name.

1. The probe puts the poison value in the launcher environment.
2. The probe starts a replace-mode PTY whose child writes its environment names.
3. The child output must not contain the poison name.
4. The probe restarts the PTY from a launcher with a second poison name.
5. The second child output must contain neither poison name.
6. The probe removes its process, registry, and files.

The runtime caches success by the exact PTY executable identity and digest.

An unproved or failed capability blocks new PTY launches. It does not stop or replace an adopted PTY.

## Compatibility effects

This change intentionally stops accidental behavior.

Programs can lose language-manager variables, credential sockets, custom certificate variables, and pager settings.

Commands can stop resolving when the selected host `PATH` omits a required tool directory.

Declarations that expand an unknown variable will fail instead of reading an ambient value.

Tests that depend on the developer shell can fail until their fixtures declare complete inputs.

Manual PTY restarts will stop acquiring new values from the operator's terminal.

The migration tool must list every removed ambient dependency. It must not add those values automatically.

Current fleet declarations already show the intended route. They explicitly add recorder paths and related settings.

Provider binaries and common tools belong in the selected host profile. Repository-specific wrappers belong in the declaration.

## Adoption and rollout behavior

An existing member keeps its current environment until an explicit replacement or a normal relaunch.

The runtime does not restart a healthy member to apply this contract.

A member launched before environment receipts existed reports `unknown` environment convergence.

A newly launched member records the exact environment generation and reports `converged` or `drifted`.

The rollout must first prove that every declared program resolves against the selected profile.

## Diagnostics

Each launch receipt records these facts:

- the sorted environment names;
- the source layer for each name;
- the selected host profile revision;
- the PTY replace-mode proof identity;
- a local keyed digest of the complete final map.

Receipts never record secret values. The digest key stays in owner-only local state.

`st3 subject show` should show names, sources, redacted value classes, the profile revision, and the digest.

`st3 doctor environment` should run disposable exec and PTY poison probes.

The doctor report must show expected, observed, missing, and unexpected name counts.

The doctor exits nonzero when it finds a leak. It exits with a different code when it cannot measure.

Linux can also compare a running member through `/proc`. Other hosts rely on launch receipts and disposable probes.

## Acceptance tests

The implementation is complete only when these tests pass:

1. An exec child receives no random ambient poison name.
2. A PTY child receives no random ambient poison name.
3. A manual PTY restart receives no new caller poison name.
4. A declared value reaches both exec and PTY children.
5. A declared `PATH` expands against the selected profile only.
6. An unknown expansion variable fails before launch.
7. Runtime-owned values replace conflicting authored values.
8. Credential sockets stay absent unless declared.
9. st2 and st3 produce the same final map from the same inputs.
10. An adopted old member remains running and reports unknown convergence.
11. The doctor detects a PTY binary that ignores replace mode.
12. A real Codex launch and a real Claude launch save normal transcripts without inherited provider markers.

Every absence test prints the examined environment-name count. An empty observation is not a pass.

## Implementation order

1. Add and prove PTY replace mode.
2. Add `SpawnEnvironment` and the behavioral probe to `st-runtime`.
3. Move st3 execution and terminal launches to the shared contract.
4. Move st2 exec and PTY launches to the same contract.
5. Add receipts, inspection, and doctor checks.
6. Migrate host profiles and explicit declaration dependencies.
7. Run the migration rehearsal and service checks before any deployment.

Each step is separately reversible. No step authorizes a live member restart.

## Decisions that need approval

The implementation needs three product decisions before code starts:

1. The durable KDL surface and storage location for a selected host profile.
2. Whether an explicit socket binding is sufficient for credentials before typed secret bindings exist.
3. Whether foreground `st3 up` refuses a missing profile or uses a small built-in platform profile.

The no-inheritance rule is not one of these decisions. It is the fixed requirement.
