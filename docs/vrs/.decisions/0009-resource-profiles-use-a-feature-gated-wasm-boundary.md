# Resource Profiles use a feature-gated wasm boundary

Status: accepted

Johannes selected the registry/SDK shape on 2026-08-26 (decision Q8), explicitly
amending the prior dotfiles direction that rejected a semantic registry. After
requesting real prototypes rather than a paper choice (Q9), he selected the
wasm-only foundation (Q10) and its goal of absorbing userland complexity once
inside a principled boundary. On 2026-08-27 he amended the decision (Q14):
catalog-relative wasm modules are first-class transactional catalog inputs
rather than external artifacts that happen to sit below the catalog root.

## Context

Root `R20` deliberately preserved Resource URIs while leaving their schemes
open and downstream-owned. Resync could observe `file://` and relative paths,
but a binding such as
`dev.schickling.agent-goal://dev3/cos` remained unwatchable even when the local
carrier was `<agent-dir>/resources/goal.md`. Hard-coding that private scheme in
st2 would violate the portable envelope; leaving resolution to every consumer
would reproduce security, lifecycle, and observability decisions in userland.

Q8 chose an upstream registry/SDK boundary rather than a fixed catalog template
or a dotfiles-owned contracts dependency. Q9 then required declarative,
out-of-process exec, and wasm approaches to be built and measured against the
same goal-resolution shape before choosing the runtime boundary.

## Decision

1. st2 owns an injectable, exact-scheme Resource Profile registry and typed
   resolver SDK contract. The built-in registry is empty; scheme meaning stays
   downstream-owned.
2. A catalog-declared profile names one wasm module and one trusted
   `immediate | coalesced | silent` notification class. Wasm is the only profile
   mechanism: no declarative template tier and no host-exec tier.
3. The module runs as closed core wasm: no WASI, no imports, finite fuel,
   bounded linear memory, fresh instance state per resolution, and host-side
   containment of returned paths under the agent directory. Compiled modules
   are cached by path.
4. Wasmtime is gated behind `wasm-resolver`. Default builds retain the baseline
   dependency tree and binary surface; feature builds consciously absorb the
   runtime cost.
5. Resolver failure is contained to the binding. It becomes unwatchable, the
   supervisor remains alive, and no alternate mechanism guesses a result.
6. A non-absolute module declaration is normalized beneath the catalog root,
   opened no-follow as a bounded regular file, and included by exact path and
   bytes in the whole-catalog projection, root hash, snapshot, prepared bundle,
   bootstrap, apply, and recovery. Duplicate normalized references deduplicate.
   Literal absolute paths remain external inputs and are not copied. Apply
   publishes new module bytes before the `catalog.kdl` that names them and
   removes stale module bytes only afterward.


## Options

| Option | Measured runtime | Isolation and complexity | Result |
| --- | --- | --- | --- |
| Declarative path templates | warm **0.624 µs**; cold registry construction + resolve **2.647 µs** | Smallest runtime and declaration surface, but creates a second mechanism whose placeholders, validation, containment, documentation, and evolution st2 must own alongside programmable profiles. | Rejected as a separate tier. Static mappings use the same wasm boundary. |
| Out-of-process exec plugins | compiled Rust cold p50 **760 µs** (p95 1,117; p99 1,603); bash p50 **2.901 ms** (p95 4.024; p99 4.941); TTL cache hits **0.1–0.4 µs** | Process isolation tests passed for exit, 60 s hang bounded by timeout, garbage, oversized output, and crash-mid-write. About **295 core LOC** once plus **12–16 LOC/profile**, but every plugin is ambient host code and repeats packaging/security review. | Rejected. Cold path is 38–145× wasm and execution is RCE by design. |
| Feature-gated wasm plugins | warm p50 **0.29 µs**; cold p50 **20.1 µs**, p99 **33.8 µs**; compile module once **2.6 ms** | Closed sandbox; 12 isolation tests cover traps, fuel-bounded infinite loops, garbage returns, invalid memory, and oversized allocation. One runtime absorbs policy for every consumer. Costs **+71 lock packages**, **+16.35 MB / +188%** binary, and **+45.7 s** serial-equivalent compile work when enabled. | **Selected**, feature-gated. |

