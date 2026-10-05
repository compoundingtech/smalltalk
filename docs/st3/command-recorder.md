# Command recorder

st records every `git` and `gh` call that it starts, so the calls can be analyzed each day. The
recorder observes. It does not enforce, block, or change a command.

## What goes through it

`st up` puts `<state>/recorder/bin` first on the `PATH` of everything it runs:

- the daemon's own commands, such as checkout and resource observation;
- every member it starts: agent seats, terminal (PTY) sessions, gates, and exec steps.

A member's declared `PATH` and the st executable directory are applied first. The recorder
directory then goes in front of them, so an authored `PATH` cannot put another `git` ahead of it.

`<state>/recorder/bin` holds `git` and `gh` links to the st executable and a marker file,
`st3-recorder.json`, that names the host, log, and optional receipts directory. The daemon writes these at startup. It links
a program only when that program is on the daemon's `PATH` or the login shell's `PATH`, so
`command -v gh` still fails on a host without `gh`. When the daemon cannot write the directory, it
prints `st: not recording git and gh calls: ...` and starts anyway.

When st starts as `git` or `gh`, it does not start the async runtime or read any configuration.
It finds the real program, runs it, waits for it, appends one line to the log, and exits the way the
real program exited.

## Finding the real program

The recorder searches `PATH` in order, as `execvp` does. It skips:

- every directory that holds a recorder marker, including this one and the recorder of another
  st daemon, such as an isolated test daemon;
- any file that is the running st executable, under any name or directory;
- anything that is not an executable file.

The first remaining match runs with the caller's arguments, environment, working directory, and
stdio. A bare `argv[0]` such as `git` is passed on unchanged. `PATH` is passed on unchanged too,
so a `git` that `gh` starts is recorded as its own call.

When no match remains, the recorder prints `st recorder: git: command not found after the
recorder on PATH`, records the call with exit code 127, and exits 127.

## The log

The log is `<state>/recorder/commands.jsonl`, one per host. It is append-only JSON Lines with mode
0600. The live daemon on a host with default settings writes
`~/.local/state/st3/recorder/commands.jsonl`.

Each call appends one line after the real program exits:

| Field | Meaning |
| --- | --- |
| `schema` | `st3.recorder.command.v1` |
| `time` | When the call started, RFC 3339 UTC with milliseconds |
| `host` | The daemon's node name |
| `actor` | `ST_AGENT`, else `ST3_SUBJECT`, else `daemon` |
| `subject` | `ST3_SUBJECT`, the st member that ran the call, or `null` |
| `step_run` | `ST_STEP_RUN`, or `null` |
| `cwd` | The working directory |
| `program` | `git` or `gh` |
| `args` | The arguments after the program name |
| `real` | The program that ran, or `null` when none was found |
| `exit_code` | The exit code, or `null` when a signal ended the program |
| `signal` | The signal number that ended the program, or `null` |
| `duration_ms` | Wall time from start to exit |
| `receipt_url` | The created GitHub issue or pull request URL, when captured; otherwise absent |

An argument longer than 4096 bytes is cut and ends with `…[N more bytes]`. The recorder replaces URL
credentials (`https://***@host`), `extraheader=` values, and `Authorization:` header values with
`***`. That redaction is a best effort, so treat the log as private to the account.

The log is not rotated. A daily job that reads it can move it aside; the next call creates a new
file.

```sh
log=~/.local/state/st3/recorder/commands.jsonl
jq -r 'select(.program == "git") | .args[0]' "$log" | sort | uniq -c | sort -rn
jq -c 'select(.exit_code != 0) | {time, actor, program, args, exit_code, signal}' "$log"
jq -r '[.actor, .program, (.args | join(" "))] | @tsv' "$log"
```

`st doctor` includes a `command-recorder` check. It warns when the recorder directory, its links,
or an appendable log is missing.

## Summarize calls

`st recorder report` summarizes the last 24 hours of the local log. `--hours` changes the window,
`--top` changes the number of slow calls, and `--json` produces structured output. To summarize
several hosts, copy their private logs to the machine running the report and pass each one with
`--log`:

```sh
st recorder report --hours 24
st recorder report --hours 24 --log ./host-a.jsonl --log ./host-b.jsonl
st --json recorder report --hours 168 --log ./host-a.jsonl --log ./host-b.jsonl
```

The report groups calls and failures by host, agent, and command; lists the slowest calls; and
counts `gh` commands whose command family normally uses GitHub's API. That last count is an
inference from command arguments, **not** a measurement of HTTP requests. Local help calls are
excluded. A command can fail before reaching GitHub, and one command can make several requests.
Malformed JSONL lines are counted as skipped. Only calls that reached the recorder appear, so a
quiet report does not prove that nothing happened.

## What a recorded call keeps

A recorded call behaves as the real call would:

- The exit code is the real program's exit code.
- When a signal ends the real program, the recorder raises the same signal, so a shell that stops a
  loop on an interrupted child still stops.
