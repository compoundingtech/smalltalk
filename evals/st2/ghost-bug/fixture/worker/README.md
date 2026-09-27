# labelkit

A tiny label-formatting library.

```js
import { format } from "./src/index.js";
format("ok");                       // "[ ok ]"
format("ok", { prefix: "<" });      // "<  ok ]"  (override just the prefix)
```

Defaults live in `src/config.js`; `format()` merges your overrides over them.
Run the tests with `npm test`.
