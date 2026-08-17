//! A dashboard for a running nano-stacks node, and a small explorer for the chain
//! it executed.
//!
//! Everything here comes from one node over its own public RPC — the same routes a
//! stock signer and a stock client use — so what is on the screen is what nano
//! tells the network, not a privileged view through a side door. Where a route
//! cannot answer, the field says so: a dashboard that renders `0` for a height it
//! failed to fetch is worse than one that renders nothing, because the reader
//! cannot tell the two apart.
//!
//! The three heights across the top are the distinction the whole project turns on.
//! A peer *advertises* a tip, this node *selects* one by signer weight and burn
//! view, and it *executes* up to some height at or below it. Only the third is a
//! block this node computed, and showing the first as if it were the third is how a
//! node that had executed nothing looked healthy for eighty minutes.

mod node;
mod receipts;

use std::{
    collections::HashSet,
    io,
    process::ExitCode,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use node::{Node, Sortition, SyncStatus};
use url::Url;

/// How often the node is polled.
///
/// A mainnet block lands every few seconds at best, so this is fast enough to look
/// live and slow enough that the dashboard is never the reason a node is busy.
const POLL: Duration = Duration::from_secs(2);
const STALL_AFTER: Duration = Duration::from_secs(30);
const OLD_SEAL: Duration = Duration::from_mins(2);

/// How many executed blocks the explorer keeps.
const HISTORY: usize = 200;

/// How far back one poll will walk to fill in blocks it did not see land.
///
/// A bound rather than "until it meets a known block", because the first poll after
/// a restart meets nothing: without this it would walk the chain to the checkpoint,
/// one request per block.
const FILL: usize = 50;

#[derive(Clone, Copy)]
enum Source {
    Sync,
    Info,
    Pox,
    Tenure,
    Sortitions,
    Metrics,
}

#[derive(Default)]
struct SourceState {
    updated: Option<Instant>,
    pending: bool,
    error: Option<String>,
}

impl SourceState {
    const fn started(&mut self) {
        self.pending = true;
    }

    fn succeeded(&mut self) {
        self.updated = Some(Instant::now());
        self.pending = false;
        self.error = None;
    }

    fn failed(&mut self, error: &str) {
        self.pending = false;
        self.error = Some(one_line_error(error));
    }

    fn description(&self) -> String {
        let age = self
            .updated
            .map(|updated| format!("{}s ago", updated.elapsed().as_secs()));
        match (&age, &self.error, self.pending) {
            (None, None, true) => "loading".to_owned(),
            (None, None, false) => "waiting".to_owned(),
            (None, Some(error), _) => format!("unavailable · {error}"),
            (Some(age), Some(error), true) => format!("stale {age} · retrying · {error}"),
            (Some(age), Some(error), false) => format!("stale {age} · {error}"),
            (Some(age), None, true) => format!("refreshing · updated {age}"),
            (Some(age), None, false) => format!("fresh {age}"),
        }
    }

    fn brief(&self) -> String {
        let age = self.updated.map(|updated| updated.elapsed().as_secs());
        match (age, self.error.is_some(), self.pending) {
            (None, false, true) => "loading".to_owned(),
            (None, false, false) => "waiting".to_owned(),
            (None, true, _) => "unavailable".to_owned(),
            (Some(age), true, _) => format!("stale {age}s"),
            (Some(age), false, true) => format!("refreshing · {age}s old"),
            (Some(age), false, false) => format!("fresh {age}s"),
        }
    }

    const fn unavailable(&self) -> bool {
        self.updated.is_none() && self.error.is_some()
    }
}

#[derive(Default)]
struct Sources {
    sync: SourceState,
    info: SourceState,
    pox: SourceState,
    tenure: SourceState,
    sortitions: SourceState,
    blocks: SourceState,
    metrics: SourceState,
}

impl Sources {
    const fn get_mut(&mut self, source: Source) -> &mut SourceState {
        match source {
            Source::Sync => &mut self.sync,
            Source::Info => &mut self.info,
            Source::Pox => &mut self.pox,
            Source::Tenure => &mut self.tenure,
            Source::Sortitions => &mut self.sortitions,
            Source::Metrics => &mut self.metrics,
        }
    }

    const fn unreachable(&self) -> bool {
        self.sync.unavailable()
            && self.info.unavailable()
            && self.pox.unavailable()
            && self.tenure.unavailable()
            && self.sortitions.unavailable()
    }

    fn degraded(&self) -> bool {
        [
            &self.sync,
            &self.info,
            &self.pox,
            &self.tenure,
            &self.sortitions,
            &self.blocks,
        ]
        .iter()
        .any(|source| source.error.is_some())
    }

    fn rpc_error(&self) -> Option<(&'static str, &str)> {
        [
            ("sync status", &self.sync),
            ("node info", &self.info),
            ("PoX", &self.pox),
            ("tenure", &self.tenure),
            ("sortition", &self.sortitions),
            ("block archive", &self.blocks),
        ]
        .into_iter()
        .find_map(|(name, source)| source.error.as_deref().map(|error| (name, error)))
    }
}

enum PollUpdate {
    Started(Source),
    Sync(Result<SyncStatus, String>),
    Info(Result<node::NodeInfo, String>),
    Pox(Result<node::Pox, String>),
    Tenure(Result<node::TenureInfo, String>),
    Sortitions(Result<Vec<Sortition>, String>),
    Metrics(Result<node::Metrics, String>),
}

struct Poller {
    updates: Receiver<PollUpdate>,
    refresh: Sender<()>,
}

impl Poller {
    fn start(node: Node) -> Self {
        let (updates, receiver) = mpsc::channel();
        let (refresh, commands) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let started = Instant::now();
                poll_routes(&node, &updates);
                let remaining = POLL.saturating_sub(started.elapsed());
                match commands.recv_timeout(remaining) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            updates: receiver,
            refresh,
        }
    }

    fn try_recv(&self) -> Option<PollUpdate> {
        self.updates.try_recv().ok()
    }

    fn refresh(&self) {
        let _ = self.refresh.send(());
    }
}

fn poll_routes(node: &Node, updates: &Sender<PollUpdate>) {
    for source in [
        Source::Sync,
        Source::Info,
        Source::Pox,
        Source::Tenure,
        Source::Sortitions,
    ] {
        let _ = updates.send(PollUpdate::Started(source));
    }
    let poll_metrics = node.metrics_url().is_some();
    if poll_metrics {
        let _ = updates.send(PollUpdate::Started(Source::Metrics));
    }
    thread::scope(|scope| {
        let send = updates.clone();
        let sync_node = node.clone();
        scope.spawn(move || {
            let _ = send.send(PollUpdate::Sync(sync_node.sync_status()));
        });
        let send = updates.clone();
        let info_node = node.clone();
        scope.spawn(move || {
            let _ = send.send(PollUpdate::Info(info_node.info()));
        });
        let send = updates.clone();
        let pox_node = node.clone();
        scope.spawn(move || {
            let _ = send.send(PollUpdate::Pox(pox_node.pox()));
        });
        let send = updates.clone();
        let tenure_node = node.clone();
        scope.spawn(move || {
            let _ = send.send(PollUpdate::Tenure(tenure_node.tenure()));
        });
        let send = updates.clone();
        let sortitions_node = node.clone();
        scope.spawn(move || {
            let _ = send.send(PollUpdate::Sortitions(sortitions_node.sortitions()));
        });
        if poll_metrics {
            let send = updates.clone();
            let metrics_node = node.clone();
            scope.spawn(move || {
                let _ = send.send(PollUpdate::Metrics(metrics_node.metrics()));
            });
        }
    });
}

struct BlockRequest {
    generation: u64,
    tip: String,
    height: u64,
    known: Vec<String>,
}

enum BlockUpdate {
    Block { generation: u64, block: node::Block },
    Finished(u64),
    Failed { generation: u64, error: String },
}

struct BlockLoader {
    updates: Receiver<BlockUpdate>,
    requests: Sender<BlockRequest>,
}

impl BlockLoader {
    fn start(node: Node) -> Self {
        let (updates, receiver) = mpsc::channel();
        let (requests, commands) = mpsc::channel();
        thread::spawn(move || load_blocks(&node, &commands, &updates));
        Self {
            updates: receiver,
            requests,
        }
    }

    fn request(&self, request: BlockRequest) {
        let _ = self.requests.send(request);
    }

    fn try_recv(&self) -> Option<BlockUpdate> {
        self.updates.try_recv().ok()
    }
}

fn load_blocks(node: &Node, commands: &Receiver<BlockRequest>, updates: &Sender<BlockUpdate>) {
    let mut pending = None;
    'requests: loop {
        let request = match pending.take() {
            Some(request) => request,
            None => match commands.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        let known = request.known.iter().collect::<HashSet<_>>();
        let mut walk = Some(request.tip.clone());
        let mut found = 0;
        while let Some(id) = walk.take() {
            match commands.try_recv() {
                Ok(request) => {
                    pending = commands.try_iter().last().or(Some(request));
                    continue 'requests;
                }
                Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            if known.contains(&id) || found >= FILL {
                break;
            }
            match node.block(&id, if found == 0 { request.height } else { 0 }) {
                Ok(block) => {
                    walk = Some(block.parent_id.clone());
                    found += 1;
                    if updates
                        .send(BlockUpdate::Block {
                            generation: request.generation,
                            block,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = updates.send(BlockUpdate::Failed {
                        generation: request.generation,
                        error,
                    });
                    continue 'requests;
                }
            }
        }
        if updates
            .send(BlockUpdate::Finished(request.generation))
            .is_err()
        {
            return;
        }
    }
}

fn one_line_error(error: &str) -> String {
    const MAX: usize = 42;
    let line = error.lines().next().unwrap_or("request failed");
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let mut shortened = line.chars().take(MAX - 1).collect::<String>();
    shortened.push('…');
    shortened
}

#[derive(Debug, Parser)]
#[command(
    name = "nano-tui",
    about = "Read-only dashboard and explorer for a nano-stacks node",
    after_help = "VIEWS:\n  1 overview   2 activity   3 election   4 operations\n\nKEYS:\n  ↑/↓ select   enter/→ open   esc/← back\n  r refresh   q quit/back   ? help"
)]
struct Args {
    /// HTTP RPC endpoint of the node to inspect.
    #[arg(long, default_value = "http://127.0.0.1:20443")]
    rpc_url: Url,

    /// Optional Prometheus endpoint used by the Operations view.
    #[arg(long)]
    metrics_url: Option<Url>,

    /// Render one 110x32 frame as text and exit.
    #[arg(long)]
    once: bool,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("nano-tui: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_main() -> io::Result<ExitCode> {
    let args = Args::parse();
    let node = Node::new(args.rpc_url.as_str())
        .with_metrics_url(args.metrics_url.as_ref().map(Url::as_str));
    // One frame as text, for a check that does not need a terminal at all: the same
    // draw against the same node, rendered into a buffer instead of onto a screen.
    if args.once {
        let (rendered, code) = render_once(&node);
        print!("{rendered}");
        return Ok(code);
    }
    let mut terminal = start()?;
    let outcome = run(&mut terminal, &node);
    stop(&mut terminal)?;
    outcome?;
    Ok(ExitCode::SUCCESS)
}

/// Draw one frame into a buffer and return it as lines of text.
fn render_once(node: &Node) -> (String, ExitCode) {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 32))
        .expect("a buffer backend cannot fail to open");
    let mut state = snapshot(node);
    let code = if state.sources.unreachable() {
        ExitCode::from(3)
    } else if state.sources.degraded() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    };
    terminal
        .draw(|frame| draw(frame, &mut state, node))
        .expect("drawing into a buffer cannot fail");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for row in 0..buffer.area.height {
        for column in 0..buffer.area.width {
            out.push_str(buffer[(column, row)].symbol());
        }
        out.push('\n');
    }
    (out, code)
}

fn snapshot(node: &Node) -> State {
    let (updates, receiver) = mpsc::channel();
    poll_routes(node, &updates);
    drop(updates);
    let mut state = State::default();
    let mut block_request = None;
    for update in receiver {
        block_request = state.apply_poll(update).or(block_request);
    }
    if let Some(request) = block_request {
        load_blocks_once(node, &mut state, request);
    }
    state
}

fn load_blocks_once(node: &Node, state: &mut State, request: BlockRequest) {
    let known = request.known.iter().collect::<HashSet<_>>();
    let mut walk = Some(request.tip);
    let mut found = 0;
    while let Some(id) = walk.take() {
        if known.contains(&id) || found >= FILL {
            break;
        }
        match node.block(&id, if found == 0 { request.height } else { 0 }) {
            Ok(block) => {
                walk = Some(block.parent_id.clone());
                found += 1;
                state.apply_block(BlockUpdate::Block {
                    generation: request.generation,
                    block,
                });
            }
            Err(error) => {
                state.apply_block(BlockUpdate::Failed {
                    generation: request.generation,
                    error,
                });
                return;
            }
        }
    }
    state.apply_block(BlockUpdate::Finished(request.generation));
}

fn start() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn stop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

/// Everything on the screen, including the last good answer from every route.
#[derive(Default)]
struct State {
    sync: Option<SyncStatus>,
    info: Option<node::NodeInfo>,
    pox: Option<node::Pox>,
    tenure: Option<node::TenureInfo>,
    sortitions: Vec<Sortition>,
    blocks: Vec<node::Block>,
    selected_block: ListState,
    selected_transaction: ListState,
    selected_participant: ListState,
    screen: Screen,
    standard_layout: bool,
    help: bool,
    help_scroll: u16,
    transaction_scroll: u16,
    operations_scroll: u16,
    sources: Sources,
    sync_baseline: Option<SyncStatus>,
    progress: ProgressState,
    metrics: Option<node::Metrics>,
    metrics_baseline: Option<node::Metrics>,
    metrics_enabled: bool,
    requested_tip: Option<String>,
    block_generation: u64,
    backfill_inserted: usize,
    receipt_blocks: Vec<receipts::BlockOutcomes>,
    receipt_stream: ReceiptStream,
}

impl State {
    fn apply_poll(&mut self, update: PollUpdate) -> Option<BlockRequest> {
        match update {
            PollUpdate::Started(source) => {
                self.sources.get_mut(source).started();
                self.metrics_enabled |= matches!(source, Source::Metrics);
                return None;
            }
            PollUpdate::Sync(result) => {
                if let Ok(sync) = &result {
                    self.progress.observe(sync.executed_stacks_height);
                }
                if self.sync_baseline.is_none() {
                    self.sync_baseline = result.as_ref().ok().cloned();
                }
                apply_reading(&mut self.sync, &mut self.sources.sync, result);
            }
            PollUpdate::Info(result) => {
                apply_reading(&mut self.info, &mut self.sources.info, result);
            }
            PollUpdate::Pox(result) => {
                apply_reading(&mut self.pox, &mut self.sources.pox, result);
            }
            PollUpdate::Tenure(result) => {
                apply_reading(&mut self.tenure, &mut self.sources.tenure, result);
            }
            PollUpdate::Sortitions(result) => match result {
                Ok(sortitions) => {
                    self.sortitions = sortitions;
                    self.sources.sortitions.succeeded();
                    select_current_participant(self);
                }
                Err(error) => self.sources.sortitions.failed(&error),
            },
            PollUpdate::Metrics(result) => {
                if self.metrics_baseline.is_none() {
                    self.metrics_baseline = result.as_ref().ok().cloned();
                }
                apply_reading(&mut self.metrics, &mut self.sources.metrics, result);
            }
        }

        let (Some(height), Some(tip)) = (
            self.sync
                .as_ref()
                .and_then(|sync| sync.executed_stacks_height),
            self.sync
                .as_ref()
                .and_then(|sync| sync.executed_stacks_tip.clone()),
        ) else {
            return None;
        };
        if self.blocks.first().is_some_and(|block| block.id == tip) {
            self.requested_tip = Some(tip);
            return None;
        }
        if self.requested_tip.as_deref() == Some(&tip) && self.sources.blocks.pending {
            return None;
        }
        self.block_generation = self.block_generation.wrapping_add(1);
        self.requested_tip = Some(tip.clone());
        self.backfill_inserted = 0;
        self.sources.blocks.started();
        Some(BlockRequest {
            generation: self.block_generation,
            tip,
            height,
            known: self.blocks.iter().map(|block| block.id.clone()).collect(),
        })
    }

