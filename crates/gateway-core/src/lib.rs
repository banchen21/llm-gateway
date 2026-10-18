//! Protocol conversion core for llm-gateway.
//!
//! The crate is layered so that no protocol logic depends on the HTTP server:
//!
//! ```text
//! ir/        the one representation every converter reads and writes
//! convert/   protocol JSON  <->  ir   (inbound parse, outbound serialize)
//! upstream/  ir  ->  outbound HTTP request,  upstream SSE  ->  ModelEvent
//! config.rs  what the operator configured
//! router.rs  model alias  ->  provider + that provider's own model id
//! ```
//!
//! A request therefore always flows `client protocol -> ir -> upstream protocol`,
//! and a response always flows `upstream SSE -> ModelEvent -> client SSE`, no matter
//! which protocol sits on either side.

pub mod config;
pub mod convert;
pub mod error;
pub mod ir;
pub mod router;
pub mod upstream;

pub use error::{Error, Result};
