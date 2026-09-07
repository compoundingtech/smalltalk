# Systemd scope wrappers disable environment expansion

Status: accepted

Accepted on 2026-09-05 for R42 launch argv transparency.

## Context

st2 adds `systemd-run` around every PTY and exec launch when Linux systemd
scope isolation is available. Rust passes the task program and arguments to
that outer process as opaque OS strings, but `systemd-run` performs its own
command-line environment expansion by default. A Linux systemd 260 experiment
passed the literal `$HOME:${UNSET}:$$` through the wrapper and observed `$$`
changed to `$`. The wrapper therefore did not preserve launch argv even though
st2 itself never invoked a shell or edited the argument.

The scope wrapper is also the mechanism behind control-plane replacement
safety. Any correction must retain scope creation, collection, inherited I/O,
PTY behavior, exit status, and outliving-child membership.

## Options

| Option | Tradeoffs |
| --- | --- |
| Disable systemd expansion before the separator — selected | Preserves the original OS strings without changing scope mechanics. |
| Keep systemd's default expansion | Rejected because the live probe changed caller-owned argument data. |
| Rewrite dollar signs into systemd escape sequences | Rejected because st2 would have to interpret every OS string and maintain a wrapper-specific transform. |
| Shell-quote the inner command | Rejected because there is no shell boundary; quote bytes would become task argument bytes, while adding a shell would introduce evaluation. |
| Drop systemd scope isolation for affected commands | Rejected because it would trade argument corruption for violation of R11 control-plane replacement safety. |

## Evidence and Argument

The Linux systemd 260 experiment distinguished the wrapper's default from the
selected option with one literal input. Default expansion changed the terminal
`$$` in `$HOME:${UNSET}:$$` to `$`; `--expand-environment=no` preserved the
complete input. With expansion disabled, non-TTY argv and standard descriptors,
real-PTY terminal status, exit propagation, outliving-child scope membership,
collection, exact-name reuse, and cleanup all retained their prior behavior.

The option is the narrowest boundary fix: systemd owns the unwanted
interpretation, and systemd exposes a switch that removes it. Rewriting task
arguments inside st2 would replace downstream interpretation with an
st2-maintained encoding and would no longer be opaque launch argv.

## Decision

Linux scope launches use this exact outer argument order:

```text
systemd-run --user --scope --collect --quiet --unit=<unit> --expand-environment=no -- <program> <arg>...
```

`--expand-environment=no` is passed as a `systemd-run` option before the `--`
separator. st2 appends the program and each argument after the separator as the
original OS strings. It does not quote, escape, expand, or render those values
through a shell. Detached and degraded-detached modes remain direct
pass-throughs.

## Consequences

- Dollar-bearing literals, including `$HOME`, `${UNSET}`, and `$$`, reach PTY
  and exec tasks byte-for-byte in scope mode.
- The systemd option sequence is part of the tested wrapper contract; the only
  addition to the prior shape is the expansion-disable option before the
  separator.
- macOS and Linux hosts without usable user scopes keep the existing direct
  program-and-argv path.
- Scope lifetime and I/O behavior are unchanged. The supporting live evidence
  is recorded in the [systemd scope argv experiment](../.experiments/2026-09-05-systemd-scope-argv-transparency.md).