    fn apply_block(&mut self, update: BlockUpdate) {
        match update {
            BlockUpdate::Block { generation, block }
                if generation == self.block_generation
                    && !self.blocks.iter().any(|known| known.id == block.id) =>
            {
                let at = self.backfill_inserted.min(self.blocks.len());
                self.blocks.insert(at, block);
                self.backfill_inserted += 1;
                self.blocks.truncate(HISTORY);
                if let Some(index) = self.selected_block.selected() {
                    self.selected_block
                        .select(Some((index + 1).min(self.blocks.len() - 1)));
                }
            }
            BlockUpdate::Finished(generation) if generation == self.block_generation => {
                self.sources.blocks.succeeded();
            }
            BlockUpdate::Failed { generation, error } if generation == self.block_generation => {
                self.sources.blocks.failed(&error);
            }
            BlockUpdate::Block { .. } | BlockUpdate::Finished(_) | BlockUpdate::Failed { .. } => {}
        }
    }

    fn apply_receipt(&mut self, update: receipts::Update) {
        match update {
            receipts::Update::Connected => {
                self.receipt_stream.connected = true;
                self.receipt_stream.error = None;
                self.receipt_stream.connections += 1;
                if self.receipt_stream.started_after_height.is_none() {
                    self.receipt_stream.started_after_height =
                        self.blocks.first().map(|block| block.height);
                }
            }
            receipts::Update::Disconnected(error) => {
                self.receipt_stream.connected = false;
                self.receipt_stream.error = Some(one_line_error(&error));
            }
            receipts::Update::Event(event) => self.apply_receipt_event(&event),
        }
    }

    fn apply_receipt_event(&mut self, event: &receipts::StreamEvent) {
        self.receipt_stream.observe_sequence(event.sequence);
        if event.kind != "new_block" {
            return;
        }
        let block = match receipts::BlockOutcomes::parse(&event.data) {
            Ok(block) => block,
            Err(error) => {
                self.receipt_stream.error = Some(one_line_error(&error));
                return;
            }
        };
        self.receipt_stream.observe_block(block.block_height);
        self.receipt_blocks
            .retain(|known| !same_id(&known.index_block_hash, &block.index_block_hash));
        self.receipt_blocks.insert(0, block);
        self.receipt_blocks.truncate(HISTORY);
    }

    fn behind(&self) -> Option<u64> {
        self.sync.as_ref().and_then(|sync| sync.blocks_behind)
    }
}

#[derive(Default)]
struct ReceiptStream {
    connected: bool,
    connections: u64,
    started_after_height: Option<u64>,
    first_height: Option<u64>,
    latest_height: Option<u64>,
    last_sequence: Option<u64>,
    pending_gap: Option<ReceiptGap>,
    gaps: Vec<ReceiptGap>,
    unsequenced: bool,
    error: Option<String>,
}

impl ReceiptStream {
    fn observe_sequence(&mut self, sequence: Option<u64>) {
        let Some(sequence) = sequence else {
            self.unsequenced = true;
            return;
        };
        if let Some(last) = self.last_sequence {
            let expected = last.saturating_add(1);
            if sequence != expected {
                let missing = sequence.saturating_sub(expected).max(1);
                self.pending_gap = Some(ReceiptGap {
                    after_height: self.latest_height,
                    before_height: None,
                    missing,
                });
            }
        }
        self.last_sequence = Some(sequence);
    }

    fn observe_block(&mut self, height: u64) {
        self.first_height.get_or_insert(height);
        self.latest_height = Some(self.latest_height.map_or(height, |known| known.max(height)));
        if let Some(mut gap) = self.pending_gap.take() {
            gap.before_height = Some(height);
            self.gaps.push(gap);
            self.gaps.truncate(HISTORY);
        }
    }

    fn gap_at(&self, height: u64) -> Option<&ReceiptGap> {
        self.gaps.iter().find(|gap| gap.covers(height)).or_else(|| {
            self.pending_gap
                .as_ref()
                .filter(|gap| gap.after_height.is_none_or(|after| height > after))
        })
    }
}

struct ReceiptGap {
    after_height: Option<u64>,
    before_height: Option<u64>,
    missing: u64,
}

impl ReceiptGap {
    fn covers(&self, height: u64) -> bool {
        self.after_height.is_none_or(|after| height > after)
            && self.before_height.is_none_or(|before| height <= before)
    }
}

#[derive(Default)]
struct ProgressState {
    height: Option<u64>,
    changed: Option<Instant>,
}

impl ProgressState {
    fn observe(&mut self, height: Option<u64>) {
        if height.is_some() && height != self.height {
            self.height = height;
            self.changed = Some(Instant::now());
        }
    }

    fn unchanged_for(&self) -> Option<Duration> {
        self.changed.map(|changed| changed.elapsed())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Health {
    Starting,
    Syncing,
    Healthy,
    Degraded,
    Stalled,
    Unreachable,
}

impl Health {
    const fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Syncing => "SYNCING",
            Self::Healthy => "HEALTHY",
            Self::Degraded => "DEGRADED",
            Self::Stalled => "STALLED",
            Self::Unreachable => "UNREACHABLE",
        }
    }

    const fn colour(self) -> Color {
        match self {
            Self::Healthy => Color::Green,
            Self::Starting | Self::Syncing => Color::Yellow,
            Self::Degraded | Self::Stalled | Self::Unreachable => Color::Red,
        }
    }
}

struct HealthSummary {
    state: Health,
    reason: String,
}

fn health_summary(state: &State) -> HealthSummary {
    if state.sources.unreachable() {
        return health(Health::Unreachable, "no RPC route answered");
    }
    if let Some((source, error)) = state.sources.rpc_error() {
        return health(Health::Degraded, format!("{source} failed: {error}"));
    }
    let Some(sync) = state.sync.as_ref() else {
        return health(Health::Starting, "waiting for the first sync status");
    };
    if let Some(reason) = observer_problem(sync, state.sync_baseline.as_ref()) {
        return health(Health::Degraded, reason);
    }
    if let Some(reason) = metrics_problem(
        sync,
        state.metrics.as_ref(),
        state.metrics_baseline.as_ref(),
    ) {
        return health(Health::Degraded, reason);
    }
    if let Some(reason) = stall_problem(state) {
        return health(Health::Stalled, reason);
    }
    match (sync.executed_stacks_height, sync.blocks_behind) {
        (None, _) => health(
            Health::Starting,
            "waiting for the first locally executed block",
        ),
        (Some(_), Some(behind)) if behind > 0 => health(
            Health::Syncing,
            format!("{} blocks behind the peer-reported tip", thousands(behind)),
        ),
        (Some(_), Some(0)) => health(Health::Healthy, "local execution matches the peer tip"),
        (Some(_), None) => health(Health::Starting, "waiting for peer lag evidence"),
        (Some(_), Some(_)) => unreachable!("positive lag handled above"),
    }
}

fn health(state: Health, reason: impl Into<String>) -> HealthSummary {
    HealthSummary {
        state,
        reason: reason.into(),
    }
}

fn observer_problem(sync: &SyncStatus, baseline: Option<&SyncStatus>) -> Option<String> {
    let observers = sync.event_observers.as_ref()?;
    for observer in observers {
        if !observer.reachable {
            return Some(format!("event observer {} is unreachable", observer.url));
        }
        let opened = baseline
            .and_then(|sync| sync.event_observers.as_ref())
            .and_then(|observers| observers.iter().find(|opened| opened.url == observer.url));
        if opened.is_some_and(|opened| observer.dropped > opened.dropped) {
            return Some(format!("event observer {} dropped events", observer.url));
        }
        if opened.is_some_and(|opened| observer.undelivered > opened.undelivered) {
            return Some(format!("event observer {} backlog grew", observer.url));
        }
    }
    None
}

fn metrics_problem(
    sync: &SyncStatus,
    metrics: Option<&node::Metrics>,
    opened: Option<&node::Metrics>,
) -> Option<String> {
    let metrics = metrics?;
    let refusal_reasons = [
        (
            "compiler-gap block refusal",
            metrics.refusal_compiler_gap,
            opened.and_then(|metrics| metrics.refusal_compiler_gap),
        ),
        (
            "state-root mismatch refusal",
            metrics.refusal_root_mismatch,
            opened.and_then(|metrics| metrics.refusal_root_mismatch),
        ),
        (
            "signature refusal",
            metrics.refusal_signature,
            opened.and_then(|metrics| metrics.refusal_signature),
        ),
        (
            "missing-context refusal",
            metrics.refusal_missing_context,
            opened.and_then(|metrics| metrics.refusal_missing_context),
        ),
        (
            "unclassified block refusal",
            metrics.refusal_other,
            opened.and_then(|metrics| metrics.refusal_other),
        ),
        (
            "pushed-block refusal",
            metrics.pushed_blocks_refused,
            opened.and_then(|metrics| metrics.pushed_blocks_refused),
        ),
        (
            "unanswered sync round",
            metrics.sync_rounds_unanswered,
            opened.and_then(|metrics| metrics.sync_rounds_unanswered),
        ),
        (
            "unanswered StackerDB round",
            metrics.stackerdb_rounds_unanswered,
            opened.and_then(|metrics| metrics.stackerdb_rounds_unanswered),
        ),
    ];
    if let Some((reason, _, _)) = refusal_reasons
        .into_iter()
        .find(|(_, current, baseline)| metric_increased(*current, *baseline))
    {
        return Some(format!("new {reason} since opened"));
    }
    let roles = sync.roles?;
    if roles.follower && metrics.serving_followers == Some(0.0) {
        return Some("follower has 0 serving peers".to_owned());
    }
    if (roles.signer || sync.queued_proposals.is_some())
        && metrics.serving_proposal_validators == Some(0.0)
    {
        return Some("proposal validator has 0 serving peers".to_owned());
    }
    if (roles.signer || sync.queued_stackerdb_chunks.is_some())
        && metrics.serving_stackerdb_replicas == Some(0.0)
    {
        return Some("StackerDB replication has 0 serving peers".to_owned());
    }
    None
}

fn metric_increased(current: Option<f64>, opened: Option<f64>) -> bool {
    current
        .zip(opened)
        .is_some_and(|(current, opened)| current > opened)
}

fn stall_problem(state: &State) -> Option<String> {
    let behind = state.behind().unwrap_or_default();
    let progress_old = state
        .progress
        .unchanged_for()
        .is_some_and(|elapsed| elapsed >= STALL_AFTER);
    let seal_old = sealed_age(state).is_some_and(|elapsed| elapsed >= OLD_SEAL);
    if behind == 0 && !progress_old {
        return None;
    }
    if !progress_old && !seal_old {
        return None;
    }
    if let Some((queue, growth)) = growing_queue(state) {
        return Some(format!(
            "{queue} grew by {} while execution did not advance",
            thousands(growth)
        ));
    }
    (behind > 0).then(|| {
        format!(
            "executed height has not advanced while {} blocks behind",
            thousands(behind)
        )
    })
}

fn growing_queue(state: &State) -> Option<(&'static str, u64)> {
    let current = state.sync.as_ref()?;
    let opened = state.sync_baseline.as_ref()?;
    [
        (
            "staged block queue",
            current.staged_blocks,
            opened.staged_blocks,
        ),
        (
            "relay validation queue",
            current.relay_offered,
            opened.relay_offered,
        ),
        (
            "relay announcement queue",
            current.relay_announcing,
            opened.relay_announcing,
        ),
        (
            "block ingestion queue",
            current.queued_blocks,
            opened.queued_blocks,
        ),
        (
            "proposal validator queue",
            current.queued_proposals,
            opened.queued_proposals,
        ),
        (
            "StackerDB relay queue",
            current.queued_stackerdb_chunks,
            opened.queued_stackerdb_chunks,
        ),
        (
            "transaction relay queue",
            current.queued_transactions,
            opened.queued_transactions,
        ),
    ]
    .into_iter()
    .find_map(|(name, current, opened)| {
        current
            .zip(opened)
            .and_then(|(current, opened)| (current > opened).then_some((name, current - opened)))
    })
}

fn sealed_timestamp(state: &State) -> Option<u64> {
    state
        .metrics
        .as_ref()
        .and_then(|metrics| metrics.last_sealed_timestamp_seconds)
        .filter(|timestamp| *timestamp > 0)
        .or_else(|| state.blocks.first().map(|block| block.timestamp))
}

fn sealed_age(state: &State) -> Option<Duration> {
    let sealed = sealed_timestamp(state)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(now.saturating_sub(sealed)))
}

