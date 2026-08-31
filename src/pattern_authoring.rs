//! Application and persistence boundary for expression-backed patterns.
//!
//! `pattern_lang` stays a pure parser/evaluator. This module is the small
//! stateful edge that applies a term to a [`PatternDefinition`], refuses to
//! overwrite diverged work without an explicit policy, and defines the
//! versioned record a project codec can serialize. Older files omit that
//! record and decode to [`PatternOrigin::Authored`].

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::pattern_lang::{
    self, EvalContext, PatternEvalDiagnostic, PatternEvalError, PatternParseError, TermHash,
};
use crate::reconstruction::ReconstructionProposalId;
use crate::sequencer::{
    PatternContent, PatternDefinition, PatternOrigin, SampleAssetId, TriggerTarget,
};

pub const PATTERN_ORIGIN_CODEC_VERSION: u16 = 1;

/// Stable serde-facing adapter. Project codecs should keep this as an
/// optional member and use [`decode_pattern_origin`]; `None` is the explicit
/// backward-compatible old-file meaning, not an ad-hoc codec decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternOriginRecord {
    pub version: u16,
    #[serde(flatten)]
    pub value: PatternOriginRecordValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatternOriginRecordValue {
    Authored,
    Expression {
        source: String,
        /// Lowercase, zero-padded 32-digit hexadecimal.
        term_hash: String,
        /// Lowercase, zero-padded 32-digit hexadecimal.
        bindings_hash: String,
        bindings: BTreeMap<String, TriggerTargetRecord>,
        diverged: bool,
    },
    Deprojected {
        proposal: u64,
        diverged: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerTargetRecord {
    InstrumentNote { instrument: u64, key: u8 },
    DrumPad { rack: u64, pad: u16 },
    Sample { asset: u64 },
    AnalysisTemplate { template: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternOriginCodecError {
    UnsupportedVersion(u16),
    InvalidHash(String),
    InvalidExpression(PatternParseError),
    TermHashMismatch,
    BindingsHashMismatch,
}

impl fmt::Display for PatternOriginCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported pattern-origin version {version}")
            }
            Self::InvalidHash(value) => write!(formatter, "invalid pattern hash {value:?}"),
            Self::InvalidExpression(error) => {
                write!(formatter, "invalid stored expression: {error}")
            }
            Self::TermHashMismatch => {
                formatter.write_str("stored expression hash does not match source")
            }
            Self::BindingsHashMismatch => {
                formatter.write_str("stored bindings hash does not match bindings")
            }
        }
    }
}

impl Error for PatternOriginCodecError {}

pub fn encode_pattern_origin(origin: &PatternOrigin) -> PatternOriginRecord {
    let value = match origin {
        PatternOrigin::Authored => PatternOriginRecordValue::Authored,
        PatternOrigin::Expression {
            source,
            term_hash,
            bindings_hash,
            bindings,
            diverged,
        } => PatternOriginRecordValue::Expression {
            source: source.clone(),
            term_hash: format_hash(*term_hash),
            bindings_hash: format_hash(*bindings_hash),
            bindings: bindings
                .iter()
                .map(|(name, target)| (name.clone(), TriggerTargetRecord::from(target)))
                .collect(),
            diverged: *diverged,
        },
        PatternOrigin::Deprojected { proposal, diverged } => {
            PatternOriginRecordValue::Deprojected {
                proposal: proposal.get(),
                diverged: *diverged,
            }
        }
    };
    PatternOriginRecord {
        version: PATTERN_ORIGIN_CODEC_VERSION,
        value,
    }
}

