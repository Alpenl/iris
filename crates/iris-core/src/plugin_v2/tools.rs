//! Host-defined semantic adapters for explicitly trusted native tools.
mod arguments;
pub mod binding;
pub mod catalog;
mod executor;
mod output;
mod proxy;
mod source;
pub use executor::{ToolExecutor, ToolSetup};

#[cfg(test)]
mod tests;
