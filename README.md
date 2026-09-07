# st2

st2 runs a declarative network of Codex, Claude, and pi agents from one catalog. It owns process
reconciliation, native messages, normalized terminal DING delivery, presence, durable context,
workspace materialization, and explicit teardown.

Canonical KDL is the authoring interface. Publish one explicit Agent Spec transactionally with
`st2 agent publish`; st2 does not compile human intent into declarations.

## Product intent and implementation contract

Read the [vision](docs/vrs/vision.md), [requirements](docs/vrs/requirements.md), and
[specification](docs/vrs/spec.md) before changing product behavior. Update `docs/vrs/spec.md` with
implementation changes. Nathan must approve changes to `docs/vrs/vision.md` or
`docs/vrs/requirements.md`.

## Install

Prerequisites:

- Rust and Cargo;
- `pty` on `PATH` with `pty run --unset-env` support;
- at least one supported harness on `PATH`: `codex`, `claude`, or `pi`;
  (`codex` is admitted by an exact version allowlist; `pi` is not version-pinned, but the shipped pi
  extension is type-checked against a pinned pi release by `nix flake check`);
- Git when a declaration materializes workspace files;
- Bash and `jq` on `PATH` when lifecycle hooks are enabled.

From a checkout:

```sh
cargo install --path . --locked
st2 --help
pty --help
st2 hooks install
st2 hooks verify
st2 claude-channel install
st2 claude-channel status
```

When upgrading, deploy and activate the compatible `pty` before this version of
`st2`. An older `pty` may silently ignore the unknown `--unset-env` option and
still launch the agent without persisting the removal. The initial environment
can therefore look correct while a later restart reintroduces the caller's
ambient value. The Nix input and development shell pin the compatible artifact;
Cargo installs rely on the operator to satisfy this runtime prerequisite.

