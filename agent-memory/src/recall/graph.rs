//! P4: provable `next` edges and bounded expansion inside one physical Archive
//! source.
//!
//! # Only one edge type
//!
//! This phase builds **`next` edges only**. Two evidence records inside the same
//! physical source (one session file / one SQLite session) are neighbours iff
//! every record position between them is present in the source and the records
//! between them are metadata-only (a `session_meta` / `turn_context` line is not
//! evidence, but it does not break the transcript). Turn boundaries are edges
//! too: turn `t` and turn `t + 1` are adjacent iff their boundary evidence records
//! are connected that way.
//!
//! No `reference`, `parent-child`, `resume`, `fork` or logical-ownership edge
//! exists. A record whose access is denied, that has no text projection, or that
//! was never reached inside the source budget is **invisible**: it breaks the
//! chain. An unknown ownership is therefore never guessed from a file name, a
//! timestamp or a neighbouring session, and two sources are never connected.
//!
//! # Subagent / sidechain
//!
//! Codex records the subagent marker in `session_meta.payload.source.subagent`
//! and Claude marks every record of a sidechain transcript with
//! `isSidechain: true` (its `isMeta` records are not evidence either). A
//! subagent session suppresses expansion entirely, and a sidechain record is
//! never a graph node: a parent session can therefore never gain a subagent
//! record through expansion or turn assembly.

use crate::recall::refs::HitRef;
use crate::recall::semantic::units::is_user_turn_record;
use serde::Serialize;
use serde_json::Value;

/// The only edge type this phase builds.
pub const EDGE_NEXT: &str = "next";
pub const DIRECTION_PREVIOUS: &str = "previous";
pub const DIRECTION_NEXT: &str = "next";

/// Largest `related_hops` a request may ask for.
pub const RELATED_MAX_HOPS: usize = 2;
/// Related nodes emitted for one hit.
pub const RELATED_NODES_PER_HIT: usize = 6;
/// Related nodes emitted for one Archive source in one search.
pub const RELATED_NODES_PER_SOURCE: usize = 24;
/// Wall-clock budget for related expansion (per source search).
pub const RELATED_MAX_ELAPSED_MS: u64 = 300;

/// Records inspected in each direction while assembling one turn.
pub const TURN_CONTEXT_MAX_RECORDS: usize = 512;
/// Segments one turn assembly may emit.
pub const TURN_CONTEXT_MAX_SEGMENTS: usize = 64;

/// One related record reported next to a primary hit. It never participates in
/// ranking or gating: it is an extra reference the caller may read back.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedItem {
    pub hit_id: String,
    pub title: String,
    #[serde(rename = "ref")]
    pub reference: HitRef,
    pub edge: &'static str,
    pub direction: &'static str,
    /// Chain distance: evidence-record steps inside the same turn, or turn steps
    /// for a neighbouring turn.
    pub hops: usize,
    /// Turn ordinal inside the physical source (`0` = records before the first
    /// user message).
    pub turn: u32,
}

/// One selected graph node, before it is turned into a [`RelatedItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelatedNode {
    /// Index inside the provider's ordered unit list.
    pub unit: usize,
    pub hops: usize,
    pub direction: &'static str,
    /// Turn ordinal of the selected record.
    pub turn: u32,
}

/// Why an expansion stopped. Every non-zero counter is reported in
/// `detail.archive.related`, so a silent omission is impossible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpansionCounters {
    /// The walk reached a record that is not visible (denied access, no text
    /// projection, or outside the scan). No edge is claimed.
    pub denied: usize,
    /// The source declares itself a subagent/sidechain session.
    pub sidechain: usize,
    /// The per-hit node budget dropped candidates.
    pub truncated: usize,
}

/// One visible record of a physical source, in source order.
#[derive(Debug, Clone)]
pub struct GraphRecord {
    pub unit: usize,
    pub turn: u32,
    /// The record carries retrieval evidence (a user/assistant message, a tool
    /// call or result). Metadata-only records are transparent: they keep the
    /// chain contiguous but are never selected as nodes.
    pub evidence: bool,
}