pub fn decode_pattern_origin(
    record: Option<PatternOriginRecord>,
) -> Result<PatternOrigin, PatternOriginCodecError> {
    let Some(record) = record else {
        return Ok(PatternOrigin::Authored);
    };
    if record.version != PATTERN_ORIGIN_CODEC_VERSION {
        return Err(PatternOriginCodecError::UnsupportedVersion(record.version));
    }
    match record.value {
        PatternOriginRecordValue::Authored => Ok(PatternOrigin::Authored),
        PatternOriginRecordValue::Deprojected { proposal, diverged } => {
            Ok(PatternOrigin::Deprojected {
                proposal: ReconstructionProposalId::from_raw(proposal),
                diverged,
            })
        }
        PatternOriginRecordValue::Expression {
            source,
            term_hash,
            bindings_hash,
            bindings,
            diverged,
        } => {
            let stored_term = parse_hash(&term_hash)?;
            let stored_bindings = parse_hash(&bindings_hash)?;
            let term =
                pattern_lang::parse(&source).map_err(PatternOriginCodecError::InvalidExpression)?;
            if pattern_lang::term_hash(&term) != stored_term {
                return Err(PatternOriginCodecError::TermHashMismatch);
            }
            let bindings: BTreeMap<_, _> = bindings
                .into_iter()
                .map(|(name, target)| (name, TriggerTarget::from(target)))
                .collect();
            if pattern_lang::bindings_hash(&bindings) != stored_bindings {
                return Err(PatternOriginCodecError::BindingsHashMismatch);
            }
            Ok(PatternOrigin::Expression {
                source,
                term_hash: stored_term,
                bindings_hash: stored_bindings,
                bindings,
                diverged,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergedOverwrite {
    Refuse,
    Confirmed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionApplication {
    pub definition: PatternDefinition,
    pub diagnostics: Vec<PatternEvalDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternAuthoringError {
    NotStepPattern,
    Diverged,
    Parse(PatternParseError),
    Evaluate(PatternEvalError),
}

impl fmt::Display for PatternAuthoringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStepPattern => {
                formatter.write_str("expressions can only realize step patterns")
            }
            Self::Diverged => formatter.write_str(
                "the realized grid has manual edits; confirm replacement before regenerating",
            ),
            Self::Parse(error) => error.fmt(formatter),
            Self::Evaluate(error) => error.fmt(formatter),
        }
    }
}

impl Error for PatternAuthoringError {}

/// Parse, evaluate, and apply an expression to one pattern definition. The
/// stored grid is the cycle-zero preview; placement scheduling evaluates the
/// same source again at each real loop cycle.
pub fn apply_expression(
    before: &PatternDefinition,
    source: &str,
    bindings: BTreeMap<String, TriggerTarget>,
    overwrite: DivergedOverwrite,
) -> Result<ExpressionApplication, PatternAuthoringError> {
    if !matches!(before.content, PatternContent::Steps(_)) {
        return Err(PatternAuthoringError::NotStepPattern);
    }
    if before.origin.diverged() && overwrite == DivergedOverwrite::Refuse {
        return Err(PatternAuthoringError::Diverged);
    }
    let term = pattern_lang::parse(source).map_err(PatternAuthoringError::Parse)?;
    let output = pattern_lang::eval_steps(
        &term,
        &EvalContext {
            bindings: &bindings,
            cycle: before.length,
            seed: 0,
            cycle_index: 0,
        },
    )
    .map_err(PatternAuthoringError::Evaluate)?;

    let mut realized = output.pattern;
    // Retain lane behavior that notation intentionally does not describe.
    if let PatternContent::Steps(existing) = &before.content {
        for lane in realized.lanes.values_mut() {
            if let Some(prior) = existing
                .lanes
                .values()
                .find(|prior| prior.target == lane.target)
            {
                lane.choke_group = prior.choke_group;
            }
        }
    }

    let mut definition = before.clone();
    definition.content = PatternContent::Steps(realized);
    definition.origin = PatternOrigin::Expression {
        source: source.to_owned(),
        term_hash: pattern_lang::term_hash(&term),
        bindings_hash: pattern_lang::bindings_hash(&bindings),
        bindings,
        diverged: false,
    };
    definition.revision = before.revision.saturating_add(1);
    Ok(ExpressionApplication {
        definition,
        diagnostics: output.diagnostics,
    })
}

/// Best available editable binding table. Authored/deprojected patterns use
/// their current lane names as retargetable aliases.
pub fn bindings_for_pattern(pattern: &PatternDefinition) -> BTreeMap<String, TriggerTarget> {
    if let PatternOrigin::Expression { bindings, .. } = &pattern.origin {
        return bindings.clone();
    }
    match &pattern.content {
        PatternContent::Steps(steps) => {
            let mut bindings = BTreeMap::new();
            for lane in steps.lanes.values() {
                let base = binding_alias(&lane.name, lane.id.get());
                let mut alias = base.clone();
                let mut suffix = 2_u32;
                while bindings.contains_key(&alias) {
                    alias = format!("{base}-{suffix}");
                    suffix = suffix.saturating_add(1);
                }
                bindings.insert(alias, lane.target.clone());
            }
            bindings
        }
        PatternContent::Notes(_) => BTreeMap::new(),
    }
}

fn binding_alias(name: &str, lane_id: u64) -> String {
    let mut alias = String::new();
    let mut separator = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_' {
            if separator && !alias.is_empty() {
                alias.push('-');
            }
            separator = false;
            alias.push(character);
        } else {
            separator = true;
        }
    }
    while alias.ends_with('-') {
        alias.pop();
    }
    if alias.is_empty() || !alias.as_bytes()[0].is_ascii_lowercase() {
        format!("lane-{lane_id}")
    } else {
        alias
    }
}

pub fn format_diagnostic(diagnostic: PatternEvalDiagnostic) -> String {
    match diagnostic {
        PatternEvalDiagnostic::RoundedToTick {
            at_tick,
            error_milliticks,
        } => format!("rounded near tick {at_tick} by {error_milliticks} milliticks"),
        PatternEvalDiagnostic::RatchetSpacingTruncated {
            at_tick,
            remainder_ticks,
        } => format!("ratchet spacing at tick {at_tick} drops a {remainder_ticks}-tick remainder"),
    }
}

fn format_hash(hash: TermHash) -> String {
    format!("{:032x}", hash.0)
}

fn parse_hash(value: &str) -> Result<TermHash, PatternOriginCodecError> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PatternOriginCodecError::InvalidHash(value.to_owned()));
    }
    u128::from_str_radix(value, 16)
        .map(TermHash)
        .map_err(|_| PatternOriginCodecError::InvalidHash(value.to_owned()))
}

