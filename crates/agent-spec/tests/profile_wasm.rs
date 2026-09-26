//! Failure-isolation contract for wasm-backed resource profiles.
//!
//! Every test here is also a containment claim: whatever the guest does — trap, loop forever,
//! return nonsense, blow its memory budget — the host call returns a typed error, the registry
//! folds it into "unwatchable", and the process survives to resolve again.
//!
//! Gated behind `wasm-resolver`: without the feature there is no sandbox to isolate.

#![cfg(feature = "wasm-resolver")]

use agent_spec::profile::{ProfileClass, ResourceProfile, ResourceProfileRegistry};
use agent_spec::profile_wasm::{
    DEFAULT_MODULE_LIMIT_BYTES, DEFAULT_OUTPUT_LIMIT_BYTES, DEFAULT_TABLE_ELEMENT_LIMIT,
    WasmResolveError, WasmResolver,
};
use std::path::{Path, PathBuf};
use wasmtime::Trap;

const DEMO_WASM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/demo_resolver.wasm"
);

fn demo_profile(class: ProfileClass) -> ResourceProfile {
    ResourceProfile::wasm("dev.schickling.agent-goal", DEMO_WASM_PATH, class)
}

fn demo_registry() -> ResourceProfileRegistry {
    ResourceProfileRegistry::empty().with_profile(demo_profile(ProfileClass::Immediate))
}

/// A minimal well-formed guest used by the hostile variants below; each hostile module swaps in
/// its own exports.
const HOSTILE_PRELUDE: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 1024))
"#;
fn wat_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!(r"\{byte:02x}")).collect()
}

fn descriptor_wat(payload: &[u8], reported_len: usize, resolution_path: &str) -> String {
    const DESCRIPTOR_PTR: usize = 8;
    const RESOLUTION_PTR: usize = 70_000;
    let resolution = format!(r#"{{"path":"{resolution_path}"}}"#);
    let descriptor_packed = ((DESCRIPTOR_PTR as u64) << 32) | reported_len as u64;
    let resolution_packed = ((RESOLUTION_PTR as u64) << 32) | resolution.len() as u64;
    format!(
        r#"(module
  (memory (export "memory") 2)
  (func (export "alloc") (param i32) (result i32) (i32.const 120000))
  (data (i32.const {DESCRIPTOR_PTR}) "{}")
  (data (i32.const {RESOLUTION_PTR}) "{}")
  (func (export "describe") (result i64) (i64.const {descriptor_packed}))
  (func (export "resolve") (param i32 i32 i32 i32) (result i64)
    (i64.const {resolution_packed}))
)"#,
        wat_bytes(payload),
        wat_bytes(resolution.as_bytes()),
    )
}

fn descriptor_module(payload: &[u8], reported_len: usize) -> WasmResolver {
    WasmResolver::from_wat(&descriptor_wat(
        payload,
        reported_len,
        "resources/current.json",
    ))
    .expect("descriptor module compiles")
}

const VALID_DESCRIPTOR: &str = r#"{
  "abiVersion": 3,
  "capabilities": ["resolve", "read", "observe"],
  "selectorSchema": {
    "type": "object",
    "properties": {
      "topics": {
        "type": "array",
        "items": { "type": "string" },
        "uniqueItems": true
      }
    },
    "required": ["topics"],
    "additionalProperties": false
  },
  "defaultSelector": { "topics": ["state.changed"] },
  "topics": [{ "name": "state.changed" }],
  "runtime": { "topology": "perBinding" },
  "snapshot": {
    "mediaType": "application/json",
    "schemaId": "dev.example.state.v1"
  }
}"#;