A combined declarative-plus-wasm design was also viable on raw performance, but
it preserved two resolver concepts forever to save roughly 18 µs on a cold call.
The selected foundation values one auditable composition boundary over that
imperceptible savings. Declarative-plus-exec was rejected because exec preserves
neither one mechanism nor a sandbox.

## Evidence and Argument

The comparison is recorded in
[`07-resource-profile/.experiments/2026-08-26-plugin-boundary-comparison.md`](../07-resource-profile/.experiments/2026-08-26-plugin-boundary-comparison.md).
All three prototypes used the same logical operation: resolve
`dev.schickling.agent-goal://<host>/<identity>` to
`<agent-dir>/resources/goal.md` and supply a notification class.

The wasm prototype established the properties that matter for a load-bearing
plugin boundary:

- a 1,469-byte Rust guest artifact implements the closed ABI;
- the module receives no host imports or WASI capabilities;
- 5,000,000 fuel units bound computation and 64 MiB bounds linear memory;
- each resolution gets fresh store/instance state while compilation is cached;
- the host validates memory, UTF-8/JSON, and lexical path containment;
- twelve hostile-module tests leave the supervisor alive;
- cold p99 is 33.8 µs, far below even a compiled exec plugin's p50.

The cost is also material and was not hidden: a feature build grew the binary
from 8,672,072 to 25,026,152 bytes, added 71 lockfile packages, and added 45.7 s
of serial-equivalent compile work. That is why optional compilation is part of
the decision rather than a later optimization. The repository dev shell also
adds `lld`, required to link `wasm32-unknown-unknown` guests.

The real-shape proof is recorded in
[`07-resource-profile/.experiments/2026-08-26-dotfiles-real-shape-e2e.md`](../07-resource-profile/.experiments/2026-08-26-dotfiles-real-shape-e2e.md).
The actual Nix-generated standing-seat declaration resolved through the wasm
profile and observed a whole-file goal rename in about 600 ms, emitted exactly
one deterministic resync event, and kept an equal-byte rewrite silent in two
fresh runs. The selected boundary therefore composes with real catalog,
watcher, and event behavior rather than only a microbenchmark.

The Q14 amendment closes a deployment gap revealed in review: the original
transaction projected `catalog.kdl` but omitted the relative module it named.
That made a generated snapshot internally incomplete and caused a prepared
catalog containing the module to fail as unprojected. Targeted transaction
tests now bind module bytes into the root hash, deduplicate references, exclude
absolute modules, reject traversal/symlink/FIFO/missing/oversized and
unprojected inputs, and load a relative module from the applied live catalog.

## Consequences

- st2 assumes a permanent guest ABI compatibility obligation; versioning is
  tracked as `DQ-P1` in the Resource Profile spec.
- Operators who need profiles build with `wasm-resolver` and distribute guest
  artifacts; operators who do not need them pay no wasmtime cost.
- Even trivial path mappings require a wasm artifact. This is intentional: one
  module boundary replaces recurring resolver-specific host logic.
- Host-exec resolvers and declarative templates are not dormant alternate paths
  or follow-up tiers. Adding either would require new evidence and a new
  decision.
- Scheme ownership remains federated. The registry maps exact strings supplied
  by the catalog; it does not turn st2 into the authority for private URI
  semantics.
- Whole-catalog publication is deliberately superset-biased across crashes.
  A failure can leave an unreferenced new or old module until recovery, but
  ordered atomic leaf replacement prevents `catalog.kdl` from pointing at a
  missing newly introduced module.
- Resolver observability and same-path module-cache invalidation remain explicit
  design questions; neither weakens the containment and feature-gating contract.

## Amendment — 2026-09-01 (Q39)

Johannes approved Q39 to make WASIp2 Component Model components the universal
execution envelope for observable Resource providers. This amendment preserves
the original decision's closed core-wasm resolver and replaces only the
observable host-process mechanism described later by
[decision 0014](./0014-resource-profiles-are-state-first-read-and-observe-capabilities.md).
Decision 0014-resource-profiles-are-state-first-read-and-observe-capabilities's state-first authority, demand semantics, typed `Publication`,
semantic filtering, and catch-up model remain in force.

### Context

