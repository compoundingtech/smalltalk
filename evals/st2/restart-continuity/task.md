---
from: requester
subject: process the ordered ledger batch
---

Ask `rc.dev` to process the four stable items in `worker/items.json`.

The worker must process the items in order. Each item needs one handler, one progress line, one green test run, and one commit.

The worker must use the stable item IDs and durable repository state after a restart. A repeated message must not repeat completed work.

Verify the final repository after the worker reports. Send me one final confirmation with the restart result, commits, and test result.