fn duration_age(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn apply_reading<T>(target: &mut Option<T>, source: &mut SourceState, result: Result<T, String>) {
    match result {
        Ok(value) => {
            *target = Some(value);
            source.succeeded();
        }
        Err(error) => source.failed(&error),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Screen {
    #[default]
    Overview,
    Activity,
    Block,
    Transaction,
    Election,
    Operations,
}

impl Screen {
    const fn name(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Activity => "activity",
            Self::Block => "block",
            Self::Transaction => "transaction",
            Self::Election => "election",
            Self::Operations => "operations",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    None,
    Refresh,
    Quit,
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, node: &Node) -> io::Result<()> {
    let mut state = State::default();
    let poller = Poller::start(node.clone());
    let blocks = BlockLoader::start(node.clone());
    let receipt_listener = receipts::Listener::start(node.clone());
    loop {
        while let Some(update) = poller.try_recv() {
            if let Some(request) = state.apply_poll(update) {
                blocks.request(request);
            }
        }
        while let Some(update) = blocks.try_recv() {
            state.apply_block(update);
        }
        while let Some(update) = receipt_listener.try_recv() {
            state.apply_receipt(update);
        }
        terminal.draw(|frame| draw(frame, &mut state, node))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match handle_key(&mut state, key.code) {
            Action::Quit => return Ok(()),
            Action::Refresh => poller.refresh(),
            Action::None => {}
        }
    }
}

fn handle_key(state: &mut State, key: KeyCode) -> Action {
    if state.help {
        match key {
            KeyCode::Char('?' | 'q' | 'h') | KeyCode::Esc | KeyCode::Left => {
                state.help = false;
                state.help_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.help_scroll = state.help_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.help_scroll = state.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                state.help_scroll = state.help_scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                state.help_scroll = state.help_scroll.saturating_sub(10);
            }
            KeyCode::Home => state.help_scroll = 0,
            KeyCode::End => state.help_scroll = u16::MAX,
            _ => {}
        }
        return Action::None;
    }

    match key {
        KeyCode::Char('?') => {
            state.help = true;
            state.help_scroll = 0;
        }
        KeyCode::Char('1') => state.screen = Screen::Overview,
        KeyCode::Char('2') => state.screen = Screen::Activity,
        KeyCode::Char('3') => {
            state.screen = Screen::Election;
            select_current_participant(state);
        }
        KeyCode::Char('4') => {
            state.screen = Screen::Operations;
            state.operations_scroll = 0;
        }
        KeyCode::Char('m') => {
            state.screen = if state.screen == Screen::Election {
                Screen::Overview
            } else {
                Screen::Election
            };
            select_current_participant(state);
        }
        KeyCode::Char('o') => {
            state.screen = if state.screen == Screen::Operations {
                Screen::Overview
            } else {
                Screen::Operations
            };
            state.operations_scroll = 0;
        }
        KeyCode::Char('q' | 'h') | KeyCode::Esc | KeyCode::Left => {
            match state.screen {
                Screen::Overview | Screen::Activity => return Action::Quit,
                Screen::Block => state.screen = Screen::Activity,
                Screen::Election | Screen::Operations => {
                    state.screen = Screen::Overview;
                }
                Screen::Transaction => state.screen = Screen::Block,
            }
            state.transaction_scroll = 0;
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match state.screen {
            Screen::Activity if !state.blocks.is_empty() => {
                if state.selected_block.selected().is_none() {
                    state.selected_block.select(Some(0));
                }
                select_first_transaction(state);
                state.screen = Screen::Block;
            }
            Screen::Block if selected_transaction(state).is_some() => {
                state.transaction_scroll = 0;
                state.screen = Screen::Transaction;
            }
            Screen::Overview
            | Screen::Activity
            | Screen::Block
            | Screen::Transaction
            | Screen::Election
            | Screen::Operations => {}
        },
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::PageDown => move_selection(state, 10),
        KeyCode::PageUp => move_selection(state, -10),
        KeyCode::Home => move_to_edge(state, false),
        KeyCode::End => move_to_edge(state, true),
        KeyCode::Char('r') => return Action::Refresh,
        _ => {}
    }
    Action::None
}

fn move_selection(state: &mut State, by: isize) {
    match state.screen {
        Screen::Activity => move_list_selection(&mut state.selected_block, state.blocks.len(), by),
        Screen::Block => {
            let transactions = selected_block(state).map_or(0, |block| block.transactions.len());
            move_list_selection(&mut state.selected_transaction, transactions, by);
        }
        Screen::Transaction => {
            state.transaction_scroll = state
                .transaction_scroll
                .saturating_add_signed(i16::try_from(by).expect("key movement fits in i16"));
        }
        Screen::Election => {
            let participants =
                mining_competition(state).map_or(0, |competition| competition.participants.len());
            move_list_selection(&mut state.selected_participant, participants, by);
        }
        Screen::Operations => {
            state.operations_scroll = state
                .operations_scroll
                .saturating_add_signed(i16::try_from(by).expect("key movement fits in i16"));
        }
        Screen::Overview => {}
    }
}

fn move_list_selection(selected: &mut ListState, length: usize, by: isize) {
    if length == 0 {
        return;
    }
    let last = length - 1;
    let next = selected
        .selected()
        .map_or(0, |index| index.saturating_add_signed(by).min(last));
    selected.select(Some(next));
}

fn move_to_edge(state: &mut State, end: bool) {
    match state.screen {
        Screen::Activity => select_edge(&mut state.selected_block, state.blocks.len(), end),
        Screen::Block => {
            let transactions = selected_block(state).map_or(0, |block| block.transactions.len());
            select_edge(&mut state.selected_transaction, transactions, end);
        }
        Screen::Transaction => state.transaction_scroll = if end { u16::MAX } else { 0 },
        Screen::Election => {
            let participants =
                mining_competition(state).map_or(0, |competition| competition.participants.len());
            select_edge(&mut state.selected_participant, participants, end);
        }
        Screen::Operations => state.operations_scroll = if end { u16::MAX } else { 0 },
        Screen::Overview => {}
    }
}

const fn select_edge(selected: &mut ListState, length: usize, end: bool) {
    if length > 0 {
        selected.select(Some(if end { length - 1 } else { 0 }));
    }
}

fn select_first_transaction(state: &mut State) {
    let transactions = selected_block(state).map_or(0, |block| block.transactions.len());
    if transactions == 0 {
        state.selected_transaction.select(None);
    } else if state
        .selected_transaction
        .selected()
        .is_none_or(|index| index >= transactions)
    {
        state.selected_transaction.select(Some(0));
    }
}

fn latest_sortition(state: &State) -> Option<&Sortition> {
    state.sortitions.first()
}

fn active_sortition(state: &State) -> Option<&Sortition> {
    state
        .sortitions
        .iter()
        .find(|sortition| sortition.elected == Some(true))
}

fn mining_competition(state: &State) -> Option<&node::MiningCompetition> {
    latest_sortition(state)?.mining_competition.as_ref()
}

fn competition_winner(
    competition: &node::MiningCompetition,
) -> Option<&node::SortitionParticipant> {
    let winner = competition.winner_txid.as_deref()?;
    competition
        .participants
        .iter()
        .find(|participant| same_id(&participant.txid, winner))
}

fn miner_identity(sortition: &Sortition) -> (Option<&str>, &'static str) {
    if let Some(hash) = sortition.miner_pk_hash160.as_deref() {
        return (Some(hash), " · signing key");
    }
    let winner = sortition
        .mining_competition
        .as_ref()
        .and_then(competition_winner);
    winner
        .and_then(|winner| winner.signing_key_hash.as_deref())
        .map_or_else(
            || {
                (
                    winner.and_then(|winner| winner.vrf_public_key.as_deref()),
                    " · VRF key",
                )
            },
            |hash| (Some(hash), " · signing key"),
        )
}

fn participant_weight(
    participant: &node::SortitionParticipant,
    competition: &node::MiningCompetition,
) -> String {
    let total: u128 = competition
        .participants
        .iter()
        .map(|participant| u128::from(participant.effective_burn_sats))
        .sum();
    if total == 0 {
        return "unavailable".to_owned();
    }
    let tenths = u128::from(participant.effective_burn_sats) * 1_000 / total;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn select_current_participant(state: &mut State) {
    let Some(competition) = mining_competition(state) else {
        state.selected_participant.select(None);
        return;
    };
    if competition.participants.is_empty() {
        state.selected_participant.select(None);
        return;
    }
    let selected = state.selected_participant.selected();
    if selected.is_some_and(|index| index < competition.participants.len()) {
        return;
    }
    let winner = competition.winner_txid.as_deref();
    let selected = winner
        .and_then(|winner| {
            competition
                .participants
                .iter()
                .position(|participant| same_id(&participant.txid, winner))
        })
        .unwrap_or_default();
    state.selected_participant.select(Some(selected));
}

fn selected_participant(state: &State) -> Option<&node::SortitionParticipant> {
    let competition = mining_competition(state)?;
    state
        .selected_participant
        .selected()
        .and_then(|index| competition.participants.get(index))
        .or_else(|| competition.participants.first())
}

fn selected_block(state: &State) -> Option<&node::Block> {
    state
        .selected_block
        .selected()
        .and_then(|index| state.blocks.get(index))
        .or_else(|| state.blocks.first())
}

fn selected_transaction(state: &State) -> Option<&node::Transaction> {
    let block = selected_block(state)?;
    state
        .selected_transaction
        .selected()
        .and_then(|index| block.transactions.get(index))
        .or_else(|| block.transactions.first())
}

fn draw(frame: &mut Frame, state: &mut State, node: &Node) {
    const MIN_WIDTH: u16 = 80;
    const MIN_HEIGHT: u16 = 24;
    const WIDE_WIDTH: u16 = 150;
    const WIDE_HEIGHT: u16 = 32;

    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        state.standard_layout = true;
        draw_too_small(frame, area, MIN_WIDTH, MIN_HEIGHT);
        return;
    }

    let wide = area.width >= WIDE_WIDTH && area.height >= WIDE_HEIGHT && !state.sources.degraded();
    state.standard_layout = !wide;
    if state.screen == Screen::Overview {
        draw_overview(frame, state, node, wide);
        if state.help {
            draw_help(frame, state);
        }
        return;
    }

    let header_height = if wide { 9 } else { 8 };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
    if wide {
        draw_sync_status(frame, areas[0], state, node);
    } else {
        draw_compact_sync_status(frame, areas[0], state, node);
    }
    match state.screen {
        Screen::Activity => draw_blocks(frame, areas[1], state),
        Screen::Block => draw_block(frame, areas[1], state),
        Screen::Transaction => draw_transaction(frame, areas[1], state),
        Screen::Election if areas[1].height >= 22 => draw_election(frame, areas[1], state),
        Screen::Election => draw_standard_election(frame, areas[1], state),
        Screen::Operations => draw_operations(frame, areas[1], state),
        Screen::Overview => unreachable!("handled by the dashboard layout"),
    }
    draw_keys(frame, areas[2], state);
    if state.help {
        draw_help(frame, state);
    }
}

fn draw_overview(frame: &mut Frame, state: &State, node: &Node, wide: bool) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if wide { 9 } else { 8 }),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(frame.area());
    if wide {
        draw_sync_status(frame, areas[0], state, node);
    } else {
        draw_compact_sync_status(frame, areas[0], state, node);
    }
    draw_protocol_story(frame, areas[1], state);
    draw_keys(frame, areas[2], state);
}

fn draw_protocol_story(frame: &mut Frame, area: Rect, state: &State) {
    let lines = if area.width < 150 {
        compact_protocol_story(state)
    } else {
        protocol_story(state)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(bordered(" how Bitcoin decisions become Stacks activity ")),
        area,
    );
}

fn compact_protocol_story(state: &State) -> Vec<Line<'static>> {
    let latest = latest_sortition(state);
    let competition = latest.and_then(|sortition| sortition.mining_competition.as_ref());
    let decision = latest.and_then(|sortition| sortition.elected).map_or_else(
        || "decision unavailable".to_owned(),
        |elected| {
            let height = latest
                .and_then(|sortition| sortition.burn_block_height)
                .map_or_else(|| "?".to_owned(), thousands);
            let time = timestamp_context(
                latest.and_then(|sortition| sortition.burn_header_timestamp),
            );
            if elected {
                format!(
                    "Bitcoin block {height}: new network miner elected from {} commitments · {time}",
                    competition.map_or(0, |competition| competition.participants.len())
                )
            } else {
                format!(
                    "Bitcoin block {height}: no successor; active tenure continued · {time}"
                )
            }
        },
    );
    let commitment = competition.and_then(competition_winner).map_or_else(
        || "no winning commitment retained".to_owned(),
        |winner| {
            format!(
                "winner {} · {} sats · {} relative weight ({}-block sample; not win probability)",
                winner
                    .signing_key_hash
                    .as_deref()
                    .or(winner.vrf_public_key.as_deref())
                    .map_or_else(|| "key unavailable".to_owned(), short),
                thousands(winner.burn_sats),
                participant_weight(winner, competition.expect("winner has competition")),
                competition.map_or(0, |competition| competition.sampled_window_blocks)
            )
        },
    );
    let active = active_sortition(state);
    let (miner, _) = active.map_or((None, ""), miner_identity);
    let tenure = state.tenure.clone().unwrap_or_default();
    let tenure = format!(
        "tenure {} · network miner {} · tip {}",
        tenure
            .consensus_hash
            .as_deref()
            .map_or_else(|| "unavailable".to_owned(), short),
        miner.map_or_else(|| "unavailable".to_owned(), short),
        tenure.tip_height.map_or_else(|| "?".to_owned(), thousands)
    );
    let next = latest
        .and_then(|sortition| sortition.burn_block_height)
        .and_then(|height| height.checked_add(1))
        .map_or_else(
            || "next Bitcoin block".to_owned(),
            |height| format!("Bitcoin block {}", thousands(height)),
        );
    vec![
        detail("1 Bitcoin decision", decision),
        detail("2 commitment", commitment),
        detail("3 tenure", tenure),
        detail("4 Stacks blocks", compact_activity_story(state)),
        detail(
            "next boundary",
            format!("{next}: may elect a successor; otherwise tenure continues"),
        ),
        detail("PoX schedule", compact_pox_story(state)),
        detail(
            "data freshness",
            format!(
                "sortition {} · tenure {} · blocks {}",
                state.sources.sortitions.description(),
                state.sources.tenure.description(),
                state.sources.blocks.description()
            ),
        ),
    ]
}

fn compact_activity_story(state: &State) -> String {
    let blocks = state
        .tenure
        .as_ref()
        .and_then(|tenure| tenure.consensus_hash.as_deref())
        .map_or_else(Vec::new, |tenure| tenure_blocks(state, tenure));
    let Some(newest) = blocks.first() else {
        return "no locally executed blocks loaded for this tenure".to_owned();
    };
    let oldest = blocks.last().expect("non-empty blocks have a last item");
    format!(
        "{} locally executed · {}→{} · {} extensions",
        blocks.len(),
        thousands(oldest.height),
        thousands(newest.height),
        tenure_extensions(&blocks).len()
    )
}

fn compact_pox_story(state: &State) -> String {
    let pox = state.pox.clone().unwrap_or_default();
    let current = pox.current_cycle.as_ref().and_then(|cycle| cycle.id);
    pox.next_cycle.as_ref().map_or_else(
        || "boundary unavailable".to_owned(),
        |next| {
            format!(
                "cycle {}→{} · {}",
                current.map_or_else(|| "?".to_owned(), |cycle| cycle.to_string()),
                next.id
                    .map_or_else(|| "?".to_owned(), |cycle| cycle.to_string()),
                cycle_phases(
                    next.blocks_until_prepare_phase,
                    next.blocks_until_reward_phase
                )
            )
        },
    )
}

fn protocol_story(state: &State) -> Vec<Line<'static>> {
    vec![
        detail("1 Bitcoin decision", bitcoin_decision_story(state)),
        detail("2 commitment", commitment_story(state)),
        detail("3 tenure", tenure_story(state)),
        detail("4 Stacks blocks", stacks_activity_story(state)),
        detail("next boundary", next_boundary_story(state)),
        detail("PoX schedule", pox_schedule_story(state)),
        detail(
            "data freshness",
            format!(
                "sortition {} · tenure {} · blocks {}",
                state.sources.sortitions.description(),
                state.sources.tenure.description(),
                state.sources.blocks.description()
            ),
        ),
    ]
}

fn bitcoin_decision_story(state: &State) -> String {
    let latest = latest_sortition(state);
    let height = latest
        .and_then(|sortition| sortition.burn_block_height)
        .map_or_else(|| "?".to_owned(), thousands);
    latest
        .and_then(|sortition| sortition.elected)
        .map_or_else(
            || "decision unavailable".to_owned(),
            |elected| {
                if elected {
                    let candidates = latest
                        .and_then(|sortition| sortition.mining_competition.as_ref())
                        .map_or(0, |competition| competition.participants.len());
                    format!(
                        "Bitcoin block {height} elected a new network miner from {candidates} candidate commitments"
                    )
                } else {
                    format!(
                        "Bitcoin block {height} elected no successor; the active tenure continued"
                    )
                }
            },
        )
}

fn commitment_story(state: &State) -> String {
    let latest = latest_sortition(state);
    let competition = latest.and_then(|sortition| sortition.mining_competition.as_ref());
    let Some((competition, winner)) = competition.and_then(|competition| {
        competition_winner(competition).map(|winner| (competition, winner))
    }) else {
        return "no winning commitment retained for this Bitcoin decision".to_owned();
    };
    let miner = winner
        .signing_key_hash
        .as_deref()
        .or(winner.vrf_public_key.as_deref())
        .map_or_else(|| "miner key unavailable".to_owned(), short);
    format!(
        "{miner} burned {} sats in commitment {} · block {} · {} relative weight in a {}-block sample, not a win probability · decided {}",
        thousands(winner.burn_sats),
        short(&winner.txid),
        latest
            .and_then(|sortition| sortition.committed_block_hash.as_deref())
            .map_or_else(|| "unavailable".to_owned(), short),
        participant_weight(winner, competition),
        competition.sampled_window_blocks,
        timestamp_context(latest.and_then(|sortition| sortition.burn_header_timestamp))
    )
}

fn tenure_story(state: &State) -> String {
    let active = active_sortition(state);
    let (miner, _) = active.map_or((None, ""), miner_identity);
    let tenure = state.tenure.clone().unwrap_or_default();
    format!(
        "tenure {} is led by network miner {} · elected at Bitcoin block {} · tip {} · start {} · parent {} / {} · reward cycle {}",
        tenure
            .consensus_hash
            .as_deref()
            .map_or_else(|| "unavailable".to_owned(), short),
        miner.map_or_else(|| "unavailable".to_owned(), short),
        active
            .and_then(|sortition| sortition.burn_block_height)
            .map_or_else(|| "?".to_owned(), thousands),
        tenure.tip_height.map_or_else(|| "?".to_owned(), thousands),
        tenure
            .tenure_start_block_id
            .as_deref()
            .map_or_else(|| "unavailable".to_owned(), short),
        tenure
            .parent_consensus_hash
            .as_deref()
            .map_or_else(|| "unavailable".to_owned(), short),
        tenure
            .parent_tenure_start_block_id
            .as_deref()
            .map_or_else(|| "unavailable".to_owned(), short),
        tenure
            .reward_cycle
            .map_or_else(|| "?".to_owned(), |cycle| cycle.to_string())
    )
}

fn stacks_activity_story(state: &State) -> String {
    let blocks = state
        .tenure
        .as_ref()
        .and_then(|tenure| tenure.consensus_hash.as_deref())
        .map_or_else(Vec::new, |tenure| tenure_blocks(state, tenure));
    let Some(newest) = blocks.first() else {
        return "no locally executed blocks from this tenure are loaded".to_owned();
    };
    let oldest = blocks
        .last()
        .expect("a non-empty block slice has a last item");
    format!(
        "{} locally executed · heights {}→{} · latest has {} transactions · {} tenure extensions",
        blocks.len(),
        thousands(oldest.height),
        thousands(newest.height),
        newest.transactions.len(),
        tenure_extensions(&blocks).len()
    )
}

fn next_boundary_story(state: &State) -> String {
    let next = latest_sortition(state)
        .and_then(|sortition| sortition.burn_block_height)
        .and_then(|height| height.checked_add(1))
        .map_or_else(
            || "the next Bitcoin block".to_owned(),
            |height| format!("Bitcoin block {}", thousands(height)),
        );
    format!("{next} may elect a successor; without one, this tenure and its network miner continue")
}

fn pox_schedule_story(state: &State) -> String {
    let pox = state.pox.clone().unwrap_or_default();
    pox.next_cycle.as_ref().map_or_else(
        || "PoX boundary unavailable".to_owned(),
        |next| {
            format!(
                "cycle {}→{} · {} · stacked {} · epoch {} · budget {} runtime, {} reads / {} bytes, {} writes / {} bytes",
                pox.current_cycle
                    .as_ref()
                    .and_then(|cycle| cycle.id)
                    .map_or_else(|| "?".to_owned(), |cycle| cycle.to_string()),
                next.id
                    .map_or_else(|| "?".to_owned(), |cycle| cycle.to_string()),
                cycle_phases(
                    next.blocks_until_prepare_phase,
                    next.blocks_until_reward_phase
                ),
                pox.current_cycle
                    .as_ref()
                    .and_then(|cycle| cycle.stacked_ustx)
                    .map_or_else(|| "unavailable".to_owned(), stx_amount),
                pox.current_epoch.as_deref().unwrap_or("unavailable"),
                exact_compact_limit(pox.current_budget().and_then(|budget| budget.runtime)),
                count_limit(pox.current_budget().and_then(|budget| budget.read_count)),
                exact_byte_limit(pox.current_budget().and_then(|budget| budget.read_length)),
                count_limit(pox.current_budget().and_then(|budget| budget.write_count)),
                exact_byte_limit(pox.current_budget().and_then(|budget| budget.write_length))
            )
        },
    )
}

fn draw_too_small(frame: &mut Frame, area: Rect, width: u16, height: u16) {
    let lines = vec![
        Line::from(Span::styled(
            "terminal too small",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "current {}x{} · required {width}x{height}",
            area.width, area.height
        )),
        Line::from("resize the terminal · q quit"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .block(bordered(" nano-tui ")),
        area,
    );
}

fn draw_sync_status(frame: &mut Frame, area: Rect, state: &State, node: &Node) {
    let sync = state.sync.clone().unwrap_or_default();
    let info = state.info.clone().unwrap_or_default();
    let health = health_summary(state);
    let title = format!(
        " {} — {} — local {} — chain {} ",
        info.server_version.as_deref().unwrap_or("nano-stacks"),
        node.url(),
        role_names(sync.roles),
        info.network_id
            .map_or_else(|| "?".to_owned(), |id| format!("{id:#010x}"))
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(health.state.colour()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let source_status = state.sources.sync.description();
    let (lag, lag_colour) = sync_lag(state.behind(), state.sources.sync.unavailable());
    let lines = vec![
        health_line(&health, state),
        Line::from(vec![
            label("verified locally  "),
            number(sync.executed_stacks_height, Color::Green),
            label("  executed and checked by this node"),
        ]),
        Line::from(vec![
            label("last fork choice  "),
            number(sync.selected_stacks_height, Color::Yellow),
            label("  selected from peer candidates; updates periodically"),
        ]),
        Line::from(vec![
            label("last peer report  "),
            number(sync.followed_stacks_height, Color::White),
            label("  height most recently advertised by the sync peer"),
        ]),
        Line::from(vec![
            label("sync source       "),
            plain_value(sync.selected_from_peer.as_deref(), "no peer selected"),
            label("  ·  "),
            Span::styled(
                source_status,
                Style::default().fg(if state.sources.sync.error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ),
        ]),
        Line::from(vec![
            label("peer pool         "),
            number(
                sync.fetching_from_peers
                    .as_ref()
                    .map(|peers| peers.len() as u64),
                Color::Green,
            ),
            label(" peers serving history  ·  "),
            number(sync.p2p_sessions, Color::White),
            label(" p2p sessions of "),
            number(sync.p2p_known_peers, Color::DarkGray),
            label(" known"),
        ]),
        Line::from(vec![
            label("sync lag          "),
            Span::styled(lag, Style::default().fg(lag_colour)),
            label("   bitcoin block "),
            number(info.burn_block_height, Color::Cyan),
            label("  · info "),
            Span::styled(
                state.sources.info.description(),
                Style::default().fg(if state.sources.info.error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_compact_sync_status(frame: &mut Frame, area: Rect, state: &State, node: &Node) {
    let sync = state.sync.clone().unwrap_or_default();
    let info = state.info.clone().unwrap_or_default();
    let health = health_summary(state);
    let version = info
        .server_version
        .as_deref()
        .unwrap_or("nano-stacks")
        .split(" (")
        .next()
        .unwrap_or("nano-stacks");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {version} — {} — local {} ",
            node.url(),
            role_names(sync.roles)
        ))
        .border_style(Style::default().fg(health.state.colour()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lag = match (state.sources.sync.unavailable(), state.behind()) {
        (true, _) => "lag unavailable".to_owned(),
        (false, Some(0)) => "at peer-reported tip".to_owned(),
        (false, Some(1)) => "1 block behind".to_owned(),
        (false, Some(behind)) => format!("{} blocks behind", thousands(behind)),
        (false, None) => "lag unknown".to_owned(),
    };
    let lines = vec![
        health_line(&health, state),
        Line::from(vec![
            label("node data  "),
            Span::styled(
                state.sources.sync.description(),
                Style::default().fg(if state.sources.sync.error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ),
            label(" · info "),
            Span::raw(state.sources.info.brief()),
        ]),
        Line::from(vec![
            label("verified locally  "),
            number(sync.executed_stacks_height, Color::Green),
            label(" · executed and checked by this node"),
        ]),
        Line::from(vec![
            label("last peer report  "),
            number(sync.followed_stacks_height, Color::White),
            label(" · last fork choice "),
            number(sync.selected_stacks_height, Color::Yellow),
            label(" · "),
            Span::raw(lag),
        ]),
        Line::from(vec![
            label("source     "),
            plain_value(sync.selected_from_peer.as_deref(), "none"),
        ]),
        Line::from(vec![
            label("peers      "),
            number(
                sync.fetching_from_peers
                    .as_ref()
                    .map(|peers| peers.len() as u64),
                Color::Green,
            ),
            label(" serving · "),
            number(sync.p2p_sessions, Color::White),
            label("/"),
            number(sync.p2p_known_peers, Color::DarkGray),
            label(" P2P · Bitcoin "),
            number(info.burn_block_height, Color::Cyan),
            label(" · chain "),
            Span::raw(
                info.network_id
                    .map_or_else(|| "?".to_owned(), |id| format!("{id:#010x}")),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn health_line(summary: &HealthSummary, state: &State) -> Line<'static> {
    let sealed = sealed_age(state).map_or_else(
        || "unavailable".to_owned(),
        |elapsed| format!("{} ago", duration_age(elapsed)),
    );
    Line::from(vec![
        label("health     "),
        Span::styled(
            summary.state.label(),
            Style::default()
                .fg(summary.state.colour())
                .add_modifier(Modifier::BOLD),
        ),
        label(" · "),
        Span::raw(summary.reason.clone()),
        label(" · last sealed "),
        Span::raw(sealed),
    ])
}

fn role_names(roles: Option<node::NodeRoles>) -> String {
    let Some(roles) = roles else {
        return "unavailable".to_owned();
    };
    let mut names = Vec::with_capacity(3);
    if roles.follower {
        names.push("follower");
    }
    if roles.signer {
        names.push("signer");
    }
    if roles.miner {
        names.push("miner");
    }
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    }
}

fn draw_operations(frame: &mut Frame, area: Rect, state: &mut State) {
    let lines = operations_lines(state);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let visible = usize::from(area.height.saturating_sub(2));
    let content = paragraph.line_count(area.width.saturating_sub(2));
    let max_scroll = u16::try_from(content.saturating_sub(visible)).unwrap_or(u16::MAX);
    state.operations_scroll = state.operations_scroll.min(max_scroll);
    let title = format!(
        " operations — RPC facts — scroll {}/{} ",
        state.operations_scroll, max_scroll
    );
    frame.render_widget(
        paragraph
            .block(bordered(&title))
            .scroll((state.operations_scroll, 0)),
        area,
    );
}

fn operations_lines(state: &State) -> Vec<Line<'static>> {
    let sync = state.sync.as_ref();
    let baseline = state.sync_baseline.as_ref();
    let health = health_summary(state);
    let mut lines = vec![
        detail(
            "derived health",
            format!("{} · {}", health.state.label(), health.reason),
        ),
        detail("RPC data", state.sources.sync.description()),
        detail("local roles", role_names(sync.and_then(|sync| sync.roles))),
        Line::default(),
        detail(
            "history sources",
            sync.and_then(|sync| sync.fetching_from_peers.as_ref())
                .map_or_else(|| "unavailable".to_owned(), |peers| peers.join(", ")),
        ),
        detail(
            "P2P sessions",
            current_count(sync.and_then(|sync| sync.p2p_sessions)),
        ),
        detail(
            "known P2P peers",
            current_count(sync.and_then(|sync| sync.p2p_known_peers)),
        ),
        Line::default(),
        Line::from(Span::styled(
            "current work queues",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        detail(
            "staged blocks",
            current_count(sync.and_then(|sync| sync.staged_blocks)),
        ),
        detail(
            "relay validation",
            current_count(sync.and_then(|sync| sync.relay_offered)),
        ),
        detail(
            "relay announcement",
            current_count(sync.and_then(|sync| sync.relay_announcing)),
        ),
        detail(
            "block ingestion",
            current_count(sync.and_then(|sync| sync.queued_blocks)),
        ),
        detail(
            "proposal validator",
            current_count(sync.and_then(|sync| sync.queued_proposals)),
        ),
        detail(
            "StackerDB relay",
            current_count(sync.and_then(|sync| sync.queued_stackerdb_chunks)),
        ),
        detail(
            "transaction relay",
            current_count(sync.and_then(|sync| sync.queued_transactions)),
        ),
        detail(
            "relay shed",
            counter_change(
                sync.and_then(|sync| sync.relay_dropped),
                baseline.and_then(|sync| sync.relay_dropped),
            ),
        ),
    ];
    lines.extend(observer_lines(sync, baseline));
    lines.extend(metrics_lines(state));
    lines
}

fn observer_lines(sync: Option<&SyncStatus>, baseline: Option<&SyncStatus>) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            "event observers",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    match sync.and_then(|sync| sync.event_observers.as_ref()) {
        None => lines.push(detail("configured", "unavailable".to_owned())),
        Some(observers) if observers.is_empty() => {
            lines.push(detail("configured", "0".to_owned()));
        }
        Some(observers) => {
            lines.push(detail("configured", observers.len().to_string()));
            for observer in observers {
                let opened = baseline
                    .and_then(|sync| sync.event_observers.as_ref())
                    .and_then(|observers| {
                        observers.iter().find(|opened| opened.url == observer.url)
                    });
                lines.push(detail(
                    &observer.url,
                    format!(
                        "{} · delivered {} · dropped {} · {} undelivered now",
                        if observer.reachable {
                            "reachable"
                        } else {
                            "unreachable"
                        },
                        counter_change(Some(observer.delivered), opened.map(|item| item.delivered)),
                        counter_change(Some(observer.dropped), opened.map(|item| item.dropped)),
                        thousands(observer.undelivered)
                    ),
                ));
            }
        }
    }
    lines
}

fn metrics_lines(state: &State) -> Vec<Line<'static>> {
    let status = if state.metrics_enabled {
        state.sources.metrics.description()
    } else {
        "not configured · pass --metrics-url for local diagnostics".to_owned()
    };
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            "optional metrics",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        detail("metrics data", status),
    ];
    let Some(metrics) = state.metrics.as_ref() else {
        return lines;
    };
    let opened = state.metrics_baseline.as_ref();
    lines.extend(peer_metrics_lines(metrics));
    lines.extend(counter_metrics_lines(metrics, opened));
    lines.extend(execution_metrics_lines(metrics, opened));
    lines.extend(cache_metrics_lines(metrics));
    lines
}

fn peer_metrics_lines(metrics: &node::Metrics) -> [Line<'static>; 4] {
    [
        detail("follower peers", metric_current(metrics.serving_followers)),
        detail(
            "proposal peers",
            metric_current(metrics.serving_proposal_validators),
        ),
        detail(
            "StackerDB peers",
            metric_current(metrics.serving_stackerdb_replicas),
        ),
        detail("mempool now", metric_current(metrics.mempool_transactions)),
    ]
}

fn counter_metrics_lines(
    metrics: &node::Metrics,
    opened: Option<&node::Metrics>,
) -> [Line<'static>; 10] {
    [
        detail(
            "refusals",
            metric_change(
                metrics.refusal_total(),
                opened.and_then(node::Metrics::refusal_total),
            ),
        ),
        detail(
            "  compiler gap",
            metric_change(
                metrics.refusal_compiler_gap,
                opened.and_then(|metrics| metrics.refusal_compiler_gap),
            ),
        ),
        detail(
            "  root mismatch",
            metric_change(
                metrics.refusal_root_mismatch,
                opened.and_then(|metrics| metrics.refusal_root_mismatch),
            ),
        ),
        detail(
            "  signature",
            metric_change(
                metrics.refusal_signature,
                opened.and_then(|metrics| metrics.refusal_signature),
            ),
        ),
        detail(
            "  missing context",
            metric_change(
                metrics.refusal_missing_context,
                opened.and_then(|metrics| metrics.refusal_missing_context),
            ),
        ),
        detail(
            "  other",
            metric_change(
                metrics.refusal_other,
                opened.and_then(|metrics| metrics.refusal_other),
            ),
        ),
        detail(
            "sync unanswered",
            metric_change(
                metrics.sync_rounds_unanswered,
                opened.and_then(|metrics| metrics.sync_rounds_unanswered),
            ),
        ),
        detail(
            "StackerDB unanswered",
            metric_change(
                metrics.stackerdb_rounds_unanswered,
                opened.and_then(|metrics| metrics.stackerdb_rounds_unanswered),
            ),
        ),
        detail(
            "peer failovers",
            metric_change(
                metrics.peer_failovers,
                opened.and_then(|metrics| metrics.peer_failovers),
            ),
        ),
        detail(
            "push refusals",
            metric_change(
                metrics.pushed_blocks_refused,
                opened.and_then(|metrics| metrics.pushed_blocks_refused),
            ),
        ),
    ]
}

fn execution_metrics_lines(
    metrics: &node::Metrics,
    opened: Option<&node::Metrics>,
) -> [Line<'static>; 7] {
    [
        detail(
            "last block txs",
            metric_current(metrics.last_block_transactions),
        ),
        detail("block execution", execution_average(metrics, opened)),
        detail(
            "last read count",
            metric_percent(metrics.last_block_read_count),
        ),
        detail(
            "last read bytes",
            metric_percent(metrics.last_block_read_length),
        ),
        detail(
            "last write count",
            metric_percent(metrics.last_block_write_count),
        ),
        detail(
            "last write bytes",
            metric_percent(metrics.last_block_write_length),
        ),
        detail("last runtime", metric_percent(metrics.last_block_runtime)),
    ]
}

fn cache_metrics_lines(metrics: &node::Metrics) -> [Line<'static>; 5] {
    [
        detail("cache memory", metric_bytes(metrics.cache_bytes())),
        detail("  MARF nodes", metric_bytes(metrics.marf_node_cache_bytes)),
        detail(
            "  MARF auxiliary",
            metric_bytes(metrics.marf_auxiliary_cache_bytes),
        ),
        detail(
            "  Clarity values",
            metric_bytes(metrics.clarity_value_cache_bytes),
        ),
        detail(
            "  Wasm modules",
            metric_bytes(metrics.wasm_module_cache_bytes),
        ),
    ]
}

fn current_count(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), thousands)
}

fn counter_change(current: Option<u64>, opened: Option<u64>) -> String {
    current.zip(opened).map_or_else(
        || "unavailable".to_owned(),
        |(current, opened)| {
            format!(
                "+{} since opened",
                thousands(current.saturating_sub(opened))
            )
        },
    )
}

fn metric_current(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.0}"))
}

fn metric_change(current: Option<f64>, opened: Option<f64>) -> String {
    current.zip(opened).map_or_else(
        || "unavailable".to_owned(),
        |(current, opened)| {
            if current >= opened {
                format!("+{:.0} since opened", current - opened)
            } else {
                format!("{current:.0} now · counter reset since opened")
            }
        },
    )
}

fn metric_percent(value: Option<f64>) -> String {
    value.map_or_else(
        || "unavailable".to_owned(),
        |value| format!("{:.1}% of block limit", value * 100.0),
    )
}

fn metric_bytes(value: Option<f64>) -> String {
    value.map_or_else(
        || "unavailable".to_owned(),
        |bytes| {
            const KIB: f64 = 1024.0;
            const MIB: f64 = KIB * 1024.0;
            const GIB: f64 = MIB * 1024.0;
            if bytes >= GIB {
                format!("{:.1} GiB current", bytes / GIB)
            } else if bytes >= MIB {
                format!("{:.1} MiB current", bytes / MIB)
            } else if bytes >= KIB {
                format!("{:.1} KiB current", bytes / KIB)
            } else {
                format!("{bytes:.0} B current")
            }
        },
    )
}

fn execution_average(metrics: &node::Metrics, opened: Option<&node::Metrics>) -> String {
    let Some((sum, count, opened_sum, opened_count)) = metrics
        .block_execution_seconds_sum
        .zip(metrics.block_execution_seconds_count)
        .zip(opened.and_then(|metrics| {
            metrics
                .block_execution_seconds_sum
                .zip(metrics.block_execution_seconds_count)
        }))
        .map(|((sum, count), (opened_sum, opened_count))| (sum, count, opened_sum, opened_count))
    else {
        return "unavailable".to_owned();
    };
    if sum < opened_sum || count < opened_count {
        return "counter reset since opened".to_owned();
    }
    let observed = count - opened_count;
    if observed == 0.0 {
        "no blocks observed since opened".to_owned()
    } else {
        format!(
            "{:.3}s average · {observed:.0} blocks since opened",
            (sum - opened_sum) / observed
        )
    }
}

fn draw_election(frame: &mut Frame, area: Rect, state: &mut State) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(area);
    draw_election_summary(frame, areas[0], state);
    draw_participants(frame, areas[1], state);
    draw_participant(frame, areas[2], state);
}

fn draw_standard_election(frame: &mut Frame, area: Rect, state: &mut State) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(4)])
        .split(area);
    draw_election_summary(frame, areas[0], state);
    draw_participants(frame, areas[1], state);
}

fn draw_election_summary(frame: &mut Frame, area: Rect, state: &State) {
    let latest = latest_sortition(state);
    let active = active_sortition(state);
    let competition = latest.and_then(|sortition| sortition.mining_competition.as_ref());
    let winner = competition.and_then(competition_winner);
    let (miner, miner_kind) = active.map_or((None, ""), miner_identity);
    let election = latest.and_then(|sortition| sortition.elected).map_or_else(
        || "unavailable".to_owned(),
        |elected| {
            if elected {
                "new tenure elected".to_owned()
            } else {
                "no election; active tenure continues".to_owned()
            }
        },
    );
    let participant_count = competition.map_or_else(
        || "unavailable".to_owned(),
        |competition| competition.participants.len().to_string(),
    );
    let winner_burn = winner.map_or_else(
        || "no winning commitment".to_owned(),
        |winner| {
            format!(
                "{} / {} sats · {} relative weight",
                thousands(winner.burn_sats),
                thousands(competition.map_or(0, |competition| competition.block_burn_sats)),
                participant_weight(winner, competition.expect("a winner has a competition"))
            )
        },
    );
    let sample = competition.map_or_else(
        || "unavailable".to_owned(),
        |competition| {
            format!(
                "{} burn blocks · block total {} sats · window median {} sats",
                competition.sampled_window_blocks,
                thousands(competition.block_burn_sats),
                thousands(competition.window_median_burn_sats)
            )
        },
    );
    let lines = vec![
        Line::from(vec![
            label("active tenure   "),
            value(active.and_then(|sortition| sortition.consensus_hash.as_deref())),
            label(" · elected at bitcoin "),
            number(
                active.and_then(|sortition| sortition.burn_block_height),
                Color::Cyan,
            ),
            label(" · parent "),
            value(active.and_then(|sortition| sortition.stacks_parent_ch.as_deref())),
        ]),
        Line::from(vec![
            label("network miner   "),
            value(miner),
            label(miner_kind),
        ]),
        Line::from(vec![
            label("latest decision "),
            Span::raw(election),
            label(" · bitcoin "),
            number(
                latest.and_then(|sortition| sortition.burn_block_height),
                Color::Cyan,
            ),
            label(" · "),
            Span::raw(participant_count),
            label(" candidate commitments in the latest decision"),
        ]),
        Line::from(vec![label("winner burn     "), Span::raw(winner_burn)]),
        Line::from(vec![label("sample          "), Span::raw(sample)]),
        Line::from(vec![
            label("weight meaning  "),
            Span::raw("relative share among candidate commitments; not win probability"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(bordered(" network election & active tenure ")),
        area,
    );
}

fn draw_participants(frame: &mut Frame, area: Rect, state: &mut State) {
    let Some(competition) = mining_competition(state).cloned() else {
        state.selected_participant.select(None);
        frame.render_widget(
            Paragraph::new("this node has no retained participant data for this sortition")
                .block(bordered(" election participants ")),
            area,
        );
        return;
    };
    if competition.participants.is_empty() {
        state.selected_participant.select(None);
        frame.render_widget(
            Paragraph::new("no candidate commitments were present in this Bitcoin block")
                .block(bordered(" election participants ")),
            area,
        );
        return;
    }
    let winner = competition.winner_txid.as_deref();
    let items = competition
        .participants
        .iter()
        .map(|participant| {
            let won = winner.is_some_and(|winner| same_id(&participant.txid, winner));
            let identity = participant
                .signing_key_hash
                .as_deref()
                .or(participant.vrf_public_key.as_deref())
                .map_or_else(|| "key unavailable".to_owned(), short);
            ListItem::new(Line::from(vec![
                Span::styled(
                    if won { "WIN " } else { "    " },
                    Style::default().fg(if won { Color::Green } else { Color::DarkGray }),
                ),
                Span::styled(format!("{identity:<18}"), Style::default().fg(Color::White)),
                Span::raw(format!(" {:>12} sat", thousands(participant.burn_sats))),
                label("  effective "),
                Span::raw(format!(
                    "{:>12}",
                    thousands(participant.effective_burn_sats)
                )),
                label("  "),
                Span::styled(
                    participant_weight(participant, &competition),
                    Style::default().fg(Color::Yellow),
                ),
                label(&format!(
                    "  active {}/{}",
                    participant.frequency, competition.sampled_window_blocks
                )),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items)
            .block(bordered(
                " election participants — miner key · burn · effective weight · activity ",
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▍"),
        area,
        &mut state.selected_participant,
    );
}

fn draw_participant(frame: &mut Frame, area: Rect, state: &State) {
    let Some(participant) = selected_participant(state) else {
        frame.render_widget(
            Paragraph::new("select a participant to inspect its commitment")
                .block(bordered(" participant details ")),
            area,
        );
        return;
    };
    let competition = mining_competition(state).expect("a selected participant has a competition");
    let won = competition
        .winner_txid
        .as_deref()
        .is_some_and(|winner| same_id(&participant.txid, winner));
    let lines = vec![
        field("signing key", participant.signing_key_hash.as_deref()),
        field("leader VRF", participant.vrf_public_key.as_deref()),
        field("commit txid", Some(&participant.txid)),
        field("Stacks block", Some(&participant.committed_block_hash)),
        Line::from(vec![
            label("weight       "),
            Span::raw(format!(
                "{} effective / {} raw sats · median {} · active {}/{} · {}",
                thousands(participant.effective_burn_sats),
                thousands(participant.burn_sats),
                thousands(participant.median_burn_sats),
                participant.frequency,
                competition.sampled_window_blocks,
                participant_weight(participant, competition)
            )),
        ]),
        Line::from(vec![
            label("burn block   "),
            value(
                latest_sortition(state).and_then(|sortition| sortition.burn_block_hash.as_deref()),
            ),
            label(" · sortition seed "),
            value(latest_sortition(state).and_then(|sortition| sortition.vrf_seed.as_deref())),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(bordered(if won {
            " winning participant details "
        } else {
            " participant details "
        })),
        area,
    );
}

/// The blocks this node has executed while the dashboard watched.
fn draw_blocks(frame: &mut Frame, area: Rect, state: &mut State) {
    if state.blocks.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "waiting for this node to execute a block…",
                Style::default().fg(Color::DarkGray),
            )))
            .block(bordered(&format!(
                " executed blocks — {} ",
                state.sources.blocks.brief()
            ))),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = state
        .blocks
        .iter()
        .map(|block| {
            let coinbase = block
                .transactions
                .iter()
                .any(|transaction| transaction.kind == "coinbase");
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>9}  ", block.height),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(short(&block.id), Style::default().fg(Color::White)),
                Span::styled(
                    format!("  {:>3} tx", block.transactions.len()),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("  {:>2} sigs", block.signatures),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    if coinbase { "  tenure start" } else { "" },
                    Style::default().fg(Color::Magenta),
                ),
            ]))
        })
        .collect();
    // The count is on the title because "how much is this holding?" is a fair
    // question to ask of a long-running dashboard, and the answer is bounded: this
    // keeps blocks in memory and nothing on disk.
    let title = format!(
        " executed blocks — {} held, {HISTORY} max, nothing on disk · {} ",
        state.blocks.len(),
        state.sources.blocks.brief()
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(bordered(&title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▍"),
        area,
        &mut state.selected_block,
    );
}

/// One block, opened.
fn draw_block(frame: &mut Frame, area: Rect, state: &mut State) {
    let Some(block) = selected_block(state).cloned() else {
        return;
    };
    let title = format!(" block {} / transactions ", block.height);
    let outer = bordered(&title);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .split(inner);
    let lines = vec![
        field("block", Some(&block.id)),
        field("parent", Some(&block.parent_id)),
        field("consensus", Some(&block.consensus_hash)),
        field("state root", Some(&block.state_index_root)),
        Line::from(vec![
            label("signatures   "),
            Span::styled(
                block.signatures.to_string(),
                Style::default().fg(Color::White),
            ),
            label("   timestamp "),
            Span::styled(
                timestamp_context((block.timestamp > 0).then_some(block.timestamp)),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), areas[0]);

    if block.transactions.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "this block has no transactions",
                Style::default().fg(Color::DarkGray),
            )),
            areas[1],
        );
        return;
    }
    let items: Vec<ListItem> = block
        .transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>3}  {:<9}", index, transaction.kind),
                    Style::default().fg(transaction_colour(&transaction.kind)),
                ),
                Span::styled(
                    short(&transaction.txid),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::raw(transaction.summary.clone()),
            ]))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▍"),
        areas[1],
        &mut state.selected_transaction,
    );
}

/// One transaction, opened from its block.
fn draw_transaction(frame: &mut Frame, area: Rect, state: &mut State) {
    let Some(transaction) = selected_transaction(state).cloned() else {
        state.screen = Screen::Block;
        return;
    };
    let block = selected_block(state)
        .cloned()
        .expect("selected transaction has a block");
    let block_height = block.height;
    let transaction_index = state.selected_transaction.selected().unwrap_or_default();
    let transaction_count = selected_block(state).map_or(0, |block| block.transactions.len());
    let mut lines = vec![detail("txid", transaction.txid.clone())];
    lines.extend(execution_lines(state, &block, &transaction.txid));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "signed intent",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        detail("type", transaction.kind),
        detail(
            "sender",
            transaction
                .origin
                .unwrap_or_else(|| "unavailable".to_owned()),
        ),
        detail(
            "sponsor",
            transaction.sponsor.unwrap_or_else(|| "none".to_owned()),
        ),
        detail("origin nonce", transaction.origin_nonce.to_string()),
        detail(
            "sponsor nonce",
            transaction
                .sponsor_nonce
                .map_or_else(|| "none".to_owned(), |nonce| nonce.to_string()),
        ),
        detail("fee", stx_amount(u128::from(transaction.fee))),
        detail("authorization", transaction.authorization),
        detail(
            "network",
            format!(
                "{} / chain {:#010x}",
                transaction.version, transaction.chain_id
            ),
        ),
        detail("anchor mode", transaction.anchor_mode),
        detail(
            "post conditions",
            format!(
                "{} / {} condition(s)",
                transaction.post_condition_mode, transaction.post_conditions
            ),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "payload",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ]);
    lines.extend(
        transaction
            .fields
            .into_iter()
            .flat_map(|(name, value)| detail_lines(&name, &value)),
    );

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let visible = usize::from(area.height.saturating_sub(2));
    let content = paragraph.line_count(area.width.saturating_sub(2));
    let max_scroll = u16::try_from(content.saturating_sub(visible)).unwrap_or(u16::MAX);
    state.transaction_scroll = state.transaction_scroll.min(max_scroll);
    let title = format!(
        " block {block_height} / transaction {}/{} — scroll {}/{} ",
        transaction_index + 1,
        transaction_count,
        state.transaction_scroll,
        max_scroll
    );
    frame.render_widget(
        paragraph
            .block(bordered(&title))
            .scroll((state.transaction_scroll, 0)),
        area,
    );
}

fn execution_lines(state: &State, block: &node::Block, txid: &str) -> Vec<Line<'static>> {
    let Some(receipt_block) = state
        .receipt_blocks
        .iter()
        .find(|receipt| same_id(&receipt.index_block_hash, &block.id))
    else {
        return vec![
            execution_heading(),
            detail("outcome", receipt_unavailable(state, block.height)),
        ];
    };
    let Some(view) = receipt_block.transaction(txid) else {
        return vec![
            execution_heading(),
            detail(
                "outcome",
                "unavailable — receipt did not contain this transaction ID".to_owned(),
            ),
        ];
    };
    let outcome = view.outcome;
    let budget = state.pox.as_ref().and_then(node::Pox::current_budget);
    let mut lines = vec![
        execution_heading(),
        detail("outcome", outcome.status().to_owned()),
        detail("result", outcome.result()),
    ];
    if let Some(error) = outcome.vm_error.as_ref() {
        lines.extend(detail_lines("VM error", error));
    }
    lines.push(Line::from(Span::styled(
        "charged cost / current block limit",
        Style::default().fg(Color::Yellow),
    )));
    let cost = outcome.execution_cost;
    lines.extend([
        cost_line(
            "runtime",
            cost.runtime,
            budget.and_then(|limit| limit.runtime),
        ),
        cost_line(
            "read count",
            cost.read_count,
            budget.and_then(|limit| limit.read_count),
        ),
        cost_line(
            "read length",
            cost.read_length,
            budget.and_then(|limit| limit.read_length),
        ),
        cost_line(
            "write count",
            cost.write_count,
            budget.and_then(|limit| limit.write_count),
        ),
        cost_line(
            "write length",
            cost.write_length,
            budget.and_then(|limit| limit.write_length),
        ),
    ]);
    lines.push(Line::from(Span::styled(
        "ordered events",
        Style::default().fg(Color::Yellow),
    )));
    if view.events.is_empty() {
        lines.push(detail("events", "none (known empty list)".to_owned()));
    } else {
        lines.extend(
            view.events
                .into_iter()
                .enumerate()
                .map(|(position, event)| {
                    detail(
                        &format!("event {}", event.index().unwrap_or(position as u64)),
                        event.description(),
                    )
                }),
        );
    }
    lines
}

fn execution_heading() -> Line<'static> {
    Line::from(Span::styled(
        "execution outcome",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn receipt_unavailable(state: &State, height: u64) -> String {
    let stream = &state.receipt_stream;
    if let Some(gap) = stream.gap_at(height) {
        return format!(
            "unavailable — live stream missed {} event(s) in this interval",
            thousands(gap.missing)
        );
    }
    if stream.unsequenced {
        return "unavailable — node sent unsequenced events, so coverage cannot be proven"
            .to_owned();
    }
    if stream
        .started_after_height
        .is_some_and(|started| height <= started)
        || stream.first_height.is_some_and(|first| height < first)
    {
        return "unavailable — block predates this live receipt stream".to_owned();
    }
    if stream.latest_height.is_some_and(|latest| height <= latest) {
        return "unavailable — no retained new_block event for this block".to_owned();
    }
    if !stream.connected {
        return stream.error.as_ref().map_or_else(
            || "unavailable — receipt stream has not connected".to_owned(),
            |error| format!("unavailable — receipt stream disconnected: {error}"),
        );
    }
    if stream.connections > 1 {
        "waiting for a live receipt after reconnect".to_owned()
    } else {
        "waiting for this block's live receipt".to_owned()
    }
}

fn cost_line(name: &str, used: u64, limit: Option<u64>) -> Line<'static> {
    let value = limit.map_or_else(
        || format!("{} exact · current limit unavailable", thousands(used)),
        |limit| {
            let share = if limit == 0 {
                "undefined".to_owned()
            } else {
                exact_percent(used, limit)
            };
            format!(
                "{} / {} exact · {share} of current limit",
                thousands(used),
                thousands(limit)
            )
        },
    );
    detail(name, value)
}

fn exact_percent(value: u64, limit: u64) -> String {
    const DECIMALS: u128 = 1_000_000;
    let scaled = u128::from(value) * 100 * DECIMALS / u128::from(limit);
    format!("{}.{:06}%", scaled / DECIMALS, scaled % DECIMALS)
}

fn transaction_colour(kind: &str) -> Color {
    match kind {
        "coinbase" => Color::Magenta,
        "tenure" => Color::Yellow,
        "deploy" => Color::Cyan,
        _ => Color::White,
    }
}

fn draw_help(frame: &mut Frame, state: &mut State) {
    let area = frame.area();
    let paragraph = Paragraph::new(help_lines(state.screen)).wrap(Wrap { trim: false });
    let visible = usize::from(area.height.saturating_sub(2));
    let content = paragraph.line_count(area.width.saturating_sub(2));
    let max_scroll = u16::try_from(content.saturating_sub(visible)).unwrap_or(u16::MAX);
    state.help_scroll = state.help_scroll.min(max_scroll);
    let title = format!(
        " help — {} — scroll {}/{} ",
        state.screen.name(),
        state.help_scroll,
        max_scroll
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        paragraph
            .block(bordered(&title))
            .scroll((state.help_scroll, 0)),
        area,
    );
}

fn help_lines(screen: Screen) -> Vec<Line<'static>> {
    let (meaning, relevance, provenance, controls) = view_help(screen);
    let mut lines = vec![
        help_heading("this view"),
        detail("meaning", meaning.to_owned()),
        detail("why it matters", relevance.to_owned()),
        detail("data provenance", provenance.to_owned()),
        detail("local controls", controls.to_owned()),
        Line::default(),
        help_heading("shared controls"),
        detail(
            "1 / 2 / 3 / 4",
            "overview / activity / election / operations".to_owned(),
        ),
        detail("r", "request a fresh sample now".to_owned()),
        detail("?", "open or close this help".to_owned()),
        detail(
            "q / esc / ←",
            "close help, go back, or quit from a primary view".to_owned(),
        ),
        detail(
            "↑↓ / j k",
            "select or scroll; page and home/end move farther".to_owned(),
        ),
        Line::default(),
        help_heading("protocol glossary"),
    ];
    lines.extend(glossary_lines());
    lines
}

const fn view_help(screen: Screen) -> (&'static str, &'static str, &'static str, &'static str) {
    match screen {
        Screen::Overview => (
            "The live protocol story from the current Bitcoin decision through this node's Stacks execution and the next PoX boundary.",
            "Confirms that the node is following, selecting, and executing a coherent chain instead of merely hearing about a tip.",
            "/nano/sync_status, /v3/sortitions/latest_and_last, /v3/tenures/info, /v3/blocks, and /v2/pox. Health and freshness are explicitly derived.",
            "1–4 change primary view; r refreshes immediately; q exits.",
        ),
        Screen::Activity => (
            "A bounded history of Stacks blocks this node has executed, newest first.",
            "Lets an operator confirm continued block production and lets a reader open the exact transactions that changed state.",
            "The executed tip from /nano/sync_status is loaded through /v3/blocks/:id and decoded locally with nano-codec.",
            "↑/↓, j/k, page, and home/end select; enter or → opens a block; 1–4 change primary view.",
        ),
        Screen::Block => (
            "One locally decoded Stacks block and its ordered transactions.",
            "Connects chain progress to a precise parent, consensus decision, state root, and set of transaction intents.",
            "/v3/blocks/:id from the configured node, decoded locally with nano-codec. Hashes are exact RPC data.",
            "↑/↓ selects a transaction; enter or → opens it; esc or ← returns to activity; 1–4 change primary view.",
        ),
        Screen::Transaction => (
            "The signed intent and live execution outcome for one transaction in the selected block.",
            "Separates what was requested from what committed, including result, exact cost and ordered STX/FT/NFT/contract events.",
            "Intent comes from /v3/blocks/:id and is decoded with nano-codec. Live outcomes come from the existing sequenced /events new_block stream; gaps stay unavailable.",
            "↑/↓, j/k, page, and home/end scroll; esc or ← returns to the block; 1–4 change primary view.",
        ),
        Screen::Election => (
            "The Bitcoin-anchored miner election for the active tenure and the node-reported candidate set.",
            "Explains why one network miner may produce the next Stacks blocks and exposes the evidence behind that choice.",
            "/v3/sortitions/latest_and_last (falling back to /v3/sortitions). Relative weight is derived from reported effective burn.",
            "↑/↓, j/k, page, and home/end select a participant; esc or ← returns to overview; 1–4 change primary view.",
        ),
        Screen::Operations => (
            "Node roles, peer counts, work queues, counters, process metrics, and an evidence-based health summary.",
            "Separates slow chain progress from peer, queue, relay, signer, miner, or process pressure when diagnosing a node.",
            "/nano/sync_status plus the optional Prometheus --metrics-url. Deltas compare the current session with its first sample.",
            "↑/↓, j/k, page, and home/end scroll; esc or ← returns to overview; r refreshes; 1–4 change primary view.",
        ),
    }
}

fn glossary_lines() -> [Line<'static>; 11] {
    [
        detail(
            "burn block",
            "A Bitcoin block observed by Stacks; it is the clock and decision boundary for miner elections.".to_owned(),
        ),
        detail(
            "commitment",
            "A miner's Bitcoin-chain commitment used as evidence in the next Stacks miner election.".to_owned(),
        ),
        detail(
            "election / sortition",
            "The Bitcoin-anchored process that selects the network miner for a Stacks tenure.".to_owned(),
        ),
        detail(
            "tenure",
            "The interval in which one elected network miner may produce a sequence of Stacks blocks.".to_owned(),
        ),
        detail(
            "extension",
            "Continuation of the current tenure when its miner remains responsible for producing blocks.".to_owned(),
        ),
        detail(
            "fork choice",
            "The rule this node uses to select one chain when it knows competing Stacks histories.".to_owned(),
        ),
        detail(
            "signer",
            "A member of the reward-set committee that validates and signs Nakamoto blocks.".to_owned(),
        ),
        detail(
            "PoX phase",
            "A Proof of Transfer cycle interval, including prepare and reward phases anchored to Bitcoin.".to_owned(),
        ),
        detail(
            "state root",
            "A cryptographic digest of the Stacks state after executing a block.".to_owned(),
        ),
        detail("uSTX", "One millionth of one STX, the exact base unit.".to_owned()),
        detail(
            "relative weight",
            "A candidate's share of reported effective burn in this sample; it is context, not a win probability.".to_owned(),
        ),
    ]
}

fn help_heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn draw_keys(frame: &mut Frame, area: Rect, state: &State) {
    let keys = match state.screen {
        Screen::Overview if state.standard_layout => {
            "1 overview   2 activity   3 election   4 operations   r refresh   ? help   q quit"
        }
        Screen::Overview => {
            "1 overview   2 activity   3 election   4 operations   r refresh   ? help   q quit"
        }
        Screen::Activity => {
            "1 overview   2 activity   3 election   4 operations   enter open   ↑/↓ select   ? help"
        }
        Screen::Block => {
            "1–4 views   enter open transaction   ↑/↓ select   esc/← back   r refresh   ? help"
        }
        Screen::Transaction => {
            "1–4 views   ↑/↓ scroll   pgup/pgdn page   home/end edges   esc/← back   ? help"
        }
        Screen::Election => {
            "1 overview   2 activity   3 election   4 operations   ↑/↓ participant   ? help"
        }
        Screen::Operations => {
            "1 overview   2 activity   3 election   4 operations   ↑/↓ scroll   ? help"
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(Color::DarkGray)))
            .alignment(Alignment::Center),
        area,
    );
}

fn bordered(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title.to_owned())
}