The closed resolver has one pure job: map an opaque Resource URI to a contained
carrier path. It needs neither provider I/O nor the Component Model. Observable
providers have a different job: call a remote or local provider, normalize its
domain state, and propose a canonical publication. A catalog-trusted native
process can perform that job, but it carries ambient host authority and creates
a second long-lived lifecycle and JSON protocol beside Wasmtime.

Five disposable prototypes tested a narrower boundary with Wasmtime 48.0.1.
Typed GitHub and PTY observations proved real domain I/O without exposing raw
HTTP, caller-selected executable/arguments, environment, filesystem, or socket
access. Fresh-Store cancellation tests ended with zero active tasks or
capabilities. Verified compiled-code reuse kept a small provider's AOT disk-hit
p50 at 0.482 ms and fresh Store plus instance observation p50 at 47.751 µs. An
independent multiprocess oracle passed three 30-process runs covering
one-winner compare-and-swap, stale generation, process-crash boundaries,
acknowledgement loss, deterministic outbox identity, and restart catch-up.

### Decision

1. The core-wasm resolver ABI, its no-import sandbox, fresh resolution Store,
   path containment, registry behavior, feature gate, and transactional module
   ownership remain the only resolution mechanism.
2. Every observable provider executes through one versioned WASIp2 Component
   Model envelope. st2 does not maintain a parallel native or host-process
   provider framework.
3. A provider component may import only explicit provider-domain capabilities
   linked by the host. The host owns credentials, allowlists, limits,
   deadlines, cancellation, and redacted typed failures. No ambient WASI
   command, environment, clock, random, process, raw HTTP, filesystem, or
   socket authority is linked.
4. Every descriptor call and observation receives a fresh Store and component
   instance. Engine, Linker, compiled Component, and compatible host-produced
   AOT bytes may be reused; Store and instance state may not be pooled or reset.
5. The component returns `Unchanged`, a typed failure, or the existing
   `Publication` payload. It never writes the carrier, state record, receipt, or
   outbox. The host pairs a publication with
   `ProposalFence { generation, revision, prior_digest }`, validates it, and
   owns the only atomic commit.
6. One durable transition makes the carrier, resulting digest and revision,
   freshness and catch-up state, and deterministic `PublicationIntent` visible
   together. A crash before publication leaves the old state. A crash after
   publication but before acknowledgement leaves the intent retryable;
   delivery remains separate and idempotent.
7. WASIp3 production execution, Store pooling, generic exec/raw
   HTTP/filesystem/socket authority, a parallel native provider framework, and
   runtime WAC graphs are explicit non-goals. Each requires new evidence and a
   further accepted decision rather than an alternate dormant path.

### Evidence and argument

The durable record is the
[WASIp2 component and atomic publication experiment](../07-resource-profile/.experiments/2026-09-01-wasip2-component-and-atomic-publication-prototypes.md).
The disposable harnesses prove the boundary; their local source, result, cache,
and state paths are not production interfaces or scaffolding.

The Component Model is selected for observable providers because its typed
imports make authority reviewable at link time and its typed export preserves
one observation result. Fresh Stores remove the need for an incomplete reset
protocol across guest memory, resources, host state, traps, and cancellation.
Host-only commit prevents a network- or command-capable guest from bypassing
validation or racing publication against settlement. The deterministic durable
outbox makes publication and delivery intent one crash-consistent fact without
claiming exactly-once effects across an external sink.

### Consequences

- A profile may carry two wasm artifacts with deliberately disjoint jobs: a
  closed core module for resolution and, only when observable, a WASIp2
  component for provider observation.
- Provider support requires a reviewed typed host capability; adding a generic
  authority under a domain-flavored name is non-conforming.
- Domain acquisition state such as ETags, cursors, rate limits, and webhook
  repair cannot rely on guest Store lifetime. It belongs in bounded host-owned
  capability state or explicit durable provider state.
- Compiled-code caching is an optimization, not an authority transfer. Any AOT
  deserialize path must authenticate exact host-produced bytes and the complete
  engine-compatibility key before crossing Wasmtime's unsafe boundary.
- Decision 0014-resource-profiles-are-state-first-read-and-observe-capabilities's host-process topology and newline-delimited JSON mechanism
  are superseded. Its publication, demand, delivery, and state-first semantics
  apply to direct component invocations.
