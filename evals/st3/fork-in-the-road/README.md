# Fork in the road

This st3 eval runs a four-agent design panel with native Codex seats.

The three proposal agents are grouped under the synthesis agent for visualization only.

The graph assigns three distinct drafts in parallel.
It then assigns six peer critiques, three revisions, and one synthesis stage.
The graph stores each proposal revision and the final message receipt.

The held-out gates check ownership, distinct designs, privacy analysis, Small Talk debate, graph products, and the final recommendation.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.
