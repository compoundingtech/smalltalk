# Continuous stewardship eval

This eval compresses a continuous project into two finite cycles.

One mission-run owns the long-lived agent and the recurring wake schedule. Each cycle is a separate nested mission-run with a unique graph path. The second cycle cannot start before the first cycle and the agent restart complete.

The schedule uses `catch-up "latest"`. A delayed daemon sends one current wake instead of replaying every missed interval.

The eval keeps both cycle resources as history. Normal completion and cancellation both enter `finally`, which stops the steward before eval cleanup removes owned runtime state.

The agent receives no eval-specific boot prompt. The runtime adds the normal `.st3/boot.md` contract.
