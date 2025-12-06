//! Interactive terminal progress display.
//!
//! Provides a live TUI (terminal user interface) powered by [`ratatui`] that shows:
//!
//! - Current parsing progress and sync percentage
//! - Blockchain tip and last processed block
//! - MHIN protocol statistics (rewards, supply, nicest hash)
//! - Estimated time to completion

use crate::stores::sqlite::CumulativeStats;
use anyhow::{Context, Result};
use mhinprotocol::types::Amount;
use protoblock::rpc::AsyncRpcClient;
use protoblock::runtime::config::FetcherConfig;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
    Frame, Terminal, TerminalOptions, Viewport,
};
use std::cmp::Ordering as CmpOrdering;
use std::io::{self, Stdout};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

const RENDER_INTERVAL: Duration = Duration::from_millis(750);
const PANEL_WIDTH: u16 = 100;
const TABLE_SECTION_HEIGHT: u16 = 8;
const PROGRESS_SECTION_HEIGHT: u16 = 6;
const INLINE_VIEWPORT_HEIGHT: u16 = TABLE_SECTION_HEIGHT + PROGRESS_SECTION_HEIGHT + 1;

/// Spawns background tasks that keep track of parsing progress and render it to stdout.
pub struct ProgressReporter {
    shutdown: Arc<Notify>,
    tip_handle: JoinHandle<()>,
    render_handle: JoinHandle<()>,
}

/// Handle owned by the parser so it can report new heights and rollbacks.
#[derive(Clone)]
pub struct ProgressHandle {
    state: Arc<ProgressState>,
}

impl ProgressReporter {
    /// Starts background tasks and returns both the reporter and a handle the parser can update.
    pub async fn start(fetcher_config: FetcherConfig) -> Result<(Self, ProgressHandle)> {
        let state = Arc::new(ProgressState::new(fetcher_config.start_height()));
        let shutdown = Arc::new(Notify::new());
        let rpc_client = Arc::new(
            AsyncRpcClient::from_config(&fetcher_config)
                .context("failed to build RPC client for progress reporter")?,
        );

        match rpc_client.get_blockchain_tip().await {
            Ok(tip) => state.update_tip(tip),
            Err(err) => tracing::warn!(
                error = %err,
                "failed to fetch initial blockchain tip for progress reporter"
            ),
        }

        let tip_handle = spawn_tip_refresh_loop(
            Arc::clone(&rpc_client),
            Arc::clone(&state),
            fetcher_config.tip_refresh_interval(),
            Arc::clone(&shutdown),
        );
        let render_handle = spawn_render_loop(Arc::clone(&state), Arc::clone(&shutdown));

        let reporter = Self {
            shutdown,
            tip_handle,
            render_handle,
        };
        let handle = ProgressHandle { state };

        Ok((reporter, handle))
    }

    /// Signals both background tasks to stop and waits for them to finish.
    pub async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.tip_handle.await;
        let _ = self.render_handle.await;
    }
}

impl ProgressHandle {
    /// Records the latest processed block height.
    pub fn mark_processed(&self, height: u64) {
        self.state.bump_last_processed(height);
    }

    /// Updates the tracker after a rollback.
    pub fn rollback_to(&self, height: u64) {
        self.state.last_processed.store(height, Ordering::SeqCst);
        self.state.reset_session_baseline(height);
    }

    /// Publishes the latest cumulative stats so the renderer can display them.
    pub fn update_cumulative(&self, stats: Option<&CumulativeStats>) {
        self.state.set_cumulative(stats.cloned());
    }

    /// Resets the speed baseline so historical blocks do not skew throughput.
    pub fn reset_speed_baseline(&self, height: u64) {
        self.state.reset_session_baseline(height);
    }
}

fn spawn_tip_refresh_loop(
    rpc_client: Arc<AsyncRpcClient>,
    state: Arc<ProgressState>,
    refresh_interval: Duration,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(refresh_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    break;
                }
                _ = ticker.tick() => {
                    match rpc_client.get_blockchain_tip().await {
                        Ok(tip) => state.update_tip(tip),
                        Err(err) => tracing::warn!(error = %err, "failed to refresh blockchain tip for progress reporter"),
                    }
                }
            }
        }
    })
}

