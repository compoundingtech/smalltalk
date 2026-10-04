# Harness login failures

An agent whose current harness reports a missing or rejected credential exposes
`harness_state: needs-login` and `state: waiting` through client-v0. Ordinary
activity, channel initialization, and a disappearing screen prompt do not clear
this condition. A successful native turn clears it on the same incarnation; a
later refusal starts a new episode. Evidence from an old incarnation cannot
change the current seat.

The driver record carries an independent `provider_auth` axis: `false` means an
explicit refusal, `true` means a successful turn, and absent means unknown. The
owning driver preserves this axis across activity updates. Its sequence advances
on explicit credential edges; a replacement driver starts without its
predecessor's evidence. Updated driver assets are required for native recovery.

The recognized sources are:

- Claude: typed authentication failures and a bounded, standalone login error in
  the native final assistant reply. The terminal fallback reads the latest
  assistant block, excluding quoted source, tool output, and earlier scrollback.
- Codex: `turn/completed` with typed `unauthorized`, or a completed successful turn
  in the controlled thread. An initial idle snapshot is not proof of login.
- OpenCode: typed `ProviderAuthError`; recovery requires a completed successful
  assistant message in the session that reported the refusal.
- Pi: its native missing-key error field (Pi 0.84.2), or its matching error screen
  before native credential evidence exists. Its ambiguous OAuth/network error
  remains unknown. Successful native completion establishes recovery.
- omp: the producer's credential classification, excluding capacity, policy, and
  transient failures. Its successful native completion establishes recovery.

A person attention item with kind `harness-login` is derived from this canonical
condition. Its stable episode, agent target, host, and attach/login guidance let
clients show one item per affected agent. Ownership follows the agent declaration
and mission requester to a concrete person; unresolved or cyclic ownership does
not select a default operator. The item disappears when the condition clears.
It does not create an independent mission lease or a historical repair request.

The daemon does not enter credentials or restart a shared harness to test this
behavior. The phone and stui must render the new state and attention kind; source
validation alone does not establish deployed UI or real-login acceptance.
