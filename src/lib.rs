//! Pasture — OpenAI-compatible local/cloud LLM routing proxy.
//!
//! Routes each request between a local inference engine (e.g. Ollama) and a
//! cloud API using deterministic, hardware-adaptive rules. Zero external
//! dependencies: standard library only (Carmack/Pike philosophy).

pub mod backend;
pub mod cache;
pub mod calibrate;
pub mod cascade;
pub mod cli;
pub mod cloud;
pub mod config;
pub mod cost;
pub mod difficulty;
pub mod doctor;
pub mod eval;
pub mod guard;
pub mod hardware;
pub mod health;
pub mod i18n;
pub mod improve;
pub mod json;
pub mod monetize;
pub mod privacy;
pub mod proxy;
pub mod pseudonymize;
pub mod ratelimit;
pub mod routing;
pub mod telemetry;

pub use backend::{Backend, BackendError, CompletionRequest, CompletionResponse, Message};
pub use config::Config;
pub use hardware::{GpuInfo, HardwareProfile};
pub use routing::{Decision, Route, RoutingEngine};

/// Crate version, sourced from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
