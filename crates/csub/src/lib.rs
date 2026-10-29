//! `csub`: a standalone owner for a pool of Claude subscriptions.
//!
//! The library exposes the command line (also mounted as `rayline
//! subscriptions`), the registry/status commands, the loopback gateway, the
//! daemon, its launchd service, and the `csub claude` launcher.

pub mod app;
mod claude_args;
pub mod cli;
pub mod gateway;
pub mod launch;
pub mod serve;
pub mod service;
pub mod supervisor;