The standard catalog is:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/st2/default/catalog
```

Every catalog-aware command accepts `--catalog`; otherwise st2 uses `$CATALOG`, then that standard
location.

### A catalog may declare its session registry

A catalog's tasks live in `<catalog>/pty` unless the catalog says otherwise. To put several catalogs
in one host-wide `pty` registry — so any viewer enumerates every session without knowing which
catalog produced it — declare it in `<catalog>/catalog.kdl`:

```kdl
catalog {
  pty-root "/run/agents/pty"
}
```

`pty-root` accepts `$VAR`/`$CATALOG`; a relative value anchors at the catalog root. The resolution
order is an exported `PTY_ROOT` (a deliberate override, used by `st2 eval` for a short socket path),
then this declaration, then `<catalog>/pty`. A catalog that declares nothing is unaffected.

Prefer the declaration over exporting `PTY_ROOT` into readers: a reader that misses the export
resolves a different registry and reports live agents as dead. When adopting it on a host whose
systemd unit was installed with an ambient `PTY_ROOT`, reinstall the unit without one
(`st2 service install`) — the export still wins, and leaving it pins the supervisor to the old
registry while everything else follows the catalog.

### A catalog may declare how long retirement is kept

`st2 up` archives retired, quiescent seats itself, so a long-lived catalog stays bounded without
anyone remembering the chore. The grace period is `archive-after`, declared beside the registry:

```kdl
catalog {
  pty-root "/run/agents/pty"
  archive-after "7d"
}
```

It accepts the usual duration spellings (`90`, `30m`, `12h`, `7d`; a bare number is seconds) and
defaults to `7d`. `archive-after "0"` disables the automatic step without disabling
`st2 catalog archive`. A value st2 cannot parse fails `st2 validate` rather than falling back to the
default: archiving on a clock the operator did not write is the one outcome this setting exists to
prevent.

Each pass archives at most 25 seats, so a catalog holding hundreds of retirements drains over
several passes, and it never queues for the authoring lock — a pass blocked behind
`st2 catalog apply` would stall every live agent's reconciliation, and a due seat is still due next
pass. Because st2 records no timestamp for a desired-state edit, the clock starts when the
supervisor first observes the retirement; it keeps that observation in
`<catalog>/.st2/retired-observed.json`, never in the spec, and drops a seat's row as soon as it
stops being retired — so un-retiring and retiring again serves a fresh grace period.

Lifecycle hooks are installed only by the explicit `st2 hooks install` command. The installer
publishes an immutable content-addressed set, then atomically selects it with a receipt. `st2 up`
verifies its own immutable set for Codex launches; any local workspace render that actually
references `$ST_HOOKS` verifies that set before writing. Selecting a successor does not invalidate
an older running binary's installed set during cutover. Hook-free materialization does not require
an installed set. These checks never create, refresh, or rewrite hooks. To select this binary's
exact hook set when the installed and candidate builds are older or cannot be ordered, use `st2
hooks install --replace`. `st2 hooks verify-own` is the read-only cutover probe for an installed
binary that may no longer be selected.

`ST_HOOKS` overrides the machine-local hook root for installation, verification, and managed tasks.
During materialization, hook commands such as `$ST_HOOKS/codex-stop.sh` resolve to the invoking
binary's immutable set, so rendered settings are versioned without embedding a machine-specific
root in the declaration.

The hooks have a small operational purpose: session-start restores durable context and exposes the
current inbox; pre-compact preserves a recovery breadcrumb when no context was written; stop and
failure hooks surface newly arrived work or a harness failure. They fail open so hook trouble does
not prevent the harness from starting or stopping.

`st2 claude-channel install` publishes the Claude Code marketplace and plugin embedded in the st2
binary. It registers the plugin for the current user and installs one machine policy fragment with
`sudo`. The policy approves the stable `st2-channel@st2` identity. The plugin starts the st2 MCP
server through the managed task's `PATH`, `CATALOG`, and `ST_AGENT`; it writes no `.mcp.json` file
into a product workspace. Re-running the command updates the marketplace and reinstalls the exact
embedded plugin. `st2 claude-channel status` verifies all four parts. `uninstall` removes only the
st2 user state and its policy fragment. Installation is optional. When the plugin is absent, the
native driver passes an inline MCP declaration and uses Claude's development channel. Claude can
show a confirmation prompt on that path, so install the plugin for unattended agents. Claude treats
the managed plugin list as an allowlist. Administrators must include any other approved channel
plugins in their managed policy.

The same immutable set also carries `pi-channel.ts`. pi has no hook mechanism of its own — an
extension is where a pi session exposes that surface — so st2 ships one and `st2 driver pi-session`
splices it into the launch from the set this binary verified. A declaration never names it, and a
host whose selected set predates it is told to run `st2 hooks install` rather than launching
without a channel. That extension carries the pi equivalent of the session-start hook: st2 composes
the same restored working state, boot ritual, and unread-inbox listing and puts it on the channel's
opening frame, and the extension waits for that frame before the session's first turn, so a
restarted pi agent boots knowing what it was doing. A managed pi seat also runs with `PI_OFFLINE=1`
and `PI_SKIP_VERSION_CHECK=1` unless its declaration sets them, so a supervised agent does not
self-update or make its boot latency depend on the network.

## Author a native agent

Start from the maintained [Codex](examples/native/agent-codex.kdl),
[Claude](examples/native/agent-claude.kdl), or [pi](examples/native/agent-pi.kdl)
declaration:

```sh
export CATALOG="${XDG_STATE_HOME:-$HOME/.local/state}/st2/default/catalog"
bundle="$(mktemp -d)"
mkdir -p "$bundle/assets"
cp examples/native/agent-codex.kdl "$bundle/agent.kdl"
cp ./composed-AGENTS.md "$bundle/assets/AGENTS.md"
${EDITOR:-vi} "$bundle/agent.kdl"
input_sha256="$(st2 agent digest --bundle "$bundle")"
st2 agent publish --catalog "$CATALOG" --bundle "$bundle" \
  --input-sha256 "$input_sha256" --expect-absent --json
```

Replace `<host>`, `<identity>`, `<workspace>`, and `<boot prompt>`. Include every file referenced by
`copy` in the bundle. For a later declaration-only update, publish `agent.kdl` with the current
declaration's SHA-256 via `--spec ... --expect-sha256 HEX`; sibling assets and state are preserved.
Bind either operation to the exact captured source with the SHA-256 returned by
`st2 agent digest`.

To prepare and apply a complete declaration-plane replacement without copying
runtime state or workspaces:

```sh
st2 catalog snapshot --catalog "$CATALOG" --output ./prepared --json
# Edit/render ./prepared. Retain rootSha256 from snapshot as the incumbent CAS,
# and afterRootSha256 from diff as the exact desired-input CAS.
st2 catalog diff --catalog "$CATALOG" --prepared ./prepared \
  --expect-sha256 <rootSha256> --json
