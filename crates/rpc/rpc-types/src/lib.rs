//! Reth RPC type definitions.
//!
//! Provides all relevant types for the various RPC endpoints, grouped by namespace.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(hidden_glob_reexports)] // TODO rm in followup PR

mod admin;
pub mod beacon;
mod eth;
mod mev;
mod net;
mod peer;
pub mod relay;
mod rpc;

// re-export for convenience
pub use alloy_rpc_types::serde_helpers;

// Ethereum specific rpc types coming from alloy.
pub use alloy_rpc_types::*;

/// Type alias for a transaction receipt with a RPC logs.
pub type AnyTransactionReceipt = WithOtherFields<TransactionReceipt<AnyReceiptEnvelope<Log>>>;

pub mod trace {
    //! RPC types for trace endpoints and inspectors.
    pub use alloy_rpc_types_trace::*;
}
// Ethereum specific rpc types related to typed transaction requests and the engine API.
pub use eth::{
    engine,
    engine::{
        ExecutionPayload, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadError,
    },
    transaction::{self, TransactionKind, TransactionRequest, TypedTransactionRequest},
};

pub use admin::*;
pub use mev::*;
pub use net::*;
pub use peer::*;
pub use rpc::*;