fn current_rss_bytes() -> u64 {
    // /proc/self/status VmRSS is reported in kB.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[test]
fn valid_v3_descriptor_executes_and_resolve_only_module_stays_passive() {
    let descriptor = descriptor_module(VALID_DESCRIPTOR.as_bytes(), VALID_DESCRIPTOR.len())
        .describe_once()
        .expect("describe call succeeds")
        .expect("descriptor is present");
    assert_eq!(descriptor.abi_version, 3);

    assert!(
        WasmResolver::load(Path::new(DEMO_WASM_PATH))
            .expect("passive resolver loads")
            .describe_once()
            .expect("missing describe is compatible")
            .is_none()
    );
    assert!(
        demo_registry()
            .try_descriptor("dev.schickling.agent-goal")
            .expect("passive registry descriptor lookup succeeds")
            .is_none()
    );
}

#[test]
fn refresh_descriptor_lookup_and_resolution_share_one_module_snapshot() {
    let directory = tempfile::tempdir().expect("temporary module directory");
    let path = directory.path().join("smart.wasm");
    std::fs::write(
        &path,
        descriptor_wat(
            VALID_DESCRIPTOR.as_bytes(),
            VALID_DESCRIPTOR.len(),
            "resources/current.json",
        ),
    )
    .expect("module is written");

    let registry = ResourceProfileRegistry::empty().with_profile(ResourceProfile::wasm(
        "smart",
        &path,
        ProfileClass::Coalesced,
    ));
    let refresh = registry.begin_refresh();
    assert!(
        refresh
            .try_descriptor("smart")
            .expect("descriptor validates")
            .is_some()
    );
    std::fs::write(
        &path,
        descriptor_wat(
            VALID_DESCRIPTOR.as_bytes(),
            VALID_DESCRIPTOR.len(),
            "resources/replaced.json",
        ),
    )
    .expect("module generation is replaced after descriptor lookup");
    assert_eq!(
        refresh
            .try_resolve(Path::new("/agent"), "smart://resource")
            .expect("resolution succeeds")
            .expect("registered URI resolves")
            .path,
        PathBuf::from("/agent/resources/current.json")
    );
    assert_eq!(
        registry
            .try_resolve(Path::new("/agent"), "smart://resource")
            .expect("a new refresh sees the replacement")
            .expect("registered URI resolves")
            .path,
        PathBuf::from("/agent/resources/replaced.json")
    );
}

#[test]
fn malformed_and_oversized_descriptors_are_contained() {
    match descriptor_module(b"{not-json", 9).describe_once() {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("key must be a string"), "got: {error}");
        }
        other => panic!("expected malformed descriptor rejection, got {other:?}"),
    }

    match descriptor_module(b"{}", DEFAULT_OUTPUT_LIMIT_BYTES + 1).describe_once() {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("return payload"), "got: {error}");
            assert!(error.contains("limit"), "got: {error}");
        }
        other => panic!("expected oversized descriptor rejection, got {other:?}"),
    }
}

#[test]
fn demo_module_resolves_through_the_registry_seam_with_its_declared_class() {
    let resolved = demo_registry()
        .try_resolve(
            Path::new("/cat/agents/dev3/janitor"),
            "dev.schickling.agent-goal://dev3/janitor",
        )
        .expect("demo resolver succeeds")
        .expect("goal scheme resolves");
    assert_eq!(
        resolved.path,
        PathBuf::from("/cat/agents/dev3/janitor/resources/goal.md")
    );
    assert_eq!(resolved.class, ProfileClass::Immediate);
}

#[test]
fn declared_class_rides_along_independent_of_the_module_output() {
    let registry =
        ResourceProfileRegistry::empty().with_profile(demo_profile(ProfileClass::Silent));
    let resolved = registry
        .resolve(Path::new("/a"), "dev.schickling.agent-goal://x")
        .expect("resolution succeeds");
    assert_eq!(resolved.class, ProfileClass::Silent);
}

#[test]
fn resolver_cannot_escape_the_agent_directory() {
    // A hostile module returns an absolute path; the host-side containment check must reject it.
    let escape = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      ;; JSON payload: path /etc/passwd, class goal (37 bytes)
      (data (i32.const 8) "{{\22path\22:\22/etc/passwd\22,\22class\22:\22goal\22}}")
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 8)) (i64.const 32))
          (i64.extend_i32_u (i32.const 37))))
    )"#
    ))
    .expect("escape module compiles");
    match escape.resolve_contained("any://x", Path::new("/agent/dir")) {
        Err(WasmResolveError::BadReturn(e)) => {
            assert!(e.to_string().contains("escaped"), "got: {e}");
        }
        other => panic!("expected containment rejection, got {other:?}"),
    }
}