impl From<&TriggerTarget> for TriggerTargetRecord {
    fn from(value: &TriggerTarget) -> Self {
        match value {
            TriggerTarget::InstrumentNote { instrument, key } => Self::InstrumentNote {
                instrument: *instrument,
                key: *key,
            },
            TriggerTarget::DrumPad { rack, pad } => Self::DrumPad {
                rack: *rack,
                pad: *pad,
            },
            TriggerTarget::Sample(asset) => Self::Sample { asset: asset.get() },
            TriggerTarget::AnalysisTemplate(template) => Self::AnalysisTemplate {
                template: *template,
            },
        }
    }
}

impl From<TriggerTargetRecord> for TriggerTarget {
    fn from(value: TriggerTargetRecord) -> Self {
        match value {
            TriggerTargetRecord::InstrumentNote { instrument, key } => {
                Self::InstrumentNote { instrument, key }
            }
            TriggerTargetRecord::DrumPad { rack, pad } => Self::DrumPad { rack, pad },
            TriggerTargetRecord::Sample { asset } => Self::Sample(SampleAssetId::from_raw(asset)),
            TriggerTargetRecord::AnalysisTemplate { template } => Self::AnalysisTemplate(template),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::{BeatDuration, PatternId, StepPattern};

    fn pattern(origin: PatternOrigin) -> PatternDefinition {
        PatternDefinition {
            id: PatternId::from_raw(1),
            name: "test".into(),
            length: BeatDuration(3_840),
            content: PatternContent::Steps(StepPattern {
                resolution: BeatDuration(240),
                swing: 0.0,
                lanes: BTreeMap::new(),
            }),
            origin,
            revision: 0,
        }
    }

    #[test]
    fn missing_codec_origin_means_authored() {
        assert_eq!(
            decode_pattern_origin(None).unwrap(),
            PatternOrigin::Authored
        );
    }

    #[test]
    fn expression_record_round_trips_with_binding_integrity() {
        let before = pattern(PatternOrigin::Authored);
        let bindings = BTreeMap::from([
            ("a".into(), TriggerTarget::AnalysisTemplate(7)),
            ("b".into(), TriggerTarget::AnalysisTemplate(9)),
        ]);
        let applied = apply_expression(
            &before,
            "swing(0.25, <a b>^0.7)",
            bindings,
            DivergedOverwrite::Refuse,
        )
        .unwrap();
        let record = encode_pattern_origin(&applied.definition.origin);
        assert_eq!(
            decode_pattern_origin(Some(record)).unwrap(),
            applied.definition.origin
        );
    }

    #[test]
    fn diverged_realization_needs_explicit_overwrite() {
        let mut before = pattern(PatternOrigin::Authored);
        let bindings = BTreeMap::from([("a".into(), TriggerTarget::AnalysisTemplate(7))]);
        before = apply_expression(&before, "a", bindings.clone(), DivergedOverwrite::Refuse)
            .unwrap()
            .definition;
        before.origin.mark_diverged();
        assert_eq!(
            apply_expression(&before, "a a", bindings, DivergedOverwrite::Refuse).unwrap_err(),
            PatternAuthoringError::Diverged
        );
    }
}