fn field<'a>(name: &'a str, value: Option<&'a str>) -> Line<'a> {
    Line::from(vec![label(&format!("{name:<13}")), self::value(value)])
}

fn detail(name: &str, value: String) -> Line<'static> {
    Line::from(vec![label(&format!("{name:<18}  ")), Span::raw(value)])
}

fn detail_lines(name: &str, value: &str) -> Vec<Line<'static>> {
    value
        .split('\n')
        .enumerate()
        .map(|(index, value)| detail(if index == 0 { name } else { "" }, value.to_owned()))
        .collect()
}

fn label(text: &str) -> Span<'static> {
    Span::styled(text.to_owned(), Style::default().fg(Color::DarkGray))
}

fn plain_value(text: Option<&str>, missing: &str) -> Span<'static> {
    text.map_or_else(
        || Span::styled(missing.to_owned(), Style::default().fg(Color::Red)),
        |text| Span::styled(text.to_owned(), Style::default().fg(Color::White)),
    )
}

fn sync_lag(behind: Option<u64>, sync_unavailable: bool) -> (String, Color) {
    if sync_unavailable {
        return (
            "unknown while sync status is unavailable".to_owned(),
            Color::Red,
        );
    }
    match behind {
        Some(0) => (
            "caught up with the last peer report".to_owned(),
            Color::Green,
        ),
        Some(1) => (
            "1 verified block behind the last peer report".to_owned(),
            Color::Yellow,
        ),
        Some(blocks) => (
            format!("{blocks} verified blocks behind the last peer report"),
            Color::Red,
        ),
        None => ("waiting for a peer comparison".to_owned(), Color::DarkGray),
    }
}

