//! Pure decision record grammar, validation, and state fold.
//! No filesystem storage, CLI, or daemon integration.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use model::defect;

mod model;
mod parsing;
mod evaluation;

pub use model::*;
pub use parsing::*;
pub use evaluation::*;

#[cfg(test)]
mod tests;
