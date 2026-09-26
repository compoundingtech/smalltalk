---
subject: "bug: custom format options leak into later calls (labelkit)"
priority: high
---
We have a real bug in the `labelkit` library (the repo your specialist `gbx.fix` owns). Users report
that our `format()` helper **leaks custom options into later calls**: once you format something with a
custom prefix, every *subsequent* default format keeps that prefix — the defaults get "stuck".

Concrete repro:
```
import { format } from "./src/format.js";
format("a", { prefix: "<", suffix: ">" });   // "< a >"  — fine
format("b");                                 // BUG: returns "< b >", should be "[ b ]"
```

The unit tests are **all green**, which is part of why this slipped through.

You're the supervisor: your specialist `gbx.fix` owns that repo — delegate this to it. Ask it to:
1. **Reproduce** the bug,
2. find the **ROOT cause** (the actual defect — not a band-aid that just hides the symptom),
3. fix it with the **smallest correct change**,
4. **add a regression test** that would have caught this (a test that FAILS on the current buggy code
   and passes after the fix), and keep the whole suite green,
5. commit, and report back the root cause, the fix (files + commit), and the new test.

Verify read-only when it reports done, then reply to me with the root cause, the commit, and your
verification — only once it's fully done and you've verified it. Stay in lanes: `gbx.fix` touches only
its own repo; coordinate by message.
