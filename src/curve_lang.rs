//! Curve expressions: deterministic control-rate generator terms.
//!
//! Normative design: `docs/LANGUAGES.md` §3. Implementation shares the
//! NOTATION lane's discipline; wiring origin metadata onto lanes follows
//! NOTEWIRE. A curve term describes authored control motion; it is not a
//! measurement, and a term pretty-printed *from* measured evidence keeps its
//! evidence reference rather than replacing it.
#![allow(dead_code)]

use std::error::Error;
use std::fmt;

use crate::automation::{
    AutomationPoint, AutomationPointId, BeatTime as AutomationBeatTime, SegmentShape, TimePosition,
};
use crate::reconstruction::ReconstructionEvidenceId;
use crate::sequencer::{BeatDuration, BeatTime, PPQ};

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
    InvalidResolution,
}

impl fmt::Display for CurveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite(parameter) => {
                write!(formatter, "non-finite curve parameter: {parameter}")
            }
            Self::EmptySpan => formatter.write_str("curve span must be non-empty and ordered"),
            Self::UnresolvedEvidence(id) => {
                write!(
                    formatter,
                    "curve evidence {} has not been resolved",
                    id.get()
                )
            }
            Self::InvalidResolution => formatter.write_str("control resolution must be positive"),
        }
    }
}

impl Error for CurveError {}

/// Compile a term to lane points at a declared control resolution, choosing
/// `SegmentShape`s that reproduce the term's meaning (`Hold` for squares,
/// `Smooth`/`CubicBezier` for sines) rather than densely sampling everything.
pub fn compile_curve(
    expr: &CurveExpr,
    span: (BeatTime, BeatTime),
    control_resolution: BeatDuration,
) -> Result<Vec<AutomationPoint>, CurveError> {
    let (start, end) = span;
    if start.0 >= end.0 {
        return Err(CurveError::EmptySpan);
    }
    if control_resolution.0 == 0 || control_resolution.0 > i64::MAX as u64 {
        return Err(CurveError::InvalidResolution);
    }
    validate(expr)?;

    // The curve signature deliberately has no tempo map. Consequently the
    // `*_hz` and envelope-second parameters use a nominal musical second of
    // one quarter note here; a caller that needs physical Hz should first
    // tempo-warp the term or its compiled points at its boundary.
    let duration_seconds = (end.0 - start.0) as f64 / PPQ as f64;
    let resolution = control_resolution.0 as i64;
    let mut ticks = Vec::new();
    let mut tick = start.0;
    while tick < end.0 {
        ticks.push(tick);
        tick = tick.saturating_add(resolution);
        if tick == i64::MAX {
            break;
        }
    }
    if ticks.last().copied() != Some(end.0) {
        ticks.push(end.0);
    }
    let outgoing = preferred_shape(expr);
    Ok(ticks
        .into_iter()
        .enumerate()
        .map(|(index, tick)| {
            let seconds = (tick - start.0) as f64 / PPQ as f64;
            AutomationPoint {
                id: AutomationPointId::from_raw(index as u64 + 1),
                position: TimePosition::Beats(AutomationBeatTime(tick)),
                value: evaluate_curve(expr, seconds, duration_seconds)
                    .expect("validated curve evaluates"),
                outgoing,
            }
        })
        .collect())
}

/// Evaluate at a nominal elapsed time inside a declared total span. This is
/// pure and useful for previews/tests independent of automation lane IDs.
pub fn evaluate_curve(
    expr: &CurveExpr,
    seconds: f64,
    duration_seconds: f64,
) -> Result<f64, CurveError> {
    validate(expr)?;
    if !seconds.is_finite() {
        return Err(CurveError::NonFinite("time"));
    }
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return Err(CurveError::EmptySpan);
    }
    eval(expr, seconds.clamp(0.0, duration_seconds), duration_seconds)
}

