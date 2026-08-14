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

use std::{
    collections::HashSet,
    io,
    process::ExitCode,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
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
    after_help = "VIEWS:\n  1 overview   2 activity   3 election   4 operations\n\nKEYS:\n  tab/shift-tab panel   ↑/↓ select   enter/→ open\n  esc/← back   r refresh   q quit/back   ? help"
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
    overview_panel: OverviewPanel,
    standard_layout: bool,
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

    fn behind(&self) -> Option<u64> {
        self.sync.as_ref().and_then(|sync| sync.blocks_behind)
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OverviewPanel {
    #[default]
    Blocks,
    Tenure,
    Sortition,
    Budget,
}

impl OverviewPanel {
    const fn next(self, backwards: bool) -> Self {
        match (self, backwards) {
            (Self::Blocks, false) | (Self::Sortition, true) => Self::Tenure,
            (Self::Tenure, false) | (Self::Budget, true) => Self::Sortition,
            (Self::Sortition, false) | (Self::Blocks, true) => Self::Budget,
            (Self::Budget, false) | (Self::Tenure, true) => Self::Blocks,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Tenure => "tenure",
            Self::Sortition => "sortition",
            Self::Budget => "budget",
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
    loop {
        while let Some(update) = poller.try_recv() {
            if let Some(request) = state.apply_poll(update) {
                blocks.request(request);
            }
        }
        while let Some(update) = blocks.try_recv() {
            state.apply_block(update);
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
    match key {
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
        KeyCode::Tab if state.screen == Screen::Overview => {
            state.overview_panel = state.overview_panel.next(false);
        }
        KeyCode::BackTab if state.screen == Screen::Overview => {
            state.overview_panel = state.overview_panel.next(true);
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

fn select_edge(selected: &mut ListState, length: usize, end: bool) {
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

/// The candidate field, sized by what the losing commitments spent.
fn competition_summary(
    competition: &node::MiningCompetition,
    winner: Option<&node::SortitionParticipant>,
) -> String {
    match competition.participants.len() {
        0 => "0 candidate commitments".to_owned(),
        1 => "1 candidate commitment".to_owned(),
        count => {
            let losing: u64 = competition
                .participants
                .iter()
                .filter(|participant| {
                    winner.is_none_or(|winner| !same_id(&participant.txid, &winner.txid))
                })
                .map(|participant| participant.burn_sats)
                .sum();
            format!("{count} candidates · losers burned {}", thousands(losing))
        }
    }
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
        if wide {
            draw_wide_overview(frame, state, node);
        } else {
            draw_standard_overview(frame, state, node);
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
}

fn draw_wide_overview(frame: &mut Frame, state: &mut State, node: &Node) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());
    draw_sync_status(frame, areas[0], state, node);
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(areas[1]);
    draw_tenure(frame, middle[0], state);
    draw_sortition(frame, middle[1], state);
    draw_tenure_budget(frame, areas[2], state);
    draw_blocks(frame, areas[3], state);
    draw_keys(frame, areas[4], state);
}

fn draw_standard_overview(frame: &mut Frame, state: &mut State, node: &Node) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());
    draw_compact_sync_status(frame, areas[0], state, node);
    draw_standard_panel(frame, areas[1], state);
    draw_keys(frame, areas[2], state);
}

fn draw_standard_panel(frame: &mut Frame, area: Rect, state: &mut State) {
    let freshness_height = if state.overview_panel == OverviewPanel::Tenure {
        4
    } else {
        3
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(freshness_height), Constraint::Min(3)])
        .split(area);
    let freshness = match state.overview_panel {
        OverviewPanel::Blocks => format!("blocks {}", state.sources.blocks.description()),
        OverviewPanel::Tenure => format!(
            "tenure {} · PoX {}",
            state.sources.tenure.description(),
            state.sources.pox.description()
        ),
        OverviewPanel::Sortition => {
            format!("sortition {}", state.sources.sortitions.description())
        }
        OverviewPanel::Budget => format!("PoX {}", state.sources.pox.description()),
    };
    frame.render_widget(
        Paragraph::new(freshness)
            .wrap(Wrap { trim: false })
            .block(bordered(&format!(
                " {} data freshness ",
                state.overview_panel.name()
            ))),
        areas[0],
    );
    match state.overview_panel {
        OverviewPanel::Blocks => draw_blocks(frame, areas[1], state),
        OverviewPanel::Tenure => draw_tenure(frame, areas[1], state),
        OverviewPanel::Sortition => draw_sortition(frame, areas[1], state),
        OverviewPanel::Budget => draw_tenure_budget(frame, areas[1], state),
    }
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

fn draw_tenure(frame: &mut Frame, area: Rect, state: &State) {
    let tenure = state.tenure.clone().unwrap_or_default();
    let pox = state.pox.clone().unwrap_or_default();
    let cycle = pox.current_cycle.unwrap_or_default();
    let next = pox.next_cycle.unwrap_or_default();
    let mut lines = tenure_history_lines(state, &tenure);
    lines.extend([
        Line::from(vec![
            label("cycle        "),
            number(cycle.id.or(tenure.reward_cycle), Color::Magenta),
            label(" → "),
            number(next.id, Color::DarkGray),
        ]),
        Line::from(vec![
            label("phases       "),
            Span::styled(
                cycle_phases(
                    next.blocks_until_prepare_phase,
                    next.blocks_until_reward_phase,
                ),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            label("stacked      "),
            Span::styled(
                cycle.stacked_ustx.map_or_else(
                    || "unavailable".to_owned(),
                    |stacked| format!("{} STX", thousands_u128(stacked / 1_000_000)),
                ),
                Style::default().fg(Color::White),
            ),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(lines).block(bordered(&format!(
            " current tenure — tenure {} · PoX {} ",
            state.sources.tenure.brief(),
            state.sources.pox.brief()
        ))),
        area,
    );
}

fn tenure_history_lines(state: &State, tenure: &node::TenureInfo) -> Vec<Line<'static>> {
    let blocks = tenure
        .consensus_hash
        .as_deref()
        .map_or_else(Vec::new, |tenure| tenure_blocks(state, tenure));
    let start_height = tenure
        .tenure_start_block_id
        .as_deref()
        .and_then(|start| blocks.iter().find(|block| same_id(&block.id, start)))
        .map(|block| block.height);
    let extensions = tenure_extensions(&blocks);
    let span = tenure_span(tenure.tip_height, start_height, blocks.len());
    let extension_count = if blocks.is_empty() {
        "waiting for tenure blocks".to_owned()
    } else if start_height.is_some() {
        format!("{} observed in the full loaded tenure", extensions.len())
    } else {
        format!(
            "{} observed in {} loaded blocks",
            extensions.len(),
            blocks.len()
        )
    };
    let latest_extension = extensions.first().map_or_else(
        || {
            if start_height.is_some() {
                "none observed · tenure-start budget".to_owned()
            } else {
                "none loaded · earlier reset unknown".to_owned()
            }
        },
        |(height, change)| {
            format!(
                "{} · block {} after {} · reset {}",
                change.cause,
                thousands(*height),
                change.previous_blocks,
                change.reset
            )
        },
    );
    vec![
        Line::from(vec![
            label("span         "),
            Span::styled(span, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            label("tenure ID    "),
            value(tenure.consensus_hash.as_deref()),
        ]),
        Line::from(vec![
            label("started at   "),
            value(tenure.tenure_start_block_id.as_deref()),
        ]),
        Line::from(vec![
            label("parent       "),
            value(tenure.parent_consensus_hash.as_deref()),
            label(" · start "),
            value(tenure.parent_tenure_start_block_id.as_deref()),
        ]),
        Line::from(vec![
            label("extensions   "),
            Span::styled(extension_count, Style::default().fg(Color::Magenta)),
        ]),
        Line::from(vec![
            label("latest reset "),
            Span::styled(latest_extension, Style::default().fg(Color::White)),
        ]),
    ]
}

/// The burn view this node executed under, as *it* derived it.
fn draw_sortition(frame: &mut Frame, area: Rect, state: &State) {
    let title = format!(
        " network miner & latest sortition — {} ",
        state.sources.sortitions.brief()
    );
    let Some(latest) = latest_sortition(state) else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "waiting for this node to derive a burnchain decision",
                Style::default().fg(Color::DarkGray),
            )))
            .block(bordered(&title)),
            area,
        );
        return;
    };
    let (outcome, colour) = match latest.elected {
        Some(true) => ("miner elected · new Stacks tenure", Color::Green),
        Some(false) => ("no election · current tenure continued", Color::DarkGray),
        None => ("election result unavailable", Color::Red),
    };
    let active = active_sortition(state);
    let (miner, miner_kind) = active.map_or((None, ""), miner_identity);
    let competition = latest.mining_competition.as_ref();
    let winner = competition.and_then(competition_winner);
    let participants = competition.map_or_else(
        || "participant data unavailable · press m".to_owned(),
        |competition| competition_summary(competition, winner),
    );
    let winner_burn = winner.map_or_else(
        || "no winning commitment".to_owned(),
        |winner| {
            format!(
                "{} of {} sats",
                thousands(winner.burn_sats),
                thousands(competition.map_or(0, |competition| competition.block_burn_sats))
            )
        },
    );
    let relative_weight = winner.map_or_else(
        || "unavailable".to_owned(),
        |winner| participant_weight(winner, competition.expect("a winner has a competition")),
    );
    let sampling = competition.map_or_else(
        || "unavailable".to_owned(),
        |competition| {
            format!(
                "{} burn blocks · median {} sats",
                competition.sampled_window_blocks,
                thousands(competition.window_median_burn_sats)
            )
        },
    );
    let lines = vec![
        Line::from(vec![
            label("bitcoin block  "),
            number(latest.burn_block_height, Color::Cyan),
            label(" · "),
            Span::styled(
                relative_time(latest.burn_header_timestamp),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            label("outcome        "),
            Span::styled(outcome, Style::default().fg(colour)),
        ]),
        Line::from(vec![
            label("network miner  "),
            value(miner),
            label(miner_kind),
        ]),
        Line::from(vec![
            label("tenure elected "),
            number(
                active.and_then(|sortition| sortition.burn_block_height),
                Color::DarkGray,
            ),
            label("  bitcoin block"),
        ]),
        Line::from(vec![label("competition    "), Span::raw(participants)]),
        Line::from(vec![label("winner burn    "), Span::raw(winner_burn)]),
        Line::from(vec![label("relative weight "), Span::raw(relative_weight)]),
        Line::from(vec![label("sample window  "), Span::raw(sampling)]),
        Line::from(vec![
            label("tenure commit  "),
            value(active.and_then(|sortition| sortition.committed_block_hash.as_deref())),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).block(bordered(&title)), area);
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

fn draw_tenure_budget(frame: &mut Frame, area: Rect, state: &State) {
    let budget = state.pox.as_ref().and_then(node::Pox::current_budget);
    let tenure = state.tenure.as_ref();
    let blocks = tenure
        .and_then(|tenure| tenure.consensus_hash.as_deref())
        .map_or_else(Vec::new, |tenure| tenure_blocks(state, tenure));
    let start = tenure.and_then(|tenure| tenure.tenure_start_block_id.as_deref());
    let start_loaded =
        start.is_some_and(|start| blocks.iter().any(|block| same_id(&block.id, start)));
    let window = tenure_extensions(&blocks).first().map_or_else(
        || {
            if start_loaded {
                format!(
                    "tenure start {} · all dimensions",
                    short(start.expect("a loaded start is present"))
                )
            } else if blocks.is_empty() {
                "unavailable".to_owned()
            } else {
                "earlier reset outside loaded block history".to_owned()
            }
        },
        |(height, change)| {
            let reset = if change.reset == "all dimensions" {
                change.reset.clone()
            } else {
                format!("{} only", change.reset)
            };
            format!(
                "reset at block {} after {} tenure blocks · {reset}",
                thousands(*height),
                change.previous_blocks
            )
        },
    );
    let limits = budget.map_or_else(
        || "unavailable from PoX info".to_owned(),
        |budget| {
            format!(
                "{} runtime · {} reads · {} writes",
                compact_limit(budget.runtime),
                count_limit(budget.read_count),
                count_limit(budget.write_count)
            )
        },
    );
    let data = budget.map_or_else(
        || "unavailable from PoX info".to_owned(),
        |budget| {
            format!(
                "{} read · {} written",
                byte_limit(budget.read_length),
                byte_limit(budget.write_length)
            )
        },
    );
    let lines = vec![
        Line::from(vec![label("budget window    "), Span::raw(window)]),
        Line::from(vec![label("operation limits "), Span::raw(limits)]),
        Line::from(vec![label("data limits      "), Span::raw(data)]),
        Line::from(vec![
            label("used / remaining "),
            Span::styled(
                "not exposed by this node's RPC",
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(bordered(&format!(
            " current tenure execution budget — {} ",
            state.sources.pox.brief()
        ))),
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
                block.timestamp.to_string(),
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
    let block_height = selected_block(state).map_or(0, |block| block.height);
    let transaction_index = state.selected_transaction.selected().unwrap_or_default();
    let transaction_count = selected_block(state).map_or(0, |block| block.transactions.len());
    let mut lines = vec![
        detail("txid", transaction.txid),
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
        detail("fee", format!("{} uSTX", transaction.fee)),
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
    ];
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

fn transaction_colour(kind: &str) -> Color {
    match kind {
        "coinbase" => Color::Magenta,
        "tenure" => Color::Yellow,
        "deploy" => Color::Cyan,
        _ => Color::White,
    }
}

fn draw_keys(frame: &mut Frame, area: Rect, state: &State) {
    let keys = match state.screen {
        Screen::Overview if state.standard_layout => {
            "1 overview   2 activity   3 election   4 operations   tab panel   ? help   q quit"
        }
        Screen::Overview => {
            "1 overview   2 activity   3 election   4 operations   tab panel   ? help   q quit"
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
    Line::from(vec![label(&format!("{name:<18}")), Span::raw(value)])
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

fn tenure_span(tip: Option<u64>, start: Option<u64>, loaded: usize) -> String {
    match (tip, start) {
        (Some(tip), Some(start)) => format!(
            "{}→{} · loaded {}/{}",
            thousands(start),
            thousands(tip),
            loaded,
            thousands(tip.saturating_sub(start) + 1)
        ),
        (Some(tip), None) if loaded > 0 => {
            format!("tip {} · loaded {loaded}; no start", thousands(tip))
        }
        (Some(tip), None) => format!("tip {} · no blocks loaded", thousands(tip)),
        (None, _) => "unavailable".to_owned(),
    }
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
    let digits = value.to_string();
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
    thousands(u64::try_from(value).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        Action, BlockUpdate, Health, PollUpdate, Poller, STALL_AFTER, Screen, Source, State, draw,
        handle_key, health_summary, node, short, thousands,
    };

    #[test]
    fn a_height_is_read_as_a_magnitude() {
        assert_eq!(thousands(8_716_524), "8,716,524");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
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
    }

    #[test]
    fn dashboard_explains_sync_state_and_shows_the_whole_peer() {
        let mut state = dashboard_state();
        let sync = render(&mut state);

        assert!(sync.contains("verified locally"));
        assert!(sync.contains("executed and checked by this node"));
        assert!(sync.contains("last fork choice"));
        assert!(sync.contains("last peer report"));
        assert!(sync.contains("http://192.0.2.123:20443"));
        assert!(sync.contains("2 blocks behind"));
        assert!(sync.contains("local follower+signer"));
        assert!(sync.contains("SYNCING"));
        assert!(sync.contains("last sealed"));
        assert!(sync.contains("chain 0x00000001"));

        handle_key(&mut state, KeyCode::Tab);
        let tenure = render(&mut state);
        assert!(tenure.contains("current tenure"));
        assert!(tenure.contains("tip 8,716,524 · loaded 1; no start"));
        assert!(tenure.contains("extensions"));
        assert!(tenure.contains("runtime limit reached"));
        assert!(tenure.contains("prepare +10 · reward +110 burn blocks"));

        handle_key(&mut state, KeyCode::Tab);
        let sortition = render(&mut state);
        assert!(sortition.contains("network miner & latest sortition"));
        assert!(sortition.contains("miner elected · new Stacks tenure"));
        assert!(sortition.contains("network miner"));
        assert!(sortition.contains("2 candidates · losers burned 40,000"));
        assert!(sortition.contains("60,000 of 100,000 sats"));
        assert!(sortition.contains("tenure commit"));
        assert!(!sortition.contains("relative weight60.0%"));

        handle_key(&mut state, KeyCode::Tab);
        let budget = render(&mut state);
        assert!(budget.contains("current tenure execution budget"));
        assert!(budget.contains("5B runtime"));
        assert!(budget.contains("reset at block 8,716,524 after 12 tenure blocks · runtime only"));
        assert!(budget.contains("not exposed by this node's RPC"));
        assert!(!budget.contains("state root"));
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
        assert!(rendered.contains("staged blocks     0"));
        assert!(rendered.contains("block ingestion   unavailable"));
        assert!(rendered.contains("proposal validator0"));
        assert!(rendered.contains("StackerDB relay   1"));
        assert!(rendered.contains("relay shed        +2 since opened"));

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

        assert!(rendered.contains("metrics data      fresh 0s ago"));
        assert!(rendered.contains("proposal peers    2"));
        assert!(rendered.contains("StackerDB peers   3"));
        assert!(rendered.contains("refusals          +2 since opened"));
        assert!(rendered.contains("peer failovers    +1 since opened"));
        assert!(rendered.contains("0.300s average · 2 blocks since opened"));
        assert!(rendered.contains("25.0% of block limit"));
        assert!(rendered.contains("cache memory      4.0 KiB current"));
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
        state.overview_panel = super::OverviewPanel::Sortition;
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
        assert!(rendered.contains("no election · current tenure continued"));
        assert!(rendered.contains("9999999999…999999"));
        assert!(rendered.contains("960,238  bitcoin block"));
    }

    #[test]
    fn layouts_cover_standard_wide_and_too_small_terminals() {
        let mut standard = dashboard_state();
        let standard = render_at(&mut standard, 80, 24);
        assert!(standard.contains("verified locally"));
        assert!(standard.contains("blocks data freshness"));
        assert!(standard.contains("1 overview"));
        assert!(standard.contains("tab panel"));

        let mut wide = dashboard_state();
        let wide = render_at(&mut wide, 160, 40);
        assert!(wide.contains("current tenure"));
        assert!(wide.contains("network miner & latest sortition"));
        assert!(wide.contains("current tenure execution budget"));

        let mut small = dashboard_state();
        let small = render_at(&mut small, 79, 23);
        assert!(small.contains("terminal too small"));
        assert!(small.contains("current 79x23 · required 80x24"));
    }

    #[test]
    fn a_standard_panel_shows_the_full_failure_beside_stale_data() {
        let mut state = dashboard_state();
        state.overview_panel = super::OverviewPanel::Tenure;
        state.sources.tenure.succeeded();
        state.sources.tenure.failed("tenure request timed out");

        let rendered = render_at(&mut state, 80, 24);
        assert!(rendered.contains("stale 0s ago"));
        assert!(rendered.contains("tenure request timed out"));
        assert!(rendered.contains("tip 8,716,524"));
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
