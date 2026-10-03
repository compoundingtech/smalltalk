# Write a gate that works

This example verifies an invented catalog index with
[`gate-recovery.kdl`](gate-recovery.kdl) and
[`verify-catalog-index.sh`](verify-catalog-index.sh).

## First, look for a built-in gate

Most gates check something st can answer itself. Use these instead of a command:

```kdl
gate "the handoff is published" { document "doc/example/catalog/handoff" }
gate "the fix merged" { merged "example/catalog#42" }
gate "linux-gate passed on main" { ci-passed "linux-gate" repo="example/catalog" branch="main" }
gate "the index suite passes on main" { cargo-test "index" package="catalog" }
```

A `document` gate never greps a listing that shows only its first page, and `cargo-test` tells
failing tests (not yet) from a build this host cannot make (broken).
[`land-and-verify.kdl`](land-and-verify.kdl) uses all four, and the
[mission reference](../../docs/st3/mission-graph-runtime.md#built-in-gates) explains each one.
When none fits, write an exec gate, as the rest of this guide does.

## Failure first: time limits and unchecked scripts

A gate like this is fragile:

```kdl
step "verify-index" timeout="1m" {
  agentless
  gate "the catalog index is valid" {
    exec "bash verify-catalog-index.sh"
    host "local"
    workspace "${ST_WORKSPACE}"
    time-limit "1m"
  }
}
```

It gives the step no time beyond its own gate and says nothing about whether the shell file
parses. The owning step can time out while its gate is still legitimately using its full minute.
A syntax error waits until runtime if nobody checks it before publication; `st missions check`
runs the gate once, now, the way a run would, and `st missions publish` refuses a gate that check
finds broken.

Gates use the same captured interactive login-shell environment as agents and daemon commands.
Programs such as `bash` resolve through that PATH; absolute binary paths are optional.
The daemon refreshes the snapshot on use every 60 seconds. `st doctor` shows the effective PATH.

## Supported recovery: run the complete files

The checked-in example makes these choices explicit:

- the example pins `/bin/bash` and its tools with absolute paths; using the captured PATH also works;
- the step allows two minutes while the gate allows one;
- the shell is checked before the mission is published.

Run it from the repository root with a disposable workspace:

```sh
gate_workspace="$(mktemp -d)"
/bin/cp examples/st3/verify-catalog-index.sh "$gate_workspace/verify-catalog-index.sh"
printf '%s\n' 'catalog version 1' >"$gate_workspace/catalog-index.txt"

/bin/bash -n examples/st3/verify-catalog-index.sh
st missions check examples/st3/gate-recovery.kdl --workspace "$gate_workspace"
st missions publish examples/st3/gate-recovery.kdl --workspace "$gate_workspace" \
  --as person/operator
st missions start example/catalog-gate \
  --id example/catalog-gate/first \
  --workspace "$gate_workspace" \
  --as person/operator \
  --follow
```

For a different gate, syntax-check the exact script used by `exec`, ensure its tools are on the
captured PATH, and make the owning step timeout strictly longer than the gate's `time-limit`.

## Three more rules that bite later

**`${NAME}` belongs to st; `$NAME` belongs to the shell.** st substitutes `${ST_WORKSPACE}`,
`${ST_ATTEMPT}`, `${loop.round}`, and the other documented names before the command runs, and it
rejects a publication that names an unknown one. The command itself runs through `sh -c`, so write
shell variables and substitutions as `$area` or `$(/usr/bin/date +%s)`. `${area}` would be read as
an st variable and refused; write `$${area}` when the shell needs the braces.

**A gate result is cached by the gate's definition and the step attempt.** A mechanical gate runs
once for its owner, attempt, name, and exact command; asking again returns the first result. A
retried step attempt runs its gates again, as [`wait-until-time.kdl`](wait-until-time.kdl) relies
on. A loop's `until` gate belongs to the loop, not to one round, so end its command with
`# round ${loop.round}` to check again each round; see
[`walkthrough-work.kdl`](walkthrough-work.kdl) and [`loop-until-green.kdl`](loop-until-green.kdl).

**A gate on files checks the committed, pushed tree.** An agent's working tree can hold
uncommitted edits, another branch, or nothing at all by the time the gate runs. Check what was
pushed: fetch the branch and read it, as [`human-review.kdl`](human-review.kdl) does with
`git cat-file`, or test it in a scratch worktree, as
[`test-pushed-branch.sh`](test-pushed-branch.sh) does. The catalog gate above reads a file in a
disposable run workspace that no agent edits, so it can read the file directly.

A graph predicate gate such as `field`, `every`, or `exists` has no command. It stays pending
while it is false and passes once the graph makes it true; it never fails on its own.
