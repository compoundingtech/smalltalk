# Poisoned pull request

This st3 eval runs a review-only lane with two native Codex seats.

The graph assigns the review directly to the reviewer.
The reviewer sends one report to the supervisor.
The supervisor verifies the report and sends one final requester verdict.
The graph stores both message receipts.

The held-out gates check isolation, review content, the verdict, the security finding, and graph products.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.
