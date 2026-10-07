// Test-only relocation for same-source nextest archives. Ordinary cargo test retains its
// compiled paths. No production version or build-identity environment value is changed.
pub(crate) fn resolve(
    name: &str,
    compiled: &str,
    build: Option<&str>,
    current: Option<&str>,
    executable: Option<&str>,
    archive: bool,
) -> Result<String, &'static str> {
    if name == "CARGO_MANIFEST_DIR" {
        match (build, current) {
            (Some(build), Some(current)) => {
                let relative = std::path::Path::new(compiled)
                    .strip_prefix(build)
                    .map_err(|_| "manifest directory is outside the recorded build workspace")?;
                std::path::Path::new(current)
                    .join(relative)
                    .to_str()
                    .map(str::to_owned)
                    .ok_or("test checkout path is not UTF-8")
            }
            (None, None) if !archive => Ok(compiled.to_owned()),
            _ => Err("both archive workspace identities are required"),
        }
    } else if name.starts_with("CARGO_BIN_EXE_") || name == "CARGO_TARGET_TMPDIR" {
        match executable {
            Some(path) => Ok(path.to_owned()),
            None if !archive => Ok(compiled.to_owned()),
            None => Err("archive test executable must be supplied by nextest"),
        }
    } else {
        Err("only test path variables are supported")
    }
}

#[allow(unused_macros)]
macro_rules! test_env {
    ($name:expr) => {
        test_env!($name, "")
    };
    ($name:expr, $suffix:literal) => {{
        static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        VALUE
            .get_or_init(|| {
                let build = std::env::var("CI_TEST_BUILD_WORKSPACE").ok();
                let current = std::env::var("CI_TEST_WORKSPACE").ok();
                let executable = if $name == "CARGO_TARGET_TMPDIR" {
                    std::env::var("CI_TEST_TARGET_TMPDIR").ok()
                } else {
                    $name.strip_prefix("CARGO_BIN_EXE_").and_then(|name| {
                        std::env::var(format!("NEXTEST_BIN_EXE_{}", name.replace('-', "_")))
                            .or_else(|_| std::env::var(format!("NEXTEST_BIN_EXE_{name}")))
                            .or_else(|_| std::env::var($name))
                            .ok()
                    })
                };
                let value = crate::ci_test_paths::resolve(
                    $name,
                    env!($name),
                    build.as_deref(),
                    current.as_deref(),
                    executable.as_deref(),
                    std::env::var_os("CI_TEST_EXTRACTED").is_some(),
                )
                .expect("valid exact-source test archive paths");
                format!("{value}{}", $suffix)
            })
            .as_str()
    }};
}

#[allow(unused_macros)]
macro_rules! test_bin {
    ($name:literal) => {
        std::path::Path::new(test_env!(concat!("CARGO_BIN_EXE_", $name)))
    };
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn a_library_fixture_uses_its_own_relocated_manifest() {
        assert_eq!(
            resolve(
                "CARGO_MANIFEST_DIR",
                "/build/crates/st3",
                Some("/build"),
                Some("/consumer"),
                None,
                true
            )
            .unwrap(),
            "/consumer/crates/st3"
        );
    }

    #[test]
    fn nextest_supplies_the_relocated_executable() {
        assert_eq!(
            resolve(
                "CARGO_BIN_EXE_st3-fixture",
                "/build/target/debug/st3-fixture",
                Some("/build"),
                Some("/consumer"),
                Some("/extract/target/debug/st3-fixture"),
                true
            )
            .unwrap(),
            "/extract/target/debug/st3-fixture"
        );
    }

    #[test]
    fn ordinary_cargo_test_retains_the_original_paths() {
        for name in [
            "CARGO_MANIFEST_DIR",
            "CARGO_BIN_EXE_st2",
            "CARGO_TARGET_TMPDIR",
        ] {
            assert_eq!(
                resolve(name, "/original", None, None, None, false).unwrap(),
                "/original"
            );
        }
    }

    #[test]
    fn stale_or_partial_archive_paths_fail_closed() {
        assert!(
            resolve(
                "CARGO_MANIFEST_DIR",
                "/stale/crate",
                Some("/build"),
                Some("/consumer"),
                None,
                true
            )
            .is_err()
        );
        assert!(
            resolve(
                "CARGO_MANIFEST_DIR",
                "/build/crate",
                Some("/build"),
                None,
                None,
                true
            )
            .is_err()
        );
        assert!(resolve("CARGO_MANIFEST_DIR", "/build/crate", None, None, None, true).is_err());
        assert!(resolve("CARGO_BIN_EXE_st2", "/old/st2", None, None, None, true).is_err());
        assert!(resolve("CARGO_TARGET_TMPDIR", "/old/target", None, None, None, true).is_err());
        assert!(resolve("CARGO_PKG_VERSION", "1", None, None, None, false).is_err());
    }
}