st2 catalog apply --catalog "$CATALOG" --prepared ./prepared \
  --input-sha256 <afterRootSha256> --expect-sha256 <rootSha256> --json
```

If the incumbent declarations cannot be parsed or must remain opaque to the
current parser, bind a one-time repair to their exact structural bytes instead:

```sh
st2 catalog snapshot --catalog "$CATALOG" --output ./invalid-preimage \
  --raw-preimage --json
# Produce a fully valid ./prepared directory from that capture.
st2 catalog digest --catalog "$CATALOG" --prepared ./prepared --json
st2 catalog apply --catalog "$CATALOG" --prepared ./prepared \
  --input-sha256 <rootSha256> --expect-sha256 <raw-rootSha256> \
  --raw-preimage --json
```

Raw-preimage mode has its own hash and receipt schemas. It makes no semantic
assertion about the live bytes—including validity, profiles, the catalog
envelope, or effective PTY root—while still fully validating the prepared and
applied catalogs. The caller-supplied raw-domain digest is the exact live
precondition. This is a byte-oriented transaction, not a validation bypass or
migration-policy engine.

To publish that exact snapshot as a new, absent catalog:

```sh
st2 catalog bootstrap --catalog "$NEW_CATALOG" --prepared ./prepared \
  --input-sha256 <rootSha256> --json
