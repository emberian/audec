//! Pattern mini-notation: term type and evaluator boundary (skeleton).
//!
//! Normative design: `docs/LANGUAGES.md` §2. Implementation: NOTATION
//! workstream in `docs/SWARM_PLAN.md`; wiring into live patterns and
//! `PatternDefinition` provenance is the separate NOTEWIRE workstream.
//!
//! A pattern term is not a recording, and a name binding is not an
//! instrument identity. Evaluation is pure and total on parseable input:
//! explicit seeds, exact rational placement over PPQ ticks, and typed
//! diagnostics wherever the grid forces rounding. The evaluator never rolls
//! dice; probabilistic fields pass through to the sequencer's seeded
//! scheduler.
#![allow(dead_code, unused_variables)]

use std::collections::BTreeMap;
use std::fmt;

use crate::reconstruction::ReconstructionProposalId;
use crate::sequencer::{BeatDuration, StepPattern, TriggerTarget};

/// FNV-1a 128 over the canonical printed form (the `assets` fingerprint
/// idiom): stable, deterministic, and explicitly non-cryptographic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermHash(pub u128);

/// Exact width as a fraction of the parent extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ratio {
    pub numerator: u32,
    pub denominator: u32,
}

/// One step in a sequence. Modifiers from the surface syntax land here:
/// `@` sets `width`, `!` sets `replicate`, `*` sets `repeat`, `?` sets
/// `probability`.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub element: Element,
    pub width: Ratio,
    pub replicate: u32,
    pub repeat: u32,
    pub probability: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Element {
    Rest,
    Name {
        binding: String,
        variant: Option<u32>,
    },
    Group(Vec<Step>),
    /// One member per cycle, round-robin by cycle index.
    Alternate(Vec<Step>),
}

/// Transforms applicable under `Every`.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternTransform {
    Rotate(i32),
    Gain(f32),
    Degrade(f32),
}

/// The pattern term. Combinators and mini-notation strings parse into the
/// same type; there are no strings-as-code anywhere below this boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternExpr {
    /// One cycle of steps whose widths partition the cycle.
    Seq(Vec<Step>),
    /// Simultaneous patterns merged into distinct lanes.
    Stack(Vec<PatternExpr>),
    Every {
        period: u32,
        offset: u32,
        transform: PatternTransform,
        inner: Box<PatternExpr>,
    },
    Rotate {
        steps: i32,
        inner: Box<PatternExpr>,
    },
    Euclid {
        hits: u32,
        slots: u32,
        rotation: i32,
        element: Element,
    },
    Fast {
        factor: u32,
        inner: Box<PatternExpr>,
    },
    Slow {
        factor: u32,
        inner: Box<PatternExpr>,
    },
    Swing {
        amount: f32,
        inner: Box<PatternExpr>,
    },
    Gain {
        linear: f32,
        inner: Box<PatternExpr>,
    },
    Degrade {
        probability: f32,
        inner: Box<PatternExpr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternParseError {
    /// Byte offset into the source string.
    pub offset: usize,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternEvalError {
    /// An identifier had no entry in the binding table.
    UnboundName(String),
    /// Two events landed on the same lane and tick; never last-writer-wins.
    StepCollision { binding: String, tick: i64 },
    EmptyPattern,
    InvalidRatio { numerator: u32, denominator: u32 },
}

/// Non-fatal evaluation facts, mirroring `reconstruction_apply`'s
/// rounded-to-tick philosophy: lossy boundaries are reported, not hidden.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PatternEvalDiagnostic {
    RoundedToTick { at_tick: i64, error_ticks: i64 },
}

pub struct EvalContext<'a> {
    /// Name → trigger binding. One step lane per distinct bound name.
    pub bindings: &'a BTreeMap<String, TriggerTarget>,
    /// One cycle's musical length.
    pub cycle: BeatDuration,
    /// Combined with stable event identities for `?` and `degrade` hashes.
    pub seed: u64,
    /// Which cycle index alternations select from.
    pub cycle_index: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalOutput {
    pub pattern: StepPattern,
    pub diagnostics: Vec<PatternEvalDiagnostic>,
}

/// Provenance for generated patterns. Wiring this into
/// `sequencer::PatternDefinition` (plus codec round-trip and old-file
/// defaulting to `Authored`) is NOTEWIRE's shared-struct change.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternOrigin {
    Authored,
    Expression {
        source: String,
        term_hash: TermHash,
        bindings_hash: TermHash,
        /// Set — never silently cleared — when the realized pattern is
        /// edited by hand after generation.
        diverged: bool,
    },
    Deprojected {
        proposal: ReconstructionProposalId,
        diverged: bool,
    },
}

impl fmt::Display for PatternParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for PatternParseError {}

pub fn parse(source: &str) -> Result<PatternExpr, PatternParseError> {
    todo!("NOTATION lane: docs/LANGUAGES.md section 2")
}

/// Canonical form; `parse(print(t)) == t` for every printable term.
pub fn print(expr: &PatternExpr) -> String {
    todo!("NOTATION lane: docs/LANGUAGES.md section 2")
}

pub fn term_hash(expr: &PatternExpr) -> TermHash {
    todo!("NOTATION lane: docs/LANGUAGES.md section 2")
}

/// Exact placement per `docs/LANGUAGES.md` §2: rational widths over PPQ,
/// deterministic grid choice with `micro_offset` residues, `*n` on a
/// single-grid-step leaf mapped to ratchets, probability passed through.
pub fn eval_steps(
    expr: &PatternExpr,
    context: &EvalContext<'_>,
) -> Result<EvalOutput, PatternEvalError> {
    todo!("NOTATION lane: docs/LANGUAGES.md section 2")
}