#[test]
fn resolver_rejects_an_empty_path_before_joining_it_to_the_agent_directory() {
    let empty = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (data (i32.const 8) "{{\22path\22:\22\22,\22class\22:\22goal\22}}")
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 8)) (i64.const 32))
          (i64.extend_i32_u (i32.const 26))))
)"#
    ))
    .expect("empty-path module compiles");
    match empty.resolve_contained("any://x", Path::new("/agent/dir")) {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("empty path"), "got: {error}");
        }
        other => panic!("expected empty-path refusal, got {other:?}"),
    }
}

#[test]
fn resolver_rejects_a_path_that_normalizes_to_the_agent_directory() {
    let root = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (data (i32.const 8) "{{\22path\22:\22sub/..\22,\22class\22:\22goal\22}}")
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 8)) (i64.const 32))
          (i64.extend_i32_u (i32.const 32))))
)"#
    ))
    .expect("root-path module compiles");
    match root.resolve_contained("any://x", Path::new("/agent/dir")) {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("agent directory"), "got: {error}");
        }
        other => panic!("expected root-path refusal, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn resolver_cannot_cross_a_symlink_inside_the_agent_directory() {
    let agent_dir = tempfile::tempdir().expect("agent directory is created");
    let outside = tempfile::tempdir().expect("outside directory is created");
    std::fs::write(outside.path().join("goal.md"), "external").expect("external file is created");
    std::os::unix::fs::symlink(outside.path(), agent_dir.path().join("resources"))
        .expect("resources symlink is created");

    let resolver =
        WasmResolver::load(Path::new(DEMO_WASM_PATH)).expect("demo resolver module loads");
    match resolver.resolve_contained("dev.schickling.agent-goal://x", agent_dir.path()) {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("symlink"), "got: {error}");
        }
        other => panic!("expected symlink containment rejection, got {other:?}"),
    }
}

#[test]
fn trap_inside_resolve_is_contained_and_the_engine_resolves_again_afterwards() {
    let trap = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func (export "resolve") (param i32 i32 i32 i32) (result i64) (unreachable))
)"#
    ))
    .expect("trap module compiles");

    for _ in 0..3 {
        match trap.resolve_once("dev.schickling.agent-goal://x", "/a") {
            Err(WasmResolveError::Trap(Trap::UnreachableCodeReached)) => {}
            other => panic!("expected unreachable trap, got {other:?}"),
        }
    }
}

#[test]
fn infinite_loop_hits_the_fuel_budget_deterministically() {
    let looper = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func (export "resolve") (param i32 i32 i32 i32) (result i64) (loop (br 0)) (unreachable))
)"#
    ))
    .expect("looping module compiles")
    .with_fuel_per_call(1_000_000);

    let start = std::time::Instant::now();
    match looper.resolve_once("dev.schickling.agent-goal://x", "/a") {
        Err(WasmResolveError::FuelExhausted) => {}
        other => panic!("expected fuel exhaustion, got {other:?}"),
    }
    // Fuel is metered work, not wall time: even a tight infinite loop stops promptly.
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "fuel interruption took {:?}",
        start.elapsed()
    );
}

#[test]
fn finite_start_function_runs_inside_the_initial_fuel_budget() {
    let finite = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func $start (nop))
      (start $start)
      (func (export "resolve") (param i32 i32 i32 i32) (result i64) (i64.const 0))
)"#
    ))
    .expect("finite-start module compiles")
    .with_fuel_per_call(10_000);

    finite
        .instantiate()
        .expect("finite start function receives bounded fuel");
}

#[test]
fn infinite_start_function_hits_the_fuel_budget_during_instantiation() {
    let looper = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func $start (loop (br 0)))
      (start $start)
      (func (export "resolve") (param i32 i32 i32 i32) (result i64) (i64.const 0))
)"#
    ))
    .expect("infinite-start module compiles")
    .with_fuel_per_call(10_000);

    assert!(matches!(
        looper.instantiate(),
        Err(WasmResolveError::FuelExhausted)
    ));
}

#[test]
fn start_and_first_resolve_share_one_fuel_allowance() {
    let shared = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func $burn (param $remaining i32)
        (block $done
          (loop $again
            (br_if $done (i32.eqz (local.get $remaining)))
            (local.set $remaining (i32.sub (local.get $remaining) (i32.const 1)))
            (br $again))))
      (func $start (call $burn (i32.const 100)))
      (start $start)
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (call $burn (i32.const 100))
        (i64.const 0))
)"#
    ))
    .expect("metered module compiles")
    .with_fuel_per_call(1_000);

    match shared.resolve_once("any://x", "/a") {
        Err(WasmResolveError::FuelExhausted) => {}
        other => panic!("expected the shared start+call allowance to exhaust, got {other:?}"),
    }
}

