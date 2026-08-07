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
    time::{Duration, Instant},
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
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
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
            "usage: nano-tui [rpc-url] [--once]\n               q quit   ↑/↓ select a block   enter inspect it   r refresh\n               --once renders one frame as text and exits, for a script or a log"
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
    selected: ListState,
    inspecting: bool,
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
        }
        // Only the executed tip, and only when it moved: the explorer is a record of
        // what this node ran, so a block enters it by having been executed here.
        let (Some(height), Some(tip)) = (
            self.sync.as_ref().and_then(|sync| sync.executed_stacks_height),
            self.sync.as_ref().and_then(|sync| sync.executed_stacks_tip.clone()),
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
        if let Some(index) = self.selected.selected() {
            self.selected
                .select(Some((index + added).min(self.blocks.len() - 1)));
        }
    }

    fn behind(&self) -> u64 {
        self.sync
            .as_ref()
            .and_then(|sync| sync.blocks_behind)
            .unwrap_or_default()
    }
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    node: &Node,
) -> io::Result<()> {
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc if !state.inspecting => return Ok(()),
            KeyCode::Esc | KeyCode::Char('q') => state.inspecting = false,
            KeyCode::Enter => state.inspecting = !state.inspecting,
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut state, 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut state, -1),
            KeyCode::Char('r') => last = None,
            _ => {}
        }
    }
}

fn move_selection(state: &mut State, by: isize) {
    if state.blocks.is_empty() {
        return;
    }
    let last = state.blocks.len() - 1;
    let next = state
        .selected
        .selected()
        .map_or(0, |index| index.saturating_add_signed(by).min(last));
    state.selected.select(Some(next));
}

fn draw(frame: &mut Frame, state: &mut State, node: &Node) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());
    draw_heights(frame, areas[0], state, node);
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(areas[1]);
    draw_tenure(frame, middle[0], state);
    draw_sortition(frame, middle[1], state);
    if state.inspecting {
        draw_block(frame, areas[2], state);
    } else {
        draw_blocks(frame, areas[2], state);
    }
    draw_keys(frame, areas[3], state);
}

/// The three heights, and the gap between the last two.
fn draw_heights(frame: &mut Frame, area: Rect, state: &State, node: &Node) {
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

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            label("executed "),
            number(sync.executed_stacks_height, Color::Green),
            label("   selected "),
            number(sync.selected_stacks_height, Color::Yellow),
            label("   peer said "),
            number(sync.followed_stacks_height, Color::DarkGray),
            label("   burn "),
            number(info.burn_block_height, Color::Cyan),
        ])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            label("tip  "),
            value(sync.executed_stacks_tip.as_deref()),
            label("  root "),
            value(sync.executed_state_index_root.as_deref()),
            label("  tenure "),
            value(info.stacks_tip_consensus_hash.as_deref()),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            label("following "),
            value(sync.selected_from_peer.as_deref()),
            label("   "),
            Span::styled(
                if state.unreachable {
                    "node unreachable".to_owned()
                } else {
                    state
                        .polled
                        .map_or_else(String::new, |at| format!("polled {}s ago", at.elapsed().as_secs()))
                },
                Style::default().fg(if state.unreachable { Color::Red } else { Color::DarkGray }),
            ),
        ])),
        rows[2],
    );
    // Behind by zero is the whole claim, so it is the one thing drawn as a bar: a
    // number that reads 0 and a bar that is full say the same thing, and the bar
    // says it from across a room.
    let behind = state.behind();
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(match behind {
                0 => Color::Green,
                1..=10 => Color::Yellow,
                _ => Color::Red,
            }))
            // Not a percentage of anything: there is no ceiling to be behind by, so
            // the bar reads full at the tip and shrinks as the gap grows.
            .ratio(match behind {
                0 => 1.0,
                behind => (1.0 / (f64::from(u32::try_from(behind).unwrap_or(u32::MAX)) + 1.0))
                    .clamp(0.02, 0.99),
            })
            .label(match behind {
                0 => "at the tip".to_owned(),
                1 => "1 block behind".to_owned(),
                behind => format!("{behind} blocks behind"),
            }),
        rows[3],
    );
}

