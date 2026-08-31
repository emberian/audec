//! Curve expressions: control-rate generator terms (skeleton).
//!
//! Normative design: `docs/LANGUAGES.md` §3. Implementation shares the
//! NOTATION lane's discipline; wiring origin metadata onto lanes follows
//! NOTEWIRE. A curve term describes authored control motion; it is not a
//! measurement, and a term pretty-printed *from* measured evidence keeps its
//! evidence reference rather than replacing it.
#![allow(dead_code, unused_variables)]

use crate::automation::AutomationPoint;
use crate::reconstruction::ReconstructionEvidenceId;
use crate::sequencer::{BeatDuration, BeatTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LfoShape {
    Sine,
    Triangle,
    Square,
    Saw,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CurveExpr {
    Const(f64),
    Line {
        from: f64,
        to: f64,
    },
    Lfo {
        shape: LfoShape,
        rate_hz: f64,
        depth: f64,
        phase: f64,
    },
    Env {
        attack: f64,
        decay: f64,
        sustain: f64,
        release: f64,
    },
    Sum(Vec<CurveExpr>),
    Scale {
        input: Box<CurveExpr>,
        multiply: f64,
        add: f64,
    },
    Clamp {
        input: Box<CurveExpr>,
        min: f64,
        max: f64,
    },
    /// Pretty-printed measured evidence: e.g. a pitch vibrato observation
    /// with `rate_hz` and peak-to-peak `extent_semitones` becomes
    /// `Lfo { Sine, rate_hz, depth: extent / 2, .. }` while retaining the
    /// evidence reference here.
    FromEvidence(ReconstructionEvidenceId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CurveError {
    NonFinite(&'static str),
    EmptySpan,
    /// `FromEvidence` requires a resolver-supplied realization; compiling it
    /// without one is an error, never a silent constant.
    UnresolvedEvidence(ReconstructionEvidenceId),
}

/// Compile a term to lane points at a declared control resolution, choosing
/// `SegmentShape`s that reproduce the term's meaning (`Hold` for squares,
/// `Smooth`/`CubicBezier` for sines) rather than densely sampling everything.
pub fn compile_curve(
    expr: &CurveExpr,
    span: (BeatTime, BeatTime),
    control_resolution: BeatDuration,
) -> Result<Vec<AutomationPoint>, CurveError> {
    todo!("NOTATION/curve lane: docs/LANGUAGES.md section 3")
}
