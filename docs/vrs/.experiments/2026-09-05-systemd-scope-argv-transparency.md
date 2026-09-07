# Systemd scope argv transparency

Date: 2026-09-05
Fixture: Linux with a functioning systemd 260 user manager

## Question

Does `systemd-run --user --scope` preserve dollar-bearing task arguments by
default, and does `--expand-environment=no` correct that boundary without
changing the scope, I/O, PTY, exit-status, collection, or name-reuse behavior
on which st2 already depends?

## Method

A disposable helper was launched through the same ordered scope options used by
st2. It received the single literal argument `$HOME:${UNSET}:$$` and reported
the value it actually received. The probe ran once with systemd's default
command-line expansion and once with `--expand-environment=no` before the `--`
separator.

The expansion-disabled form was then exercised through two transport fixtures:
a non-TTY process with distinct argv, stdin, stdout, stderr, and exit-status
probes, and a real PTY process that reported terminal status for all three
standard descriptors. A final lifetime probe left a child alive after the
wrapper exited, observed scope membership and collection, reused the same unit
name after collection, and checked fixture cleanup.

## Result

| Probe | Observation |
| --- | --- |
| Default `systemd-run` expansion | The received value differed from the supplied literal: the terminal `$$` became `$`. |
| `--expand-environment=no` before `--` | The helper received the complete literal `$HOME:${UNSET}:$$` unchanged. |
| Non-TTY transport | argv, stdin, stdout, and stderr probes were preserved; the wrapper returned the task's exit status 37. |
| Real PTY transport | stdin, stdout, and stderr remained TTYs; the wrapper returned the task's exit status 23. |
| Outliving child | The child remained in the same active scope after wrapper exit. `--collect` unloaded the scope only after the child exited. |
| Unit reuse and cleanup | Exact-name reuse succeeded after collection. No matching process, unit, or temporary fixture path remained. |

## Conclusion

Systemd's default command-line expansion is not transparent to caller-owned
argv. Passing `--expand-environment=no` before the command separator is the
narrow correction: it preserves the complete dollar-bearing literal while all
measured scope lifecycle, descriptor, PTY, exit-status, collection, and reuse
semantics remain unchanged. This supports [R42 launch argv transparency](../requirements.md)
and [decision 0016](../.decisions/0016-systemd-scope-wrappers-disable-environment-expansion.md).

## VRS Impact

- `requirements.md` adds R42 launch argv transparency.
- `ontology.md` defines **launch argv** as the canonical task-wide term.
- `spec.md` fixes the exact systemd scope wrapper order and its deterministic
  scope/pass-through tests.
- Decision 0016 records the selected systemd option and rejected alternatives.