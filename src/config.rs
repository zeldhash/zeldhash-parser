//! Application configuration management.
//!
//! Handles loading and merging configuration from multiple sources:
//! - Command-line arguments (highest priority)
//! - Environment variables
//! - TOML configuration file
//! - Default values (lowest priority)

mod app;
mod overlay;
mod protoblock;
mod rollblock;

pub use app::{load_runtime_paths, AppConfig, RuntimePaths};
