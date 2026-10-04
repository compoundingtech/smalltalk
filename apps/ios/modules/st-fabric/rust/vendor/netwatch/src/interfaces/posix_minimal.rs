//! Interface lookups for POSIX platforms without `netdev` (e.g. esp-idf).
//!
//! No interface enumeration, default route, or home router is available on
//! these platforms, so every lookup reports empty or absent.

#[cfg(not(target_os = "ios"))]
use std::collections::HashMap;

use super::{DefaultRouteDetails, HomeRouter};
#[cfg(not(target_os = "ios"))]
use super::State;
#[cfg(not(target_os = "ios"))]
use crate::ip::LocalAddresses;

#[cfg(not(target_os = "ios"))]
pub(super) async fn get_state() -> State {
    State {
        interfaces: HashMap::new(),
        local_addresses: LocalAddresses::default(),
        have_v6: false,
        have_v4: true,
        is_expensive: false,
        default_route_interface: None,
        last_unsuspend: None,
    }
}

pub(super) async fn default_route() -> Option<DefaultRouteDetails> {
    None
}

pub(super) fn home_router() -> Option<HomeRouter> {
    None
}

// iOS still enumerates actual interfaces; only BSD route ioctl metadata is absent.
#[cfg(target_os = "ios")]
pub(super) use super::netdev_impl::get_state;
