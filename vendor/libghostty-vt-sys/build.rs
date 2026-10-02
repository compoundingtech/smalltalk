use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "a887df42c56f6de86c0fe6da9c4eeca37931e083";

#[derive(Clone, Copy)]
enum LinkMode {
    Dynamic,
    Static,
}

impl LinkMode {
    fn current() -> Self {
        if cfg!(feature = "link-dynamic") {
            Self::Dynamic
        } else {
            Self::Static
        }
    }

    fn artifact_kind(self) -> &'static str {
        match self {
            Self::Dynamic => "shared library",
            Self::Static => "static library",
        }
    }

    fn matches_library(self, target: &str, file_name: &str) -> bool {
        match self {
            Self::Dynamic => {
                if target.contains("darwin") {
                    file_name.starts_with("libghostty-vt") && file_name.ends_with(".dylib")
                } else if target.contains("windows") {
                    file_name == "ghostty-vt.lib"
                        || file_name == "ghostty-vt.dll"
                        || file_name == "libghostty-vt.dll.lib"
                        || file_name == "libghostty-vt.dll.a"
                } else {
                    file_name == "libghostty-vt.so" || file_name.starts_with("libghostty-vt.so.")
                }
            }
            Self::Static => {
                if target.contains("windows") {
                    file_name == "ghostty-vt-static.lib"
                } else {
                    file_name == "libghostty-vt.a"
                }
            }
        }
    }

    #[cfg(feature = "pkg-config")]
    fn pkg_config_name(self) -> &'static str {
        match self {
            Self::Dynamic => "libghostty-vt",
            Self::Static => "libghostty-vt-static",
        }
    }
}

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    let link_mode = LinkMode::current();

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    println!("cargo:rerun-if-changed=crates/libghostty-vt-sys/build.rs");

    // An explicit source override should stay authoritative even when the
    // pkg-config feature is enabled, so local Ghostty checkouts remain easy to
    // test against.
    if env::var_os("GHOSTTY_SOURCE_DIR").is_some() {
        build_vendored(link_mode);
        return;
    }

    // When the pkg-config feature is enabled, prefer an installed library over
    // fetching Ghostty. libghostty is pre-1.0, so this crate intentionally does
    // not promise compatibility with every installed C API revision.
    #[cfg(feature = "pkg-config")]
    if try_pkg_config(link_mode) {
        return;
    }

    build_vendored(link_mode);
}

