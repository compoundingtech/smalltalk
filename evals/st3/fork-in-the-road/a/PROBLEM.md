# Design problem — sharing one agent team across MULTIPLE humans

Today this agent team effectively serves one human (the operator). We are about to let a **small group of
humans** share it. Design **how humans relate to the agent team when there is more than one of them.**

This is an **open design problem** — there is no single right answer, only tradeoffs. People have
floated a few shapes as a *starting hint* (not the answer set — decide the real set yourselves):

- **one chief-of-staff (CoS) per human** — each human has their own CoS; the CoSs coordinate.
- **one shared CoS** — a single CoS serves everyone.
- **a federated CoS-mesh** — a hybrid: per-human CoS + a shared coordination layer / specialist pool.

## What a good answer contains
- Several **genuinely distinct** approaches (not three flavors of one), each argued at its **strongest**
  and **honest** about where it is weak / what it trades away.
- A real **debate of the tradeoffs** — engage each other's points, update where warranted, and state
  crisply where a real fork remains.
- A **justified recommendation** (or "prototype X first, because Y") that follows from the analysis.
- Explicitly separate **what the team can decide** (the technical shape) from **what it should NOT
  decide for the humans** — escalate the values/irreversible calls back to them.

Deliverable: each proposer writes `PROPOSAL.md` in their own dir; the supervisor synthesizes
`RECOMMENDATION.md` in its own dir. Coordinate everything through the st2 bus. Stay in your own dir.