fn cycle_phases(prepare: Option<i64>, reward: Option<i64>) -> String {
    match (prepare, reward) {
        (Some(prepare), Some(reward)) => format!(
            "prepare {} · reward {} burn blocks",
            phase_distance(prepare),
            phase_distance(reward)
        ),
        (Some(prepare), None) => {
            format!("prepare {} · reward unavailable", phase_distance(prepare))
        }
        (None, Some(reward)) => format!("prepare unavailable · reward {}", phase_distance(reward)),
        (None, None) => "timing unavailable".to_owned(),
    }
}

fn phase_distance(blocks: i64) -> String {
    match blocks {
        0 => "now".to_owned(),
        blocks if blocks > 0 => format!("+{blocks}"),
        blocks => format!("{} ago", blocks.unsigned_abs()),
    }
}

fn tenure_blocks<'a>(state: &'a State, tenure: &str) -> Vec<&'a node::Block> {
    state
        .blocks
        .iter()
        .filter(|block| same_id(&block.consensus_hash, tenure))
        .collect()
}

fn tenure_extensions<'a>(blocks: &[&'a node::Block]) -> Vec<(u64, &'a node::TenureChange)> {
    blocks
        .iter()
        .flat_map(|block| {
            block.transactions.iter().filter_map(|transaction| {
                transaction
                    .tenure_change
                    .as_ref()
                    .filter(|change| change.is_extension)
                    .map(|change| (block.height, change))
            })
        })
        .collect()
}