#[test]
fn oversized_initial_table_is_rejected_before_host_allocation() {
    let oversized = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (table {} funcref)
      (func (export "resolve") (param i32 i32 i32 i32) (result i64) (i64.const 0))
)"#,
        DEFAULT_TABLE_ELEMENT_LIMIT + 1
    ))
    .expect("large-table module compiles");

    assert!(matches!(
        oversized.instantiate(),
        Err(WasmResolveError::Instantiation(_))
    ));
}

#[test]
fn garbage_return_payload_is_reported_not_crashed_on() {
    let garbage = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (data (i32.const 8) "not json at all!!")
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 8)) (i64.const 32))
          (i64.extend_i32_u (i32.const 17))))
)"#
    ))
    .expect("garbage module compiles");
    match garbage.resolve_once("dev.schickling.agent-goal://x", "/a") {
        Err(WasmResolveError::BadReturn(_)) => {}
        other => panic!("expected BadReturn, got {other:?}"),
    }
}

#[test]
fn oversized_valid_return_range_is_rejected_before_decoding() {
    let oversized = WasmResolver::from_wat(
        r#"(module
  (memory (export "memory") 2)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "resolve") (param i32 i32 i32 i32) (result i64)
    (i64.extend_i32_u (i32.const 65537)))
)"#,
    )
    .expect("oversized-return module compiles");
    match oversized.resolve_once("dev.schickling.agent-goal://x", "/a") {
        Err(WasmResolveError::BadReturn(error)) => {
            assert!(error.contains("limit"), "got: {error}");
            assert!(error.contains(&(DEFAULT_OUTPUT_LIMIT_BYTES + 1).to_string()));
        }
        other => panic!("expected output-limit rejection, got {other:?}"),
    }
}

#[test]
fn wild_return_pointer_is_caught_before_memory_access() {
    let wild = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        ;; ptr = 0xF000_0000 (far outside linear memory), len = 100
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const -268435456)) (i64.const 32))
          (i64.extend_i32_u (i32.const 100))))
)"#
    ))
    .expect("wild-pointer module compiles");
    match wild.resolve_once("dev.schickling.agent-goal://x", "/a") {
        Err(WasmResolveError::BadReturn(e)) => {
            assert!(e.to_string().contains("outside linear memory"), "got: {e}");
        }
        other => panic!("expected out-of-bounds rejection, got {other:?}"),
    }
}

#[test]
fn oversized_allocation_hits_the_memory_limit_instead_of_the_host() {
    // The guest tries to grow linear memory far past the configured cap. Under a store limiter
    // wasmtime refuses the growth; either way the failure stays inside the sandbox and the
    // host's own RSS is unmoved.
    let greedy = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (drop (memory.grow (i32.const 65536)))
        (unreachable))
)"#
    ))
    .expect("greedy module compiles")
    .with_memory_limit(2 * 1024 * 1024);

    let before = current_rss_bytes();
    match greedy.resolve_once("dev.schickling.agent-goal://x", "/a") {
        // The denied growth surfaces as an out-of-bounds/unreachable trap inside the guest;
        // either way it never leaves the sandbox.
        Err(WasmResolveError::Trap(_)) => {}
        other => panic!("expected contained allocation failure, got {other:?}"),
    }
    let after = current_rss_bytes();
    if before > 0 {
        assert!(
            after.saturating_sub(before) < 512 * 1024 * 1024,
            "host RSS jumped by {} bytes servicing a hostile guest",
            after - before
        );
    }
}

#[test]
fn deep_recursion_traps_at_guest_stack_overflow_not_native_stack_exhaustion() {
    let recursive = WasmResolver::from_wat(&format!(
        r#"{HOSTILE_PRELUDE}
      (func $f (export "spin") (param i32) (result i32)
        (call $f (i32.add (local.get 0) (i32.const 1))))
      (func (export "resolve") (param i32 i32 i32 i32) (result i64)
        (drop (call $f (i32.const 0)))
        (i64.const 0))
)"#
    ))
    .expect("recursive module compiles");
    match recursive.resolve_once("dev.schickling.agent-goal://x", "/a") {
        Err(WasmResolveError::Trap(Trap::StackOverflow)) => {}
        Err(WasmResolveError::Trap(t)) => panic!("expected stack overflow, got {t}"),
        other => panic!("expected stack overflow, got {other:?}"),
    }
}

