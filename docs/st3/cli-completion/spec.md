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

`crates/st3/src/completion.rs` defines `Entity`. Each kind lists through one generated-client list
call with the operational (non-history) default and a page of 200.

| Entity | Subject | Source | Filter | Description |
| --- | --- | --- | --- | --- |
| `Terminal` | owner `agent/…` | `terminals_list` joined with `agents_list` | state `running` | state · host · harness · harness state · current work or queue · `up` age |
| `Agent { running_only }` | `agent/…` | `agents_list` | `running_only`: state `running` | state · host · harness · harness state · current work or queue |
| `Mission` | `mission/…` | `missions_list` | none | title · state · active runs |
| `MissionRun { unfinished_only }` | `mission-run/…` | `missions_list` run details | `unfinished_only`: phase not `terminal` and no outcome | mission title · status · phase · current step · age |
| `MissionOrRun` | both | `missions_list` | none | as above |
| `Work { state }` | `step-run/…` | `work_list` | `state`: exact state | title or path · state · mission · claimant or assignee |
| `Attention` | `attention/…` | `attention_list` | state `open` | quoted title · priority · requester · age |
| `Message` | `message/…` | `messages_list_for_recipient(caller)` | not archived | sender · quoted title or first line · age |
| `Lane` | `lane/…` | `lanes_list` | none | name · state · entry count |
| `Host` | `host/…` | `machines_list` | none | state · runtime count |

The caller for `Message` is `ST_AGENT` (prefixed with `agent/` when needed), else `person` from
the st config. Without a caller, `Message` offers nothing.

A description joins its non-empty parts with ` · `, collapses whitespace to single spaces, and is
at most 96 characters (longer text ends with `…`). Quoted titles are at most 48 characters.

### Argument mapping

| Command | Argument | Entity |
| --- | --- | --- |
| `terminals attach, peek, screen, attach-info, stream, input-client, send, signal` | subject | `Terminal` |
| `agents show`, `agents queue`, `agents queue move`, `missions queued` | agent | `Agent { running_only: false }` |
| `agents stop` | subject | `Agent { running_only: true }` |
| `agents new --host`, `agents start --host` | host | `Host` |
| `conversations send --to` | recipient | `Agent { running_only: false }` |
| `missions show` | mission or run | `MissionOrRun` |
| `missions start`, `missions retire` | mission | `Mission` |
| `missions start --after`, `missions cancel`, `agents queue move` run/`--before`/`--after`, `work revise` | run | `MissionRun { unfinished_only: true }` |
| `missions outcome`, `work revision show`, `work revision generations`, `now --owner-run`, `trace --owner-run` | run | `MissionRun { unfinished_only: false }` |
| `work show`, `work claim, renew, progress, complete, fail, release`, `attention request --step` | step | `Work { state: None }` |
| `work wake` | step | `Work { state: Some("ready") }` |
| `work retry` | step | `Work { state: Some("failed") }` |
| `attention show, resolve, withdraw` | item | `Attention` |
| `conversations read, reply, archive, thread`, `conversations send --in-reply-to` | message | `Message` |
| `lanes show, join, leave, move, mark, approve` | lane | `Lane` |

Arguments that name any subject kind (`subject show`, `trace`) and hidden plumbing commands have no
entity completer.

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
- A timeout, transport error, or config error yields no entity candidates. Completion writes
  nothing to stderr. The `Terminal` join degrades to terminal facts when only `agents_list` fails.
- The endpoint is `ST3_ENDPOINT` when set, else the configured socket. An HTTP endpoint yields no
  entity candidates, because client-v0 lists need the trusted Unix socket.

## Installation (R11, R12)

`st completions <shell>` writes clap_complete's registration stub for `st`, bound to the absolute
path of the running executable. The Nix package runs it at build time for bash, zsh, and fish
(`flake.nix` `postInstall`) and installs the results with `installShellCompletion`, so the stub
calls the store path of the installed package.

## Verification

- Unit tests in `completion.rs` cover the resolution ladder, description bounds, ages, and plurals.
- A shell check drives `COMPLETE=fish st -- st terminals attach ''` against a live daemon and
  expects running terminals with descriptions, and drives it with an unreachable endpoint and
  expects empty stdout and stderr.
