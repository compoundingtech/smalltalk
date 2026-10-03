// A separate, non-installed test executable shares the CLI implementation. The binary name is
// a compile-time Cargo value; renaming st3 or setting an environment variable cannot enable it.
include!("main.rs");
