//! st's side of sekrets: the daemon vouches for its seats (`daemon::attest`), records each
//! gateway's log as local observations (`daemon::spawn_importer`), and makes GitHub API requests
//! through a gateway with a token it never holds (`authorized`). The gateway and the `sekrets`
//! command are the `sekrets` crate. See `docs/st3/sekrets.md`.

pub use ::sekrets::{client, files, identity, keys, policy, protocol, store};

pub mod authorized;
pub mod daemon;
