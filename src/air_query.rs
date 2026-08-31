//! AIR query combinators: typed questions with derivations (skeleton).
//!
//! Normative design: `docs/LANGUAGES.md` §4. Implementation: QUERY
//! workstream in `docs/SWARM_PLAN.md`. Queries are pure, deterministic,
//! stratified, and terminating; every returned fact names the premises that
//! admitted it. A query result is evidence-linked observation, not asserted
//! truth, and `ontology` remains the sole owner of the fact base.
#![allow(dead_code, unused_variables)]

use crate::aspect::{Aspect, AspectResolver, ConcreteAspect};
use crate::ontology;
use crate::reconstruction::ReconstructionProposalId;

/// A typed reference into the AIR fact base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FactRef {
    Object(ontology::ObjectId),
    Source(ontology::SourceId),
    Parameter(ontology::ParameterId),
    Hypothesis(ontology::HypothesisId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactKind {
    Object,
    Source,
    Parameter,
    Hypothesis,
}

/// Read-only fact views supplied by the project. Kind-filtered relation
/// traversal is added by the QUERY lane against `ontology::RelationKind`'s
/// real (data-carrying) variants; the skeleton keeps traversal unfiltered
/// rather than inventing a mirror of that enum.
pub trait AirFacts {
    fn facts(&self, kind: FactKind) -> Vec<FactRef>;
    fn evidence_of(&self, fact: FactRef) -> Vec<FactRef>;
    fn related(&self, fact: FactRef) -> Vec<FactRef>;
    fn extent(&self, fact: FactRef) -> Option<ConcreteAspect>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Kind(FactKind),
    Within(Aspect),
    Related {
        to: Box<Query>,
    },
    NotExplainedBy(ReconstructionProposalId),
    And(Vec<Query>),
    Or(Vec<Query>),
    /// Stratified: evaluated against the finite fact universe only.
    Not(Box<Query>),
}

/// Why a fact was admitted. Provenance completeness is the module's gate.
#[derive(Clone, Debug, PartialEq)]
pub struct Derivation {
    pub rule: &'static str,
    pub premises: Vec<FactRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryError {
    Aspect(crate::aspect::AspectError),
    UnresolvableReference(String),
}

/// Deterministic result order: stable sort by typed reference.
pub fn run(
    query: &Query,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
) -> Result<Vec<(FactRef, Derivation)>, QueryError> {
    todo!("QUERY lane: docs/LANGUAGES.md section 4")
}
