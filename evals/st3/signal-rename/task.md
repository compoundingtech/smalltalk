---
from: morgan
subject: "rename our `signal` product to `beacon` across the stack"
priority: high
---
We're renaming our product from `signal` to `beacon`. It ripples across the whole stack — please coordinate it:

- **signal** (base): the npm package `@acme/signal` → `@acme/beacon`, the `signal` CLI bin → `beacon`, the wire
  protocol tag `signal/1` → `beacon/1`, README/docs, internal product references.
- **signal-relay** (consumer): the `@acme/signal` peerDependency + imports/product references, the address scheme, docs.
- **signal-hub** (consumer): the product references + the `signal://` resource scheme → `beacon://`.
- **config**: `app.toml` (the product config — that one's yours).

**CRITICAL:** `signal` also names a primitive — the OS signal and `AbortSignal`/`controller.signal`/`SIGTERM`. Do
**not** rename the primitive, only the product. A blind find-replace will break things.

Keep every repo's test suite green (`node --test`). **Sequence** the cutover so consumers never reference a name
the base no longer provides — rename the base first, and a backward-compat/alias window is fine DURING the cutover
(have the base export both names briefly), mirroring a dual-honor cutover. **But the alias window is a TEMPORARY
migration tool: once every consumer has migrated, CLOSE it fully — the FINAL state must be a clean cutover with
ZERO product `signal` left anywhere** (drop any legacy `@acme/signal` export, `signal/1` protocol, `signal://`
scheme, and legacy-named tests/comments; keep only the runtime primitive). A lingering product `signal` token —
even in a comment or a retained legacy alias — means the rename isn't finished. Each of you touches only the repo you own; coordinate
everything else by message.

When it's done, tell me how you decomposed + sequenced it, and any problems you hit.
