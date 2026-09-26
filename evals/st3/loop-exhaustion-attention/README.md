# Loop exhaustion attention

This model-free eval proves that a bounded loop requests attention when it reaches its round limit.

The mission fails by design. Its two rounds leave the exit resource unready.

The failure creates one attention item for `person/nathan`. The item targets the loop and its root mission run.

The reconciler unit test also repeats reconciliation and proves that the request stays idempotent.
