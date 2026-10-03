# st shell completion specification

This document specifies how st completes command lines and resolves short entity names. It builds
on [requirements.md](./requirements.md).

## Status

Active.

## Scope

Defines: the completion mechanism, the entity kinds and their descriptions, per-argument filters,
matching and short-name resolution, failure behavior, and installation.

Does not define: the entity model (see [../design.md](../design.md)) or the client list API (see
[../client-v0/collections.md](../client-v0/collections.md)).

## Mechanism (R01, R04, R11, R12)

st uses clap_complete's dynamic engine (`unstable-dynamic`, pinned with `=` in
`crates/st3/Cargo.toml`, T01). The shell stub passes the command line back to st on every TAB:

```
shell TAB
  └─ stub: COMPLETE=<shell> <st path> -- <words…>
       └─ st main(): CompleteEnv::complete()      before config, tokio, or daemon work
            ├─ subcommands, flags, value sets     from the clap command tree (R04)
            └─ entity argument                    Complete(Entity) value completer
                 └─ private current-thread runtime, 300 ms deadline
                      └─ daemon list call(s) over the trusted Unix socket
```

The engine prints one candidate per line in the shell's format. Values and descriptions are
separated by a tab (fish) or a colon (zsh). Bash receives values only (T02).

`CompleteEnv::complete()` runs in `main` right after the recorder check. It returns immediately
when `COMPLETE` is unset. When `COMPLETE` is set, it exits after it answers.

Arguments whose type is a path complete to files through clap's path value hint. Every other
argument without a completer or a value set offers nothing (R05).

## Entity kinds (R01, R02, R03)

`crates/st3/src/completion.rs` defines `Entity`. Each kind lists through generated-client list
calls with the operational (non-history) default. A list follows `next_cursor` page by page (200
items per page) until the collection ends or 5,000 items are read, so every current entity is
offered.

| Entity | Subject | Source | Filter | Description |
| --- | --- | --- | --- | --- |
| `Terminal` | owner `agent/…` | `terminals_list`, joined with `agents_list` | state `running` | state · host · harness · harness state · current work or queue · `up` age |
| `Agent { running_only }` | `agent/…` | `agents_list` | `running_only`: state `running` | state · host · harness · harness state · current work or queue |
| `Mission` | `mission/…` | `missions_list` | none | title · state · active runs |
| `MissionRun { unfinished_only }` | `mission-run/…` | `missions_list` run details | `unfinished_only`: phase not `terminal` and no outcome | mission title · status · phase · current step · age |
| `MissionOrRun` | both | `missions_list` | none | as above |
| `Work(WorkFilter)` | `step-run/…` | `work_list` | see below | title or path · state · mission · claimant or assignee |
| `Attention` | `attention/…` | `attention_list` | state `open` | quoted title · priority · requester · age |
| `Message` | `message/…` | `messages_list_for_recipient(caller)` | not archived | sender · quoted title or first line · age |
| `Lane` | `lane/…` | `lanes_list` | none | name · state · entry count |
| `Host` | `host/…` | `machines_list` | none | state · runtime count |
| `Launch` | `launch/…` | `launches_list` | none | quoted title · phase · planner · variant count |
| `Device` | `device/…` | `devices_list` as the configured person | state `active` | name · state · scope count · expiry date |
| `Subscription` | `subscription/…` | `subscriptions_list` | none | state · observer · mission it starts |
| `Session` | `session/…` | `sessions_list` as the configured person | none | owner · state · start age |
| `NativeSession { importable_only }` | `session/…` | `sessions_list_native` | not managed; `importable_only`: importable | state · harness · workspace · start age |
| `Person` | `person/…` | configured person, joined with `attention_list` and `devices_list` | none | why the person is offered |
| `Actor` | `person/…`, `agent/…` | configured person and caller, joined with `agents_list` | none | as `Agent`, or why it is offered |

`WorkFilter` selects the steps a work command can act on:

| Filter | Steps |
| --- | --- |
| `Any` | every current step |
| `Ready` | state `ready` |
| `Failed` | state `failed` |
| `Claimed` | state `claimed`; when the caller is an agent, only steps it claimed |

The caller is `ST_AGENT` (prefixed with `agent/` when needed), else `person` from the st config.
Without a caller, `Message` offers nothing. Without a configured person, `Device` and `Session`
offer nothing.

A terminal's `up` age comes from its incarnation (`PID:STARTED_AT`), not from `updated_at`, which
moves with every observation.

A description joins its non-empty parts with ` · `, collapses whitespace to single spaces, and is
at most 96 characters (longer text ends with `…`). Quoted titles are at most 48 characters.

### Argument mapping

