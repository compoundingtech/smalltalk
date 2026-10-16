//! Complete current runtime resources, published off the HTTP request path.
use super::*;

pub(crate) struct RuntimeRows {
    pub(crate) items: Arc<Vec<Value>>,
    /// Computed once by the refresher, never by a page read.
    pub(crate) digest: String,
    pub(crate) time_unix_ms: u128,
}

impl Store {
    pub(crate) fn published_runtime_list(&self) -> &published_list::PublishedList<RuntimeRows> {
        &self.smalltalk.published_runtimes
    }
}
