//! Generate invented benchmark data for the ignored cold-open snapshot harness.
//! This starts no reconciler or agent runtime, and reads no production store.
#![allow(dead_code, unused_imports)]
#[path = "../tests/daemon_bench.rs"]
mod daemon_bench;

fn main() {
    let keep = std::path::PathBuf::from(std::env::args().nth(1).expect("synthetic fixture directory"));
    std::fs::create_dir_all(&keep).unwrap();
    let scale = std::env::args().nth(2).map_or(1.0, |scale| scale.parse().expect("numeric fixture scale"));
    // Use the existing sample-count generator, with all invented claims local:
    // replication setup is not part of an uncontended collection snapshot.
    let path = keep.join(format!("generated-{scale}.sqlite3"));
    assert!(!path.exists(), "use a fresh synthetic fixture directory");
    let store = st3::store::Store::open(&path, daemon_bench::NODE).unwrap();
    store.bind_fleet(daemon_bench::FLEET).unwrap();
    daemon_bench::generate(&store, "host", scale);
    println!("fixture claims={}", store.index().unwrap());
    drop(store);
    println!("ST_COLLECTION_FIXTURE={}", path.display());
}
