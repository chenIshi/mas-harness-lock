//! Harness-enforced locking for multi-agent coordination.
//!
//! Single-threaded by design (handover §11), so the `async_fn_in_trait` Send bound is not needed.
#![allow(async_fn_in_trait)]

pub mod history;
pub mod lock;
pub mod types;
pub mod store;
pub mod harness;
pub mod checker;