```

Ordinary `catalog apply` is policy-free. It rejects state/control content,
symlinks, unprojected workspace facts, catalog-local/default PTY roots, and
effective PTY-root changes. Raw-preimage apply replaces those live semantic
checks with its exact byte-domain CAS. Bootstrap is a separate create-only
declaration transaction, not an apply mode. It atomically publishes absence or
the complete catalog, initializes its persistent lock and generation before
visibility, and never reads or writes the external PTY registry. Process
adoption and PTY-root migration remain separate because that registry has
independent producers. A crash during apply leaves a durable marker and
content-addressed stage. `st2 catalog apply --catalog "$CATALOG" --resume --json`
resumes without the original prepared source. Snapshots own the complete
bounded `_templates` library and empty canonical per-agent `.workspace`
directory facts, but never traverse, hash, copy, or delete workspace content.

A retired declaration is runtime teardown only: it keeps its spec and `resources/` byte-identical
and stays reversible, so a long-lived catalog accumulates retired identities. Archival is the
pressure valve that keeps the live plane bounded:

```sh
st2 catalog archive --catalog "$CATALOG" --all-retired --dry-run --json
st2 catalog archive --catalog "$CATALOG" --identity <identity> --json
st2 catalog unarchive --catalog "$CATALOG" <identity> --json
```

Archival moves the whole identity directory from `agents/<host>/<identity>` to
`.st2/archive/<host>/<identity>` under the exclusive catalog-authoring lock, in one generation
commit, as a same-filesystem rename — the archive root is a child of the catalog root, so no bytes
are copied and no partial bundle can exist. `.st2` is control space at any depth, so an archived
declaration is undiscoverable rather than filtered, and the whole-catalog transaction never projects
it. A tombstone beside the moved directory keeps the identity traceable as one `archived` row in
`st2 catalog graph --json`.

Eligibility fails closed against the local host only, because another host's runtime records are not
observable from here: the declaration must sit at its canonical path, be retired in either spelling,
have no live or dead record for any declared task (the rule `st2 doctor` already applies to
retirement), and be named as `supervisor` by no declaration that stays behind. `--identity` refuses
the whole run if any named identity is ineligible; `--all-retired` reports the ineligible ones and
archives the rest. `st2 catalog unarchive` is the exact reverse move.

`st2 up` applies exactly this gate on its own once a retirement outlives `archive-after`, so these
verbs are for the seats you do not want to wait out and for putting one back.

The compact declaration shape is:

```kdl
agent "<identity>" {
  host "<host>"
  workspace "<workspace>"
  resource "work" uri="github-issue://example/project/123" reason="release work item"
  // Optional metadata:
  // role "worker"
  // supervisor "<supervisor-bus-id>"
  // name "Release worker"
  // description "Owns release preparation and verification."
  argv "codex" "--dangerously-bypass-approvals-and-sandbox" "--dangerously-bypass-hook-trust" "<boot prompt>"
  ding

  render {
    copy "assets/AGENTS.md" "AGENTS.md"
    json-upsert ".codex/hooks.json" #"""
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"$ST_HOOKS/codex-session-start.sh","timeout":5}]}]}}
"""#
    git-exclude "AGENTS.md" ".codex/hooks.json"
  }
}
```

For a zero-interruption migration, add `lifecycle "adopt-only"` to the compact
agent or to an explicit `pty`/`exec` task. st2 adopts that task when its current
generation is alive. If the generation is dead or absent, st2 reports the task
as `held` and does not remove or launch that task. A compact `ding` is derived
from the generated agent task: while that agent is held, st2 suppresses a
missing DING and stops an exact generated DING proved live:

```kdl
agent "<identity>" {
  host "<host>"
  workspace "<workspace>"
  lifecycle "adopt-only"
  argv "codex" "<boot prompt>"
}
```

This is a fence, not a restart policy. After inspecting or recovering the
original generation, deliberately change the lifecycle back to `"service"` (or
remove the field) to authorize ordinary absent launch and dead replacement.
Whole-agent desired state remains an explicit, separate lifecycle instruction
and takes precedence over task lifecycle. To keep an agent in the catalog while
running none of its tasks:

```sh
st2 --catalog <catalog> agent desired-state <identity> suspended \
  --reason "Waiting for capacity"
st2 --catalog <catalog> agent desired-state <identity> running
st2 --catalog <catalog> agent desired-state <identity> retired \
  --reason "Mission complete"
```

The canonical KDL is `desired-state "suspended" reason="..."`. Running is
canonically omitted. New suspended and retired states require a bounded reason;
legacy `retired #true` remains readable. Suspension stops the agent and its
derived DING without deleting its declaration, inbox, context, or Resources.
Resume uses ordinary `service`, `adopt-only`, and `keep` behavior. Nix-owned
declarations must be changed at their Nix source.

`resource` binds an agent-local semantic name to an exact RFC 3986 absolute URI. The URI scheme
selects an open, downstream-owned Resource profile; st2 preserves URI bytes without normalization.
It neither registers schemes, owns profile schemas, nor resolves targets.
Binding order is irrelevant and names must be unique within the agent:

```kdl
resource "work" uri="github-issue://example/project/123" reason="release work item"
resource "source" uri="worktree://github.com/example/project/change" reason="primary checkout"
resource "delivery" uri="ding://host/agent" reason="notification channel for this agent"
```

The envelope is `name` + `uri` + a required human-facing `reason`, plus an optional
`inactive-reason` that preserves a retired binding without deleting it. It carries no
access, readiness, or lifecycle policy, and URI possession conveys no authority. A Resource URI may
be referenced by any number of agent declarations. Resource-only declaration edits do not stop,
replace, or relaunch a live task. Resource profiles and resolvers remain opaque to st2; catalog
readers use the public `agent-spec` crate to inspect the bindings, and `st2 resource ls|read`
projects them for one agent.

The positional agent value is the stable automation identity. Optional `name` and `description`
fields are presentation only; they never route messages, select tasks, or rename durable state.
Mutate a catalog-owned KDL declaration through the constrained commands:

```sh
st2 rename <stable-id> "Release worker"
st2 describe <stable-id> "Owns release preparation and verification."
st2 rename <stable-id> --clear
st2 resource add <name> --uri <uri> --reason "<why this agent carries it>"
st2 resource remove <name>
st2 resource rename <old> <new>
```

These commands preserve unrelated KDL bytes and serialize local writers through the persistent
shared `.st2/catalog-authoring.lock`. They refuse TOML, JSON, and
explicitly `meta { managed-by "nix" }` targets. Nix generators must emit that marker before the
compatible st2 binary is activated. In the trusted single-operator fleet, caller-supplied
`ST_AGENT` limits an invocation to itself or declared descendants; it is a guardrail rather than
authentication, and absence selects the operator path. The sibling `<agent-dir>/name` convention
is hard-retired and ignored.

`argv` launches its first value directly with the remaining values as arguments. It resolves a bare
program such as `codex` through the task environment's `PATH`, preserves argument boundaries, and
does not introduce a shell. Use `command #"..."#` instead when the task intentionally needs shell
syntax such as pipelines, redirects, or variable expansion; `command` continues to run under
`sh -c`. A runnable task must declare exactly one of `argv` or `command`.

### Scheduled work is coming soon, not implemented

st2 does not parse or run scheduled entries today. The intended direction starts with a declarative
DING targeted at the containing agent; scheduled PTY or exec work may be considered later. This is a
non-functional preview, and `st2 validate` correctly rejects it today:

```kdl
agent "<identity>" {
  // Current implemented fields and tasks go here.

  // FUTURE ONLY — reserved and rejected by the current contract.
  schedule "local-health" {
    every "2h"
    ding "Run the local health check."
  }
}
```

st2 provides `CATALOG`, flat native `ST_ROOT`, local `PTY_ROOT`, `ST_AGENT`, and `ST_HOOKS` to the
task. The complete st2-managed overlay is also persisted in PTY metadata, so a manual `pty restart`
retains those values. Declarations should not contain machine-specific install paths.

## Validate and materialize

Gate the declaration before starting anything:

```sh
st2 hooks verify
st2 validate --catalog "$CATALOG" --strict
st2 up --catalog "$CATALOG" --host <host> --materialize-only
```

Validation always checks the whole synced catalog structurally. External workspace and task-path
presence is checked only for the selected run host, which defaults to the local short hostname; use
`--host <host>` to validate another machine's local paths. Supervisors may be declared as either a
bare identity or the fully-qualified `<host>.<identity>` bus id.

Materialization simulates all content operations before writing. It refuses any real change to a
Git-tracked target, including a mode-only change; byte-identical content with the declared mode is
accepted. Inspect the declared targets and keep generated overlays untracked. Detection invokes
`git` and fails closed if the executable is unavailable or a workspace that appears to be a worktree
cannot be inspected.

Each content directive accepts `executable=#true`. st2 applies exact mode `0755` when the property is
true and exact mode `0644` when it is absent or false. Materialization repairs a wrong existing mode
even when the file bytes already match. A `copy` source mode does not affect the destination mode.
An operation whose bytes and mode already match is not reported as materialized.

Inline content uses the existing `file` directive. st2 writes the decoded KDL string without adding
or removing a newline. A blank line before a multiline string's closing delimiter encodes one final
newline:

```kdl
file ".st2/bin/probe" executable=#true {
  content #"""