fn same_id(left: &str, right: &str) -> bool {
    left.trim_start_matches("0x")
        .eq_ignore_ascii_case(right.trim_start_matches("0x"))
}

fn relative_time(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "time unavailable".to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(timestamp, |duration| duration.as_secs());
    if timestamp > now {
        return format!("{}s from now", timestamp - now);
    }
    let elapsed = now - timestamp;
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

fn timestamp_context(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "time unavailable".to_owned();
    };
    let absolute = i64::try_from(timestamp)
        .ok()
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .map_or_else(
            || format!("Unix {timestamp}"),
            |timestamp| timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        );
    format!("{absolute} · {}", relative_time(Some(timestamp)))
}

fn stx_amount(amount: u128) -> String {
    let whole = thousands_u128(amount / 1_000_000);
    let remainder = amount % 1_000_000;
    let stx = if remainder == 0 {
        whole
    } else {
        format!("{whole}.{remainder:06}")
    };
    format!("{stx} STX · {} uSTX exact", thousands_u128(amount))
}

fn compact_limit(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    for (divisor, suffix) in [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "k")] {
        if value >= divisor && value.is_multiple_of(divisor) {
            return format!("{}{suffix}", value / divisor);
        }
    }
    thousands(value)
}

fn exact_compact_limit(value: Option<u64>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |value| {
            format!(
                "{} ({} exact)",
                compact_limit(Some(value)),
                thousands(value)
            )
        },
    )
}

fn count_limit(value: Option<u64>) -> String {
    value.map_or_else(|| "—".to_owned(), thousands)
}

fn byte_limit(value: Option<u64>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |value| {
            if value.is_multiple_of(1_000_000) {
                format!("{} MB", value / 1_000_000)
            } else {
                format!("{} bytes", thousands(value))
            }
        },
    )
}

fn exact_byte_limit(value: Option<u64>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |value| {
            format!(
                "{} ({} bytes exact)",
                byte_limit(Some(value)),
                thousands(value)
            )
        },
    )
}

/// A field the node could not answer says so, rather than rendering as empty.
fn value(text: Option<&str>) -> Span<'static> {
    text.map_or_else(
        || Span::styled("unavailable", Style::default().fg(Color::Red)),
        |text| Span::styled(short(text), Style::default().fg(Color::White)),
    )
}

/// Hashes are 64 characters and a terminal is not, so they are shown by both ends:
/// enough to recognise one and to tell two apart, which is what a hash is read for.
fn short(text: &str) -> String {
    let text = text.trim_start_matches("0x");
    if text.len() <= 20 {
        return text.to_owned();
    }
    format!("{}…{}", &text[..10], &text[text.len() - 6..])
}

fn number(value: Option<u64>, colour: Color) -> Span<'static> {
    value.map_or_else(
        || Span::styled("—", Style::default().fg(Color::Red)),
        |value| {
            Span::styled(
                thousands(value),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            )
        },
    )
}

/// Mainnet heights are eight digits and are read as magnitudes, not as strings.
fn thousands(value: u64) -> String {
    grouped_digits(&value.to_string())
}

