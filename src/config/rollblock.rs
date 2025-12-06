use std::path::Path;

use anyhow::{anyhow, Context, Result};
use rollblock::{
    orchestrator::DurabilityMode, MhinStoreBlockFacade, RemoteServerSettings, StoreConfig,
};

use crate::cli::{
    RollblockDurabilityKind, RollblockMode, RollblockOptions,
    DEFAULT_ROLLBLOCK_ASYNC_MAX_PENDING_BLOCKS, DEFAULT_ROLLBLOCK_ASYNC_RELAXED_SYNC_EVERY,
    DEFAULT_ROLLBLOCK_COMPRESS_JOURNAL, DEFAULT_ROLLBLOCK_INITIAL_CAPACITY,
    DEFAULT_ROLLBLOCK_REMOTE_PASSWORD, DEFAULT_ROLLBLOCK_REMOTE_PORT,
    DEFAULT_ROLLBLOCK_REMOTE_USER, DEFAULT_ROLLBLOCK_SHARDS_COUNT,
    DEFAULT_ROLLBLOCK_SYNCHRONOUS_RELAXED_SYNC_EVERY, DEFAULT_ROLLBLOCK_THREAD_COUNT,
};

use super::overlay::Overlay;

#[derive(Debug, Clone)]
pub struct RollblockSettings {
    pub mode: RollblockMode,
    pub store_config: StoreConfig,
}

impl RollblockOptions {
    pub fn build(self, data_dir: &Path) -> Result<RollblockSettings> {
        let rollblock_data_dir = data_dir.join("utxodb");
        let mode = if rollblock_data_dir.exists() {
            RollblockMode::Existing
        } else {
            RollblockMode::New
        };

        let mut config = match mode {
            RollblockMode::New => {
                let shards = self.shards_count.unwrap_or(DEFAULT_ROLLBLOCK_SHARDS_COUNT);
                let capacity = self
                    .initial_capacity
                    .unwrap_or(DEFAULT_ROLLBLOCK_INITIAL_CAPACITY);
                let thread_count = self.thread_count.unwrap_or(DEFAULT_ROLLBLOCK_THREAD_COUNT);
                let compress = self
                    .compress_journal
                    .unwrap_or(DEFAULT_ROLLBLOCK_COMPRESS_JOURNAL);
                StoreConfig::new(
                    &rollblock_data_dir,
                    shards,
                    capacity,
                    thread_count,
                    compress,
                )
                .context("failed to create rollblock StoreConfig")?
            }
            RollblockMode::Existing => {
                let mut cfg = StoreConfig::existing(&rollblock_data_dir);
                if let Some(shards) = self.shards_count {
                    let capacity = self.initial_capacity.ok_or_else(|| {
                        anyhow!(
                            "rollblock_initial_capacity is required when overriding shard layout"
                        )
                    })?;
                    cfg = cfg
                        .with_shard_layout(shards, capacity)
                        .context("invalid shard layout override")?;
                }
                if let Some(thread_count) = self.thread_count {
                    cfg.thread_count = thread_count;
                }
                if let Some(compress) = self.compress_journal {
                    cfg = cfg.with_journal_compression(compress);
                }
                cfg
            }
        };

        if let Some(interval) = self.snapshot_interval {
            config = config.with_snapshot_interval(interval);
        }
        if let Some(interval) = self.max_snapshot_interval {
            config = config.with_max_snapshot_interval(interval);
        }
        if let Some(level) = self.journal_compression_level {
            config = config.with_journal_compression_level(level);
        }
        if let Some(bytes) = self.journal_chunk_size_bytes {
            config = config.with_journal_chunk_size(bytes);
        }
        if let Some(size) = self.lmdb_map_size {
            config = config.with_lmdb_map_size(size);
        }
        if let Some(window) = self.min_rollback_window {
            config = config
                .with_min_rollback_window(window)
                .context("invalid rollblock_min_rollback_window")?;
        }
        if let Some(interval) = self.prune_interval {
            config = config
                .with_prune_interval(interval)
                .context("invalid rollblock_prune_interval")?;
        }
        if let Some(profile) = self.bootstrap_block_profile {
            config = config
                .with_bootstrap_block_profile(profile)
                .context("invalid rollblock_bootstrap_block_profile")?;
        }

        let async_pending = self
            .async_max_pending_blocks
            .unwrap_or(DEFAULT_ROLLBLOCK_ASYNC_MAX_PENDING_BLOCKS);
        let async_relaxed_sync_every = self
            .async_relaxed_sync_every
            .unwrap_or(DEFAULT_ROLLBLOCK_ASYNC_RELAXED_SYNC_EVERY);
        let synchronous_relaxed_sync_every = self
            .synchronous_relaxed_sync_every
            .unwrap_or(DEFAULT_ROLLBLOCK_SYNCHRONOUS_RELAXED_SYNC_EVERY);

        let mode_kind = self
            .durability_mode
            .unwrap_or(RollblockDurabilityKind::AsyncRelaxed);

        config = match mode_kind {
            RollblockDurabilityKind::Async => config.with_async_max_pending(async_pending),
            RollblockDurabilityKind::AsyncRelaxed => {
                config.with_async_relaxed(async_pending, async_relaxed_sync_every)
            }
            RollblockDurabilityKind::Synchronous => {
                config.with_durability_mode(DurabilityMode::Synchronous)
            }
            RollblockDurabilityKind::SynchronousRelaxed => {
                config.with_durability_mode(DurabilityMode::SynchronousRelaxed {
                    sync_every_n_blocks: synchronous_relaxed_sync_every.max(1),
                })
            }
        };

        let remote_username = self
            .user
            .unwrap_or_else(|| DEFAULT_ROLLBLOCK_REMOTE_USER.to_string());
        let remote_password = self
            .password
            .unwrap_or_else(|| DEFAULT_ROLLBLOCK_REMOTE_PASSWORD.to_string());
        let remote_port = self.port.unwrap_or(DEFAULT_ROLLBLOCK_REMOTE_PORT);

        let mut remote_settings =
            RemoteServerSettings::default().with_basic_auth(remote_username, remote_password);
        remote_settings.bind_address.set_port(remote_port);

        config = config.with_remote_server(remote_settings);
        config = config
            .enable_remote_server()
            .context("failed to enable rollblock remote server")?;

        Ok(RollblockSettings {
            mode,
            store_config: config,
        })
    }
}

