# signal-rename-codex — a graph-owned cross-package rename

This st3 eval uses four native Codex agents.

The st3 mission owns decomposition, assignment, sequence, nested progress, required revisions, final verification, and cleanup.

The agents rename the Signal product to Beacon across a base package, two consumers, and the root configuration.

The agents preserve `AbortSignal`, `controller.signal`, signal cancellation options, `SIGTERM`, and `SIGINT`.

## Work graph

```text
materialize
    |
start-team
    |
open-base-compatibility           sig.base
    +--------------------+
migrate-relay              migrate-hub
sig.relay                  sig.hub
    +----------+-----------+
       update-root-and-config     sig.sup
                 |
       close-base-compatibility   sig.base
                 |
       integrate-and-verify       sig.sup
                 |
         held-out-gates
                 |
        publish-final-report      sig.sup

cleanup runs after every terminal result
```

Each assigned parent step contains an up-front nested mission. st3 publishes one Small Talk message when the parent becomes ready.

Inherited nested steps use that parent message. An explicit nested reassignment publishes a new Small Talk message.

The native driver only transports Small Talk graph messages. It does not create a separate message type or source of truth.

Each lane publishes a `vcs.commit` resource claim. The mission does not infer completion from prose or exported messages.

The final report remains a message because communication is its product. A resource claim records its publication.

## Run

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.

The graph screen shows the current root and nested work state. It also shows recent semantic state transitions without streaming agent logs.

`materialize.sh` creates a bare origin and four authored clones from the frozen synthetic graph.

The script holds the end-to-end test outside every agent workspace. It also copies each persona to its clone as `AGENTS.md`.

Five mechanical gates grade the integrated clone. A bounded Codex gate inspects the mission claims, work state, revision products, and Git history.

The `receipts/` directory contains historical runs. Each receipt records the exact KDL hash that produced it.
