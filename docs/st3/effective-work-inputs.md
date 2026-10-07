# Effective work inputs

`store/effective_work.rs` supplies one calculator for effective work state.
The existing `apply_effective_step_state` wrapper uses `SqlInputs`, retaining
the same Store sources and the order in which they are consulted. Reconciler
effect fences, work actions, timing folds and queue selection are unchanged.

The internal `Inputs` trait lets a keyed source adapter supply the same inputs
without copying the calculation. It loads the owning run, selected/current
generation and root first. A missing owner preserves the existing reader's
early return. Person-assigned work then checks its request, followed by terminal
owner fences, inclusive lease expiry and blockers when relevant. Terminal owners
skip blocker reads. Expiry clears the claimant/incarnation/expiry and saturates
the readiness epoch exactly as before; person blocker release retains the
existing epoch arithmetic.

Alternative providers must certify all dependencies in the same authorized
snapshot and at the supplied captured time. Unknown coverage must return an
error; it cannot be represented as a missing owner/request or empty blockers.
The trait itself does not establish readiness, existence or authority. No
alternative provider, IVM registration or production reader activation ships
with this extraction.

The existing SQL person-request/blocker sources still perform their established
canonical queries. Replacing them with complete keyed sources is separate work;
this extraction makes no new bounded-maintenance or performance claim.
