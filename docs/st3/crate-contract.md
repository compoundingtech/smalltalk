# st3 crates: what is where and who may use what

Four packages come from what used to be the single `st3` package:

| package | path | holds |
|---|---|---|
| `st3-kernel` | `crates/st3-kernel` | the library's leaf modules, which name no store, api or reconcile code: `accounts`, `archive`, `boot`, `cli_output`, `conversation_header`, `conversation_search`, `disk`, `evidence`, `fleet`, `gate_kinds`, `gate_report`, `github_admission`, `harness_events`, `hooks`, `instance`, `lane`, `memory`, `model`, `native_views`, `pricing`, `projection`, `publication`, `read_cache`, `rules`, `seat_queue`, `skill`, `terminal_binding`. st3-daemon re-exports each one at its old path (`pub use st3_kernel::model;`), so `crate::model` and `st3::model` read the same |
| `st3-telemetry` | `crates/st3-telemetry` | the library's telemetry modules, which name no store, api or reconcile code: `otel`, `otel_client_class`, `reconcile_telemetry`, `recorder`, `recorder_report`, `relay_trace`, `slo_history`, `startup_telemetry`. It depends on `st3-kernel` and `smallclaims`. st3-daemon re-exports each one at its old path. `otlp`, `telemetry`, `slo`, `incremental` and `recorder_receipts` join once their store use moves up |
| `st3-daemon` | `crates/st3-daemon` | the library: the claims graph, store, API, reconciliation and every module the daemon runs, its `hooks/`, `fixtures/` and `proptest-regressions/` |
| `st3` | `crates/st3` | the `st3` binary (`main.rs` and the modules only the binary uses: `cli_help`, `completion`, `directive_note`, `doctor_cli`, `change_cli`, `presentation`, `setup`, `onboarding`), the `st3-fixture` binary, `build.rs`, `tests/`, `examples/`, and an empty library target (`src/package_marker.rs`, see below) |

The binary and the tests depend on the library under the name `st3`
(`st3 = { package = "st3-daemon", path = "../st3-daemon" }`), so `use st3::...` reads the same in
the binary, the tests and every crate that already used it. The reason for the split is Cargo's: a
package has one dependency set for its library and its binary, so a library that sat next to the
binary was rebuilt whenever `stui`, `st3-conversation-ui` or `st3-feed` changed, although it uses
none of them.

## The public surface of st3-daemon

It is the library surface `st3` had: the modules `st3::{api, client, config, graph, model, store,
reconcile, ...}`, the functions `parse_intent` and `validate_mission_runtimes`, and the `cli_print`
macros. Other crates use a part of it:

| consumer | uses |
|---|---|
| the `st3` binary and its tests | everything it needs |
| `stui` (dev) | `api`, `store`, `model`, `reconcile`, `client`, `graph`, `parse_intent` |
| `st3-client` (dev) | `api`, `model`, `graph`, `store`, `test_support` |
| `st3-feed` (dev) | `api`, `model`, `store`, `graph` |
| `st3-migrate` | `parse_intent`, `validate_mission_runtimes`, `model` |

## What stays true, and what pins it

- **The library does not depend on `stui` or `st3-conversation-ui`.** Both are binary-side. If the
  library needs something from them, move it to the binary or into a crate both can use.
- **`st3` and `st3-daemon` have the same version.** The library reports `CARGO_PKG_VERSION` in the
  health endpoint, the published fleet endpoints and the MCP `serverInfo`; a different version
  would change what the binary prints.
- **`crates/st3/build.rs` and `crates/st3-daemon/build.rs` are the same.** Both read the same
  `ST3_FIXTURE_*` variables for the same `test-support` feature: the library reads them in
  `test_support.rs`, the tests read them with `env!`.
- **`cargo test -p st3 --features test-support` still works**: the feature forwards to the library.
- **The `st3` package switches on its own `test-support` in tests.** Before the split a dev-dependency
  of the package on itself did it, and `build.rs` (the `ST3_FIXTURE_*` variables), the `st3-fixture`
  binary and the `required-features` targets depend on it. Cargo allows that only for a package with
  a library target, so `crates/st3/src/package_marker.rs` is an empty library (named
  `st3_package_marker`, in a file other than `lib.rs` so that other pull requests' edits to
  `lib.rs` follow the moved library) and `st3_package_self` is the dev-dependency.
- **The fixtures only the library's unit tests read moved with it**
  (`crates/st3-daemon/tests/fixtures/`). The binary's tests keep `crates/st3/tests/fixtures/`.
- The nextest overrides select a library unit test by `package(st3)` or `package(st3-daemon)`.
- **st3-kernel names nothing above it.** Its modules have no path to `store`, `api`, `reconcile` or
  `test_support`; a kernel unit test that needs daemon code (intent parsing, a store constant) lives
  in st3-daemon's `kernel_tests` and reaches the kernel through the re-exports. A module joins the
  kernel once nothing it names (in production code or its tests) is outside the kernel.
- **st3-telemetry names nothing above the kernel.** Its modules reach `st3_kernel` and `smallclaims` only;
  `startup_telemetry`'s source-reading test still reads `crates/st3/src/main.rs` through `include_str!`.

`scripts/check-module-layers` reports the first three.
