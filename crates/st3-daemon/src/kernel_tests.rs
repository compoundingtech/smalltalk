//! Unit tests of st3-kernel modules that need daemon code (intent parsing, store constants) to run.
//! They test the same kernel items through st3-daemon's re-exports.
mod hooks;
mod model_launch_change;
mod publication;
mod terminal_binding;
