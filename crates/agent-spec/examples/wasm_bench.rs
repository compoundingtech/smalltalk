//! Benchmark: wasm resolver plugin boundary — identical protocol to the exec experiment.
//!
//! Measures, over 1,000 resolutions each:
//! - **warm**: one instance reused across calls (`instantiate` amortized away)
//! - **cold**: fresh instantiation per call (the registry's default posture)
//! - **instantiation only**: `WasmResolver::instantiate()` with no `resolve` call
//! - **call only**: warm minus the per-call overhead already included
//!
//! Percentiles are computed over per-operation wall times taken with `Instant` (best-effort;
//! single-run medians on an otherwise idle machine). Run with:
//! ```text
//! nix develop -c cargo run -p agent-spec --release --example wasm_bench
//! ```

use agent_spec::profile::{ProfileClass, ResourceProfile, ResourceProfileRegistry};
use std::path::Path;
use std::time::Instant;

const DEMO_WASM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/demo_resolver.wasm"
);
const N: usize = 1_000;

fn percentile(samples: &mut Vec<u128>, p: f64) -> u128 {
    samples.sort_unstable();
    let idx = (((samples.len() as f64 - 1.0) * p / 100.0).round()) as usize;
    samples[idx.min(samples.len() - 1)]
}

fn report(label: &str, mut samples: Vec<u128>) {
    let p50 = percentile(&mut samples, 50.0);
    let p95 = percentile(&mut samples, 95.0);
    let p99 = percentile(&mut samples, 99.0);
    let max = *samples.last().unwrap();
    let total_us: u128 = samples.iter().sum();
    println!(
        "{:<28} {:>10.2} {:>10.2} {:>10.2} {:>12.2} {:>12.1}",
        label,
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        max as f64 / 1000.0,
        total_us as f64 / 1000.0 // total microseconds -> milliseconds
    );
}

fn main() {
    let fixture = Path::new(DEMO_WASM_PATH);
    if !fixture.exists() {
        panic!(
            "missing {DEMO_WASM_PATH}; build it with `cargo build -p demo-resolver-wasm --target wasm32-unknown-unknown --release`"
        );
    }

    let resolver =
        agent_spec::profile_wasm::WasmResolver::load(fixture).expect("demo module loads");
    let mut warm_instance = resolver.instantiate().expect("demo instantiates");
    let uri = "dev.schickling.agent-goal://dev3/janitor";
    let agent_dir = "/cat/agents/dev3/janitor";

    // Sanity: all three paths agree.
    let via_registry = ResourceProfileRegistry::empty()
        .with_profile(ResourceProfile::wasm(
            "dev.schickling.agent-goal",
            DEMO_WASM_PATH,
            ProfileClass::Immediate,
        ))
        .resolve(Path::new(agent_dir), uri)
        .expect("registry resolves demo module");
    assert_eq!(
        via_registry.path,
        path_buf_of(agent_dir, "/resources/goal.md")
    );
    let warm_result = warm_instance.resolve(uri, agent_dir).expect("warm resolve");
    assert_eq!(warm_result.path, format!("{agent_dir}/resources/goal.md"));

    println!("wasm resolver plugin benchmark — {N} resolutions per mode");
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>12} {:>12}",
        "mode", "p50µs", "p95µs", "p99µs", "maxµs", "total_ms"
    );

    // Warm: reuse one instance; measures fuel charge + memory copies + guest work + JSON parse.
    let mut warm = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        let r = warm_instance.resolve(uri, agent_dir).expect("warm resolve");
        assert_eq!(r.path, format!("{agent_dir}/resources/goal.md"));
        warm.push(t.elapsed().as_nanos());
    }
    report("warm (instance reused)", warm);

    // Cold: fresh instantiation + call per resolution (registry default).
    let mut cold = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        let r = resolver.resolve_once(uri, agent_dir).expect("cold resolve");
        assert_eq!(r.path, format!("{agent_dir}/resources/goal.md"));
        cold.push(t.elapsed().as_nanos());
    }
    report("cold (instantiate+call)", cold);

    // Instantiation only: the fixed cost of a fresh sandbox per resolution.
    let mut inst = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        drop(resolver.instantiate().expect("instantiate"));
        inst.push(t.elapsed().as_nanos());
    }
    report("instantiation only", inst);

    // Compile cost, once, for reference.
    let t = Instant::now();
    drop(agent_spec::profile_wasm::WasmResolver::load(fixture));
    let compile_once = t.elapsed();

    println!();
    println!(
        "module compile (once): {:.2} ms",
        compile_once.as_secs_f64() * 1000.0
    );
}

fn path_buf_of(a: &str, b: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{a}{b}"))
}
