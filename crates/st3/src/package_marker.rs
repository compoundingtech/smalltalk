//! An empty library so the `st3` package can switch on its own `test-support` feature in tests.
//!
//! The package keeps only the binary, its binary-side modules and the tests. Cargo lets a package
//! enable its own feature through a dev-dependency on itself only when the package has a library
//! target; the dev-dependency below `[dev-dependencies]` in Cargo.toml relies on this one.