fn grouped_digits(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The same, for an amount rather than a height.
fn thousands_u128(value: u128) -> String {
    grouped_digits(&value.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        Action, BlockUpdate, HISTORY, Health, PollUpdate, Poller, STALL_AFTER, Screen, Source,
        State, draw, exact_byte_limit, exact_compact_limit, handle_key, health_summary, node,
        pox_schedule_story, receipt_unavailable, receipts, short, stx_amount, thousands,
        thousands_u128, timestamp_context,
    };

    #[test]
    fn a_height_is_read_as_a_magnitude() {
        assert_eq!(thousands(8_716_524), "8,716,524");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }

    #[test]
    fn human_units_retain_exact_protocol_values() {
        assert_eq!(stx_amount(1_000_000), "1 STX · 1,000,000 uSTX exact");
        assert_eq!(stx_amount(2), "0.000002 STX · 2 uSTX exact");
        assert_eq!(
            thousands_u128(u128::MAX),
            "340,282,366,920,938,463,463,374,607,431,768,211,455"
        );

        let timestamp = timestamp_context(Some(1_700_000_000));
        assert!(timestamp.starts_with("2023-11-14 22:13:20 UTC · "));
        assert!(timestamp.ends_with("ago"));
        assert_eq!(
            exact_compact_limit(Some(5_000_000_000)),
            "5B (5,000,000,000 exact)"
        );
        assert_eq!(
            exact_byte_limit(Some(200_000_000)),
            "200 MB (200,000,000 bytes exact)"
        );

        let mut state = dashboard_state();
        let pox = pox_schedule_story(&state);
        assert!(pox.contains("5B (5,000,000,000 exact)"));
        assert!(pox.contains("200 MB (200,000,000 bytes exact)"));
        let overview = render_at(&mut state, 160, 32);
        assert!(overview.contains("1 STX · 1,000,000 uSTX exact"));
        assert!(overview.contains("2023-11-14 22:13:20 UTC"));
    }

    /// Both ends, because a hash is read to recognise one and to tell two apart.
    #[test]
    fn a_hash_is_shown_by_both_ends() {
        let hash = "c7e968363059b410b8584e3fe238c7cbb04dfb41eefc80c8c5a6d7150f041e81";
        assert_eq!(short(hash), "c7e9683630…041e81");
        assert_eq!(short(&format!("0x{hash}")), "c7e9683630…041e81");
        // Short enough to show whole is shown whole rather than padded with an
        // ellipsis that hides nothing.
        assert_eq!(short("a06c505c"), "a06c505c");
    }

    #[test]
    fn explorer_navigation_keeps_each_selection_level() {
        let mut state = explorer_state();

        assert_eq!(handle_key(&mut state, KeyCode::Enter), Action::None);
        assert_eq!(state.screen, Screen::Block);
        assert_eq!(state.selected_block.selected(), Some(0));
        assert_eq!(state.selected_transaction.selected(), Some(0));

        handle_key(&mut state, KeyCode::Enter);
        assert_eq!(state.screen, Screen::Transaction);
        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.transaction_scroll, 1);

        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.screen, Screen::Block);
        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.screen, Screen::Activity);
        assert_eq!(handle_key(&mut state, KeyCode::Char('q')), Action::Quit);
    }

    #[test]
    fn number_keys_select_the_four_primary_views_from_anywhere() {
        let mut state = dashboard_state();

        handle_key(&mut state, KeyCode::Char('2'));
        assert_eq!(state.screen, Screen::Activity);
        handle_key(&mut state, KeyCode::Char('3'));
        assert_eq!(state.screen, Screen::Election);
        handle_key(&mut state, KeyCode::Char('4'));
        assert_eq!(state.screen, Screen::Operations);
        handle_key(&mut state, KeyCode::Char('1'));
        assert_eq!(state.screen, Screen::Overview);

        state.screen = Screen::Transaction;
        handle_key(&mut state, KeyCode::Char('3'));
        assert_eq!(state.screen, Screen::Election);
    }

    #[test]
    fn contextual_help_explains_every_view_and_its_provenance() {
        for screen in [
            Screen::Overview,
            Screen::Activity,
            Screen::Block,
            Screen::Transaction,
            Screen::Election,
            Screen::Operations,
        ] {
            let mut state = dashboard_state();
            state.screen = screen;

            assert_eq!(handle_key(&mut state, KeyCode::Char('?')), Action::None);
            let top = render_at(&mut state, 80, 24);
            assert!(top.contains(&format!("help — {}", screen.name())));
            assert!(top.contains("meaning"));
            assert!(top.contains("why it matters"));
            assert!(top.contains("data provenance"));
            assert!(top.contains("local controls"));

            handle_key(&mut state, KeyCode::End);
            let bottom = render_at(&mut state, 80, 24);
            assert!(bottom.contains("state root"));
            assert!(bottom.contains("uSTX"));
            assert!(bottom.contains("relative weight"));

            assert_eq!(handle_key(&mut state, KeyCode::Char('q')), Action::None);
            assert!(!state.help);
        }
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_value_and_marks_it_stale() {
        let mut state = State::default();
        state.apply_poll(PollUpdate::Sync(Ok(node::SyncStatus {
            executed_stacks_height: Some(42),
            ..node::SyncStatus::default()
        })));
        state.apply_poll(PollUpdate::Started(Source::Sync));
        state.apply_poll(PollUpdate::Sync(Err("sync route timed out".to_owned())));

        assert_eq!(
            state
                .sync
                .as_ref()
                .and_then(|sync| sync.executed_stacks_height),
            Some(42)
        );
        let freshness = state.sources.sync.description();
        assert!(freshness.contains("stale 0s ago"));
        assert!(freshness.contains("sync route timed out"));
    }

    #[test]
    fn one_live_route_prevents_a_partial_failure_from_becoming_unreachable() {
        let mut state = State::default();
        for source in [
            Source::Sync,
            Source::Info,
            Source::Pox,
            Source::Tenure,
            Source::Sortitions,
        ] {
            state.apply_poll(PollUpdate::Started(source));
        }
        state.apply_poll(PollUpdate::Sync(Err("sync failed".to_owned())));
        state.apply_poll(PollUpdate::Info(Err("info failed".to_owned())));
        state.apply_poll(PollUpdate::Pox(Err("pox failed".to_owned())));
        state.apply_poll(PollUpdate::Tenure(Err("tenure failed".to_owned())));
        state.apply_poll(PollUpdate::Sortitions(Err("sortitions failed".to_owned())));
        assert!(state.sources.unreachable());
        assert_eq!(health_summary(&state).state, Health::Unreachable);

        state.apply_poll(PollUpdate::Info(Ok(node::NodeInfo::default())));
        assert!(!state.sources.unreachable());
        assert!(state.sources.sync.unavailable());
    }

    #[test]
    fn missing_optional_metrics_do_not_degrade_a_responsive_rpc() {
        let mut state = State::default();
        state.apply_poll(PollUpdate::Started(Source::Metrics));
        state.apply_poll(PollUpdate::Metrics(Err("connection refused".to_owned())));
        state.apply_poll(PollUpdate::Info(Ok(node::NodeInfo::default())));

        assert!(!state.sources.unreachable());
        assert!(!state.sources.degraded());
        assert!(
            state
                .sources
                .metrics
                .description()
                .contains("connection refused")
        );
        assert_ne!(health_summary(&state).state, Health::Degraded);
    }

    #[test]
    fn health_distinguishes_healthy_catch_up_and_starting() {
        let mut state = dashboard_state();
        assert_eq!(health_summary(&state).state, Health::Syncing);

        state.sync.as_mut().expect("sync fixture").blocks_behind = Some(0);
        assert_eq!(health_summary(&state).state, Health::Healthy);

        state
            .sync
            .as_mut()
            .expect("sync fixture")
            .executed_stacks_height = None;
        assert_eq!(health_summary(&state).state, Health::Starting);
    }

    #[test]
    fn a_growing_queue_and_stationary_height_are_stalled() {
        let mut state = dashboard_state();
        state.progress.height = Some(8_716_524);
        state.progress.changed = Some(
            Instant::now()
                .checked_sub(STALL_AFTER + Duration::from_secs(1))
                .expect("test instant is old enough"),
        );
        state.sync.as_mut().expect("sync fixture").staged_blocks = Some(4);
        let mut baseline = state.sync.clone().expect("sync fixture");
        baseline.staged_blocks = Some(1);
        state.sync_baseline = Some(baseline);

        let health = health_summary(&state);
        assert_eq!(health.state, Health::Stalled);
        assert!(health.reason.contains("staged block queue grew by 3"));
    }

    #[test]
    fn new_refusals_and_missing_role_peers_name_the_subsystem() {
        let mut state = dashboard_state();
        state.metrics_baseline = Some(node::Metrics {
            refusal_signature: Some(2.0),
            ..node::Metrics::default()
        });
        state.metrics = Some(node::Metrics {
            refusal_signature: Some(3.0),
            serving_followers: Some(1.0),
            ..node::Metrics::default()
        });

        let refusal = health_summary(&state);
        assert_eq!(refusal.state, Health::Degraded);
        assert!(refusal.reason.contains("signature refusal"));

        state.metrics_baseline = Some(node::Metrics::default());
        state.metrics = Some(node::Metrics {
            serving_followers: Some(0.0),
            ..node::Metrics::default()
        });
        let peer = health_summary(&state);
        assert_eq!(peer.state, Health::Degraded);
        assert_eq!(peer.reason, "follower has 0 serving peers");
    }

    #[test]
    fn an_unreachable_event_observer_degrades_delivery_health() {
        let mut state = dashboard_state();
        state.sync.as_mut().expect("sync fixture").event_observers =
            Some(vec![node::ObserverStatus {
                url: "http://observer.example/events".to_owned(),
                delivered: 0,
                dropped: 0,
                undelivered: 0,
                reachable: false,
            }]);

        let health = health_summary(&state);
        assert_eq!(health.state, Health::Degraded);
        assert!(health.reason.contains("observer.example"));
        assert!(health.reason.contains("unreachable"));
    }

    #[test]
    fn incremental_backfill_keeps_newest_first_and_the_cursor_on_its_block() {
        let mut state = State::default();
        state.blocks.push(block("old", "older", 40));
        state.selected_block.select(Some(0));
        state.block_generation = 7;
        state.sources.blocks.started();

        state.apply_block(BlockUpdate::Block {
            generation: 7,
            block: block("tip", "parent", 42),
        });
        state.apply_block(BlockUpdate::Block {
            generation: 7,
            block: block("parent", "old", 41),
        });
        state.apply_block(BlockUpdate::Finished(7));

        assert_eq!(
            state
                .blocks
                .iter()
                .map(|block| block.id.as_str())
                .collect::<Vec<_>>(),
            ["tip", "parent", "old"]
        );
        assert_eq!(state.selected_block.selected(), Some(2));
        assert!(state.sources.blocks.error.is_none());
        assert!(!state.sources.blocks.pending);
    }

    #[test]
    fn polling_announces_loading_before_waiting_for_the_node() {
        let poller = Poller::start(node::Node::new("http://127.0.0.1:9"));
        let update = poller
            .updates
            .recv_timeout(Duration::from_millis(200))
            .expect("loading update before the request");
        assert!(matches!(update, PollUpdate::Started(Source::Sync)));
    }

    #[test]
    fn transaction_page_renders_the_call() {
        let mut state = explorer_state();
        state.screen = Screen::Transaction;
        state.selected_block.select(Some(0));
        state.selected_transaction.select(Some(0));
        let rendered = render(&mut state);

        assert!(rendered.contains("SP123.contract"));
        assert!(rendered.contains("function"));
        assert!(rendered.contains("argument 0"));
        assert!(rendered.contains("u42"));
        assert!(rendered.contains("0.000002 STX · 2 uSTX exact"));
        assert!(rendered.contains("execution outcome"));
        assert!(rendered.contains("receipt stream has not connected"));
        assert!(rendered.contains("signed intent"));
    }

    #[test]
    fn a_live_receipt_joins_by_block_and_transaction_and_renders_exact_costs() {
        let mut state = explorer_state();
        state.pox = Some(pox());
        state.screen = Screen::Transaction;
        state.selected_block.select(Some(0));
        state.selected_transaction.select(Some(0));
        state.apply_receipt(receipts::Update::Connected);
        state.apply_receipt(receipts::Update::Event(receipts::StreamEvent {
            sequence: Some(7),
            kind: "new_block".to_owned(),
            data: receipt_payload("block", 42, "transaction", true),
        }));

        let rendered = render(&mut state);
        assert!(rendered.contains("success · committed"));
        assert!(rendered.contains("result"));
        assert!(rendered.contains("true"));
        assert!(rendered.contains("5 / 5,000,000,000 exact"));
        assert!(rendered.contains("0.000000% of current limit"));
        assert!(rendered.contains("event 0"));
        assert!(rendered.contains("transfer 9 uSTX from A to B"));
        assert!(rendered.contains("signed intent"));
    }

    #[test]
    fn an_empty_event_list_is_not_an_unavailable_outcome() {
        let mut state = explorer_state();
        state.screen = Screen::Transaction;
        state.selected_block.select(Some(0));
        state.selected_transaction.select(Some(0));
        state.apply_receipt(receipts::Update::Connected);
        state.apply_receipt(receipts::Update::Event(receipts::StreamEvent {
            sequence: Some(1),
            kind: "new_block".to_owned(),
            data: receipt_payload("block", 42, "transaction", false),
        }));

        let rendered = render(&mut state);
        assert!(rendered.contains("none (known empty list)"));
        assert!(!rendered.contains("outcome unavailable"));
    }

    #[test]
    fn a_reconnected_sequence_gap_is_explicit_and_keeps_decoded_blocks() {
        let mut state = explorer_state();
        state.blocks.insert(0, block("missing", "block", 44));
        state.apply_receipt(receipts::Update::Connected);
        state.apply_receipt(receipts::Update::Event(receipts::StreamEvent {
            sequence: Some(10),
            kind: "new_block".to_owned(),
            data: receipt_payload("earlier", 43, "other", false),
        }));
        state.apply_receipt(receipts::Update::Disconnected(
            "connection reset".to_owned(),
        ));
        state.apply_receipt(receipts::Update::Connected);
        state.apply_receipt(receipts::Update::Event(receipts::StreamEvent {
            sequence: Some(12),
            kind: "new_block".to_owned(),
            data: receipt_payload("later", 45, "other", false),
        }));

        assert_eq!(state.blocks.len(), 2, "stream loss does not discard blocks");
        assert_eq!(state.receipt_stream.connections, 2);
        assert!(receipt_unavailable(&state, 44).contains("missed 1 event"));
    }

    #[test]
    fn receipt_history_is_bounded_like_block_history() {
        let mut state = State::default();
        state.apply_receipt(receipts::Update::Connected);
        for height in 1..=HISTORY + 1 {
            state.apply_receipt(receipts::Update::Event(receipts::StreamEvent {
                sequence: Some(u64::try_from(height).expect("small test height")),
                kind: "new_block".to_owned(),
                data: receipt_payload(
                    &format!("block-{height}"),
                    u64::try_from(height).expect("small test height"),
                    "transaction",
                    false,
                ),
            }));
        }

        assert_eq!(state.receipt_blocks.len(), HISTORY);
        assert_eq!(
            state.receipt_blocks.first().map(|block| block.block_height),
            Some(u64::try_from(HISTORY + 1).expect("small history"))
        );
        assert_eq!(
            state.receipt_blocks.last().map(|block| block.block_height),
            Some(2)
        );
    }

    #[test]
    fn dashboard_explains_sync_state_and_shows_the_whole_peer() {
        let mut state = dashboard_state();
        let overview = render(&mut state);

        assert!(overview.contains("verified locally"));
        assert!(overview.contains("executed and checked by this node"));
        assert!(overview.contains("last fork choice"));
        assert!(overview.contains("last peer report"));
        assert!(overview.contains("http://192.0.2.123:20443"));
        assert!(overview.contains("2 blocks behind"));
        assert!(overview.contains("local follower+signer"));
        assert!(overview.contains("SYNCING"));
        assert!(overview.contains("last sealed"));
        assert!(overview.contains("chain 0x00000001"));
        assert!(overview.contains("1 Bitcoin decision"));
        assert!(overview.contains("Bitcoin block 960,240: new network miner elected"));
        assert!(overview.contains("2 commitment"));
        assert!(overview.contains("60,000 sats"));
        assert!(overview.contains("probability"));
        assert!(overview.contains("3 tenure"));
        assert!(overview.contains("4 Stacks blocks"));
        assert!(overview.contains("1 locally executed"));
        assert!(overview.contains("extensions"));
        assert!(overview.contains("next boundary"));
        assert!(overview.contains("Bitcoin block 960,241"));
        assert!(overview.contains("prepare +10 · reward +110 burn blocks"));

        handle_key(&mut state, KeyCode::Char('3'));
        let election = render(&mut state);
        assert!(election.contains("network election & active tenure"));
        assert!(election.contains("2 candidate commitments"));
        assert!(
            election.contains("relative share among candidate commitments; not win probability")
        );
    }

    #[test]
    fn operations_distinguishes_current_zero_unavailable_and_session_counters() {
        let mut state = dashboard_state();
        let sync = state.sync.as_mut().expect("sync fixture");
        sync.fetching_from_peers = Some(vec!["http://history.example:20443".to_owned()]);
        sync.p2p_sessions = Some(4);
        sync.p2p_known_peers = Some(9);
        sync.staged_blocks = Some(0);
        sync.relay_offered = Some(2);
        sync.relay_announcing = Some(3);
        sync.relay_dropped = Some(5);
        sync.queued_blocks = None;
        sync.queued_proposals = Some(0);
        sync.queued_stackerdb_chunks = Some(1);
        sync.queued_transactions = Some(0);
        sync.event_observers = Some(vec![node::ObserverStatus {
            url: "http://observer.example/events".to_owned(),
            delivered: 12,
            dropped: 2,
            undelivered: 0,
            reachable: true,
        }]);
        let mut baseline = sync.clone();
        baseline.relay_dropped = Some(3);
        let observer = baseline.event_observers.as_mut().expect("observer fixture");
        observer[0].delivered = 10;
        observer[0].dropped = 1;
        state.sync_baseline = Some(baseline);

        handle_key(&mut state, KeyCode::Char('o'));
        let rendered = render(&mut state);

        assert!(rendered.contains("operations — RPC facts"));
        assert!(rendered.contains("staged blocks       0"));
        assert!(rendered.contains("block ingestion     unavailable"));
        assert!(rendered.contains("proposal validator  0"));
        assert!(rendered.contains("StackerDB relay     1"));
        assert!(rendered.contains("relay shed          +2 since opened"));

        handle_key(&mut state, KeyCode::End);
        let rendered = render(&mut state);
        assert!(rendered.contains("http://observer.example/events"));
        assert!(rendered.contains("delivered +2 since opened"));
        assert!(rendered.contains("dropped +1 since opened"));
        assert!(rendered.contains("undelivered now"));

        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.screen, Screen::Overview);
    }

    #[test]
    fn operations_show_current_metric_gauges_and_session_deltas() {
        let mut state = dashboard_state();
        state.screen = Screen::Operations;
        state.metrics_enabled = true;
        state.sources.metrics.succeeded();
        state.metrics_baseline = Some(node::Metrics {
            refusal_signature: Some(10.0),
            peer_failovers: Some(2.0),
            block_execution_seconds_sum: Some(1.0),
            block_execution_seconds_count: Some(2.0),
            ..node::Metrics::default()
        });
        state.metrics = Some(node::Metrics {
            refusal_signature: Some(12.0),
            peer_failovers: Some(3.0),
            serving_followers: Some(4.0),
            serving_proposal_validators: Some(2.0),
            serving_stackerdb_replicas: Some(3.0),
            mempool_transactions: Some(7.0),
            last_block_transactions: Some(5.0),
            last_block_runtime: Some(0.25),
            block_execution_seconds_sum: Some(1.6),
            block_execution_seconds_count: Some(4.0),
            marf_node_cache_bytes: Some(1024.0),
            marf_auxiliary_cache_bytes: Some(1024.0),
            clarity_value_cache_bytes: Some(1024.0),
            wasm_module_cache_bytes: Some(1024.0),
            ..node::Metrics::default()
        });

        let rendered = render_at(&mut state, 110, 70);

        assert!(rendered.contains("metrics data        fresh 0s ago"));
        assert!(rendered.contains("proposal peers      2"));
        assert!(rendered.contains("StackerDB peers     3"));
        assert!(rendered.contains("refusals            +2 since opened"));
        assert!(rendered.contains("peer failovers      +1 since opened"));
        assert!(rendered.contains("0.300s average · 2 blocks since opened"));
        assert!(rendered.contains("25.0% of block limit"));
        assert!(rendered.contains("cache memory        4.0 KiB current"));
    }

    #[test]
    fn mining_view_lists_and_inspects_every_participant() {
        let mut state = dashboard_state();

        handle_key(&mut state, KeyCode::Char('m'));
        assert_eq!(state.screen, Screen::Election);
        assert_eq!(state.selected_participant.selected(), Some(0));
        let winner = render(&mut state);
        assert!(winner.contains("network election & active tenure"));
        assert!(winner.contains("2 candidate commitments"));
        assert!(winner.contains("election participants"));
        assert!(winner.contains("WIN"));
        assert!(winner.contains("winning participant details"));
        assert!(winner.contains("50,000 effective / 60,000 raw sats"));
        assert!(winner.contains("relative share among candidate commitments; not win probability"));

        handle_key(&mut state, KeyCode::Down);
        assert_eq!(state.selected_participant.selected(), Some(1));
        let competitor = render(&mut state);
        assert!(competitor.contains("participant details"));
        assert!(competitor.contains("bbbbbbbbbb…bbbbbb"));
        assert!(competitor.contains("40,000 effective / 40,000 raw sats"));

        handle_key(&mut state, KeyCode::Esc);
        assert_eq!(state.screen, Screen::Overview);
    }

    #[test]
    fn mining_view_distinguishes_missing_data_from_no_commitments() {
        let mut missing = dashboard_state();
        missing.sortitions[0].mining_competition = None;
        handle_key(&mut missing, KeyCode::Char('m'));
        assert!(
            render(&mut missing)
                .contains("this node has no retained participant data for this sortition")
        );

        let mut empty = dashboard_state();
        empty.sortitions[0].mining_competition = Some(node::MiningCompetition::default());
        handle_key(&mut empty, KeyCode::Char('m'));
        assert!(
            render(&mut empty)
                .contains("no candidate commitments were present in this Bitcoin block")
        );
    }

    #[test]
    fn a_sortitionless_burn_block_keeps_the_last_elected_miner_current() {
        let mut state = dashboard_state();
        state.sortitions[0].elected = Some(false);
        state.sortitions[0].miner_pk_hash160 = None;
        state.sortitions[0]
            .mining_competition
            .as_mut()
            .expect("competition")
            .winner_txid = None;
        state.sortitions[1].miner_pk_hash160 =
            Some("9999999999999999999999999999999999999999".to_owned());

        let rendered = render(&mut state);
        assert!(rendered.contains("no successor; active tenure continued"));
        assert!(rendered.contains("9999999999…999999"));
        assert!(rendered.contains("Bitcoin block 960,240"));
    }

    #[test]
    fn layouts_cover_standard_wide_and_too_small_terminals() {
        let mut standard = dashboard_state();
        let standard = render_at(&mut standard, 80, 24);
        assert!(standard.contains("verified locally"));
        assert!(standard.contains("Bitcoin decisions become Stacks activity"));
        assert!(standard.contains("next boundary"));
        assert!(standard.contains("PoX schedule"));
        assert!(standard.contains("data freshness"));
        assert!(standard.contains("1 overview"));

        let mut wide = dashboard_state();
        let wide = render_at(&mut wide, 160, 40);
        assert!(wide.contains("Bitcoin block 960,240 elected a new network miner"));
        assert!(wide.contains("4 Stacks blocks"));
        assert!(wide.contains("Bitcoin block 960,241"));

        let mut small = dashboard_state();
        let small = render_at(&mut small, 79, 23);
        assert!(small.contains("terminal too small"));
        assert!(small.contains("current 79x23 · required 80x24"));
    }

    #[test]
    fn overview_shows_the_full_failure_beside_stale_data() {
        let mut state = dashboard_state();
        state.sources.tenure.succeeded();
        state.sources.tenure.failed("tenure request timed out");

        let rendered = render_at(&mut state, 80, 24);
        assert!(rendered.contains("stale 0s ago"));
        assert!(rendered.contains("tenure request timed out"));
        assert!(rendered.contains("3 tenure"));
    }

    fn dashboard_state() -> State {
        let mut state = State {
            sync: Some(node::SyncStatus {
                roles: Some(node::NodeRoles {
                    follower: true,
                    signer: true,
                    miner: false,
                }),
                followed_stacks_height: Some(8_716_526),
                selected_stacks_height: Some(8_716_525),
                selected_from_peer: Some("http://192.0.2.123:20443".to_owned()),
                executed_stacks_height: Some(8_716_524),
                blocks_behind: Some(2),
                ..node::SyncStatus::default()
            }),
            info: Some(node::NodeInfo {
                burn_block_height: Some(960_240),
                server_version: Some("nano-stacks".to_owned()),
                network_id: Some(1),
            }),
            tenure: Some(tenure_info()),
            pox: Some(pox()),
            sortitions: sortitions(),
            blocks: vec![tenure_block()],
            ..State::default()
        };
        state.blocks[0].timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_secs();
        state
    }

    fn tenure_info() -> node::TenureInfo {
        node::TenureInfo {
            consensus_hash: Some("0x1111111111111111111111111111111111111111".to_owned()),
            tenure_start_block_id: Some(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            ),
            parent_consensus_hash: Some("0x2222222222222222222222222222222222222222".to_owned()),
            parent_tenure_start_block_id: Some(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            ),
            tip_height: Some(8_716_524),
            reward_cycle: Some(140),
        }
    }

    fn pox() -> node::Pox {
        node::Pox {
            current_cycle: Some(node::Cycle {
                id: Some(140),
                stacked_ustx: Some(1_000_000),
            }),
            next_cycle: Some(node::NextCycle {
                id: Some(141),
                blocks_until_prepare_phase: Some(10),
                blocks_until_reward_phase: Some(110),
            }),
            current_epoch: Some("Epoch40".to_owned()),
            epochs: vec![node::Epoch {
                epoch_id: Some("Epoch40".to_owned()),
                block_limit: Some(node::ExecutionBudget {
                    write_length: Some(15_000_000),
                    write_count: Some(15_000),
                    read_length: Some(200_000_000),
                    read_count: Some(30_000),
                    runtime: Some(5_000_000_000),
                }),
            }],
        }
    }

    fn sortitions() -> Vec<node::Sortition> {
        vec![
            node::Sortition {
                burn_block_hash: Some(
                    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
                ),
                burn_block_height: Some(960_240),
                burn_header_timestamp: Some(1_700_000_000),
                consensus_hash: Some("0x1111111111111111111111111111111111111111".to_owned()),
                elected: Some(true),
                miner_pk_hash160: Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_owned()),
                stacks_parent_ch: Some("0x2222222222222222222222222222222222222222".to_owned()),
                committed_block_hash: Some(
                    "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned(),
                ),
                vrf_seed: Some(
                    "1212121212121212121212121212121212121212121212121212121212121212".to_owned(),
                ),
                mining_competition: Some(node::MiningCompetition {
                    winner_txid: Some(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_owned(),
                    ),
                    block_burn_sats: 100_000,
                    window_median_burn_sats: 95_000,
                    sampled_window_blocks: 6,
                    participants: vec![
                        node::SortitionParticipant {
                            txid:
                                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                                    .to_owned(),
                            signing_key_hash: Some(
                                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_owned(),
                            ),
                            vrf_public_key: Some(
                                "1313131313131313131313131313131313131313131313131313131313131313"
                                    .to_owned(),
                            ),
                            committed_block_hash:
                                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                                    .to_owned(),
                            burn_sats: 60_000,
                            effective_burn_sats: 50_000,
                            median_burn_sats: 50_000,
                            frequency: 6,
                        },
                        node::SortitionParticipant {
                            txid:
                                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                                    .to_owned(),
                            signing_key_hash: None,
                            vrf_public_key: Some(
                                "3333333333333333333333333333333333333333333333333333333333333333"
                                    .to_owned(),
                            ),
                            committed_block_hash:
                                "4444444444444444444444444444444444444444444444444444444444444444"
                                    .to_owned(),
                            burn_sats: 40_000,
                            effective_burn_sats: 40_000,
                            median_burn_sats: 45_000,
                            frequency: 5,
                        },
                    ],
                }),
            },
            node::Sortition {
                burn_block_height: Some(960_238),
                elected: Some(true),
                ..node::Sortition::default()
            },
        ]
    }

    fn render(state: &mut State) -> String {
        render_at(state, 110, 32)
    }

    fn render_at(state: &mut State, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, state, &node::Node::new("http://node")))
            .expect("render state");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn explorer_state() -> State {
        let mut state = State {
            screen: Screen::Activity,
            ..State::default()
        };
        state.blocks.push(node::Block {
            height: 42,
            id: "block".to_owned(),
            parent_id: "parent".to_owned(),
            consensus_hash: "consensus".to_owned(),
            state_index_root: "root".to_owned(),
            transactions: vec![node::Transaction {
                txid: "transaction".to_owned(),
                kind: "call".to_owned(),
                summary: "contract::function".to_owned(),
                origin: Some("sender".to_owned()),
                sponsor: None,
                origin_nonce: 1,
                sponsor_nonce: None,
                fee: 2,
                authorization: "single signature".to_owned(),
                version: "Mainnet".to_owned(),
                chain_id: 1,
                anchor_mode: "Any".to_owned(),
                post_condition_mode: "Deny".to_owned(),
                post_conditions: 0,
                tenure_change: None,
                fields: vec![
                    ("contract".to_owned(), "SP123.contract".to_owned()),
                    ("function".to_owned(), "method".to_owned()),
                    ("argument 0".to_owned(), "u42".to_owned()),
                ],
            }],
            signatures: 1,
            timestamp: 2,
        });
        state
    }

    fn receipt_payload(block_id: &str, height: u64, txid: &str, eventful: bool) -> String {
        let events = eventful.then(|| {
            serde_json::json!({
                "txid": format!("0x{txid}"),
                "event_index": 0,
                "type": "stx_transfer_event",
                "stx_transfer_event": {
                    "amount": "9",
                    "sender": "A",
                    "recipient": "B"
                }
            })
        });
        serde_json::json!({
            "index_block_hash": format!("0x{block_id}"),
            "block_height": height,
            "transactions": [{
                "txid": format!("0x{txid}"),
                "status": "success",
                "raw_result": "0x0703",
                "vm_error": null,
                "execution_cost": {
                    "write_length": 1,
                    "write_count": 2,
                    "read_length": 3,
                    "read_count": 4,
                    "runtime": 5
                }
            }],
            "events": events.into_iter().collect::<Vec<_>>()
        })
        .to_string()
    }

    fn block(id: &str, parent_id: &str, height: u64) -> node::Block {
        node::Block {
            height,
            id: id.to_owned(),
            parent_id: parent_id.to_owned(),
            consensus_hash: "tenure".to_owned(),
            state_index_root: "root".to_owned(),
            transactions: Vec::new(),
            signatures: 0,
            timestamp: 0,
        }
    }

    fn tenure_block() -> node::Block {
        node::Block {
            height: 8_716_524,
            id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned(),
            parent_id: "parent".to_owned(),
            consensus_hash: "1111111111111111111111111111111111111111".to_owned(),
            state_index_root: "root".to_owned(),
            transactions: vec![node::Transaction {
                txid: "extension".to_owned(),
                kind: "tenure".to_owned(),
                summary: "runtime extension".to_owned(),
                origin: None,
                sponsor: None,
                origin_nonce: 0,
                sponsor_nonce: None,
                fee: 0,
                authorization: "single signature".to_owned(),
                version: "Mainnet".to_owned(),
                chain_id: 1,
                anchor_mode: "OnChainOnly".to_owned(),
                post_condition_mode: "Deny".to_owned(),
                post_conditions: 0,
                tenure_change: Some(node::TenureChange {
                    is_extension: true,
                    cause: "runtime limit reached".to_owned(),
                    reset: "runtime".to_owned(),
                    previous_blocks: 12,
                }),
                fields: Vec::new(),
            }],
            signatures: 1,
            timestamp: 2,
        }
    }
}
