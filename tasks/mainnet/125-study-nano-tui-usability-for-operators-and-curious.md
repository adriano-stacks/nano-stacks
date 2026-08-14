---
id: "125"
title: "Study nano-tui usability for operators and curious users"
status: completed
priority: high
effort: medium
type: chore
group: mainnet
dependencies: []
tags: ["tui", "ux", "research", "explorer"]
touches: ["tasks/mainnet"]
created_at: 2026-08-14
completed_at: 2026-08-14
---

# Study nano-tui usability for operators and curious users

## Objective

Evaluate the implemented terminal UI as both an operational console and an
explanation of the live Stacks protocol, then turn the evidence into a small,
prioritized feature direction that preserves the TUI's read-only honesty.

## Tasks

- [x] Audit the implemented screens, navigation, RPC inputs and failure states.
- [x] Exercise the dashboard against a live nano node and at its supported render size.
- [x] Map operator and curious-user questions to what the UI answers today.
- [x] Rank usability and feature gaps by impact, evidence and implementation cost.
- [x] Propose an information architecture and staged delivery backlog.
- [x] Record the study and delivery slices in taskmd tasks 125--129.
- [x] Run taskmd strict validation and finish this task.

## Acceptance Criteria

- The study distinguishes observed behavior from recommendations and names its
  research limits.
- Both audiences have explicit jobs, successful current paths and unmet needs.
- Every high-priority recommendation is grounded in an implemented RPC or names
  the smallest missing node surface.
- The proposal covers onboarding, degraded/stale states, navigation, terminal
  sizes and accessibility as well as protocol features.
- Follow-up work is ordered into independently useful slices rather than a
  wholesale rewrite.

## Study conclusion

`nano-tui` is already a credible protocol inspector, but it is not yet a
dependable operator console or a self-guided explanation of Stacks.

Its strongest property is provenance. It separates executed, selected and
peer-reported heights; distinguishes unavailable data from zero and from an
uncontested election; decodes blocks with nano's own codec; and explicitly says
that commitment weight is relative rather than a win probability. Those are
trust properties and must remain.

The main weakness is hierarchy. The overview gives health, protocol context and
raw detail similar visual weight, without answering either audience's first
question. An operator sees lag but not a conservative health decision, the
constrained subsystem or when a condition began. A curious user sees tenure,
sortition, fork choice and PoX terminology before the relationship among them is
explained.

Use one layered interface rather than separate beginner/operator modes:

1. a plain-language, status-first overview;
2. focused Activity, Election and Operations views;
3. contextual explanations through `?`; and
4. exact identifiers and consensus fields one level down.

## Evidence and limits

This was an expert heuristic and feature audit, not a participant study. It
covered every screen and key path, the HTTP failure behavior, completed TUI
tasks 089--094 and peer-reporting task 108, plus unused queue, observer and
metrics surfaces already published by `nano-rpc`.

The TUI was rendered against a live mainnet nano node at `127.0.0.1:20492` on
2026-08-14. It was healthy at height 8,762,449 with five history peers, eight
P2P sessions and zero lag. At the fixed and test-covered 110x32 size, that frame
still clipped the parent tenure and latest-reset explanations and joined
`relative weight` to `22.4%` without a separator.

A partial-failure check found that `/nano/sync_status` can fail while standard
tenure, PoX and sortition routes still answer. The current frame calls the whole
node unreachable but continues to display those values as if current. The
foreground poll performs five or six sequential requests with four-second
timeouts, then may backfill 50 blocks in the same input/render loop.

The current tests pass (11/11), but they render only 110x32. There are no
operator or newcomer interviews, comprehension measures, smaller-terminal
tests, slow-server tests or partial-freshness tests. Recommendations remain
hypotheses until the validation below is run.

## Audience jobs and current fit

| Question | Current answer | Fit |
|---|---|---|
| What has this node verified? | Executed height is distinct from fork choice and peer report. | Strong |
| Is it caught up? | Lag is stated in blocks with honest provenance. | Strong but not full health |
| Is every panel current? | Poll age follows only sync status; other panels retain stale data silently. | Missing |
| Why is it unhealthy? | Queue, observer, refusal, execution and resource data exist but are unused. | Missing |
| What role is this process running? | The network's current miner is shown; local follower/miner/signer roles are not. | Missing and ambiguous |
| Does it have independent peers? | Pool, session and known-peer counts are shown. | Partial |
| What did Bitcoin decide? | Miner, commitments, burn and sample window are inspectable. | Strong but dense |
| How did that create Stacks blocks? | Tenure and sortition sit beside one another without a causal narrative. | Partial |
| What did a transaction ask for? | Sender, fee, payload, function and Clarity arguments are decoded. | Strong |
| Did it succeed and what changed? | Receipt, result, events and charged cost are absent. | Missing |
| Can history be explored? | 200 in-memory observed blocks; first poll backfills at most 50; no jump or older path. | Partial |
| How are terms and keys learned? | Per-screen footer only; no contextual help, glossary or repository launch instructions. | Weak |
| Does it fit a normal terminal? | Only 110x32 is tested and live content clips there. | Weak |

The operator's ordered jobs are: identify the intended node/network/role; decide
health and progress; attribute a fault to execution, peers, P2P, queues,
StackerDB, observers or a refusal; establish when it began and whether it is
improving; then inspect exact evidence.

The curious user's ordered jobs are: understand what the node and network are
doing now; relate a Bitcoin decision to a tenure and its Stacks blocks;
understand why a miner commitment was selected; distinguish transaction intent
from execution outcome; and define unfamiliar visible terms in context.

## Delivery decision

The implementation backlog is:

| Order | Task | Outcome |
|---|---|---|
| 1 | 126 | Non-blocking polling, per-source freshness, responsive layout and honest CLI/onboarding |
| 2 | 127 | Conservative health plus existing queue, observer and metrics diagnostics |
| 3 | 128 | Bitcoin -> commitment -> tenure -> Stacks narrative and contextual help |
| 4 | 129 | Live transaction receipts, results, events and costs over existing SSE |

Trust and legibility precede more data. The TUI remains read-only, bounded,
independent of hosted services and sourced from the selected node. Do not add
process controls, configuration mutation, an embedded log viewer, a hosted
explorer or a receipt archive in these slices. Load older blocks, trends,
copy/export, no-color/ASCII modes and structured reorg/refusal history remain
post-validation candidates rather than committed scope.

The smallest missing node surfaces identified are this process's enabled roles
for task 127 and, only if live receipts prove insufficient, bounded historical
receipt lookup after task 129. All other high-priority diagnostics already exist
in `/nano/sync_status`, `/metrics`, `/events` or current stock-compatible RPC.

## Validation plan

Test a healthy node, a catching-up node, partial RPC failure, queue pressure and
an eventful transaction with at least three working operators and three people
who know Bitcoin but not Stacks.

Success means:

- operators identify health and its evidence in 10 seconds;
- operators identify the constrained subsystem in 30 seconds without logs;
- every participant can tell which data is stale;
- curious participants can explain Bitcoin decision -> tenure -> Stacks blocks
  in their own words;
- curious participants distinguish transaction intent from outcome and can find
  the definition of an unfamiliar visible term;
- 80x24, 110x32 and wide renders preserve the full status and focused panel; and
- a timed-out endpoint never prevents redraw or keyboard handling for more than
  250 ms.

Record completion time, wrong answers, help opens and abandoned paths. When a
task fails, fix terminology and hierarchy before adding more data.
