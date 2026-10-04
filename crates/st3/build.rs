fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    if std::env::var_os("CARGO_FEATURE_TEST_SUPPORT").is_some() {
        let bash = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bash"))
            .find(|candidate| candidate.is_file())
            .expect("st3 fixture tests need bash on the build PATH");
        println!("cargo:rustc-env=ST3_FIXTURE_BASH={}", bash.display());
        println!(
            "cargo:rustc-env=ST3_FIXTURE_PATH={}",
            std::env::var("PATH").unwrap()
        );
    }
}