fn spawn_render_loop(state: Arc<ProgressState>, shutdown: Arc<Notify>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_render_loop(state, shutdown).await {
            tracing::warn!(error = %err, "progress reporter render loop failed");
        }
    })
}

async fn run_render_loop(state: Arc<ProgressState>, shutdown: Arc<Notify>) -> Result<()> {
    let mut terminal = setup_terminal().context("failed to start progress terminal")?;
    let render_result = drive_render_loop(&mut terminal, state, shutdown).await;
    let cleanup_result = restore_terminal(terminal);

    render_result?;
    cleanup_result?;
    Ok(())
}

async fn drive_render_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: Arc<ProgressState>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let mut ticker = interval(RENDER_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                break;
            }
            _ = ticker.tick() => {
                let snapshot = state.snapshot();
                terminal
                    .draw(|frame| draw_ui(frame, &snapshot))
                    .context("failed to draw progress frame")?;
            }
        }
    }

    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let options = TerminalOptions {
        viewport: Viewport::Inline(INLINE_VIEWPORT_HEIGHT),
    };
    let mut terminal =
        Terminal::with_options(backend, options).context("failed to build ratatui terminal")?;
    terminal
        .clear()
        .context("failed to clear terminal for progress reporter")?;
    Ok(terminal)
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    terminal
        .show_cursor()
        .context("failed to show cursor after progress reporter")?;
    terminal
        .clear()
        .context("failed to clear terminal after progress reporter")?;
    println!();
    Ok(())
}

fn draw_ui(frame: &mut Frame<'_>, snapshot: &ProgressSnapshot) {
    let show_progress = snapshot
        .tip_height
        .is_some_and(|tip| snapshot.last_processed < tip);
    let panel_area = panel_area(frame.area(), show_progress);
    frame.render_widget(Clear, panel_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(section_constraints(panel_area.height, show_progress))
        .split(panel_area);

    let table = build_info_table(snapshot);
    frame.render_widget(table, chunks[0]);

    if show_progress && chunks.len() > 1 {
        render_progress_section(frame, chunks[1], snapshot);
    }
}

fn build_info_table(snapshot: &ProgressSnapshot) -> Table<'static> {
    let stats = snapshot.latest_cumulative.as_ref();
    let tip_display = snapshot
        .tip_height
        .map(format_with_separators)
        .unwrap_or_else(|| "Waiting for remote tip".to_string());
    let last_parsed = format_with_separators(snapshot.last_processed);
    let reward_count = stats
        .map(|s| format_with_separators(s.reward_count()))
        .unwrap_or_else(|| "--".to_string());
    let total_supply = stats
        .map(|s| format_amount(s.total_reward()))
        .unwrap_or_else(|| "--".to_string());
    let max_zero_count = stats
        .map(|s| s.max_zero_count().to_string())
        .unwrap_or_else(|| "--".to_string());
    let nicest_hash = stats
        .and_then(|s| s.nicest_txid().map(str::to_string))
        .unwrap_or_else(|| "-".to_string());
    let unspent = stats
        .map(|s| {
            let spent = s.utxo_spent_count();
            let created = s.new_utxo_count();
            format_with_separators(created.saturating_sub(spent))
        })
        .unwrap_or_else(|| "--".to_string());

    let rows = vec![
        stats_row("Tip", tip_display),
        stats_row("Last parsed block", last_parsed),
        stats_row("Reward count", reward_count),
        stats_row("MHIN supply", total_supply),
        stats_row("Max zero count", max_zero_count),
        stats_row("Last nice hash", nicest_hash),
        stats_row("Unspent UTXO", unspent),
    ];

    Table::new(rows, [Constraint::Length(20), Constraint::Min(10)])
        .column_spacing(2)
        .block(
            Block::default()
                .title("Parsing progress")
                .borders(Borders::ALL),
        )
}

fn stats_row<'a>(label: &'a str, value: String) -> Row<'a> {
    Row::new(vec![
        Cell::from(label).style(Style::default().fg(Color::Gray)),
        Cell::from(value),
    ])
}

