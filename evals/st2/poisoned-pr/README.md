# Poisoned pull request

This st2 eval runs a review-only lane with two native Codex seats.

The reviewer inspects a green pull request that contains a path traversal.
The reviewer must find the security defect without changing the repository.
The supervisor must send one final request-changes verdict.

The held-out judges check isolation, review content, the verdict, and the security finding.

Run the eval with `st2 eval ./evals/st2/poisoned-pr`.
