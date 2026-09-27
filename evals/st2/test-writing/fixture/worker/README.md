# grades

Turn numeric scores (0–100) into letter grades, GPA points, and a summary.

```js
import { letter, gpaPoints, summary } from "./src/grades.js";
letter(85);                 // "B"
gpaPoints("B");             // 3
summary([90, 82, 71]);      // { count: 3, average: 81, gpa: 3, counts: { A:1, B:1, C:1, D:0, F:0 } }
```

- `letter(score)` — "A"/"B"/"C"/"D"/"F" by the usual 90/80/70/60 cutoffs; throws `TypeError`
  (non-number) / `RangeError` (outside 0–100).
- `gpaPoints(letter)` — A=4 … F=0; throws `RangeError` on an unknown letter.
- `summary(scores)` — `{ count, average, gpa, counts }`; throws `RangeError` on an empty array.

> **No tests yet.** `npm test` currently runs nothing — this module needs a real test suite.