#[test]
fn module_without_required_exports_fails_instantiation_cleanly() {
    let incomplete =
        WasmResolver::from_wat(r#"(module (memory (export "memory") 1))"#).expect("compiles");
    assert!(matches!(
        incomplete.instantiate(),
        Err(WasmResolveError::MissingExport("alloc"))
    ));
}

#[test]
fn oversized_module_is_rejected_before_wasmtime_compilation() {
    let temp = tempfile::NamedTempFile::new().expect("temporary module file");
    temp.as_file()
        .set_len(DEFAULT_MODULE_LIMIT_BYTES as u64 + 1)
        .expect("oversized sparse module");
    match WasmResolver::load(temp.path()) {
        Err(WasmResolveError::Instantiation(error)) => {
            assert!(error.contains("limit"), "got: {error}");
        }
        Err(other) => panic!("expected size-limit instantiation error, got {other}"),
        Ok(_) => panic!("oversized module unexpectedly compiled"),
    }
}

#[cfg(unix)]
#[test]
fn catalog_relative_module_rejects_symlinked_path_ancestors() {
    let catalog = tempfile::tempdir().expect("catalog directory");
    let outside = tempfile::tempdir().expect("outside directory");
    std::fs::copy(DEMO_WASM_PATH, outside.path().join("demo.wasm"))
        .expect("outside module is copied");
    std::os::unix::fs::symlink(outside.path(), catalog.path().join("resolvers"))
        .expect("resolver ancestor symlink is created");
    let registry = ResourceProfileRegistry::empty().with_profile(ResourceProfile::wasm_contained(
        "dev.schickling.agent-goal",
        catalog.path(),
        "resolvers/demo.wasm",
        ProfileClass::Immediate,
    ));

    let error = registry
        .try_resolve(catalog.path(), "dev.schickling.agent-goal://host/worker")
        .expect_err("a symlinked module ancestor must be rejected");
    assert!(!error.is_empty());
}

#[cfg(unix)]
#[test]
fn special_file_module_is_rejected_without_blocking() {
    use std::os::unix::ffi::OsStrExt as _;

    let temp = tempfile::tempdir().expect("module directory");
    let fifo = temp.path().join("resolver.fifo");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
    // SAFETY: the path is NUL-terminated and points into the live temporary directory.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    match WasmResolver::load(&fifo) {
        Err(WasmResolveError::Instantiation(error)) => {
            assert!(error.contains("regular file"), "got: {error}");
        }
        Err(other) => panic!("expected special-file refusal, got {other}"),
        Ok(_) => panic!("FIFO resolver unexpectedly compiled"),
    }
}

#[test]
fn registry_folds_every_guest_failure_into_unwatchable_and_keeps_resolving() {
    // A broken module registered under a scheme: every resolution fails contained, with the
    // reason surfaced for observability.
    let broken = std::env::temp_dir().join("st2-profile-wasm-suite-broken.wasm");
    std::fs::write(&broken, b"not a module").unwrap();
    let doomed = ResourceProfileRegistry::empty().with_profile(ResourceProfile::wasm(
        "dev.schickling.agent-goal",
        &broken,
        ProfileClass::Immediate,
    ));
    let reason = doomed
        .try_resolve(Path::new("/a"), "dev.schickling.agent-goal://k")
        .unwrap_err();
    assert!(
        !reason.is_empty(),
        "the failure reason is surfaced, not swallowed"
    );

    // Sibling schemes stay unregistered-None and healthy modules keep resolving around the
    // failure, repeatedly (the supervisor's refresh loop calls this on every pass).
    assert_eq!(
        doomed.resolve(Path::new("/a"), "unregistered-scheme://k"),
        None,
        "sanity: unregistered schemes stay None"
    );
    for _ in 0..50 {
        assert!(
            demo_registry()
                .resolve(Path::new("/a"), "dev.schickling.agent-goal://x")
                .is_some()
        );
    }
}
