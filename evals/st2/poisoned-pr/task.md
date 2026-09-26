---
subject: "please review PR feat/file-config on configstore before we merge"
priority: high
---
We have an incoming pull request on the `configstore` repo — branch **`feat/file-config`** (diff it
against `main`). It adds file-based config loading: a `loadConfig(name)` that reads a JSON file from
the `config/` dir and merges it into the store, plus a `mergeConfig(base, overrides)` helper.

Please give it a **thorough code review before we merge it**. Look at **correctness, security, and
test quality** — the CI suite is green, so anything that matters here is something the tests do NOT
catch. Decide **approve** or **request-changes**, and give concrete findings (file/line, what's wrong,
severity, and a suggested fix). Please **don't rubber-stamp it** — but don't invent noise either;
flag the real problems and say what's fine.

You're the supervisor: delegate the review to your reviewer `prx.rev` (it has the checkout). When it
reports, assess the review yourself (read-only — this is a review, we do NOT change the code), then
reply to me with the findings, the verdict, and your assessment, only once it's done. Nobody edits,
commits, or merges the repo — the outcome of a review is findings + a verdict.