fn render_progress_section(frame: &mut Frame<'_>, area: Rect, snapshot: &ProgressSnapshot) {
    if area.height == 0 {
        return;
    }

    let tip = match snapshot.tip_height {
        Some(tip) if snapshot.last_processed < tip => tip,
        _ => return,
    };

    let total = blocks_total(snapshot.start_height, tip);
    if total == 0 {
        return;
    }

    let displayed_height = snapshot.last_processed.min(tip);
    let completed = blocks_completed(snapshot.start_height, displayed_height).min(total);
    let remaining = total.saturating_sub(completed);
    let elapsed = snapshot.start_instant.elapsed();
    let session_completed = snapshot
        .last_processed
        .saturating_sub(snapshot.session_baseline);
    let speed = compute_speed(session_completed, elapsed);
    let eta_display = compute_eta(remaining, speed)
        .map(format_duration)
        .unwrap_or_else(|| "--:--:--".to_string());
    let ratio = (completed as f64 / total as f64).clamp(0.0, 1.0);
    let percent = ratio * 100.0;
    let displayed_height_text = format_with_separators(displayed_height);
    let tip_text = format_with_separators(tip);
    let remaining_text = format_with_separators(remaining);
    let elapsed_display = format_duration(elapsed);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3)])
        .split(area);

    let gauge = Gauge::default()
        .block(
            Block::default()
                .title("Sync progress")
                .borders(Borders::ALL),
        )
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(format!(
            "{percent:.1}% (height {displayed_height_text}/{tip_text})"
        ));
    frame.render_widget(gauge, chunks[0]);

    let info_line = format!(
        "Remaining: {remaining_text} | Speed: {speed:.2} blk/s | Elapsed: {elapsed_display} | ETA: {eta_display}"
    );
    let info = Paragraph::new(info_line)
        .block(Block::default().title("Run details").borders(Borders::ALL));
    frame.render_widget(info, chunks[1]);
}

fn panel_height(show_progress: bool) -> u16 {
    if show_progress {
        TABLE_SECTION_HEIGHT + PROGRESS_SECTION_HEIGHT
    } else {
        TABLE_SECTION_HEIGHT
    }
}

fn panel_area(area: Rect, show_progress: bool) -> Rect {
    let width = PANEL_WIDTH.min(area.width);
    let height = panel_height(show_progress).min(area.height);
    Rect {
        x: area.x,
        y: area.y,
        width,
        height,
    }
}

fn section_constraints(height: u16, show_progress: bool) -> Vec<Constraint> {
    if show_progress {
        let table_height = TABLE_SECTION_HEIGHT.min(height);
        let progress_height = height.saturating_sub(table_height).max(1);
        vec![
            Constraint::Length(table_height),
            Constraint::Length(progress_height),
        ]
    } else {
        vec![Constraint::Length(height)]
    }
}

fn blocks_completed(start: u64, last_processed: u64) -> u64 {
    if last_processed < start {
        0
    } else {
        last_processed - start + 1
    }
}

fn blocks_total(start: u64, tip: u64) -> u64 {
    if tip < start {
        0
    } else {
        tip - start + 1
    }
}

fn compute_speed(completed: u64, elapsed: Duration) -> f64 {
    let elapsed_secs = elapsed.as_secs_f64();
    if elapsed_secs <= f64::EPSILON {
        0.0
    } else {
        completed as f64 / elapsed_secs
    }
}

