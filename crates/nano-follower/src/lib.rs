//! The consensus and durable acquisition core of the mainnet follower.

pub mod checkpoint;
pub mod sortition;
pub mod staging;

pub use checkpoint::{
    Checkpoint, CheckpointAttestation, CheckpointBundleReceipt, CheckpointManifest,
    CheckpointProvenance, CheckpointTrustError, adopt_checkpoint, adopt_checkpoint_bundle,
    attest_checkpoint,
};