impl RollblockSettings {
    pub fn determine_start_height(&self) -> Result<u64> {
        if matches!(self.mode, RollblockMode::New) {
            return Ok(0);
        }

        self.read_start_height_from_store()
    }

    fn read_start_height_from_store(&self) -> Result<u64> {
        let mut peek_config = self.store_config.clone();
        peek_config.enable_server = false;
        peek_config.remote_server = None;

        let store = MhinStoreBlockFacade::new(peek_config)
            .context("failed to open rollblock store to determine start height")?;
        let current_block = store
            .current_block()
            .context("failed to read rollblock current block height")?;
        store
            .close()
            .context("failed to close rollblock store after determining start height")?;

        current_block.checked_add(1).context(
            "rollblock current block reached u64::MAX; cannot derive protoblock start height",
        )
    }
}

impl Overlay for RollblockOptions {
    fn overlay(self, overrides: Self) -> Self {
        Self {
            user: overrides.user.or(self.user),
            password: overrides.password.or(self.password),
            port: overrides.port.or(self.port),
            shards_count: overrides.shards_count.or(self.shards_count),
            initial_capacity: overrides.initial_capacity.or(self.initial_capacity),
            thread_count: overrides.thread_count.or(self.thread_count),
            compress_journal: overrides.compress_journal.or(self.compress_journal),
            snapshot_interval: overrides.snapshot_interval.or(self.snapshot_interval),
            max_snapshot_interval: overrides
                .max_snapshot_interval
                .or(self.max_snapshot_interval),
            journal_compression_level: overrides
                .journal_compression_level
                .or(self.journal_compression_level),
            journal_chunk_size_bytes: overrides
                .journal_chunk_size_bytes
                .or(self.journal_chunk_size_bytes),
            lmdb_map_size: overrides.lmdb_map_size.or(self.lmdb_map_size),
            min_rollback_window: overrides.min_rollback_window.or(self.min_rollback_window),
            prune_interval: overrides.prune_interval.or(self.prune_interval),
            bootstrap_block_profile: overrides
                .bootstrap_block_profile
                .or(self.bootstrap_block_profile),
            durability_mode: overrides.durability_mode.or(self.durability_mode),
            async_max_pending_blocks: overrides
                .async_max_pending_blocks
                .or(self.async_max_pending_blocks),
            async_relaxed_sync_every: overrides
                .async_relaxed_sync_every
                .or(self.async_relaxed_sync_every),
            synchronous_relaxed_sync_every: overrides
                .synchronous_relaxed_sync_every
                .or(self.synchronous_relaxed_sync_every),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn determine_start_height_is_zero_for_new_store() {
        let tmp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions::default();
        let settings = options.build(tmp.path()).expect("rollblock settings build");

        assert!(matches!(settings.mode, RollblockMode::New));

        let start_height = settings
            .determine_start_height()
            .expect("start height should default to 0 for new stores");
        assert_eq!(start_height, 0);
    }

    #[test]
    fn overlay_prefers_override_and_keeps_base_when_missing() {
        let base = RollblockOptions {
            thread_count: Some(2),
            compress_journal: Some(false),
            async_max_pending_blocks: Some(10),
            user: Some("base-user".to_string()),
            password: Some("base-pass".to_string()),
            port: Some(DEFAULT_ROLLBLOCK_REMOTE_PORT),
            ..Default::default()
        };
        let overrides = RollblockOptions {
            thread_count: Some(8),
            compress_journal: None,
            async_max_pending_blocks: Some(20),
            user: None,
            password: None,
            port: None,
            ..Default::default()
        };

        let merged = base.overlay(overrides);

        assert_eq!(merged.thread_count, Some(8));
        assert_eq!(
            merged.compress_journal,
            Some(false),
            "should retain base when override missing"
        );
        assert_eq!(merged.async_max_pending_blocks, Some(20));
        assert_eq!(merged.user.as_deref(), Some("base-user"));
        assert_eq!(merged.password.as_deref(), Some("base-pass"));
        assert_eq!(merged.port, Some(DEFAULT_ROLLBLOCK_REMOTE_PORT));
    }

    #[test]
    fn build_new_store_applies_remote_defaults() {
        let temp = tempdir().expect("tempdir should be created");
        let settings = RollblockOptions::default()
            .build(temp.path())
            .expect("rollblock settings build");

        let remote = settings
            .store_config
            .remote_server
            .as_ref()
            .expect("remote server should be configured");

        assert_eq!(remote.auth.username, DEFAULT_ROLLBLOCK_REMOTE_USER);
        assert_eq!(remote.auth.password, DEFAULT_ROLLBLOCK_REMOTE_PASSWORD);
        assert_eq!(remote.bind_address.port(), DEFAULT_ROLLBLOCK_REMOTE_PORT);
    }

    #[test]
    fn build_overrides_remote_settings_from_options() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            user: Some("alice".to_string()),
            password: Some("wonderland".to_string()),
            port: Some(9555),
            ..Default::default()
        };
        let settings = options
            .build(temp.path())
            .expect("rollblock settings build");
        let remote = settings
            .store_config
            .remote_server
            .as_ref()
            .expect("remote server should be configured");

        assert_eq!(remote.auth.username, "alice");
        assert_eq!(remote.auth.password, "wonderland");
        assert_eq!(remote.bind_address.port(), 9555);
    }

