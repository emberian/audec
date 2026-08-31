//! Persistent explanation and comparison domain.
//!
//! This store owns semantic recipes and recorded observations, never rendered
//! PCM, analysis payloads, cache entries, or UI selection. Put-style commands
//! make later command-envelope integration mechanical and preserve stale/dead
//! comparisons as inspectable records rather than silently dropping them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::comparison::{ComparisonDefinition, ComparisonId, ComparisonObservation};
use crate::explanation::{ExplanationDefinition, ExplanationId, ExplanationScope};

#[derive(Clone, Debug, PartialEq)]
pub struct InterpretationStore {
    explanations: BTreeMap<ExplanationId, ExplanationDefinition>,
    comparisons: BTreeMap<ComparisonId, ComparisonDefinition>,
    observations: BTreeMap<ComparisonId, ComparisonObservation>,
    next_explanation_id: u64,
    next_comparison_id: u64,
}

impl Default for InterpretationStore {
    fn default() -> Self {
        Self {
            explanations: BTreeMap::new(),
            comparisons: BTreeMap::new(),
            observations: BTreeMap::new(),
            next_explanation_id: 1,
            next_comparison_id: 1,
        }
    }
}

impl InterpretationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn explanations(&self) -> &BTreeMap<ExplanationId, ExplanationDefinition> {
        &self.explanations
    }

    pub fn comparisons(&self) -> &BTreeMap<ComparisonId, ComparisonDefinition> {
        &self.comparisons
    }

    pub fn observations(&self) -> &BTreeMap<ComparisonId, ComparisonObservation> {
        &self.observations
    }

    pub fn explanation(&self, id: ExplanationId) -> Option<&ExplanationDefinition> {
        self.explanations.get(&id)
    }

    pub fn comparison(&self, id: ComparisonId) -> Option<&ComparisonDefinition> {
        self.comparisons.get(&id)
    }

    pub fn observation(&self, id: ComparisonId) -> Option<&ComparisonObservation> {
        self.observations.get(&id)
    }

    pub fn allocate_explanation_id(&mut self) -> Result<ExplanationId, InterpretationError> {
        let id = ExplanationId(self.next_explanation_id);
        self.next_explanation_id = self
            .next_explanation_id
            .checked_add(1)
            .ok_or(InterpretationError::IdentityExhausted)?;
        Ok(id)
    }

    pub fn allocate_comparison_id(&mut self) -> Result<ComparisonId, InterpretationError> {
        let id = ComparisonId(self.next_comparison_id);
        self.next_comparison_id = self
            .next_comparison_id
            .checked_add(1)
            .ok_or(InterpretationError::IdentityExhausted)?;
        Ok(id)
    }

    /// Apply atomically to a candidate, validate the whole interpretation
    /// graph, then publish. Returned commands are the exact inverse.
    pub fn apply(
        &mut self,
        commands: &[InterpretationCommand],
    ) -> Result<Vec<InterpretationCommand>, InterpretationError> {
        if commands.is_empty() {
            return Err(InterpretationError::EmptyCommandBatch);
        }
        let mut candidate = self.clone();
        for (index, command) in commands.iter().enumerate() {
            candidate
                .apply_one(command)
                .map_err(|error| InterpretationError::Command {
                    index,
                    detail: error.to_string(),
                })?;
        }
        let issues = candidate.validate();
        if !issues.is_empty() {
            return Err(InterpretationError::Invalid(issues));
        }
        *self = candidate;
        Ok(commands
            .iter()
            .rev()
            .map(InterpretationCommand::inverse)
            .collect())
    }

    pub fn validate(&self) -> Vec<InterpretationValidationIssue> {
        let mut issues = Vec::new();
        for (id, definition) in &self.explanations {
            let path = format!("explanations[{}]", id.0);
            if definition.id != *id {
                issues.push(InterpretationValidationIssue::new(
                    &path,
                    "map key and embedded identity differ",
                ));
            }
            let mut normalized = definition.clone();
            if let Err(error) = normalized.normalize_and_validate() {
                issues.push(InterpretationValidationIssue::new(&path, error.to_string()));
            } else if &normalized != definition {
                issues.push(InterpretationValidationIssue::new(
                    &path,
                    "definition is not in canonical form",
                ));
            }
            if let ExplanationScope::Group(members) = &definition.scope {
                for member in members {
                    if !self.explanations.contains_key(member) {
                        issues.push(InterpretationValidationIssue::new(
                            &path,
                            format!("group member {} is missing", member.0),
                        ));
                    }
                }
            }
        }
        validate_group_cycles(&self.explanations, &mut issues);

        for (id, comparison) in &self.comparisons {
            let path = format!("comparisons[{}]", id.0);
            if comparison.id != *id {
                issues.push(InterpretationValidationIssue::new(
                    &path,
                    "map key and embedded identity differ",
                ));
            }
            if let Err(error) = comparison.validate() {
                issues.push(InterpretationValidationIssue::new(&path, error.to_string()));
            }
            if !self.explanations.contains_key(&comparison.explanation) {
                issues.push(InterpretationValidationIssue::new(
                    &path,
                    format!("explanation {} is missing", comparison.explanation.0),
                ));
            }
        }
        for comparison in self.observations.keys() {
            if !self.comparisons.contains_key(comparison) {
                issues.push(InterpretationValidationIssue::new(
                    format!("observations[{}]", comparison.0),
                    "observation has no comparison definition",
                ));
            }
        }
        if self.next_explanation_id == 0
            || self
                .explanations
                .keys()
                .any(|id| id.0 >= self.next_explanation_id)
        {
            issues.push(InterpretationValidationIssue::new(
                "next_explanation_id",
                "allocator does not exceed every explanation identity",
            ));
        }
        if self.next_comparison_id == 0
            || self
                .comparisons
                .keys()
                .any(|id| id.0 >= self.next_comparison_id)
        {
            issues.push(InterpretationValidationIssue::new(
                "next_comparison_id",
                "allocator does not exceed every comparison identity",
            ));
        }
        issues
    }

    fn apply_one(&mut self, command: &InterpretationCommand) -> Result<(), PutError> {
        match command {
            InterpretationCommand::PutExplanation { before, after } => {
                let id = matching_id(
                    before.as_ref().map(|value| value.id),
                    after.as_ref().map(|value| value.id),
                )?;
                put(&mut self.explanations, id, before.as_ref(), after.as_ref())?;
                self.next_explanation_id = self
                    .next_explanation_id
                    .max(id.0.checked_add(1).ok_or(PutError::IdentityExhausted)?);
            }
            InterpretationCommand::PutComparison { before, after } => {
                let id = matching_id(
                    before.as_ref().map(|value| value.id),
                    after.as_ref().map(|value| value.id),
                )?;
                put(&mut self.comparisons, id, before.as_ref(), after.as_ref())?;
                self.next_comparison_id = self
                    .next_comparison_id
                    .max(id.0.checked_add(1).ok_or(PutError::IdentityExhausted)?);
            }
            InterpretationCommand::PutObservation {
                comparison,
                before,
                after,
            } => put(
                &mut self.observations,
                *comparison,
                before.as_ref(),
                after.as_ref(),
            )?,
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InterpretationCommand {
    PutExplanation {
        before: Option<ExplanationDefinition>,
        after: Option<ExplanationDefinition>,
    },
    PutComparison {
        before: Option<ComparisonDefinition>,
        after: Option<ComparisonDefinition>,
    },
    PutObservation {
        comparison: ComparisonId,
        before: Option<ComparisonObservation>,
        after: Option<ComparisonObservation>,
    },
}

impl InterpretationCommand {
    pub fn inverse(&self) -> Self {
        match self {
            Self::PutExplanation { before, after } => Self::PutExplanation {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutComparison { before, after } => Self::PutComparison {
                before: after.clone(),
                after: before.clone(),
            },
            Self::PutObservation {
                comparison,
                before,
                after,
            } => Self::PutObservation {
                comparison: *comparison,
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

fn matching_id<T: Copy + PartialEq>(before: Option<T>, after: Option<T>) -> Result<T, PutError> {
    match (before, after) {
        (None, None) => Err(PutError::Empty),
        (Some(id), None) | (None, Some(id)) => Ok(id),
        (Some(left), Some(right)) if left == right => Ok(left),
        (Some(_), Some(_)) => Err(PutError::IdentityMismatch),
    }
}

fn put<K, V>(
    values: &mut BTreeMap<K, V>,
    id: K,
    before: Option<&V>,
    after: Option<&V>,
) -> Result<(), PutError>
where
    K: Copy + Ord,
    V: Clone + PartialEq,
{
    if values.get(&id) != before {
        return Err(PutError::Stale);
    }
    match after {
        Some(value) => {
            values.insert(id, value.clone());
        }
        None => {
            values.remove(&id);
        }
    }
    Ok(())
}

fn validate_group_cycles(
    definitions: &BTreeMap<ExplanationId, ExplanationDefinition>,
    issues: &mut Vec<InterpretationValidationIssue>,
) {
    fn visit(
        id: ExplanationId,
        definitions: &BTreeMap<ExplanationId, ExplanationDefinition>,
        visiting: &mut BTreeSet<ExplanationId>,
        visited: &mut BTreeSet<ExplanationId>,
    ) -> bool {
        if visited.contains(&id) {
            return false;
        }
        if !visiting.insert(id) {
            return true;
        }
        let cyclic = definitions.get(&id).is_some_and(|definition| {
            if let ExplanationScope::Group(members) = &definition.scope {
                members.iter().copied().any(|member| {
                    definitions.contains_key(&member)
                        && visit(member, definitions, visiting, visited)
                })
            } else {
                false
            }
        });
        visiting.remove(&id);
        visited.insert(id);
        cyclic
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in definitions.keys().copied() {
        if visit(id, definitions, &mut visiting, &mut visited) {
            issues.push(InterpretationValidationIssue::new(
                format!("explanations[{}]", id.0),
                "explanation groups contain a cycle",
            ));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretationValidationIssue {
    pub path: String,
    pub message: String,
}

impl InterpretationValidationIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpretationError {
    IdentityExhausted,
    EmptyCommandBatch,
    Command { index: usize, detail: String },
    Invalid(Vec<InterpretationValidationIssue>),
}

impl fmt::Display for InterpretationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityExhausted => formatter.write_str("interpretation identity exhausted"),
            Self::EmptyCommandBatch => formatter.write_str("interpretation command batch is empty"),
            Self::Command { index, detail } => {
                write!(formatter, "interpretation command {index} failed: {detail}")
            }
            Self::Invalid(issues) => write!(
                formatter,
                "interpretation store has {} validation issue(s)",
                issues.len()
            ),
        }
    }
}

impl std::error::Error for InterpretationError {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PutError {
    Empty,
    IdentityMismatch,
    IdentityExhausted,
    Stale,
}

impl fmt::Display for PutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("put has neither before nor after"),
            Self::IdentityMismatch => formatter.write_str("put identities differ"),
            Self::IdentityExhausted => formatter.write_str("identity exhausted"),
            Self::Stale => formatter.write_str("before value does not match current value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{Aspect, ChannelMask, ExplanationRef};
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::comparison::SourceCitation;
    use crate::explanation::ExplanationScope;
    use crate::ontology::{Producer, Provenance};

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn explanation(id: u64) -> ExplanationDefinition {
        ExplanationDefinition {
            id: ExplanationId(id),
            label: format!("explanation {id}"),
            scope: ExplanationScope::Group(vec![ExplanationId(id + 1)]),
            extent: Aspect::ExplainedBy(ExplanationRef::Definition(id)),
            evidence: Vec::new(),
            provenance: provenance(),
        }
    }

    #[test]
    fn put_batch_is_atomic_and_inverse_restores_values_without_reusing_ids() {
        let mut store = InterpretationStore::new();
        let leaf = ExplanationDefinition {
            id: ExplanationId(2),
            label: "leaf".into(),
            scope: ExplanationScope::ArrangementClip(crate::arrangement::ClipId::from_raw(1)),
            extent: Aspect::All,
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let commands = vec![
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(leaf),
            },
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(explanation(1)),
            },
        ];
        let inverse = store.apply(&commands).unwrap();
        assert_eq!(store.explanations.len(), 2);
        let next = store.next_explanation_id;
        store.apply(&inverse).unwrap();
        assert!(store.explanations.is_empty());
        assert_eq!(store.next_explanation_id, next);
    }

    #[test]
    fn comparison_cannot_commit_without_its_explanation() {
        let comparison = ComparisonDefinition {
            id: ComparisonId(1),
            label: "orphan".into(),
            source: SourceCitation {
                asset: AssetId(1),
                source_range: AssetFrameRange {
                    start: SampleFrames(0),
                    end: SampleFrames(10),
                },
                project_span: crate::aspect::FrameSpan { start: 0, end: 10 },
                channels: ChannelMask(1),
            },
            explanation: ExplanationId(9),
            provenance: provenance(),
        };
        let mut store = InterpretationStore::new();
        let error = store
            .apply(&[InterpretationCommand::PutComparison {
                before: None,
                after: Some(comparison),
            }])
            .unwrap_err();
        assert!(matches!(error, InterpretationError::Invalid(_)));
        assert!(store.comparisons.is_empty());
    }
}
