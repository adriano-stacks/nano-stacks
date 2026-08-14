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
    io,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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

/// How often the node is polled.
///
/// A mainnet block lands every few seconds at best, so this is fast enough to look
/// live and slow enough that the dashboard is never the reason a node is busy.
const POLL: Duration = Duration::from_secs(2);

/// How many executed blocks the explorer keeps.
const HISTORY: usize = 200;

/// How far back one poll will walk to fill in blocks it did not see land.
///
/// A bound rather than "until it meets a known block", because the first poll after
/// a restart meets nothing: without this it would walk the chain to the checkpoint,
/// one request per block.
const FILL: usize = 50;

fn main() -> io::Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:20443".to_owned());
    if url == "--help" || url == "-h" {
        println!(
            "usage: nano-tui [rpc-url] [--once]\n               ↑/↓ select   enter/→ open   m mining   esc/← back   r refresh   q quit/back\n               --once renders one frame as text and exits, for a script or a log"
        );
        return Ok(());
    }
    // One frame as text, for a check that does not need a terminal at all: the same
    // draw against the same node, rendered into a buffer instead of onto a screen.
    if std::env::args().any(|argument| argument == "--once") {
        print!("{}", render_once(&Node::new(&url)));
        return Ok(());
    }
    let mut terminal = start()?;
    let outcome = run(&mut terminal, &Node::new(&url));
    stop(&mut terminal)?;
    outcome
}

/// Draw one frame into a buffer and return it as lines of text.
fn render_once(node: &Node) -> String {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 32))
        .expect("a buffer backend cannot fail to open");
    let mut state = State::default();
    state.refresh(node);
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
    out
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

/// Everything on the screen, and the one poll that produced it.
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
    transaction_scroll: u16,
    /// The last poll that answered at all, so a node that has just gone away is
    /// visibly stale rather than silently frozen.
    polled: Option<Instant>,
    unreachable: bool,
}

impl State {
    /// Take one poll's worth of answers, and pick up any block the tip moved past.
    fn refresh(&mut self, node: &Node) {
        let sync = node.sync_status();
        self.unreachable = sync.is_none();
        if sync.is_some() {
            self.polled = Some(Instant::now());
        }
        self.sync = sync.or_else(|| self.sync.take());
        self.info = node.info().or_else(|| self.info.take());
        self.pox = node.pox().or_else(|| self.pox.take());
        self.tenure = node.tenure().or_else(|| self.tenure.take());
        if let Some(sortitions) = node.sortitions() {
            self.sortitions = sortitions;
            select_current_participant(self);
        }
        // Only the executed tip, and only when it moved: the explorer is a record of
        // what this node ran, so a block enters it by having been executed here.
        let (Some(height), Some(tip)) = (
            self.sync
                .as_ref()
                .and_then(|sync| sync.executed_stacks_height),
            self.sync
                .as_ref()
                .and_then(|sync| sync.executed_stacks_tip.clone()),
        ) else {
            return;
        };
        if self.blocks.first().is_some_and(|block| block.id == tip) {
            return;
        }
        // Walked back from the new tip through its parents, not sampled at it. The
        // node executes several blocks between two polls whenever it is catching up
        // -- and often when it is not -- so taking only the tip left holes in the
        // list: 8,716,645, 8,716,644, 8,716,643, 8,716,641. A hole is not a small
        // cosmetic problem here, because the list claims to be what this node
        // executed, and a reader cannot tell a block that was skipped by the poll
        // from one the node never ran.
        let mut found = Vec::new();
        let mut walk = Some(tip);
        while let Some(id) = walk.take() {
            if self.blocks.iter().any(|block| block.id == id) || found.len() >= FILL {
                break;
            }
            // The height is read from the block's own header, except for the tip,
            // whose height the node has already stated.
            let Some(block) = node.block(&id, if found.is_empty() { height } else { 0 }) else {
                break;
            };
            walk = Some(block.parent_id.clone());
            found.push(block);
        }
        if found.is_empty() {
            return;
        }
        // `found` is newest-first, and so is the list.
        let added = found.len();
        self.blocks.splice(0..0, found);
        self.blocks.truncate(HISTORY);
        // Keep the cursor on the block it was on, rather than letting new tips slide
        // the selection out from under the reader.
        if let Some(index) = self.selected_block.selected() {
            self.selected_block
                .select(Some((index + added).min(self.blocks.len() - 1)));
        }
    }

