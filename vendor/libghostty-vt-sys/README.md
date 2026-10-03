# libghostty-vt-sys

Small Talk carries the published `libghostty-vt-sys` 0.2.1 crate here (libghostty-rs
`46a9d2ac`) through `[patch.crates-io]` with one change to `build.rs`. When Cargo builds
libghostty-vt with Zig on Apple silicon and the macOS SDK lists no `arm64-macos` link stubs,
as in the macOS 26.5 and 27 SDKs, the build gives Zig 0.15.2 a view of that SDK that does
(see `macos_sdk` in `build.rs`). Builds that find the library through pkg-config, including the
Nix package, never reach that code. Drop this copy when the pinned Ghostty commit builds with a
Zig that accepts current SDKs.

The upstream README follows.

---


Raw FFI bindings for libghostty-vt.

- Fetches and builds `libghostty-vt.a` from ghostty sources via Zig by default.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Static linking is the baseline rather than a Cargo feature. Enable the
  additive `link-dynamic` feature to link the shared library instead.
- Set `GHOSTTY_SOURCE_DIR` to force the build to use a local Ghostty checkout.
- Set `GHOSTTY_ZIG_SYSTEM_DIR` to force Zig package resolution through a
  pre-fetched `zig build --system` directory. This is intended for Nix and other
  sandboxed package managers that cannot fetch during build scripts.
- Set `LIBGHOSTTY_VT_SYS_OPTIMIZE` to `Debug`, `ReleaseSafe`, `ReleaseFast`, or
  `ReleaseSmall` to override the Zig optimize mode used by vendored builds.
- If the `pkg-config` feature is enabled, the build will use an installed
  `libghostty-vt` found through `pkg-config` only when `GHOSTTY_SOURCE_DIR` is
  unset. With the default static link mode, it probes Ghostty's
  `libghostty-vt-static` pkg-config module instead.
- libghostty-vt is pre-1.0, so these bindings do not guarantee compatibility
  with arbitrary installed C API revisions.
