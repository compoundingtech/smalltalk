# Automatic GitHub intake

This eval uses a local file as a deterministic repository event source.

The observer establishes a baseline. A later change starts one exact concurrent review mission with an immutable resource input.

The root mission waits for the observer baseline before it changes the source file.

A bounded wait program then observes the asynchronous review result.

The review stays inside its isolated workspace. Unit tests cover the GitHub repository normalization rules.
