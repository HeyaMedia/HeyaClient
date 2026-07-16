//! Origin-scoped native music playback.

mod bridge;
pub mod core;
mod manager;
mod protocol;
mod validation;

pub use bridge::*;
pub use manager::*;
pub use protocol::*;
pub(crate) use validation::*;
