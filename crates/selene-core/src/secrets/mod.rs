//! Secret storage for Selene.
//!
//! Currently this is the OS-keychain wrapper in [`keychain`]; it is always
//! compiled (credential storage is not gated behind any driver feature). The
//! module is the single place that touches the platform credential store.

pub mod keychain;

pub use keychain::{KeychainStore, KEYCHAIN_SERVICE};
