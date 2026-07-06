//! procflowd internals, exposed as a library so integration tests can drive
//! the server without a privileged install.

pub mod collector;
pub mod server;
pub mod store;