/// Ordered, access-filtered view of **one** physical source.
///
/// `push` is called in source order for every visible record only. A record that
/// is skipped (denied, unprojectable, sidechain) leaves its unit index out of the
/// list, so [`SourceGraph::contiguous`] refuses to connect the two sides of the
/// gap.
#[derive(Debug, Default)]
pub struct SourceGraph {
    pub records: Vec<GraphRecord>,
    turn: u32,
    /// Codex `session_meta.payload.source.subagent`: the whole source is a
    /// subagent session and no edge is ever claimed from it.
    pub subagent: bool,
    /// Sidechain records were seen and excluded from the graph.
    pub sidechain_records: usize,
    sidechain_units: std::collections::BTreeSet<usize>,
}

impl SourceGraph {
    pub fn new() -> Self {
        SourceGraph::default()
    }

    /// Feed one visible, non-sidechain record in source order.
    pub fn push(&mut self, unit: usize, provider: &str, text: &str, evidence: bool) {
        if is_user_turn_record(provider, text) {
            self.turn += 1;
        }
        self.records.push(GraphRecord {
            unit,
            turn: self.turn,
            evidence,
        });
    }

    /// Record a sidechain/subagent marker that must not become a node.
    pub fn note_sidechain(&mut self, unit: usize) {
        self.sidechain_records += 1;
        self.sidechain_units.insert(unit);
    }

    pub fn is_sidechain_unit(&self, unit: usize) -> bool {
        self.sidechain_units.contains(&unit)
    }

    fn position(&self, unit: usize) -> Option<usize> {
        self.records
            .binary_search_by_key(&unit, |record| record.unit)
            .ok()
    }

    fn turn_first(&self, turn: u32) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.turn == turn && record.evidence)
    }

    fn turn_last(&self, turn: u32) -> Option<usize> {
        self.records
            .iter()
            .rposition(|record| record.turn == turn && record.evidence)
    }

    /// True when positions `a < b` are connected by a present, unbroken record
    /// sequence. A skipped record (denied / invisible) breaks it.
    fn contiguous(&self, a: usize, b: usize) -> bool {
        b > a && self.records[b].unit == self.records[a].unit + (b - a)
    }

    /// The `n`-th evidence record in `direction`, skipping metadata-only records.
    /// A missing unit index is `Blocked`; running out of records is `End`.
    fn nth_evidence(&self, position: usize, direction: &str, n: usize) -> Step {
        let mut cursor = position;
        let mut count = 0usize;
        loop {
            if direction == DIRECTION_PREVIOUS {
                if cursor == 0 {
                    return Step::End;
                }
                if self.records[cursor - 1].unit + 1 != self.records[cursor].unit {
                    return Step::Blocked;
                }
                cursor -= 1;
            } else {
                if cursor + 1 >= self.records.len() {
                    return Step::End;
                }
                if self.records[cursor].unit + 1 != self.records[cursor + 1].unit {
                    return Step::Blocked;
                }
                cursor += 1;
            }
            if !self.records[cursor].evidence {
                continue;
            }
            count += 1;
            if count == n {
                return Step::At(cursor);
            }
        }
    }

    /// The adjacent turn in `direction`, only when the boundary evidence records
    /// are connected without a gap.
    fn adjacent_turn(&self, turn: u32, direction: &str) -> TurnStep {
        if direction == DIRECTION_PREVIOUS {
            if turn == 0 {
                return TurnStep::End;
            }
            let Some(first) = self.turn_first(turn) else {
                return TurnStep::End;
            };
            let Some(previous_last) = self.turn_last(turn - 1) else {
                return TurnStep::End;
            };
            if !self.contiguous(previous_last, first) {
                return TurnStep::Blocked;
            }
            TurnStep::At(turn - 1)
        } else {
            let Some(last) = self.turn_last(turn) else {
                return TurnStep::End;
            };
            let Some(next_first) = self.turn_first(turn + 1) else {
                return TurnStep::End;
            };
            if !self.contiguous(last, next_first) {
                return TurnStep::Blocked;
            }
            TurnStep::At(turn + 1)
        }
    }

    /// Bounded expansion over `next` edges: neighbouring turns (represented by
    /// their first evidence record) plus same-turn record neighbours.
    ///
    /// Deterministic for a fixed source: hop-major order, previous before next.
    pub fn related_nodes(&self, unit: usize, hops: usize) -> (Vec<RelatedNode>, ExpansionCounters) {
        let mut counters = ExpansionCounters::default();
        if hops == 0 || hops > RELATED_MAX_HOPS {
            return (Vec::new(), counters);
        }
        if self.subagent || self.is_sidechain_unit(unit) {
            counters.sidechain += 1;
            return (Vec::new(), counters);
        }
        let Some(position) = self.position(unit) else {
            counters.denied += 1;
            return (Vec::new(), counters);
        };
        let target_turn = self.records[position].turn;
        let mut nodes: Vec<RelatedNode> = Vec::new();

        for hop in 1..=hops {
            // Turn-level `next` edges: walk `hop` adjacent turn boundaries.
            for direction in [DIRECTION_PREVIOUS, DIRECTION_NEXT] {
                let mut cursor = target_turn;
                let mut reached: Option<u32> = None;
                for _ in 0..hop {
                    match self.adjacent_turn(cursor, direction) {
                        TurnStep::At(turn) => {
                            cursor = turn;
                            reached = Some(turn);
                        }
                        TurnStep::Blocked => {
                            counters.denied += 1;
                            reached = None;
                            break;
                        }
                        TurnStep::End => {
                            reached = None;
                            break;
                        }
                    }
                }
                let Some(turn) = reached else {
                    continue;
                };
                let Some(first) = self.turn_first(turn) else {
                    continue;
                };
                nodes.push(RelatedNode {
                    unit: self.records[first].unit,
                    hops: hop,
                    direction,
                    turn,
                });
            }

            // Record-level `next` edges inside the target turn.
            for direction in [DIRECTION_PREVIOUS, DIRECTION_NEXT] {
                match self.nth_evidence(position, direction, hop) {
                    Step::At(next) => {
                        if self.records[next].turn == target_turn {
                            nodes.push(RelatedNode {
                                unit: self.records[next].unit,
                                hops: hop,
                                direction,
                                turn: target_turn,
                            });
                        }
                    }
                    Step::Blocked => counters.denied += 1,
                    Step::End => {}
                }
            }
        }

        // Deterministic order, then the per-hit node budget.
        nodes.sort_by(|a, b| {
            a.hops
                .cmp(&b.hops)
                .then_with(|| direction_rank(a).cmp(&direction_rank(b)))
                .then_with(|| a.unit.cmp(&b.unit))
        });
        nodes.dedup_by_key(|node| node.unit);
        if nodes.len() > RELATED_NODES_PER_HIT {
            counters.truncated += nodes.len() - RELATED_NODES_PER_HIT;
            nodes.truncate(RELATED_NODES_PER_HIT);
        }
        (nodes, counters)
    }
}