/// Build libghostty-vt from source via zig. The zig build itself generates
/// shared and static artifacts plus pkg-config files in `share/pkgconfig/`.
fn build_vendored(link_mode: LinkMode) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let target = env::var("TARGET").expect("TARGET must be set");
    let host = env::var("HOST").expect("HOST must be set");

    // Locate ghostty source: env override > fetch into OUT_DIR.
    let ghostty_dir = match env::var("GHOSTTY_SOURCE_DIR") {
        Ok(dir) => {
            let p = PathBuf::from(dir);
            assert!(
                p.join("build.zig").exists(),
                "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
                p.display()
            );
            p
        }
        Err(_) => fetch_ghostty(&out_dir),
    };

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");
    let zig_cache_dir = out_dir.join("zig-cache");
    let zig_global_cache_dir = out_dir.join("zig-global-cache");

    let optimize = zig_optimize_mode();

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg(format!("-Doptimize={optimize}"))
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(&zig_cache_dir)
        .current_dir(&ghostty_dir);

    // Package managers can provide Ghostty's Zig package cache ahead of time
    // and ask Zig to resolve packages from that immutable store path instead
    // of fetching during this Cargo build script.
    if let Ok(dir) = env::var("GHOSTTY_ZIG_SYSTEM_DIR") {
        assert!(
            !dir.is_empty(),
            "GHOSTTY_ZIG_SYSTEM_DIR must not be empty when set"
        );
        let zig_system_dir = PathBuf::from(dir);
        assert!(
            zig_system_dir.exists(),
            "GHOSTTY_ZIG_SYSTEM_DIR does not exist: {}",
            zig_system_dir.display()
        );
        build
            .arg("--system")
            .arg(&zig_system_dir)
            .arg("--global-cache-dir")
            .arg(&zig_global_cache_dir);
    }

    // Only pass -Dtarget when cross-compiling. For native builds, let zig
    // auto-detect the host (matches how ghostty's own CMakeLists.txt works).
    if target != host {
        let zig_target = zig_target(&target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    #[cfg(target_os = "macos")]
    if let Some(xcrun_dir) = macos_sdk::xcrun_for_zig(&out_dir, &host, &target) {
        let path = env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(xcrun_dir).chain(env::split_paths(&path));
        build.env(
            "PATH",
            env::join_paths(paths).expect("PATH entries must be joinable"),
        );
    }

    run(build, "zig build");

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");
    let search_dirs = library_search_dirs(&target, &install_prefix);
    warn_unused_xcframework(&lib_dir);

    let has_requested_library = search_dirs.iter().any(|dir| {
        std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
            .any(|entry| {
                let entry = entry.unwrap_or_else(|error| {
                    panic!("failed to read entry from {}: {error}", dir.display())
                });
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    return false;
                };

                link_mode.matches_library(&target, file_name)
            })
    });
    assert!(
        has_requested_library,
        "expected libghostty-vt {} in one of {:?}",
        link_mode.artifact_kind(),
        search_dirs
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    for dir in &search_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    match link_mode {
        LinkMode::Dynamic => println!("cargo:rustc-link-lib=dylib=ghostty-vt"),
        LinkMode::Static => println!("cargo:rustc-link-lib=static=ghostty-vt"),
    }
    emit_include_metadata(&[include_dir]);
}

fn warn_unused_xcframework(lib_dir: &Path) {
    let xcframework = lib_dir.join("ghostty-vt.xcframework");
    if xcframework.exists() {
        println!(
            "cargo:warning=unused libghostty-vt XCFramework emitted at {}; Cargo links the dylib or archive directly",
            xcframework.display()
        );
    }
}

#[cfg(feature = "pkg-config")]
fn try_pkg_config(link_mode: LinkMode) -> bool {
    let mut config = pkg_config::Config::new();
    let lib = match link_mode {
        LinkMode::Dynamic => config.probe(link_mode.pkg_config_name()),
        LinkMode::Static => config
            .statik(true)
            .cargo_metadata(false)
            .probe(link_mode.pkg_config_name()),
    };
    let lib = match lib {
        Ok(lib) => lib,
        Err(_) => return false,
    };

    if let LinkMode::Static = link_mode {
        emit_static_pkg_config_metadata(&lib);
    }
    emit_include_metadata(&lib.include_paths);
    true
}

#[cfg(feature = "pkg-config")]
fn emit_static_pkg_config_metadata(lib: &pkg_config::Library) {
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for path in &lib.link_files {
        if let Some(parent) = path.parent() {
            println!("cargo:rustc-link-search=native={}", parent.display());
        }
    }
    for path in &lib.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in &lib.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    println!("cargo:rustc-link-lib=static=ghostty-vt");
    for library in &lib.libs {
        if library != "ghostty-vt" {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    for args in &lib.ld_args {
        if !args.is_empty() {
            println!("cargo:rustc-link-arg=-Wl,{}", args.join(","));
        }
    }
}

fn emit_include_metadata(include_paths: &[PathBuf]) {
    if include_paths.is_empty() {
        return;
    }

    let joined = env::join_paths(include_paths)
        .unwrap_or_else(|error| panic!("failed to join include paths for cargo metadata: {error}"));
    println!("cargo:include={}", joined.to_string_lossy());
}

/// Decide which Zig `OptimizeMode` to pass to `zig build`.
///
/// The `LIBGHOSTTY_VT_SYS_OPTIMIZE` environment variable overrides this unconditionally; accepted
/// values are the four Zig `OptimizeMode` names (`Debug`, `ReleaseSafe`, `ReleaseFast`,
/// `ReleaseSmall`).
///
/// Defaults to `ReleaseFast` for optimized builds. If `DEBUG` is `true` (as cargo sets for the
/// `dev` profile), `Debug` mode is used. Otherwise, if `OPT_LEVEL` is `s` or `z`, `ReleaseSmall`
/// is used.
fn zig_optimize_mode() -> &'static str {
    if let Ok(override_mode) = env::var("LIBGHOSTTY_VT_SYS_OPTIMIZE") {
        return match override_mode.as_str() {
            "Debug" => "Debug",
            "ReleaseSafe" => "ReleaseSafe",
            "ReleaseFast" => "ReleaseFast",
            "ReleaseSmall" => "ReleaseSmall",
            other => panic!(
                "LIBGHOSTTY_VT_SYS_OPTIMIZE must be one of Debug, ReleaseSafe, ReleaseFast, ReleaseSmall (got '{other}')"
            ),
        };
    }

    if env::var("DEBUG").as_deref() == Ok("true") {
        return "Debug";
    }

    match env::var("OPT_LEVEL").as_deref() {
        Ok("s") | Ok("z") => "ReleaseSmall",
        _ => "ReleaseFast",
    }
}

/// Clone ghostty at the pinned commit into OUT_DIR/ghostty-src.
/// Reuses an existing clone if the commit matches.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let src_dir = out_dir.join("ghostty-src");
    let stamp = src_dir.join(".ghostty-commit");

    // Skip fetch if we already have the right commit.
    if stamp.exists()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing.trim() == GHOSTTY_COMMIT
    {
        return src_dir;
    }

    // Clean and clone fresh.
    if src_dir.exists() {
        std::fs::remove_dir_all(&src_dir)
            .unwrap_or_else(|e| panic!("failed to remove {}: {e}", src_dir.display()));
    }

    eprintln!("Fetching ghostty {GHOSTTY_COMMIT} ...");

    let mut clone = Command::new("git");
    clone
        .arg("clone")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(GHOSTTY_REPO)
        .arg(&src_dir);
    run(clone, "git clone ghostty");

    let mut checkout = Command::new("git");
    checkout
        .arg("checkout")
        .arg(GHOSTTY_COMMIT)
        .current_dir(&src_dir);
    run(checkout, "git checkout ghostty commit");

    std::fs::write(&stamp, GHOSTTY_COMMIT).unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

    src_dir
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

/// Returns directories to search for the built library artifact.
/// On Windows, Zig may place the DLL in `bin/` and the import lib in `lib/`,
/// so both are included.
fn library_search_dirs(target: &str, install_prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![install_prefix.join("lib")];
    if target.contains("windows") {
        dirs.push(install_prefix.join("bin"));
    }
    dirs
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        "x86_64-pc-windows-gnu" => "x86_64-windows-gnu",
        "aarch64-pc-windows-gnullvm" => "aarch64-windows-gnu",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        "aarch64-linux-android" => "aarch64-linux-android",
        "x86_64-linux-android" => "x86_64-linux-android",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}

/// Zig 0.15.2 links Ghostty's build tools and the shared library against the
/// macOS SDK's text stubs (`.tbd`), and it only accepts a stub that lists the
/// exact target it links. The macOS 26.5 and 27 SDKs list `arm64e-macos` but no
/// longer `arm64-macos`, so on Apple silicon every libc symbol is undefined.
/// Zig 0.16 accepts these SDKs, but the pinned Ghostty commit requires 0.15.2.
///
/// When the SDK that `xcrun` selects has no `arm64-macos` stubs, this builds a
/// view of that same SDK in `OUT_DIR` whose stubs also list `arm64-macos`, and
/// returns a directory holding an `xcrun` that reports the view, so Zig's SDK
/// detection finds it. Everything else in the view links to the real SDK.
#[cfg(target_os = "macos")]
mod macos_sdk {
    use std::collections::HashSet;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Change when the view's contents change, so existing views are rebuilt.
    const VIEW_VERSION: &str = "1";

    pub fn xcrun_for_zig(out_dir: &Path, host: &str, target: &str) -> Option<PathBuf> {
        if ![host, target]
            .iter()
            .any(|triple| triple.starts_with("aarch64-apple-darwin"))
        {
            return None;
        }
        let output = Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sdk = fs::canonicalize(String::from_utf8(output.stdout).ok()?.trim()).ok()?;
        let libsystem_path = sdk.join("usr/lib/libSystem.B.tbd");
        let libsystem = fs::read_to_string(&libsystem_path).ok()?;
        if first_targets(&libsystem)?.contains("arm64-macos") {
            return None;
        }

        // An Xcode update can replace the SDK in place, so the stamp also
        // records when its libSystem stub last changed.
        let updated = fs::metadata(&libsystem_path).and_then(|m| m.modified()).ok();
        let root = out_dir.join("zig-macos-sdk");
        let stamp = root.join("source");
        let source = format!("{VIEW_VERSION}\n{}\n{updated:?}\n", sdk.display());
        if fs::read_to_string(&stamp).ok().as_deref() != Some(source.as_str()) {
            build_view(&sdk, &root);
            fs::write(&stamp, source)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", stamp.display()));
        }
        Some(root.join("bin"))
    }

    fn build_view(sdk: &Path, root: &Path) {
        if root.exists() {
            fs::remove_dir_all(root)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", root.display()));
        }
        let view = root.join(sdk.file_name().expect("SDK path has a name"));
        let include = sdk.join("usr/include");

        // Only directories on the way to a stub, and the header directory that
        // gets a new math.h, are real directories in the view.
        let mut real_dirs = HashSet::new();
        collect_stub_dirs(sdk, &mut real_dirs);
        real_dirs.insert(include.clone());
        mirror(sdk, &view, &real_dirs);

        let math_h = view.join("usr/include/math.h");
        fs::remove_file(&math_h)
            .unwrap_or_else(|e| panic!("failed to remove {}: {e}", math_h.display()));
        write(&math_h, &math_h_with_fallbacks(&include.join("math.h")));

        let xcrun = root.join("bin/xcrun");
        fs::create_dir_all(root.join("bin"))
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", root.display()));
        write(&xcrun, &xcrun_reporting(&view));
        fs::set_permissions(&xcrun, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("failed to make {} executable: {e}", xcrun.display()));
    }

    fn collect_stub_dirs(dir: &Path, found: &mut HashSet<PathBuf>) -> bool {
        let mut has_stub = false;
        for entry in read_dir(dir) {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .unwrap_or_else(|e| panic!("failed to stat {}: {e}", path.display()));
            if file_type.is_dir() {
                has_stub |= collect_stub_dirs(&path, found);
            } else if is_stub(&path) {
                has_stub = true;
            }
        }
        if has_stub {
            found.insert(dir.to_path_buf());
        }
        has_stub
    }

    fn mirror(from_dir: &Path, to_dir: &Path, real_dirs: &HashSet<PathBuf>) {
        fs::create_dir_all(to_dir)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", to_dir.display()));
        for entry in read_dir(from_dir) {
            let from = entry.path();
            let to = to_dir.join(entry.file_name());
            let file_type = entry
                .file_type()
                .unwrap_or_else(|e| panic!("failed to stat {}: {e}", from.display()));
            if file_type.is_symlink() {
                // SDK links are relative (`libSystem.tbd`, `Versions/Current`),
                // so they keep resolving to the rewritten stubs in the view.
                let link = fs::read_link(&from)
                    .unwrap_or_else(|e| panic!("failed to read {}: {e}", from.display()));
                link_to(&link, &to);
            } else if file_type.is_dir() && real_dirs.contains(&from) {
                mirror(&from, &to, real_dirs);
            } else if file_type.is_file() && is_stub(&from) {
                let stub = fs::read_to_string(&from)
                    .unwrap_or_else(|e| panic!("failed to read {}: {e}", from.display()));
                write(&to, &with_arm64(&stub));
            } else {
                link_to(&from, &to);
            }
        }
    }

    /// Adds `arm64-macos` to each target list that names `arm64e-macos` without it.
    /// Apple's linker already links arm64 code against arm64e stubs.
    fn with_arm64(stub: &str) -> String {
        let mut out = String::with_capacity(stub.len() + 4096);
        let mut rest = stub;
        while let Some(open) = rest.find('[') {
            let Some(len) = rest[open..].find(']') else {
                break;
            };
            let list = &rest[open..=open + len];
            out.push_str(&rest[..open]);
            if list.contains("arm64e-macos") && !list.contains("arm64-macos") {
                out.push_str(&list.replacen("arm64e-macos", "arm64-macos, arm64e-macos", 1));
            } else {
                out.push_str(list);
            }
            rest = &rest[open + len + 1..];
        }
        out.push_str(rest);
        out
    }

    fn first_targets(stub: &str) -> Option<&str> {
        let rest = &stub[stub.find("targets:")?..];
        Some(&rest[rest.find('[')?..=rest.find(']')?])
    }

    /// The macOS 27 math.h asks Clang's float.h for INFINITY and NAN through
    /// `__need_infinity_nan`, which Zig 0.15.2's bundled float.h predates, so
    /// libc++ fails to compile for the shared library. Ghostty's later
    /// pkg/apple-sdk overlay supplies the same fallbacks.
    fn math_h_with_fallbacks(sdk_math_h: &Path) -> String {
        format!(
            "#include \"{}\"\n\n\
             #ifndef INFINITY\n#define INFINITY (__builtin_inff())\n#endif\n\n\
             #ifndef NAN\n#define NAN (__builtin_nanf(\"\"))\n#endif\n",
            sdk_math_h.display()
        )
    }

    /// Zig 0.15.2 finds the SDK with exactly `xcrun --sdk macosx --show-sdk-path`.
    fn xcrun_reporting(view: &Path) -> String {
        let quoted = format!("'{}'", view.display().to_string().replace('\'', "'\\''"));
        format!(
            "#!/bin/sh\n\
             if [ $# -eq 3 ] && [ \"$1\" = --sdk ] && [ \"$2\" = macosx ] && [ \"$3\" = --show-sdk-path ]; then\n  \
             printf '%s\\n' {quoted}\n  exit 0\n\
             fi\n\
             exec /usr/bin/xcrun \"$@\"\n"
        )
    }

    fn is_stub(path: &Path) -> bool {
        path.extension().is_some_and(|extension| extension == "tbd")
    }

    fn read_dir(dir: &Path) -> impl Iterator<Item = fs::DirEntry> + '_ {
        fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
            .map(move |entry| {
                entry.unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
            })
    }

    fn link_to(original: &Path, link: &Path) {
        symlink(original, link)
            .unwrap_or_else(|e| panic!("failed to link {}: {e}", link.display()));
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }
}
