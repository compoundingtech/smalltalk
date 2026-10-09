# Codex seats without full access

A Codex seat used to launch with `--dangerously-bypass-approvals-and-sandbox` and
`--dangerously-bypass-hook-trust`: no sandbox, no questions. That is still the most capable setting,
and it stays available. A new Codex seat now starts with `--approve-for-me` instead, and the st
operations a seat needs reach the daemon through a small tool bridge rather than through its shell.

Every measurement below is from Codex **0.160.1** on Linux, driving the app-server the way a seat does.
Nothing here is claimed for another release.

## What a seat gets

`st agents new --harness codex` declares `args "--approve-for-me"`. Codex runs commands in the
workspace-write sandbox. When a command needs more (a write outside the workspace, the network, the
daemon socket) Codex asks its automatic reviewer instead of the person.

`--approve-for-me` exists from Codex 0.147. When the installed Codex is older, the driver launches the
seat with `--dangerously-bypass-approvals-and-sandbox` (and `--dangerously-bypass-hook-trust` from
0.131, the first release that has it) and records an `approveForMeUnsupported` diagnostic with the
version. An unreadable version keeps the declared flags.

Planner seats keep `--dangerously-bypass-approvals-and-sandbox`: they run `st launch submit` and other
commands from the shell. They no longer pass the hook-trust flag.

## The tool bridge

The workspace-write sandbox cannot reach the daemon socket. Granting it a socket path works only on
macOS in 0.160.1, and enabling sandbox networking opens every host and port. So the driver gives each
controlled Codex seat a stdio MCP server instead. It adds these to the app-server it starts:

```
-c mcp_servers.st.command="<this st executable>"
-c mcp_servers.st.args=["driver","codex-bridge","--subject=<the seat>"]
-c mcp_servers.st.env_vars=[ST_AGENT, ST3_ENDPOINT, ST3_INCARNATION, ...]
-c mcp_servers.st.default_tools_approval_mode="approve"
```

Nothing is written to the person's Codex configuration. A declaration that sets its own
`mcp_servers.st.*` keeps them, and a Codex session st did not start gets no bridge.

Codex starts the bridge as a trusted local process outside the command sandbox and does not review
its calls, so the bridge enforces the scope itself:

- The acting identity comes from the launch (`--subject`), never from a tool argument.
- Each tool is one fixed st command built as an argument vector, never a shell string. Every value is
  one `--flag=value` argument, so a value cannot become another option.
- Identifiers are checked for shape: a message reference, a `step-run/…` step, an `agent/…` or
  `person/…` recipient, a duration like `30m`.
- Unknown arguments are rejected. Output is bounded, and a command is killed after two minutes.

The tools are the message and mission-work operations `st skill` teaches a seat: `st_messages`,
`st_message_read`, `st_message_reply`, `st_message_archive`, `st_message_send`, `st_work_ls`,
`st_work_show`, `st_work_claim`, `st_work_progress`, `st_work_complete`, `st_work_fail`,
`st_work_release`, `st_work_extend`. Read-only tools carry `readOnlyHint`. Nothing that creates or
stops seats, publishes missions or changes configuration is exposed; those stay shell commands, which
the sandbox blocks and the reviewer decides.

### How Codex approves a mutating MCP tool

Measured by calling tools through the app-server with a probe server (one tool each marked read-only,
destructive, mutating, and unannotated):

| Setup (workspace-write sandbox) | Read-only tool | Every other tool |
| --- | --- | --- |
| `approval_policy=on-request`, default | runs | `mcpServer/elicitation/request` before each call, even with `destructiveHint=false` |
| `on-request` + `approvals_reviewer=auto_review` (`--approve-for-me`) | runs | runs; the reviewer decides, no question reaches the person |
| `on-request` + `mcp_servers.<name>.default_tools_approval_mode="approve"` | runs | runs; no reviewer call |
| `on-request` + `mcp_servers.<name>.tools.<tool>.approval_mode="approve"` | runs | only the named tool runs; the rest ask |