| Command | Argument | Entity |
| --- | --- | --- |
| `terminals attach, peek, screen, attach-info, stream, input-client, send, signal` | subject | `Terminal` |
| `agents show, queue, queue move, hold, rename, resume, restart`, `missions queued` | agent | `Agent { running_only: false }` |
| `agents stop, suspend` | subject | `Agent { running_only: true }` |
| `agents new --host`, `agents start --host`, `terminals new --host` | host | `Host` |
| `conversations send --to`, `conversations search --agent` | agent | `Agent { running_only: false }` |
| `missions show` | mission or run | `MissionOrRun` |
| `missions start`, `missions retire` | mission | `Mission` |
| `missions start --after`, `missions cancel`, `agents queue move` run/`--before`/`--after`, `work revise` | run | `MissionRun { unfinished_only: true }` |
| `missions outcome`, `work revision show`, `work revision generations`, `now --owner-run`, `trace --owner-run` | run | `MissionRun { unfinished_only: false }` |
| `missions requests` | subscription | `Subscription` |
| `work show, done`, `work ask --step`, `attention request --step` | step | `Work(Any)` |
| `work claim`, `work wake` | step | `Work(Ready)` |
| `work retry` | step | `Work(Failed)` |
| `work renew, progress, complete, fail, release, extend, publish-mission` | step | `Work(Claimed)` |
| `attention show, resolve, withdraw` | item | `Attention` |
| `conversations read, reply, archive, thread, status`, `conversations send --in-reply-to` | message | `Message` |
| `conversations timeline, follow` | session | `Session` |
| `import show` | session | `NativeSession { importable_only: false }` |
| `import run` | session | `NativeSession { importable_only: true }` |
| `lanes show, join, leave, move, mark, approve` | lane | `Lane` |
| `launch show, preview, submit, compare, propose, revise, approve, approve-and-start, run, question, answer, cancel` | session | `Launch` |
| `devices revoke` | device | `Device` |
| every `--as` with a person parser, `attention request --for`, `work ask --for`, `work update --for` | person | `Person` |
| every other `--as`, `conversations send --from`, `conversations reply --from`, `conversations ls` identity and `--sender` | actor | `Actor` |

`work claim, renew, progress, complete, fail, release` share one argument struct; each subcommand
replaces the struct's `Work(Any)` completer with its own through `mut_arg`.

These arguments have no entity completer:

- Arguments that name any subject kind: `subject show`, `trace`, `trace wait`, `claim`,
  `schema`, `attention request --target`, and review targets.
- Arguments whose candidates depend on another argument on the same line: lane entries, launch
  variants and decisions, mission requests, and revision proposals. The completion engine passes
  a completer only the word being completed.
- Hidden plumbing commands and external identifiers such as pull requests and gate references.

## Matching (R08)

The completer keeps candidates whose subject starts with the typed text. Substring matching is not
used: zsh's default matcher drops candidates that do not share the typed prefix, so it would behave
differently per shell.

## Short-name resolution (R09, R10)

`terminals attach, peek, send, signal`, `agents stop`, `missions cancel`, and `attention show`
resolve their subject before they act. A word that contains `/` is used as typed. For a bare word,
st lists the argument's entity kind (with the same filter as completion) and applies these steps
in order. The first step that matches anything decides:

1. The subject equals `<namespace>/<word>` (the previous literal meaning).
2. The last path segment of the subject equals the word.
3. The last path segment starts with the word.
4. The subject contains the word.

One match resolves to that subject. Several matches fail with exit status 2 and list each subject
with its description. No match, a daemon that does not answer within 2 s, or a non-Unix endpoint
keeps `<namespace>/<word>` (R10), so a local attach works while the daemon is down.

```
$ st terminals attach eu-ci        → agent/dev3.eu-ci-bottleneck
$ st terminals attach interactive
st: `interactive` matches 9 subjects; name one exactly:
  agent/interactive/dev3/43b16cd2-ec69-44  running · dev3 · omp · idle · up 2h
  …
```

## Failure behavior (R06, R07)

- The completion client uses no outage wait, so an unreachable socket fails at once.
- All list calls of one TAB, joins included, share one 300 ms deadline.
  `ST3_COMPLETION_DEADLINE_MS` replaces it; the integration tests use it against a debug daemon.
- A join (agents for `Terminal` and `Actor`; attention items and devices for `Person`) only
  enriches candidates. It has its own 200 ms deadline, and a join that fails or runs out is dropped:
  terminals keep their terminal facts, and `Person` and `Actor` still offer the configured person
  and the caller.
- A timeout, transport error, or config error on the primary list yields no entity candidates.
  Completion writes nothing to stderr.
- The endpoint is the last `--endpoint` on the line being completed, else `ST3_ENDPOINT`, else the
  configured socket. An HTTP endpoint yields no entity candidates, because client-v0 lists need the
  trusted Unix socket.

## Installation (R11, R12)

`st completions <shell>` writes clap_complete's registration stub for `st`, bound to the absolute
path of the running executable. The Nix package runs it at build time for bash, zsh, and fish
(`flake.nix` `postInstall`) and installs the results with `installShellCompletion`, so the stub
calls the store path of the installed package.

## Verification

- Unit tests in `completion.rs` cover the resolution ladder, filters (terminal state, work
  filters, native sessions, people and actors), the terminal join and its age, pagination,
  `--endpoint` parsing, description bounds, ages, and plurals.
- `crates/st3/tests/completion_shells.rs` serves an in-process daemon with 205 running terminals
  and one stopped terminal. It checks that completion offers every running terminal across pages
  with descriptions; that a missing or silent daemon yields no entity candidates, nothing on
  stderr, and an answer within seconds; and that the stubs from `st completions` complete a live
  terminal in real fish (`complete -C`), bash (the stub's completion function), and zsh (TAB in an
  interactive zsh under `zpty`). The shells come from `nativeCheckInputs` and the dev shell; a
  missing shell fails the test.
- `checks.st3-help` runs `scripts/check-installed-completions` against the built package: each
  installed stub names the package's `st3`, fish and bash complete a subcommand through it, zsh
  registers it, and with no daemon no entity is offered.