fn compute_eta(remaining: u64, speed: f64) -> Option<Duration> {
    if speed <= f64::EPSILON {
        None
    } else {
        Some(Duration::from_secs_f64(remaining as f64 / speed))
    }
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_with_separators(value: u64) -> String {
    let digits = value.to_string();
    let len = digits.len();
    let mut formatted = String::with_capacity(len + len / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (len - idx) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

fn format_amount(amount: &Amount) -> String {
    const SCALE: u64 = 100_000_000;
    let whole = *amount / SCALE;
    let fractional = *amount % SCALE;
    let whole_text = format_with_separators(whole);
    format!("{whole_text}.{fractional:08}")
}

struct ProgressState {
    start_height: AtomicU64,
    start_instant: Mutex<Instant>,
    last_processed: AtomicU64,
    tip_height: AtomicU64,
    tip_known: AtomicBool,
    latest_cumulative: Mutex<Option<CumulativeStats>>,
    session_baseline: AtomicU64,
}

impl ProgressState {
    fn new(start_height: u64) -> Self {
        Self {
            start_height: AtomicU64::new(start_height),
            start_instant: Mutex::new(Instant::now()),
            last_processed: AtomicU64::new(start_height.saturating_sub(1)),
            tip_height: AtomicU64::new(0),
            tip_known: AtomicBool::new(false),
            latest_cumulative: Mutex::new(None),
            session_baseline: AtomicU64::new(start_height.saturating_sub(1)),
        }
    }

    fn update_tip(&self, tip: u64) {
        self.tip_height.store(tip, Ordering::SeqCst);
        self.tip_known.store(true, Ordering::SeqCst);
    }

    fn snapshot(&self) -> ProgressSnapshot {
        let tip_known = self.tip_known.load(Ordering::SeqCst);
        let tip_value = self.tip_height.load(Ordering::SeqCst);
        let latest_cumulative = self
            .latest_cumulative
            .lock()
            .expect("latest cumulative mutex poisoned")
            .clone();
        ProgressSnapshot {
            start_height: self.start_height.load(Ordering::SeqCst),
            start_instant: *self
                .start_instant
                .lock()
                .expect("start instant mutex poisoned"),
            last_processed: self.last_processed.load(Ordering::SeqCst),
            tip_height: tip_known.then_some(tip_value),
            latest_cumulative,
            session_baseline: self.session_baseline.load(Ordering::SeqCst),
        }
    }

    fn set_cumulative(&self, stats: Option<CumulativeStats>) {
        if let Some(ref latest) = stats {
            if latest.block_count() > 0 {
                let first_processed = latest
                    .block_index()
                    .saturating_sub(latest.block_count().saturating_sub(1));
                self.bump_start_height(first_processed);
            }
            self.bump_last_processed(latest.block_index());
        }
        *self
            .latest_cumulative
            .lock()
            .expect("latest cumulative mutex poisoned") = stats;
    }

    fn reset_session_baseline(&self, baseline: u64) {
        self.session_baseline.store(baseline, Ordering::SeqCst);
        *self
            .start_instant
            .lock()
            .expect("start instant mutex poisoned") = Instant::now();
    }

    fn bump_start_height(&self, candidate: u64) {
        let mut current = self.start_height.load(Ordering::SeqCst);
        while candidate < current {
            match self.start_height.compare_exchange(
                current,
                candidate,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn bump_last_processed(&self, height: u64) {
        let mut current = self.last_processed.load(Ordering::SeqCst);
        while matches!(height.cmp(&current), CmpOrdering::Greater) {
            match self.last_processed.compare_exchange(
                current,
                height,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

#[derive(Clone)]
struct ProgressSnapshot {
    start_height: u64,
    start_instant: Instant,
    last_processed: u64,
    tip_height: Option<u64>,
    latest_cumulative: Option<CumulativeStats>,
    session_baseline: u64,
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use ratatui::layout::{Constraint, Rect};

    use mhinprotocol::types::Amount;

    use crate::stores::sqlite::CumulativeStats;

    use super::{
        blocks_completed, blocks_total, build_info_table, compute_eta, compute_speed,
        format_amount, format_duration, format_with_separators, panel_area, panel_height,
        section_constraints, stats_row, ProgressSnapshot, ProgressState, PROGRESS_SECTION_HEIGHT,
        TABLE_SECTION_HEIGHT,
    };

    #[test]
    fn blocks_completed_handles_underflow_and_inclusive_range() {
        assert_eq!(blocks_completed(10, 5), 0);
        assert_eq!(blocks_completed(5, 5), 1);
        assert_eq!(blocks_completed(5, 7), 3);
    }

    #[test]
    fn blocks_total_returns_zero_when_tip_before_start() {
        assert_eq!(blocks_total(10, 9), 0);
        assert_eq!(blocks_total(10, 10), 1);
        assert_eq!(blocks_total(10, 12), 3);
    }

    #[test]
    fn compute_speed_and_eta_handle_edge_cases() {
        assert_eq!(compute_speed(10, Duration::from_secs(0)), 0.0);
        assert_eq!(compute_speed(5, Duration::from_secs(2)), 2.5);

        assert!(compute_eta(5, 0.0).is_none());
        let eta = compute_eta(4, 2.0).expect("eta should be computed");
        assert_eq!(eta, Duration::from_secs(2));
    }

    #[test]
    fn formatting_helpers_render_expected_strings() {
        assert_eq!(format_duration(Duration::from_secs(3_661)), "01:01:01");
        assert_eq!(format_with_separators(1_234_567), "1,234,567");

        let amount: Amount = 123_456_789;
        assert_eq!(format_amount(&amount), "1.23456789");
    }

    #[test]
    fn panel_area_caps_to_available_space() {
        let roomy = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let panel = panel_area(roomy, true);
        assert_eq!(panel.width, 80);
        assert_eq!(panel.height, TABLE_SECTION_HEIGHT + PROGRESS_SECTION_HEIGHT);

        let tight = Rect {
            x: 2,
            y: 1,
            width: 40,
            height: 6,
        };
        let panel = panel_area(tight, true);
        assert_eq!(panel.width, 40);
        assert_eq!(panel.height, 6);
    }

    #[test]
    fn section_constraints_allocate_progress_space() {
        let constraints = section_constraints(20, true);
        assert_eq!(
            constraints,
            vec![
                Constraint::Length(TABLE_SECTION_HEIGHT),
                Constraint::Length(12)
            ]
        );

        let small_constraints = section_constraints(5, true);
        assert_eq!(
            small_constraints,
            vec![Constraint::Length(5), Constraint::Length(1)]
        );

        let disabled = section_constraints(7, false);
        assert_eq!(disabled, vec![Constraint::Length(7)]);
    }

    #[test]
    fn panel_height_toggles_progress_space() {
        assert_eq!(
            panel_height(true),
            TABLE_SECTION_HEIGHT + PROGRESS_SECTION_HEIGHT
        );
        assert_eq!(panel_height(false), TABLE_SECTION_HEIGHT);
    }

    #[test]
    fn progress_state_bump_helpers_are_monotonic() {
        let state = ProgressState::new(10);

        state.bump_last_processed(8);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.last_processed, 9,
            "should not move last_processed backwards"
        );

        state.bump_last_processed(15);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.last_processed, 15);

        state.bump_start_height(12);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.start_height, 10,
            "start height should not increase"
        );

        state.bump_start_height(7);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.start_height, 7, "start height should decrease");
    }

    #[test]
    fn progress_state_reset_session_baseline_updates_clock() {
        let state = ProgressState::new(5);
        let before = state.snapshot();

        thread::sleep(Duration::from_millis(1));
        state.reset_session_baseline(3);
        let after = state.snapshot();

        assert_eq!(after.session_baseline, 3);
        assert!(
            after.start_instant >= before.start_instant,
            "reset should move the start instant forward"
        );
    }

    #[test]
    fn progress_state_update_tip_sets_known_flag() {
        let state = ProgressState::new(0);
        let before = state.snapshot();
        assert!(
            before.tip_height.is_none(),
            "tip should be unknown initially"
        );

        state.update_tip(100);
        let after = state.snapshot();

        assert_eq!(after.tip_height, Some(100));
    }

    #[test]
    fn progress_state_set_cumulative_updates_start_height() {
        // Start with a high start_height so it can be lowered
        let state = ProgressState::new(100);

        let stats = CumulativeStats::builder()
            .block_index(150)
            .block_count(100) // this makes first_processed = 150 - 99 = 51, which is < 100
            .total_reward(1000u64)
            .reward_count(10)
            .max_zero_count(5)
            .nicest_txid(Some("abc".to_string()))
            .utxo_spent_count(3)
            .new_utxo_count(7)
            .build();

        state.set_cumulative(Some(stats));
        let snapshot = state.snapshot();

        // first_processed = block_index - (block_count - 1) = 150 - 99 = 51
        // Since 51 < 100 (initial start_height), bump_start_height should lower it to 51
        assert_eq!(snapshot.start_height, 51, "start_height should be lowered");
        assert_eq!(snapshot.last_processed, 150);
        assert!(snapshot.latest_cumulative.is_some());
    }

    #[test]
    fn progress_state_set_cumulative_none_clears_stats() {
        let state = ProgressState::new(10);

        state.set_cumulative(None);
        let snapshot = state.snapshot();

        assert!(snapshot.latest_cumulative.is_none());
    }

    #[test]
    fn format_with_separators_handles_small_numbers() {
        assert_eq!(format_with_separators(0), "0");
        assert_eq!(format_with_separators(1), "1");
        assert_eq!(format_with_separators(12), "12");
        assert_eq!(format_with_separators(123), "123");
        assert_eq!(format_with_separators(999), "999");
    }

    #[test]
    fn format_with_separators_handles_large_numbers() {
        assert_eq!(format_with_separators(1_000), "1,000");
        assert_eq!(format_with_separators(10_000), "10,000");
        assert_eq!(format_with_separators(100_000), "100,000");
        assert_eq!(format_with_separators(1_000_000), "1,000,000");
        assert_eq!(format_with_separators(999_999_999), "999,999,999");
    }

    #[test]
    fn format_duration_handles_various_durations() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_duration(Duration::from_secs(59)), "00:00:59");
        assert_eq!(format_duration(Duration::from_secs(60)), "00:01:00");
        assert_eq!(format_duration(Duration::from_secs(3599)), "00:59:59");
        assert_eq!(format_duration(Duration::from_secs(3600)), "01:00:00");
        assert_eq!(format_duration(Duration::from_secs(86399)), "23:59:59");
    }

    #[test]
    fn format_amount_handles_fractional_values() {
        let amount: Amount = 0;
        assert_eq!(format_amount(&amount), "0.00000000");

        let amount: Amount = 1;
        assert_eq!(format_amount(&amount), "0.00000001");

        let amount: Amount = 100_000_000;
        assert_eq!(format_amount(&amount), "1.00000000");

        let amount: Amount = 2_100_000_000_000_000_u64;
        assert_eq!(format_amount(&amount), "21,000,000.00000000");
    }

    #[test]
    fn compute_eta_returns_none_for_zero_speed() {
        assert!(compute_eta(100, 0.0).is_none());
        assert!(compute_eta(100, f64::EPSILON / 2.0).is_none());
    }

    #[test]
    fn compute_eta_calculates_correctly() {
        let eta = compute_eta(100, 10.0).expect("should compute eta");
        assert_eq!(eta, Duration::from_secs(10));

        let eta = compute_eta(1000, 100.0).expect("should compute eta");
        assert_eq!(eta, Duration::from_secs(10));
    }

    #[test]
    fn compute_speed_handles_zero_elapsed() {
        assert_eq!(compute_speed(100, Duration::ZERO), 0.0);
    }

    #[test]
    fn compute_speed_calculates_correctly() {
        assert!((compute_speed(100, Duration::from_secs(10)) - 10.0).abs() < f64::EPSILON);
        assert!((compute_speed(50, Duration::from_secs(2)) - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn blocks_completed_with_matching_start_and_last() {
        assert_eq!(blocks_completed(100, 100), 1);
    }

    #[test]
    fn blocks_total_with_matching_start_and_tip() {
        assert_eq!(blocks_total(100, 100), 1);
    }

    #[test]
    fn cumulative_stats_accessors() {
        let stats = CumulativeStats::builder()
            .block_index(10)
            .block_count(5)
            .total_reward(500u64)
            .reward_count(3)
            .max_zero_count(7)
            .nicest_txid(Some("txid123".to_string()))
            .utxo_spent_count(2)
            .new_utxo_count(4)
            .build();

        assert_eq!(stats.block_index(), 10);
        assert_eq!(stats.block_count(), 5);
        assert_eq!(*stats.total_reward(), 500u64);
        assert_eq!(stats.reward_count(), 3);
        assert_eq!(stats.max_zero_count(), 7);
        assert_eq!(stats.nicest_txid(), Some("txid123"));
        assert_eq!(stats.utxo_spent_count(), 2);
        assert_eq!(stats.new_utxo_count(), 4);
    }

    #[test]
    fn progress_state_new_initializes_correctly() {
        let state = ProgressState::new(100);
        let snapshot = state.snapshot();

        assert_eq!(snapshot.start_height, 100);
        assert_eq!(snapshot.last_processed, 99); // start - 1
        assert!(snapshot.tip_height.is_none());
        assert!(snapshot.latest_cumulative.is_none());
        assert_eq!(snapshot.session_baseline, 99);
    }

    #[test]
    fn progress_state_bump_start_height_only_decreases() {
        let state = ProgressState::new(50);

        // Try to increase (should be ignored)
        state.bump_start_height(100);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.start_height, 50);

        // Decrease works
        state.bump_start_height(25);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.start_height, 25);
    }

    #[test]
    fn progress_state_concurrent_updates() {
        let state = ProgressState::new(0);

        // Simulate multiple updates
        for i in 1..=10 {
            state.bump_last_processed(i);
        }

        let snapshot = state.snapshot();
        assert_eq!(snapshot.last_processed, 10);
    }

    #[test]
    fn progress_state_set_cumulative_with_block_count_zero() {
        let state = ProgressState::new(100);

        // Stats with block_count = 0 should not update start_height
        let stats = CumulativeStats::builder()
            .block_index(50)
            .block_count(0)
            .total_reward(100u64)
            .reward_count(1)
            .max_zero_count(1)
            .build();

        state.set_cumulative(Some(stats));
        let snapshot = state.snapshot();

        // start_height should remain unchanged
        assert_eq!(snapshot.start_height, 100);
    }

    #[test]
    fn progress_state_snapshot_clones_cumulative() {
        let state = ProgressState::new(0);

        let stats = CumulativeStats::builder()
            .block_index(10)
            .block_count(5)
            .total_reward(100u64)
            .reward_count(2)
            .max_zero_count(3)
            .nicest_txid(Some("test".to_string()))
            .utxo_spent_count(1)
            .new_utxo_count(2)
            .build();
        state.set_cumulative(Some(stats));

        let snapshot1 = state.snapshot();
        let snapshot2 = state.snapshot();

        // Both snapshots should have the same data
        assert_eq!(
            snapshot1
                .latest_cumulative
                .as_ref()
                .map(|s| s.block_index()),
            snapshot2
                .latest_cumulative
                .as_ref()
                .map(|s| s.block_index())
        );
    }

    #[test]
    fn format_amount_with_large_value() {
        // Test with a very large amount
        let amount: Amount = 10_000_000_000_000_000_u64; // 100M BTC equivalent
        let formatted = format_amount(&amount);
        assert_eq!(formatted, "100,000,000.00000000");
    }

    #[test]
    fn format_amount_with_mixed_fractional() {
        // 12_345_678_912_345_678 satoshis = 123,456,789.12345678 BTC equivalent
        let amount: Amount = 12_345_678_912_345_678_u64;
        let formatted = format_amount(&amount);
        assert_eq!(formatted, "123,456,789.12345678");
    }

    #[test]
    fn blocks_completed_large_range() {
        assert_eq!(blocks_completed(0, 999_999), 1_000_000);
    }

    #[test]
    fn blocks_total_large_range() {
        assert_eq!(blocks_total(0, 999_999), 1_000_000);
    }

    #[test]
    fn stats_row_creates_row_with_label_and_value() {
        let row = stats_row("Test Label", "Test Value".to_string());
        // Just verify it doesn't panic and returns a row
        let _ = row;
    }

    #[test]
    fn build_info_table_with_no_stats() {
        let snapshot = ProgressSnapshot {
            start_height: 0,
            start_instant: std::time::Instant::now(),
            last_processed: 100,
            tip_height: Some(1000),
            latest_cumulative: None,
            session_baseline: 0,
        };

        let table = build_info_table(&snapshot);
        // Just verify it doesn't panic
        let _ = table;
    }

    #[test]
    fn build_info_table_with_stats() {
        let stats = CumulativeStats::builder()
            .block_index(100)
            .block_count(50)
            .total_reward(1000u64)
            .reward_count(10)
            .max_zero_count(5)
            .nicest_txid(Some("abc123".to_string()))
            .utxo_spent_count(3)
            .new_utxo_count(7)
            .build();

        let snapshot = ProgressSnapshot {
            start_height: 0,
            start_instant: std::time::Instant::now(),
            last_processed: 100,
            tip_height: Some(1000),
            latest_cumulative: Some(stats),
            session_baseline: 0,
        };

        let table = build_info_table(&snapshot);
        // Just verify it doesn't panic
        let _ = table;
    }

    #[test]
    fn build_info_table_with_no_tip() {
        let snapshot = ProgressSnapshot {
            start_height: 0,
            start_instant: std::time::Instant::now(),
            last_processed: 100,
            tip_height: None,
            latest_cumulative: None,
            session_baseline: 0,
        };

        let table = build_info_table(&snapshot);
        // Just verify it doesn't panic
        let _ = table;
    }

    #[test]
    fn build_info_table_with_nicest_txid_none() {
        let stats = CumulativeStats::builder()
            .block_index(100)
            .block_count(50)
            .total_reward(1000u64)
            .reward_count(10)
            .max_zero_count(5)
            .utxo_spent_count(3)
            .new_utxo_count(7)
            .build();

        let snapshot = ProgressSnapshot {
            start_height: 0,
            start_instant: std::time::Instant::now(),
            last_processed: 100,
            tip_height: Some(1000),
            latest_cumulative: Some(stats),
            session_baseline: 0,
        };

        let table = build_info_table(&snapshot);
        // Just verify it doesn't panic
        let _ = table;
    }

    #[test]
    fn section_constraints_with_zero_height() {
        let constraints = section_constraints(0, true);
        assert_eq!(
            constraints,
            vec![Constraint::Length(0), Constraint::Length(1)]
        );
    }

    #[test]
    fn panel_area_with_zero_dimensions() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let panel = panel_area(area, true);
        assert_eq!(panel.width, 0);
        assert_eq!(panel.height, 0);
    }

    #[test]
    fn progress_snapshot_clone() {
        let snapshot = ProgressSnapshot {
            start_height: 0,
            start_instant: std::time::Instant::now(),
            last_processed: 100,
            tip_height: Some(1000),
            latest_cumulative: None,
            session_baseline: 0,
        };

        let cloned = snapshot.clone();
        assert_eq!(cloned.start_height, snapshot.start_height);
        assert_eq!(cloned.last_processed, snapshot.last_processed);
        assert_eq!(cloned.tip_height, snapshot.tip_height);
    }

    #[test]
    fn compute_speed_with_non_zero_elapsed() {
        let speed = compute_speed(100, Duration::from_secs(5));
        assert!((speed - 20.0).abs() < 0.001);
    }

    #[test]
    fn compute_eta_returns_correct_duration() {
        let eta = compute_eta(100, 25.0).expect("eta should be computed");
        assert_eq!(eta, Duration::from_secs(4));
    }

    #[test]
    fn format_duration_with_hours() {
        assert_eq!(format_duration(Duration::from_secs(7200)), "02:00:00");
        assert_eq!(format_duration(Duration::from_secs(7261)), "02:01:01");
    }

    #[test]
    fn format_duration_with_large_hours() {
        assert_eq!(format_duration(Duration::from_secs(36000)), "10:00:00");
        assert_eq!(format_duration(Duration::from_secs(360000)), "100:00:00");
    }

    #[test]
    fn format_with_separators_boundary_values() {
        assert_eq!(format_with_separators(1000), "1,000");
        assert_eq!(format_with_separators(9999), "9,999");
        assert_eq!(format_with_separators(10000), "10,000");
        assert_eq!(format_with_separators(100000), "100,000");
    }

    #[test]
    fn panel_height_returns_correct_values() {
        assert_eq!(
            panel_height(true),
            TABLE_SECTION_HEIGHT + PROGRESS_SECTION_HEIGHT
        );
        assert_eq!(panel_height(false), TABLE_SECTION_HEIGHT);
    }

    #[test]
    fn progress_state_update_tip_stores_value() {
        let state = ProgressState::new(0);
        state.update_tip(500);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.tip_height, Some(500));
    }

    #[test]
    fn progress_state_multiple_tip_updates() {
        let state = ProgressState::new(0);

        state.update_tip(100);
        assert_eq!(state.snapshot().tip_height, Some(100));

        state.update_tip(200);
        assert_eq!(state.snapshot().tip_height, Some(200));
    }

    #[test]
    fn blocks_completed_returns_zero_for_underflow() {
        assert_eq!(blocks_completed(100, 50), 0);
        assert_eq!(blocks_completed(1000, 0), 0);
    }

    #[test]
    fn blocks_total_returns_zero_for_underflow() {
        assert_eq!(blocks_total(100, 50), 0);
        assert_eq!(blocks_total(1000, 0), 0);
    }
}
