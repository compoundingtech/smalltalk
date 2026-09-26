---
from: morgan
subject: "Fix the above-range clamp regression"
priority: high
---

The `clampkit` test suite has one failing above-range case.

`clamp(15, 0, 10)` must return `10`, but it returns `0`.

Fix the root cause in `src/clamp.js`. Do not delete, skip, or weaken the regression test.

Run `node --test`. Commit the fix on this checkout's current branch.

Report the revision, branch, and green test result.
