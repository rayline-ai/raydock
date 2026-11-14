//! The ARC router's decision step.
//!
//! Given an ARC release and one request, [`decide`] answers which action (a
//! model at one thinking control) should serve it. The request is turned
//! into the encoder's input by `rayline-arc-input`; a [`Readouts`] source
//! runs the encoder; the rest is arithmetic on the release's head tensors,
//! on the CPU:
//!
//! - stage one predicts, per candidate model, the probability the task
//!   passes and the tokens the rest of the episode will use, and at a
//!   boundary (a task or subagent start, or where the request's schedule
//!   admits a switch) chooses the model with the best
//!   `P(pass) - cost weight x predicted cost - latency weight x log latency`.
//!   Off a boundary the model the session holds stays;
//! - stage two scores that model's offered arms and chooses the thinking
//!   control.
//!
//! Prices are the caller's: a release may carry or be given its own rate
//! per arm ([`Release::with_prices`]), and a request's `routing_prices` are
//! read over them.
//!
//! # What is not computed here
//!
//! The policy service infers some facts from the request's text when the
//! caller does not state them: whether a turn is a typed user message, a
//! harness compaction, or a subagent's first turn. This crate reads them
//! only from the request's `event`. Without one, a request with a held
//! model holds it, and one without decides as a task start. With this
//! release's settings a held model also stays at every boundary other than
//! a start, so the choice is the same; the receipt's labels can differ.
//!
//! Also absent: episode memory (the call is stateless; see
//! [`DecideInput::stage_one_held`]), exploration, OpenAI-format requests,
//! images, long-context price tiers, fixed and proxy arms, and head variants
//! the published release does not use. A release or request that needs one
//! is refused, never decided without it.

mod canon;
mod decide;
mod encoder;
mod error;
mod prices;
mod release;
mod runtime;
mod stage_one;
mod stage_two;
mod tensors;
mod wire;

pub use decide::{ReadoutRequest, Readouts, decide};
pub use encoder::{EncodeStats, LocalEncoder};
pub use error::DecideError;
pub use release::{
    Action, Arm, PRICE_LEGS, Rates, Release, ReleaseError, ReleaseFiles, StageOneSettings,
    Thinking, ToolFree,
};
pub use runtime::{
    ManifestFile, RUNTIME_FILE, RUNTIME_SCHEMA, RuntimeAction, RuntimeCatalog, is_runtime_release,
    manifest_files, runtime_catalog,
};
pub use wire::*;
