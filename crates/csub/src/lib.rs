//! `csub`: a standalone owner for a pool of Claude subscriptions.
//!
//! The library exposes the registry/status commands (also used by the Rayline
//! CLI), the loopback gateway, the daemon and its launchd service.

pub mod cli;
pub mod gateway;
pub mod serve;
pub mod service;