- stdin and stderr are the caller's own descriptors. stdout is also the caller's descriptor except
  for the non-TTY creation receipts described below. The recorder never reads stdin, so input that
  the real program leaves unread stays for the next command.
- The real program runs in the caller's process group, so the terminal, job control, pagers, and
  editors work as before.
- A signal sent to the recorder, such as `SIGTERM` from a timeout, reaches the real program:
  `SIGHUP`, `SIGINT`, `SIGQUIT`, `SIGTERM`, `SIGUSR1`, `SIGUSR2`, `SIGALRM`, and `SIGWINCH`. A
  signal the caller ignored stays ignored. On Linux, a terminal interrupt, quit, or resize is not
  relayed, because the terminal already sent it to the whole process group.
- On Linux, killing the recorder with `SIGKILL` also kills the real program.
- A log that cannot be opened or written is skipped without output. A FIFO without a reader does
  not hold the command.

[`crates/st3/tests/command_recorder.rs`](../../crates/st3/tests/command_recorder.rs) compares
recorded and direct calls for each of these.

## Creation receipts

For `gh issue create` and `gh pr create`, the recorder captures stdout only when the caller's stdout
is not a terminal and the marker advertises a `receipts` directory. It forwards every byte unchanged
and retains only the last line, bounded to 1 KiB. Terminal output and every other command keep the
direct descriptor. A wrapper before the recorder on `PATH` can exec it without changing this check:
the recorder checks the caller's real stdout descriptor.

`--dry-run`, browser mode (`--web`/`-w`), and help do not capture receipts. In
[`cli/cli` v2.100.0's PR create implementation](https://github.com/cli/cli/blob/v2.100.0/pkg/cmd/pr/create/create.go),
dry-run renders the proposed body instead of submitting, while browser mode returns through
`previewPR`; neither creates a PR. Issue create also uses browser mode for preview. A body ending
in an existing PR URL must not credit the caller with creating that PR.

When the last line is a GitHub issue or pull request URL, the log includes `receipt_url` and the
recorder atomically writes a private mode-0600 receipt under `<state>/recorder/receipts`. The receipt
names the URL, actor, `ST_MISSION_RUN`, exit code, and timestamp. A URL is published even when the
command exits nonzero, because creation can succeed before a later attachment fails. Without the
marker's `receipts` field, no stdout capture or receipt publication occurs.

Signal forwarding remains active when the caller stops reading stdout. Once the signaled child
exits, a stalled consumer cannot hold up the command's exit status: undeliverable queued output
is abandoned and the incomplete capture produces no receipt.

The shim never contacts the daemon. The daemon consumes the spool asynchronously and publishes
the resource's `opened_by` and `opened_by_run` facts. When either opener field is already recorded,
it consumes the receipt without appending a claim, including a capture by another agent.
Duplicate receipts for the same URL and actor also collapse through an idempotency key.

Receipts use the existing `resource.observed` claim with `attribution_only: true`. They contain
attribution and required resource identity fields only, never the daemon's previous observer
snapshot. The projection merges only the opener into observed facts; a receipt cannot replace
state, head, title, checks, or the observation's timestamp. Before any observation, it establishes
identity and attribution. The first real observation retains that opener and still triggers
issue or review delivery normally, including when the observation arrives through replication.
The spool directory is mode 0700 (including after reinstall).
Filesystem notifications wake ingestion immediately; a fallback scan backs off from two seconds
to at most sixty seconds while idle and returns to one second after ingestion. Failed appends
retain their retry count in `.attempt-N.json` filenames and a not-before deadline in the file's
modification time across daemon restarts. The deadline is set before renaming; each scan ignores
pending retries even when filesystem notifications wake it. Retries wait at least one, two, four,
then eight seconds; fallback scans may add delay. After five actual failures the receipt becomes
a `.dead-letter` file, is logged once, and is no longer retried. To retry after correcting the
cause, rename it to a fresh `.json` name. Abandoned `.tmp` files older than one hour are removed;
fresh temporary files are left alone so an active publisher can finish.

## Limits

- A program called by its absolute path, such as `/usr/bin/git`, skips the recorder. So does a
  shell whose startup files replace `PATH` rather than extend it. A quiet log therefore does not
  prove that nothing ran.
- Only `git` and `gh` are recorded.
- A call is recorded when it ends. A call whose recorder is killed with `SIGKILL` is not recorded.
- The real program always starts with the default `SIGPIPE` action, as it does when st or most
  other programs start it directly. A caller that ignored `SIGPIPE` cannot pass that on through the
  recorder.
- A stop signal sent to the real program alone is not reflected in the recorder. A terminal stop
  (`Ctrl-Z`) stops both, because both are in the same process group.
- On macOS the recorder relays every listed signal, so a program can receive a terminal interrupt
  twice.
- A member launched before the daemon that added the recorder keeps its old `PATH` until it is
  relaunched.
