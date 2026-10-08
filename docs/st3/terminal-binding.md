# Binding an agent to a person's terminal

A native driver can run as a child of a shell in an existing st terminal. Declare its seat with `bind-terminal` before starting the driver:

```kdl
version 2
agent "example/terminal-claude" {
  name "Terminal/claude"
  host "orchid"
  workspace "/work"
  harness "claude" {}
  bind-terminal "pty/person/avery/019a0000-0000-7000-8000-000000000001" incarnation="42:created" id="019a0000-0000-7000-8000-000000000002"
}
```

The terminal must already be declared on the same host. Publish with the terminal owner's person credential; agent and daemon credentials cannot create a binding. `incarnation` is the running terminal's PTY incarnation, including its PID and creation timestamp. `id` is a fresh UUID for each harness invocation. For a terminal with an explicit runtime ID, supply that exact ID with the optional `runtime-id` property.

This declaration records a harness configuration but never launches it. The caller runs `st driver claude --subject agent/example/terminal-claude -- <provider argv>` as a child of its shell with its chosen permission settings, driver environment, channel and hook registrations. The launcher owns argument merging and unmanaged fallback. Declaring a binding alone does not arrange those registrations.

The driver keeps the terminal's `st3.subject` tag. It reads the binding and matches that terminal's tag and exact PTY incarnation in the local registry. Its agent incarnation appends `:bound:<invocation UUID>` to the PTY incarnation. Mail and driver exit reports use that invocation fence, so a prior harness in the same shell cannot end the next one.

Reconciliation adopts the running PTY without a start. Once the driver records an exit for the current invocation, the seat stays exited across daemon restarts while the terminal's shell remains running. A missing or replacement PTY ends the binding without a launch. Stopping a bound seat detaches its graph runtime; it does not stop the harness process or shell. The person can exit the harness from its terminal. Bound seats cannot declare checkouts, rendering, sidecars, rollout, fresh-context recovery or one-shot retirement and cannot be materialized inside mission runs.

A killed driver that cannot record its exit is a limitation: the shell's liveness alone cannot prove whether its harness still lives. This change does not add process discovery or a terminal PATH shim.

No database migration or full replay is required. Existing declarations serialize identically. New bindings persist a `terminal-bound` member lifecycle. Older binaries cannot interpret that lifecycle; upgrade the terminal-owning daemon before creating bindings. Rollback is supported before bindings exist. Once bindings exist, roll forward: an older reconciler's stop fallback is not aware that the runtime belongs to a surviving shell.