#!/bin/sh
printf 'ready\n'

"""#
}
```

Use `ensure-line` when a workspace-owned text file must import a catalog-owned contract:

```kdl
copy "_templates/CONTRACT.md" ".st2/CONTRACT.md"
ensure-line "CLAUDE.md" "@.st2/CONTRACT.md"
ensure-line "AGENTS.md" "@.st2/CONTRACT.md"
```

Each harness loads the import from its native contract file. A later boot-prompt rewrite cannot
silently remove the contract-loading instruction. On 2026-09-01, Nathan stated that Codex supports
`@` imports in `AGENTS.md`. This Codex behavior is undocumented and was not source-verified for this
change.

`ensure-line` searches a UTF-8 file for one exact full line. An existing match causes no write.
If the line is absent, st2 preserves all existing bytes, adds a separator newline when necessary,
and appends the declared line with a final newline. st2 creates a missing target in a non-Git
workspace or when Git does not track that path.

For a Git-tracked target, `ensure-line` is a verifier. st2 accepts an exact existing line but refuses
to add a missing line. The refusal occurs before any render operation writes to the workspace. The
repository owner must add and commit the line outside st2. This rule prevents materialization from
leaving a customer repository dirty or changing a repository that assistants can only read.

`git-exclude` is advisory. `copy`, `file`, `json-upsert`, and `ensure-line` are boot-gating.

## Run

On headless Linux, install the systemd user service:

```sh
st2 service install --catalog "$CATALOG" --host <host>
st2 service status
```

Each task runs in its own transient scope, so restarting the supervisor does not kill live agents.

On macOS, use a manual one-shot reconcile:

```sh
st2 up --catalog "$CATALOG" --host <host> --once
```

There is intentionally no resident macOS service path.

For a shortest-path change to one exact task, render only its owning agent and reconcile only that
task in a bounded pass:

```sh
st2 up --catalog "$CATALOG" --host <host> --once --task <host.agent.task>
```

Unknown, ambiguous, and wrong-host task selectors refuse before workspace writes or PTY inspection.
A selected generated DING that is dead or absent is reported as `held`, because starting its
canonical agent would broaden the exact-task operation. Explicit sibling tasks remain independently
selectable.

`st2 doctor` accepts the absence of a live host lock as the normal manual/`--once` mode. For a
resident `st2 up` deployment, use `st2 doctor --require-supervisor` to make a missing loop fail the
health check. A stale lock left by a dead supervisor is always a failure. The underlying
non-interactive `pty list --json` runtime probe is bounded; a wedged client becomes an explicit
doctor failure instead of hanging the health check. Its short outer deadline is containment for a
wedged runtime, not a fleet-scalability mechanism.

For a foreground supervisor on any host:

```sh
st2 up --catalog "$CATALOG" --host <host>
```

### Typed task inventory (diagnostic only)

Use the typed inventory instead of parsing `doctor` prose:

```sh
st2 tasks --catalog "$CATALOG" --host <host> --json
```

The `st2.task-inventory.v2` envelope joins the selected host's desired PTY and exec tasks to
read-only runtime evidence. A complete observation exits zero. Catalog parse errors, declaration
drift during observation, duplicate runtime IDs, timeouts, malformed output, PID reuse, and
otherwise unprovable generations emit `complete: false` and exit non-zero. Missing runtime rows
become `absent` only when the corresponding backend observation is complete; uncertainty remains
`indeterminate`.

Every runtime has a tagged `resourceTarget`. A live Linux process reports
`{"type":"linuxCgroupV2","path":"/..."}` from its exact unified
`/proc/<pid>/cgroup` membership; a live Darwin process reports
`{"type":"darwinProcessTree","rootPid":...}` as a best-effort tree root. All
other cases report `{"type":"unavailable","reason":"..."}` from a bounded
reason set. These are per-observation locators, not identity: consumers keep
using `runtimeId` as the stable task key and rediscover the target on each
sample.

A PTY root positively absent at admission is not passed to `pty` and remains absent; an absent exec
state root likewise remains absent. If an admitted PTY root is concurrently removed, the result is
incomplete because its filesystem identity changed, but the external `pty list` implementation may
recreate its registry before st2 can detect the race. Observation never rewrites an existing exec PID
record. It also does not serialize catalog or runtime writers, reconcile tasks, or authorize a
control-plane cutover. Consumers that require a zero-write boundary under concurrent root deletion
or a transactional declaration boundary need a separate protocol.

### Staged control-plane replacement gate

`st2 up` is a replaceable control plane, not the lifetime owner of an agent. Stopping it normally
or killing it must leave every agent running. `st2 down` and declaration retirement are separate,
explicit lifecycle actions and must never be used merely to replace the control plane.

Every staged recovery or binary cutover must retain a pre-stop receipt of each agent's stable task
identity, PID, and process-creation identity. Install the verified replacement binary atomically
while those tasks continue running, restart the control plane, and do not accept the host until its
first reconcile proves all of the following:

- every pre-existing agent is still usable with the same PID and creation identity;
- the replacement reports those agents as adopted and does not launch or duplicate them;
- only genuinely missing declared work is launched;
- explicit teardown remains the only path that stops an agent.

The executable gate drives the real st2 binary and both PTY and exec backends through normal stop,
forced kill, atomic binary replacement, adoption, a live-task heartbeat, and a duplicate-boot
receipt:

```sh
cargo test --test nomad_survival --all-features
```

## Messages, DING, status, and context

Inside a managed task, `CATALOG`, `ST_ROOT`, and `ST_AGENT` are already set:

```sh
st2 status "$ST_AGENT" --set available
st2 message send <recipient> --subject "work" -m "Please handle the scoped task."
st2 message ls
st2 message read <filename>
st2 message reply <filename> -m "Handled."
st2 message archive <filename>
st2 agents --json --enrich
st2 agents --identity "$ST_AGENT" --json
st2 context read --full
```

The roster includes suspended and retired declarations instead of silently conflating desired
lifecycle with runtime presence. Both JSON shapes keep stable `identity` separate from optional
`name` and `description`, and contain the compatibility `retired` projection, `desiredState`,
`desiredStateReason`, plus the declaration's ordered `resources` descriptors. `--enrich`
additionally supplies `lastActivity` and `inbox`. Human output prints the same presentation fields
as separate columns and appends the non-running state and rationale.
`--identity <host>.<identity>` selects exactly one qualified Agent Spec or fails,
so external harness hooks can read its current name and description without parsing KDL or relying
on a duplicate state file.

For a catalog-backed agent, every native bus operation resolves the same agent directory used by
the roster: presence is `<agent-dir>/status`, while unread messages, archive receipts, and context
live under `<agent-dir>/resources/`. The flat `<root>/<identity>` layout remains only as the
intentional catalog-less fallback used by isolated folder evals. In a catalog-backed root,
`st2 message ls` rejects an absent identity; recovery inspection of a deliberately orphaned flat
box must be explicit with `st2 message ls <identity> --orphan` (and optionally `--archive`).

Services use a separate declared request surface; they do not borrow an Agent
Spec identity. Declare the endpoint without creating a task:

```kdl
// <catalog>/principals/host-a/example-ci/principal.kdl
principal "example-ci" host="host-a"
```

Then publish once, let the addressed agent reply from its normal inbox, and
observe the typed result. Bodies are JSON and `--tag` is a repeatable
`key=value` map:

```sh
st2 request send host-a.repair-agent \
  --as host-a.example-ci \
  --idempotency-key 'escalate:repo#7:abc' \
  --tag kind=example-ci.escalation \
  -m '{"candidate":"abc"}' --json

st2 request read <request-filename> --json

st2 request reply <request-filename> \
  --tag outcome=needs-human \
  -m '{"outcome":"needs-human"}' --json

st2 request status --as host-a.example-ci \
  --idempotency-key 'escalate:repo#7:abc' --json
```

The key atomically reserves one message filename and exact envelope before
publication. Exact retries return that filename with `deduplicated: true`; a
different request under the same key fails. Replies route only to the declared
principal's canonical `resources/inbox`, never a flat orphan mailbox.

Adopters should cut directly to the native layout. Before launching a migrated identity, install and
verify hooks, validate and materialize its hand-authored declaration, stop any predecessor transport,
and decide how any unread predecessor backlog will be archived or forwarded. Never run predecessor
and native DING owners concurrently for the same identity.

A pi agent does not use DING. Its messages are delivered natively into the live session by the
channel extension st2 injects, which calls pi's own message API and reads pi's own idle proof, so
no screen is inspected on that path. Declare it with a typed `pi {}` driver or with
`deliver "pi-channel"`; a declaration carrying both `ding` and `deliver` is refused.

Native DING watches the recipient inbox and delivers a normalized notice:

```text
[DING] new st2 message: [id:<rand6>] <subject> (from <sender>); check your inbox
```

Consumers must key on the `[DING]` prefix and stable id, not descriptive words. Every maintained
harness that uses DING uses the same transport: normalize untrusted fields into bounded,
single-line printable text, positively identify an empty current Codex or Claude composer, and send one bracketed-paste
sequence without Return. The sidecar then observes for a short bounded window and sends a separate
bare Return only after two immediately adjacent inspections show the exact notice in a positively
idle composer. A human draft, active turn, modal, changed composer, unreadable screen, command
timeout, or unknown renderer defers submission. Once a paste command starts, the sidecar retains
ownership and retries by inspection only, so a timeout cannot duplicate the paste. This measured
screen heuristic is fail-closed for the maintained renderer versions but is not an evented TUI
contract; renderer changes can defer delivery and remain an explicit design gap.

Agents must declare `busy` before actively executing work and return to `available` only when
yielding or ready for new work, but `busy` never suppresses DING. Fresh `dnd` is the only delivery
hold. Each status record keeps the state on its first line. Version 1 writers add `v1 <unix-ms>` on
the second line. New readers use that origin timestamp and accept a legacy bare state. Old readers
keep using the first line and file mtime. A live session owner refreshes non-DND presence every five
minutes. This changes the replicated bytes and stays below the 15-minute stale limit. A legacy
`dnd` upgrades once with its existing mtime, then remains unchanged. A malformed versioned record or
a timestamp more than 60 seconds in the future reads as `unknown` without an mtime fallback. Version
1 status contributes its origin timestamp to `lastActivity`. New arrivals remain FIFO.
Same-filename archive receipts shadow and clean
restored inbox duplicates. Failed or uncertain PTY operations retain the notice for safe retry. Unsafe
delivery retries use a bounded backoff, so an active composer cannot make the sidecar spawn a fresh
PTY probe on every inbox poll. On start
or restart, the sidecar first adopts an exact staged recovery/backlog notice when present, then sends
one generic check-inbox recovery DING if unread work remains; it does not replay every backlog
message.

Both DING and the catalog supervisor use filesystem events only as an early-wake optimization.
Create, modify, rename, and remove events wake them; read/open access events do not. Their own inbox
and catalog reads therefore cannot self-trigger a Linux inotify loop, and the bounded timer remains
the correctness fallback.

## Cleanup

Explicit teardown is the only operation that ends declared tasks:

```sh
st2 down --catalog "$CATALOG" --host <host>
```

On Linux, remove the supervisor service after teardown when it is no longer wanted:

```sh
st2 service uninstall
```

## Command surface

```text
ls, up, down, validate, doctor
message, ding, agents, status, context, resource, rename, describe
env, pty, shell, pretrust
hooks, service, claude-channel, eval
agent digest, agent publish
catalog bootstrap, catalog snapshot, catalog apply, catalog archive, catalog unarchive
completions
```

`st2 completions <shell>` emits completions from the live command tree. No generated completion or
manpage tree is committed.

## Clean-room verification

The test suite builds a temporary `PATH` containing only the current `st2` binary, required Git,
and `pty`, `codex`, and `claude` shims. It installs and verifies a scratch hook receipt, instantiates
both maintained hand-authored KDL examples in fresh Git workspaces, validates and materializes them
twice, and verifies the help/doctor/native authoring surface without a predecessor transport binary:

```sh
cargo test --test native_only --all-features
```

Run the complete local gate with:

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

The linker can exhaust a small temporary filesystem during a complete gate. A space or quota error
is an environment failure, not a compile failure. Set `CARGO_TARGET_DIR` to a filesystem with enough
space, reduce parallel jobs or debug information if necessary, and rerun the same gate. Use
`cargo clean --target-dir <path>` to remove the temporary build artifacts after the run.

## Eval contract

The sole canonical agent contract is
[`AGENT-SPEC.md`](https://github.com/compoundingtech/evals/blob/main/AGENT-SPEC.md) in the evals
repository. Eval corpus definitions, execution/readiness evidence, authorization, and results belong
to that repository; st2 does not duplicate or pin its ledgers here.

An eval that exercises production Agent Specs opts in explicitly:

```kdl
eval {
  copy "./fixture"
  run "publish-canonical-team" { command "..." }
  canonical-agents
  message { from "requester"; to "evalhost.supervisor"; content "./task.md" }
  max-timeout "300s"
  judges { /* held-out checks */ }
}
```

`copy` and deterministic `run` steps populate the hermetic temporary catalog first.
`canonical-agents` then recursively discovers the catalog and projects declarations resolved to the
eval host. Explicit `identity` and `host` fields remain authoritative independent of organizational
placement; each declaration parent remains its native state/resource anchor. The directive is
mutually exclusive with compact `team` / `agent` declarations, so the local Agent Spec vector is the
sole authority for launch, kickoff routing, supervision, logs, and teardown. Strict catalog
validation, fleet-unique nonempty task runtime IDs, and warning-free local materialization all finish
before an agent task starts; backend launch errors are fatal. Remote-host declarations remain inert.
Native inbox/archive paths are frozen from the admitted local vector, so later catalog mutation
cannot redirect eval traffic. A multi-agent team completes only after the existing worker-report ordering.
For a singleton, the requester inbox is snapshotted before kickoff; only a newly appearing
interviewer reply at-or-after the exact kickoff receipt completes it. Canonical completion gates the
verdict. Without the directive, Agent Spec-shaped files inside a fixture remain inert and compact
evals retain their flat bus and completion semantics.

`st2 agent publish --catalog ROOT (--spec FILE | --bundle DIR) --input-sha256 HEX
(--expect-absent | --expect-sha256 HEX) [--managed-by MARKER]` is the single-agent declaration
writer. Replacing an incumbent that carries `meta { managed-by "nix" }` requires
`--managed-by nix`, because that publication rewrites bytes the Nix projection owns; an assertion
is admitted only when it names exactly the marker the incumbent carries. Creating a declaration
and republishing byte-identical bytes need no assertion.
`st2 catalog apply --catalog ROOT
(--prepared DIR --input-sha256 INPUT_HEX --expect-sha256 ROOT_HEX [--raw-preimage] | --resume)` is the complete
declaration-plane writer. Each admits the complete prospective catalog under a
compare-and-swap lock before making one atomic change.
`st2 catalog digest --catalog ROOT --prepared DIR` computes the exact desired
projection digest consumed by apply. Raw-preimage repair uses it for the fully
validated successor while binding the opaque incumbent through the separate
raw-domain digest. Ordinary valid-catalog workflows reuse `afterRootSha256`
from the required policy-inspection diff instead.
`st2 catalog bootstrap --catalog ROOT --prepared DIR --input-sha256 ROOT_HEX`
is the create-only writer for an absent catalog. An exact completed replay is
`unchanged`; any different or incomplete existing target fails closed.