fn direction_rank(node: &RelatedNode) -> u8 {
    if node.direction == DIRECTION_PREVIOUS {
        0
    } else {
        1
    }
}

enum Step {
    At(usize),
    Blocked,
    End,
}

enum TurnStep {
    At(u32),
    Blocked,
    End,
}

/// Truthy JSON: `true`, `"true"`, or a non-zero number. Missing/false is false.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text == "true",
        Some(Value::Number(number)) => number.as_i64().map(|n| n != 0).unwrap_or(false),
        _ => false,
    }
}

/// A Claude record that belongs to a sidechain transcript or is parser metadata.
pub fn is_sidechain_record(provider: &str, record: &str) -> bool {
    if provider != "claude" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(record) else {
        return false;
    };
    truthy(value.get("isSidechain")) || truthy(value.get("isMeta"))
}

/// A record that declares the whole physical source a subagent session.
///
/// Codex is the only provider with a captured marker: `session_meta.payload.
/// source.subagent` (the same predicate the Codex session parser uses).
pub fn is_subagent_session_record(provider: &str, record: &str) -> bool {
    if provider != "codex" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(record) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return false;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("source"))
        .and_then(Value::as_object)
        .map(|source| source.contains_key("subagent"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(provider: &str, role: &str) -> String {
        if provider == "claude" {
            serde_json::json!({
                "type": role,
                "message": {"role": role, "content": format!("{role} text")}
            })
            .to_string()
        } else {
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": role, "content": format!("{role} text")}
            })
            .to_string()
        }
    }

    /// Alternating user/assistant records, one turn per two records.
    fn graph(units: &[usize]) -> SourceGraph {
        let mut graph = SourceGraph::new();
        for (index, unit) in units.iter().enumerate() {
            let role = if index % 2 == 0 { "user" } else { "assistant" };
            graph.push(*unit, "codex", &record("codex", role), true);
        }
        graph
    }

    #[test]
    fn a_gap_in_record_positions_breaks_the_chain() {
        // Units 0..2 are turn 1; unit 4 opens turn 2, but unit 3 (the boundary
        // record) is missing: the turns are never connected.
        let mut graph = graph(&[0, 1]);
        graph.push(4, "codex", &record("codex", "user"), true);
        let (nodes, counters) = graph.related_nodes(1, 2);
        assert!(
            nodes.iter().all(|node| node.unit != 4),
            "no edge may cross the missing record: {nodes:?}"
        );
        assert!(counters.denied >= 1);
    }

    #[test]
    fn adjacent_turns_and_same_turn_records_are_reported_with_hops() {
        let graph = graph(&[0, 1, 2, 3, 4, 5]);
        // Turn 1: units 0..2, turn 2: 2..4, turn 3: 4..5 (as fed by `graph`).
        assert_eq!(graph.records[0].turn, 1);
        assert_eq!(graph.records[2].turn, 2);
        assert_eq!(graph.records[4].turn, 3);

        let (nodes, counters) = graph.related_nodes(1, 1);
        assert_eq!(counters, ExpansionCounters::default());
        let units: Vec<usize> = nodes.iter().map(|node| node.unit).collect();
        assert!(units.contains(&0), "{units:?}");
        assert!(units.contains(&2), "{units:?}");
        assert!(nodes.iter().all(|node| node.hops == 1));

        let (two, _) = graph.related_nodes(1, 2);
        assert!(
            two.iter().any(|node| node.unit == 4 && node.hops == 2),
            "{two:?}"
        );
    }

    #[test]
    fn metadata_records_keep_the_chain_but_are_never_nodes() {
        let mut graph = SourceGraph::new();
        graph.push(0, "codex", r#"{"type":"session_meta","payload":{"id":"s"}}"#, false);
        graph.push(1, "codex", &record("codex", "user"), true);
        graph.push(2, "codex", &record("codex", "assistant"), true);
        graph.push(3, "codex", r#"{"type":"turn_context"}"#, false);
        graph.push(4, "codex", &record("codex", "user"), true);
        graph.push(5, "codex", &record("codex", "assistant"), true);

        let (nodes, counters) = graph.related_nodes(2, 1);
        assert_eq!(counters, ExpansionCounters::default());
        let units: Vec<usize> = nodes.iter().map(|node| node.unit).collect();
        // The metadata records are transparent: turn 2 follows turn 1 directly.
        assert!(units.contains(&4), "next turn representative: {units:?}");
        assert!(!units.contains(&0) && !units.contains(&3), "{units:?}");
    }

    #[test]
    fn subagent_sources_and_sidechain_records_never_expand() {
        let mut subagent_graph = graph(&[0, 1, 2]);
        subagent_graph.subagent = true;
        let (nodes, counters) = subagent_graph.related_nodes(1, 2);
        assert!(nodes.is_empty());
        assert_eq!(counters.sidechain, 1);

        let mut sidechain_graph = graph(&[0, 1, 2]);
        sidechain_graph.note_sidechain(1);
        let (nodes, counters) = sidechain_graph.related_nodes(1, 2);
        assert!(nodes.is_empty());
        assert_eq!(counters.sidechain, 1);

        assert!(is_subagent_session_record(
            "codex",
            &serde_json::json!({
                "type": "session_meta",
                "payload": {"id": "s", "source": {"subagent": {"thread_spawn": {}}}}
            })
            .to_string()
        ));
        assert!(is_sidechain_record(
            "claude",
            &serde_json::json!({"type": "user", "isSidechain": true}).to_string()
        ));
        assert!(!is_sidechain_record("claude", "{}"));
    }

    #[test]
    fn hops_outside_the_bound_are_rejected() {
        let graph = graph(&[0, 1]);
        for hops in [0, RELATED_MAX_HOPS + 1] {
            let (nodes, _) = graph.related_nodes(0, hops);
            assert!(nodes.is_empty(), "hops={hops}");
        }
    }
}
