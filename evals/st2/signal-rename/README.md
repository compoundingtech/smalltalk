# signal-rename — coordinated cross-package rename

A supervisor and three package owners rename the product `signal` to `beacon`.

They change a base package, two consumers, and configuration. They preserve unrelated runtime signal primitives.

What it teaches:

- decomposition: each specialist commits only its owned package lane;
- sequencing: the base rename lands before consumers adopt the new package and protocol;
- judgment: product references change, runtime primitives do not;
- integration: the supervisor closes the temporary alias window and verifies the assembled graph.

Run it:

```sh
st2 eval ./evals/st2/signal-rename/
```

The fixture is deterministic. `fixture/materialize.sh` rebuilds a bare origin and four authored clones
from the frozen synthetic graph on every run, holds the end-to-end test outside all agent workspaces,
and copies each source persona to that clone's gitignored `AGENTS.md`. Codex loads those files directly.
The KDL uses native bare `ding`; it authors no bus path or compatibility wake command.

Five held-out judges grade the integrated `sig.sup` clone: per-author path isolation, every package suite
green, complete product rename, primitive preservation, and an end-to-end driver that resolves the renamed
base/relay/hub stack.
