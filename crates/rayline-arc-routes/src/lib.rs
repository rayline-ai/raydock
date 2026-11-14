//! Routes API V1 (`POST /v1/routes`) answered by a local ARC policy.
//!
//! The execution core asks one question per turn: which model, on which
//! worker, at which thinking level. Hosted, a router answers it. This crate
//! answers it on loopback for a core in `execution_context_v2` mode:
//!
//! - [`parse`] reads the request (the harness body, `routing.prices`,
//!   `routing.policy`, the version 2 `routing.execution`, and the conversation,
//!   branch, wire-format and cell headers) into a typed [`DecideInput`] and
//!   what the answer must restate, or the [`Refusal`] the contract names.
//! - [`shape`] turns the policy's decide answer into the V1 answer the core
//!   validates.
//! - [`Router`] serves `/v1/routes` through a [`Decider`] and forwards every
//!   other path, streaming, to a configured upstream.
//!
//! Nothing here logs a header or request content.

mod catalog;
mod check;
mod digest;
mod error;
mod execution;
mod fallback;
mod parse;
mod policy;
mod server;
mod shape;

pub use catalog::{Action, Catalog, CatalogError};
pub use digest::{jcs, offer_sha256, price_sha256, sha256_hex};
pub use error::Refusal;
pub use execution::{
    ActionCache, Attribution, CapacityFact, Compaction, Event, PinStatus, Pinned, Turn,
};
pub use parse::{
    CacheContext, DecideInput, EpisodeMode, Legs, OperatingPoint, PackageRef, ParseContext, Parsed,
    RequestHeaders, Restate, RoutingPrices, Selection, Tier, parse,
};
pub use policy::{Overrides, Policy, Schedule, Selection as SelectionThresholds};
pub use server::{DecideFuture, Decider, HttpDecider, Router, RouterConfig, RouterError};
pub use shape::shape;

/// The price legs of one model, in USD per million tokens, in contract order.
pub const PRICE_LEGS: [&str; 4] = ["input", "cache_read", "cache_write", "output"];
