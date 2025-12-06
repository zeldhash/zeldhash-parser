//! Persistent storage backends for MHIN data.
//!
//! This module provides two storage systems:
//!
//! - [`sqlite`] — SQLite database for block statistics and reward tracking
//! - [`utxo`] — UTXO store backed by rollblock for O(1) lookups and rollbacks

pub mod queries;
pub mod sqlite;
pub mod utxo;
