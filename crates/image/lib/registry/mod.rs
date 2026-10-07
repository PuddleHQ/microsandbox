mod builder;
mod client;
mod manifest;
pub(crate) mod retry;

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

pub use builder::RegistryBuilder;
pub use client::Registry;