    fn behind(&self) -> Option<u64> {
        self.sync.as_ref().and_then(|sync| sync.blocks_behind)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Screen {
    #[default]
    Blocks,
    Block,
    Transaction,
    Mining,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    None,
    Refresh,
    Quit,
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, node: &Node) -> io::Result<()> {
    let mut state = State::default();
    // `None` rather than a time in the past: the first pass polls, and a clock that
    // cannot go back that far is not a thing to reason about.
    let mut last: Option<Instant> = None;
    loop {
        if last.is_none_or(|last| last.elapsed() >= POLL) {
            state.refresh(node);
            last = Some(Instant::now());
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
            Action::Refresh => last = None,
            Action::None => {}
        }
    }
}

fn handle_key(state: &mut State, key: KeyCode) -> Action {
    match key {
        KeyCode::Char('m') => {
            state.screen = if state.screen == Screen::Mining {
                Screen::Blocks
            } else {
                Screen::Mining
            };
            select_current_participant(state);
        }
        KeyCode::Char('q' | 'h') | KeyCode::Esc | KeyCode::Left => {
            match state.screen {
                Screen::Blocks => return Action::Quit,
                Screen::Block | Screen::Mining => state.screen = Screen::Blocks,
                Screen::Transaction => state.screen = Screen::Block,
            }
            state.transaction_scroll = 0;
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match state.screen {
            Screen::Blocks if !state.blocks.is_empty() => {
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
            Screen::Blocks | Screen::Block | Screen::Transaction | Screen::Mining => {}
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
        Screen::Blocks => move_list_selection(&mut state.selected_block, state.blocks.len(), by),
        Screen::Block => {
            let transactions = selected_block(state).map_or(0, |block| block.transactions.len());
            move_list_selection(&mut state.selected_transaction, transactions, by);
        }
        Screen::Transaction => {
            state.transaction_scroll = state
                .transaction_scroll
                .saturating_add_signed(i16::try_from(by).expect("key movement fits in i16"));
        }
        Screen::Mining => {
            let participants =
                mining_competition(state).map_or(0, |competition| competition.participants.len());
            move_list_selection(&mut state.selected_participant, participants, by);
        }
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
        Screen::Blocks => select_edge(&mut state.selected_block, state.blocks.len(), end),
        Screen::Block => {
            let transactions = selected_block(state).map_or(0, |block| block.transactions.len());
            select_edge(&mut state.selected_transaction, transactions, end);
        }
        Screen::Transaction => state.transaction_scroll = if end { u16::MAX } else { 0 },
        Screen::Mining => {
            let participants =
                mining_competition(state).map_or(0, |competition| competition.participants.len());
            select_edge(&mut state.selected_participant, participants, end);
        }
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
    if state.screen != Screen::Blocks {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Min(6),
                Constraint::Length(1),
            ])
            .split(frame.area());
        draw_sync_status(frame, areas[0], state, node);
        match state.screen {
            Screen::Block => draw_block(frame, areas[1], state),
            Screen::Transaction => draw_transaction(frame, areas[1], state),
            Screen::Mining => draw_mining(frame, areas[1], state),
            Screen::Blocks => unreachable!("handled by the dashboard layout"),
        }
        draw_keys(frame, areas[2], state);
        return;
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
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

fn draw_sync_status(frame: &mut Frame, area: Rect, state: &State, node: &Node) {
    let sync = state.sync.clone().unwrap_or_default();
    let info = state.info.clone().unwrap_or_default();
    let title = format!(
        " {} — {} — chain {} ",
        info.server_version.as_deref().unwrap_or("nano-stacks"),
        node.url(),
        info.network_id
            .map_or_else(|| "?".to_owned(), |id| format!("{id:#010x}"))
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if state.unreachable {
            Color::Red
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let source_status = if state.unreachable {
        "node unreachable".to_owned()
    } else {
        state.polled.map_or_else(
            || "waiting for first poll".to_owned(),
            |at| format!("polled {}s ago", at.elapsed().as_secs()),
        )
    };
    let (lag, lag_colour) = sync_lag(state.behind(), state.unreachable);
    let lines = vec![
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
                Style::default().fg(if state.unreachable {
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
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
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
        Paragraph::new(lines).block(bordered(" current tenure ")),
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
    let Some(latest) = latest_sortition(state) else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "waiting for this node to derive a burnchain decision",
                Style::default().fg(Color::DarkGray),
            )))
            .block(bordered(" latest burnchain decision ")),
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
            label("current miner  "),
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
        Line::from(vec![label("relative weight"), Span::raw(relative_weight)]),
        Line::from(vec![label("sample window  "), Span::raw(sampling)]),
        Line::from(vec![
            label("tenure commit  "),
            value(active.and_then(|sortition| sortition.committed_block_hash.as_deref())),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(bordered(" current miner & latest sortition ")),
        area,
    );
}

fn draw_mining(frame: &mut Frame, area: Rect, state: &mut State) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ])
        .split(area);
    draw_mining_summary(frame, areas[0], state);
    draw_participants(frame, areas[1], state);
    draw_participant(frame, areas[2], state);
}

fn draw_mining_summary(frame: &mut Frame, area: Rect, state: &State) {
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
            label("current miner   "),
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
        Paragraph::new(lines).block(bordered(" active tenure miner & election ")),
        area,
    );
}

fn draw_participants(frame: &mut Frame, area: Rect, state: &mut State) {
    let Some(competition) = mining_competition(state).cloned() else {
        state.selected_participant.select(None);
        frame.render_widget(
            Paragraph::new("this node has no retained participant data for this sortition")
                .block(bordered(" sortition participants ")),
            area,
        );
        return;
    };
    if competition.participants.is_empty() {
        state.selected_participant.select(None);
        frame.render_widget(
            Paragraph::new("no candidate commitments were present in this Bitcoin block")
                .block(bordered(" sortition participants ")),
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
                " sortition participants — miner key · burn · effective weight · activity ",
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
        Paragraph::new(lines).block(bordered(" current tenure execution budget ")),
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
            .block(bordered(" executed blocks ")),
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
        " executed blocks — {} held, {HISTORY} max, nothing on disk ",
        state.blocks.len()
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
        Screen::Blocks => "enter/→ open block   m mining   ↑/↓ select   r refresh   q quit",
        Screen::Block => {
            "enter/→ open transaction   m mining   ↑/↓ select   esc/← back   r refresh"
        }
        Screen::Transaction => {
            "↑/↓ scroll   m mining   pgup/pgdn page   home/end edges   esc/← back   r refresh"
        }
        Screen::Mining => "↑/↓ select participant   home/end edges   m/esc/← overview   r refresh",
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

fn sync_lag(behind: Option<u64>, unreachable: bool) -> (String, Color) {
    if unreachable {
        return (
            "unknown while the node is unreachable".to_owned(),
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
    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{Action, Screen, State, draw, handle_key, node, short, thousands};

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
        assert_eq!(state.screen, Screen::Blocks);
        assert_eq!(handle_key(&mut state, KeyCode::Char('q')), Action::Quit);
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
        let rendered = render(&mut state);

        assert!(rendered.contains("verified locally"));
        assert!(rendered.contains("executed and checked by this node"));
        assert!(rendered.contains("last fork choice"));
        assert!(rendered.contains("last peer report"));
        assert!(rendered.contains("http://192.0.2.123:20443"));
        assert!(rendered.contains("2 verified blocks behind"));
        assert!(rendered.contains("current tenure"));
        assert!(rendered.contains("tip 8,716,524 · loaded 1; no start"));
        assert!(rendered.contains("extensions"));
        assert!(rendered.contains("runtime limit reached"));
        assert!(rendered.contains("prepare +10 · reward +110 burn blocks"));
        assert!(rendered.contains("current tenure execution budget"));
        assert!(rendered.contains("5B runtime"));
        assert!(
            rendered.contains("reset at block 8,716,524 after 12 tenure blocks · runtime only")
        );
        assert!(rendered.contains("not exposed by this node's RPC"));
        assert!(rendered.contains("current miner & latest sortition"));
        assert!(rendered.contains("miner elected · new Stacks tenure"));
        assert!(rendered.contains("current miner"));
        assert!(rendered.contains("2 candidates · losers burned 40,000"));
        assert!(rendered.contains("60,000 of 100,000 sats"));
        assert!(rendered.contains("tenure commit"));
        assert!(!rendered.contains("state root"));
    }

    #[test]
    fn mining_view_lists_and_inspects_every_participant() {
        let mut state = dashboard_state();

        handle_key(&mut state, KeyCode::Char('m'));
        assert_eq!(state.screen, Screen::Mining);
        assert_eq!(state.selected_participant.selected(), Some(0));
        let winner = render(&mut state);
        assert!(winner.contains("active tenure miner & election"));
        assert!(winner.contains("2 candidate commitments"));
        assert!(winner.contains("sortition participants"));
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
        assert_eq!(state.screen, Screen::Blocks);
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
        assert!(rendered.contains("no election · current tenure continued"));
        assert!(rendered.contains("9999999999…999999"));
        assert!(rendered.contains("960,238  bitcoin block"));
    }

    fn dashboard_state() -> State {
        State {
            sync: Some(node::SyncStatus {
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
        }
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
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).expect("test terminal");
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
        let mut state = State::default();
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
