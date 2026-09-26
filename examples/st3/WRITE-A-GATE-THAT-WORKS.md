# Write a gate that works

This example verifies an invented catalog index with
[`gate-recovery.kdl`](gate-recovery.kdl) and
[`verify-catalog-index.sh`](verify-catalog-index.sh).

## Failure first: three independent clocks and environments

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

It assumes `bash` is on `PATH`, gives the step no time beyond its own gate, and says nothing about
whether the shell file parses. Gates run without a login shell and with a minimal `PATH`; a command
that works interactively can therefore fail immediately. Equal limits create the opposite failure:
the owning step can time out while its gate is still legitimately using its full minute. A syntax
error waits until runtime if nobody checks it before publication.

## Supported recovery: run the complete files

The checked-in example addresses all three failures:

- the gate invokes `/bin/bash`, and every external command inside the script has an absolute path;
- the step allows two minutes while the gate allows one;
- the shell is checked before the mission is published.

Run it from the repository root with a disposable workspace:

```sh
gate_workspace="$(mktemp -d)"
/bin/cp examples/st3/verify-catalog-index.sh "$gate_workspace/verify-catalog-index.sh"
printf '%s\n' 'catalog version 1' >"$gate_workspace/catalog-index.txt"

/bin/bash -n examples/st3/verify-catalog-index.sh
st3 missions publish examples/st3/gate-recovery.kdl --as person/operator
st3 missions start example/catalog-gate \
  --id example/catalog-gate/first \
  --workspace "$gate_workspace" \
  --as person/operator \
  --follow
```

For a different gate, syntax-check the exact script used by `exec`, keep every invoked binary
absolute, and make the owning step timeout strictly longer than the gate's `time-limit`.
