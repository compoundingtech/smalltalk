# ledger

A tiny command-dispatch service. Handlers are registered into a dispatch map and
looked up by command name:

```js
import { register, dispatch } from "./src/dispatch.js";
register("upper", (s) => String(s).toUpperCase());
dispatch("upper", "hi");   // "HI"
```

`register()` is **last-wins** — registering the same command twice is harmless.
The work-list lives in `items.json`; `PROGRESS.md` records what's done. Run the
tests with `npm test`.