fn eval(expr: &CurveExpr, time: f64, duration: f64) -> Result<f64, CurveError> {
    Ok(match expr {
        CurveExpr::Const(value) => *value,
        CurveExpr::Line { from, to } => from + (to - from) * (time / duration),
        CurveExpr::Lfo {
            shape,
            rate_hz,
            depth,
            phase,
        } => {
            let turn = (time * rate_hz + phase).rem_euclid(1.0);
            let carrier = match shape {
                LfoShape::Sine => (turn * std::f64::consts::TAU).sin(),
                LfoShape::Triangle => 1.0 - 4.0 * (turn - 0.5).abs(),
                LfoShape::Square => {
                    if turn < 0.5 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                LfoShape::Saw => turn * 2.0 - 1.0,
            };
            carrier * depth
        }
        CurveExpr::Env {
            attack,
            decay,
            sustain,
            release,
        } => {
            let release_start = (duration - release).max(0.0);
            if *attack > 0.0 && time < *attack {
                time / attack
            } else if *decay > 0.0 && time < attack + decay {
                1.0 - (1.0 - sustain) * ((time - attack) / decay)
            } else if *release > 0.0 && time >= release_start {
                sustain * (1.0 - (time - release_start) / release).clamp(0.0, 1.0)
            } else {
                *sustain
            }
        }
        CurveExpr::Sum(members) => {
            let mut value = 0.0;
            for member in members {
                value += eval(member, time, duration)?;
            }
            value
        }
        CurveExpr::Scale {
            input,
            multiply,
            add,
        } => eval(input, time, duration)? * multiply + add,
        CurveExpr::Clamp { input, min, max } => eval(input, time, duration)?.clamp(*min, *max),
        CurveExpr::FromEvidence(id) => return Err(CurveError::UnresolvedEvidence(*id)),
    })
}

fn validate(expr: &CurveExpr) -> Result<(), CurveError> {
    let finite = |value: f64, name| {
        value
            .is_finite()
            .then_some(())
            .ok_or(CurveError::NonFinite(name))
    };
    match expr {
        CurveExpr::Const(value) => finite(*value, "constant"),
        CurveExpr::Line { from, to } => {
            finite(*from, "line from")?;
            finite(*to, "line to")
        }
        CurveExpr::Lfo {
            rate_hz,
            depth,
            phase,
            ..
        } => {
            finite(*rate_hz, "lfo rate")?;
            finite(*depth, "lfo depth")?;
            finite(*phase, "lfo phase")?;
            if *rate_hz < 0.0 {
                return Err(CurveError::NonFinite("negative lfo rate"));
            }
            Ok(())
        }
        CurveExpr::Env {
            attack,
            decay,
            sustain,
            release,
        } => {
            for (value, name) in [
                (*attack, "envelope attack"),
                (*decay, "envelope decay"),
                (*sustain, "envelope sustain"),
                (*release, "envelope release"),
            ] {
                finite(value, name)?;
            }
            if *attack < 0.0 || *decay < 0.0 || *release < 0.0 {
                return Err(CurveError::NonFinite("negative envelope time"));
            }
            Ok(())
        }
        CurveExpr::Sum(members) => {
            for member in members {
                validate(member)?;
            }
            Ok(())
        }
        CurveExpr::Scale {
            input,
            multiply,
            add,
        } => {
            validate(input)?;
            finite(*multiply, "scale multiply")?;
            finite(*add, "scale add")
        }
        CurveExpr::Clamp { input, min, max } => {
            validate(input)?;
            finite(*min, "clamp minimum")?;
            finite(*max, "clamp maximum")?;
            if min > max {
                return Err(CurveError::NonFinite("inverted clamp"));
            }
            Ok(())
        }
        CurveExpr::FromEvidence(id) => Err(CurveError::UnresolvedEvidence(*id)),
    }
}

fn preferred_shape(expr: &CurveExpr) -> SegmentShape {
    match expr {
        CurveExpr::Const(_)
        | CurveExpr::Lfo {
            shape: LfoShape::Square,
            ..
        } => SegmentShape::Hold,
        CurveExpr::Lfo {
            shape: LfoShape::Sine,
            ..
        } => SegmentShape::Smooth,
        _ => SegmentShape::Linear,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_compiles_with_declared_resolution_and_endpoint() {
        let points = compile_curve(
            &CurveExpr::Line { from: 0.0, to: 1.0 },
            (BeatTime(0), BeatTime(960)),
            BeatDuration(240),
        )
        .unwrap();
        assert_eq!(points.len(), 5);
        assert_eq!(points.first().unwrap().value, 0.0);
        assert_eq!(points.last().unwrap().value, 1.0);
        assert!(points
            .iter()
            .all(|point| point.outgoing == SegmentShape::Linear));
    }

    #[test]
    fn square_lfo_uses_hold_segments() {
        let points = compile_curve(
            &CurveExpr::Lfo {
                shape: LfoShape::Square,
                rate_hz: 1.0,
                depth: 0.5,
                phase: 0.0,
            },
            (BeatTime(0), BeatTime(960)),
            BeatDuration(120),
        )
        .unwrap();
        assert!(points
            .iter()
            .all(|point| point.outgoing == SegmentShape::Hold));
        assert!(points.iter().all(|point| point.value.abs() == 0.5));
    }
}