fn draw_tenure(frame: &mut Frame, area: Rect, state: &State) {
    let tenure = state.tenure.clone().unwrap_or_default();
    let pox = state.pox.clone().unwrap_or_default();
    let cycle = pox.current_cycle.unwrap_or_default();
    let next = pox.next_cycle.unwrap_or_default();
    let lines = vec![
        field("consensus", tenure.consensus_hash.as_deref()),
        field("start block", tenure.tenure_start_block_id.as_deref()),
        field("parent", tenure.parent_consensus_hash.as_deref()),
        Line::from(vec![
            label("tip height   "),
            number(tenure.tip_height, Color::White),
        ]),
        Line::from(vec![
            label("reward cycle "),
            number(cycle.id.or(tenure.reward_cycle), Color::Magenta),
            label("   next "),
            number(next.id, Color::DarkGray),
            label(" in "),
            Span::styled(
                next.blocks_until_reward_phase
                    .map_or_else(|| "?".to_owned(), |blocks| format!("{blocks}")),
                Style::default().fg(Color::DarkGray),
            ),
            label("   prepare in "),
            Span::styled(
                next.blocks_until_prepare_phase
                    .map_or_else(|| "?".to_owned(), |blocks| format!("{blocks}")),
                Style::default().fg(Color::DarkGray),
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
    ];
    frame.render_widget(
        Paragraph::new(lines).block(bordered(" tenure ")),
        area,
    );
}

/// The burn view this node executed under, as *it* derived it.
fn draw_sortition(frame: &mut Frame, area: Rect, state: &State) {
    let Some(latest) = state.sortitions.first() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "this node derives no sortitions yet",
                Style::default().fg(Color::DarkGray),
            )))
            .block(bordered(" sortition ")),
            area,
        );
        return;
    };
    let won = latest.elected.unwrap_or_default();
    let lines = vec![
        Line::from(vec![
            label("burn "),
            number(latest.burn_block_height, Color::Cyan),
            label("   "),
            Span::styled(
                if won { "elected a miner" } else { "elected nobody" },
                Style::default().fg(if won { Color::Green } else { Color::DarkGray }),
            ),
        ]),
        field("consensus", latest.consensus_hash.as_deref()),
        field("miner key", latest.miner_pk_hash160.as_deref()),
        field("committed", latest.committed_block_hash.as_deref()),
        field("previous", latest.last_sortition_ch.as_deref()),
        field("vrf seed", latest.vrf_seed.as_deref()),
    ];
    frame.render_widget(Paragraph::new(lines).block(bordered(" sortition ")), area);
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
    frame.render_stateful_widget(
        List::new(items)
            .block(bordered(" executed blocks "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▍"),
        area,
        &mut state.selected,
    );
}

/// One block, opened.
fn draw_block(frame: &mut Frame, area: Rect, state: &State) {
    let Some(block) = state
        .selected
        .selected()
        .and_then(|index| state.blocks.get(index))
        .or_else(|| state.blocks.first())
    else {
        return;
    };
    let mut lines = vec![
        field("block", Some(&block.id)),
        field("consensus", Some(&block.consensus_hash)),
        field("state root", Some(&block.state_index_root)),
        Line::from(vec![
            label("signatures   "),
            Span::styled(block.signatures.to_string(), Style::default().fg(Color::White)),
            label("   timestamp "),
            Span::styled(block.timestamp.to_string(), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
    ];
    for transaction in &block.transactions {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<9}", transaction.kind),
                Style::default().fg(match transaction.kind.as_str() {
                    "coinbase" => Color::Magenta,
                    "tenure" => Color::Yellow,
                    "deploy" => Color::Cyan,
                    _ => Color::White,
                }),
            ),
            Span::styled(short(&transaction.txid), Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::raw(transaction.detail.clone()),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(bordered(&format!(" block {} ", block.height)))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_keys(frame: &mut Frame, area: Rect, state: &State) {
    let keys = if state.inspecting {
        "enter/esc back   ↑/↓ select   r refresh   q quit"
    } else {
        "enter inspect a block   ↑/↓ select   r refresh   q quit"
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

fn label(text: &str) -> Span<'static> {
    Span::styled(text.to_owned(), Style::default().fg(Color::DarkGray))
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
    use super::{short, thousands};

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
}