`codex exec` hides this: with no one to ask, it ran every tool. Only the app-server path shows the
questions. The bridge sets `default_tools_approval_mode="approve"` so its tools run whatever the
approval setting, and relies on its own scope checks. A Codex that predates the key ignores it, and
that seat is asked instead.

End to end on 0.160.1 with `on-request`, `workspace-write` and sandbox networking off, a seat called
`st_work_ls` and a mutating `st_work_progress` with no approval request, and the bridge refused a
`--help` step before running anything.

## What each level can do

Eleven scripts, each run by the model as a separate shell command in a scratch workspace. "Outside"
means a path that is not the workspace, `/tmp` or `$TMPDIR` (the sandbox always allows those).

| Task | (c) full bypass | (b) `never` + `workspace-write` + network | (a) `--approve-for-me`, model told not to escalate | (a), model may escalate |
| --- | --- | --- | --- | --- |
| cargo build, target dir in the workspace | pass | pass | pass | pass |
| cargo build, target dir outside | pass | **fail** (read-only file system) | fail | pass (reviewer approved) |
| git init and commit | pass | pass | pass | pass |
| local clone into the workspace | pass | pass | pass | pass |
| local clone outside | pass | **fail** | fail | pass (approved) |
| HTTPS request | pass | pass | fail (no DNS) | pass (approved) |
| download an install script | pass | pass | fail | pass (approved) |
| `git ls-remote` over HTTPS | pass | pass | fail | pass (approved) |
| write under `~/.cache` | pass | **fail** | fail | pass (approved) |
| `st` call from the shell | pass | pass* | fail (socket denied) | pass (approved) |
| open a pty | pass | pass | pass | pass |
| st operations through the bridge | n/a | n/a | pass, no escalation | pass, no escalation |

\* Passes only because sandbox networking also opens the daemon socket.

Flags and keys:

- (a) `--approve-for-me`, which is `approval_policy="on-request"`, `approvals_reviewer="auto_review"`,
  `sandbox_mode="workspace-write"`; plus the bridge overrides above.
- (b) `--ask-for-approval never --sandbox workspace-write -c sandbox_workspace_write.network_access=true`.
- (c) `--dangerously-bypass-approvals-and-sandbox`.

What this means:

- Real builders write caches outside the workspace (cargo, package managers, toolchains). At (b) those
  fail outright, and `never` means the seat cannot ask. Only (a) with a reviewer, or (c), can do that work.
- (a) trades speed for control: each blocked command costs a reviewer call, and the reviewer is a model,
  so a refusal is possible. In this run it approved all seven blocked tasks; that is one run, not a
  guarantee. A refused request reaches the person, which an unattended seat cannot answer.
- (c) is unchanged and remains the choice for a seat that must never stop.

## The hook-trust flag

`--dangerously-bypass-hook-trust` is gone from `st agents new`, the planner seats and the st3 evals and
scripts. Tested with a seat-shaped launch (an app-server and `codex --remote` over a Unix socket, an
isolated `CODEX_HOME`), with and without the flag:

| Case | Result |
| --- | --- |
| fresh user, no hooks | starts, takes a turn |
| user `hooks.json` (listed `untrusted`) | starts with no review dialog, takes a turn, the hook ran |
| `resume` of that thread after an app-server restart, permissions given to the app-server | starts, takes a turn, the hook ran |

The flag changed nothing in these runs. A hooks file in a *project* is a different matter: without
the flag, Codex disables project-local hooks until the project is trusted (the app-server reported
it; the flag with a project hooks file was not run). st2 seats, which render a
project `.codex/hooks.json`, keep the flag in their examples and evals, and so does the native session
import controller, which starts a plain (non-remote) Codex that was not tested.

## Not tested

- Any Codex other than 0.160.1, including the 0.147 and 0.131 cut-offs, which come from the onboarding
  design and research notes, not from a run.
- macOS. The bridge is a plain stdio process, but the sandbox behavior was measured on Linux only.
- A seat started by the daemon end to end. The launch pieces (bridge, app-server flags, TUI, resume)
  were run separately and by unit test.
- A reviewer refusal and what an unattended seat does next.
- The fallback of a per-seat `CODEX_HOME` with a rules file. The bridge made it unnecessary, so it was
  not built.