    #[test]
    fn build_existing_store_requires_capacity_when_overriding_shards() {
        let temp = tempdir().expect("tempdir should be created");
        let utxodb_path = temp.path().join("utxodb");
        std::fs::create_dir_all(&utxodb_path).expect("create utxodb dir");

        let options = RollblockOptions {
            shards_count: Some(8),
            // Missing initial_capacity should trigger the validation error.
            initial_capacity: None,
            ..Default::default()
        };

        let err = options
            .build(temp.path())
            .expect_err("should fail without capacity override");
        assert!(
            err.to_string()
                .contains("rollblock_initial_capacity is required"),
            "error should mention missing capacity, got: {err}"
        );
    }

    #[test]
    fn build_new_store_applies_defaults() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions::default();
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
        // Just verify it built successfully with a new store
        assert!(settings.store_config.enable_server);
        let remote = settings
            .store_config
            .remote_server
            .as_ref()
            .expect("remote server should be configured");
        assert_eq!(remote.bind_address.port(), DEFAULT_ROLLBLOCK_REMOTE_PORT);
    }

    #[test]
    fn build_applies_durability_mode_async() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            durability_mode: Some(RollblockDurabilityKind::Async),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
        // Just verify it builds successfully with this mode
        assert!(settings.store_config.enable_server);
    }

    #[test]
    fn build_applies_durability_mode_synchronous() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            durability_mode: Some(RollblockDurabilityKind::Synchronous),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_durability_mode_synchronous_relaxed() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            durability_mode: Some(RollblockDurabilityKind::SynchronousRelaxed),
            synchronous_relaxed_sync_every: Some(50),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_snapshot_interval() {
        use std::time::Duration;

        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            snapshot_interval: Some(Duration::from_secs(300)),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_max_snapshot_interval() {
        use std::time::Duration;

        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            max_snapshot_interval: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_journal_compression_level() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            journal_compression_level: Some(3),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_journal_chunk_size_bytes() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            journal_chunk_size_bytes: Some(1024 * 1024),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_lmdb_map_size() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            lmdb_map_size: Some(1024 * 1024 * 1024),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_min_rollback_window() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            min_rollback_window: Some(100),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_prune_interval() {
        use std::time::Duration;

        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            prune_interval: Some(Duration::from_secs(3600)),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn build_applies_optional_bootstrap_block_profile() {
        let temp = tempdir().expect("tempdir should be created");
        let options = RollblockOptions {
            bootstrap_block_profile: Some(1000),
            ..Default::default()
        };
        let settings = options.build(temp.path()).expect("build settings");

        assert!(matches!(settings.mode, RollblockMode::New));
    }

    #[test]
    fn rollblock_mode_default_is_existing() {
        let mode = RollblockMode::default();
        assert!(matches!(mode, RollblockMode::Existing));
    }

    #[test]
    fn overlay_handles_all_optional_fields() {
        use std::time::Duration;

        let base = RollblockOptions {
            shards_count: Some(8),
            initial_capacity: Some(1_000_000),
            thread_count: Some(4),
            compress_journal: Some(true),
            snapshot_interval: Some(Duration::from_secs(60)),
            max_snapshot_interval: Some(Duration::from_secs(120)),
            journal_compression_level: Some(3),
            journal_chunk_size_bytes: Some(1024),
            lmdb_map_size: Some(1024 * 1024),
            min_rollback_window: Some(50),
            prune_interval: Some(Duration::from_secs(300)),
            bootstrap_block_profile: Some(500),
            durability_mode: Some(RollblockDurabilityKind::Async),
            async_max_pending_blocks: Some(100),
            async_relaxed_sync_every: Some(10),
            synchronous_relaxed_sync_every: Some(5),
            user: Some("base-user".to_string()),
            password: Some("base-pass".to_string()),
            port: Some(DEFAULT_ROLLBLOCK_REMOTE_PORT),
        };

        let overrides = RollblockOptions {
            shards_count: Some(16),
            // Leave others as None to test preservation
            ..Default::default()
        };

        let merged = base.overlay(overrides);

        assert_eq!(merged.shards_count, Some(16));
        assert_eq!(merged.initial_capacity, Some(1_000_000));
        assert_eq!(merged.thread_count, Some(4));
        assert_eq!(merged.compress_journal, Some(true));
        assert_eq!(merged.snapshot_interval, Some(Duration::from_secs(60)));
        assert_eq!(merged.max_snapshot_interval, Some(Duration::from_secs(120)));
        assert_eq!(merged.journal_compression_level, Some(3));
        assert_eq!(merged.journal_chunk_size_bytes, Some(1024));
        assert_eq!(merged.lmdb_map_size, Some(1024 * 1024));
        assert_eq!(merged.min_rollback_window, Some(50));
        assert_eq!(merged.prune_interval, Some(Duration::from_secs(300)));
        assert_eq!(merged.bootstrap_block_profile, Some(500));
        assert!(matches!(
            merged.durability_mode,
            Some(RollblockDurabilityKind::Async)
        ));
        assert_eq!(merged.async_max_pending_blocks, Some(100));
        assert_eq!(merged.async_relaxed_sync_every, Some(10));
        assert_eq!(merged.synchronous_relaxed_sync_every, Some(5));
        assert_eq!(merged.user.as_deref(), Some("base-user"));
        assert_eq!(merged.password.as_deref(), Some("base-pass"));
        assert_eq!(merged.port, Some(DEFAULT_ROLLBLOCK_REMOTE_PORT));
    }
}
