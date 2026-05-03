//! Library entry point. The binary in `main.rs` re-exports everything
//! through this module so that integration tests under `tests/` can use the
//! same types and helpers without going through the CLI surface.

pub mod agents;
pub mod bash;
pub mod bus;
pub mod cli;
pub mod config;
pub mod index;
pub mod job_cancel;
pub mod messages;
pub mod ollama;
pub mod proposals;
pub mod server;
pub mod slash;
pub mod tools;
pub mod workspace_path;
