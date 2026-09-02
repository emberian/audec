//! AIR query combinators: typed questions with derivations (skeleton).
//!
//! Normative design: `docs/LANGUAGES.md` §4. Implementation: QUERY
//! workstream in `docs/archive/SWARM_PLAN.md`. Queries are pure, deterministic,
//! stratified, and terminating; every returned fact names the premises that
//! admitted it. A query result is evidence-linked observation, not asserted
//! truth, and `ontology` remains the sole owner of the fact base.
use std::collections::{BTreeMap, BTreeSet};

use crate::aspect::{Aspect, AspectResolver, ConcreteAspect};
use crate::ontology;
use crate::reconstruction::ReconstructionProposalId;

#[path = "reading_query_workbench.rs"]
pub mod workbench;

/// A typed reference into the AIR fact base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FactRef {
    Object(ontology::ObjectId),
    Source(ontology::SourceId),
    Parameter(ontology::ParameterId),
    Hypothesis(ontology::HypothesisId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    Cancelled,
}

/// Read-only cancellation seam shared by GUI tasks and headless clients.
pub trait QueryCancellation {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancel;

impl QueryCancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Deterministic result order: stable sort by typed reference.
pub fn run(
    query: &Query,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
) -> Result<Vec<(FactRef, Derivation)>, QueryError> {
    run_cancellable(query, facts, resolver, &NeverCancel)
}

pub fn run_cancellable(
    query: &Query,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
    cancellation: &dyn QueryCancellation,
) -> Result<Vec<(FactRef, Derivation)>, QueryError> {
    check_cancelled(cancellation)?;
    let universe = fact_universe(facts, cancellation)?;
    let matches = evaluate_query(query, facts, resolver, &universe, cancellation)?;
    check_cancelled(cancellation)?;
    Ok(matches.into_iter().collect())
}

fn fact_universe(
    facts: &dyn AirFacts,
    cancellation: &dyn QueryCancellation,
) -> Result<BTreeSet<FactRef>, QueryError> {
    let mut universe = BTreeSet::new();
    for kind in [
        FactKind::Object,
        FactKind::Source,
        FactKind::Parameter,
        FactKind::Hypothesis,
    ] {
        for fact in facts.facts(kind) {
            check_cancelled(cancellation)?;
            universe.insert(fact);
        }
    }
    Ok(universe)
}

fn evaluate_query(
    query: &Query,
    facts: &dyn AirFacts,
    resolver: &dyn AspectResolver,
    universe: &BTreeSet<FactRef>,
    cancellation: &dyn QueryCancellation,
) -> Result<BTreeMap<FactRef, Derivation>, QueryError> {
    check_cancelled(cancellation)?;
    match query {
        Query::Kind(kind) => Ok(facts
            .facts(*kind)
            .into_iter()
            .filter(|fact| !cancellation.is_cancelled() && universe.contains(fact))
            .map(|fact| {
                (
                    fact,
                    Derivation {
                        rule: "kind",
                        premises: vec![fact],
                    },
                )
            })
            .collect()),
        Query::Within(aspect) => {
            let selection =
                crate::aspect::evaluate(aspect, resolver).map_err(QueryError::Aspect)?;
            Ok(universe
                .iter()
                .copied()
                .filter(|fact| {
                    !cancellation.is_cancelled()
                        && facts
                            .extent(*fact)
                            .is_some_and(|extent| concrete_overlaps(&selection, &extent))
                })
                .map(|fact| {
                    (
                        fact,
                        Derivation {
                            rule: "within",
                            premises: vec![fact],
                        },
                    )
                })
                .collect())
        }
        Query::Related { to } => {
            let targets = evaluate_query(to, facts, resolver, universe, cancellation)?;
            Ok(universe
                .iter()
                .copied()
                .filter_map(|fact| {
                    if cancellation.is_cancelled() {
                        return None;
                    }
                    let mut premises = facts
                        .related(fact)
                        .into_iter()
                        .filter(|related| targets.contains_key(related))
                        .collect::<Vec<_>>();
                    premises.sort();
                    premises.dedup();
                    (!premises.is_empty()).then_some((
                        fact,
                        Derivation {
                            rule: "related",
                            premises,
                        },
                    ))
                })
                .collect())
        }
        Query::NotExplainedBy(proposal) => {
            let residual = crate::aspect::evaluate(
                &Aspect::ResidualOf(crate::aspect::ExplanationRef::Proposal(*proposal)),
                resolver,
            )
            .map_err(QueryError::Aspect)?;
            Ok(universe
                .iter()
                .copied()
                .filter(|fact| {
                    !cancellation.is_cancelled()
                        && facts
                            .extent(*fact)
                            .is_some_and(|extent| concrete_overlaps(&residual, &extent))
                })
                .map(|fact| {
                    (
                        fact,
                        Derivation {
                            rule: "not-explained-by",
                            premises: vec![fact],
                        },
                    )
                })
                .collect())
        }
        Query::And(children) => {
            if children.is_empty() {
                return Ok(universe
                    .iter()
                    .copied()
                    .map(|fact| {
                        (
                            fact,
                            Derivation {
                                rule: "and-identity",
                                premises: vec![fact],
                            },
                        )
                    })
                    .collect());
            }
            let mut child_results = Vec::with_capacity(children.len());
            for child in children {
                child_results.push(evaluate_query(
                    child,
                    facts,
                    resolver,
                    universe,
                    cancellation,
                )?);
            }
            Ok(universe
                .iter()
                .copied()
                .filter_map(|fact| {
                    if cancellation.is_cancelled() {
                        return None;
                    }
                    child_results
                        .iter()
                        .all(|result| result.contains_key(&fact))
                        .then(|| {
                            let premises = merged_premises(
                                fact,
                                child_results.iter().filter_map(|result| result.get(&fact)),
                            );
                            (
                                fact,
                                Derivation {
                                    rule: "and",
                                    premises,
                                },
                            )
                        })
                })
                .collect())
        }
        Query::Or(children) => {
            let mut admitted: BTreeMap<FactRef, Vec<Derivation>> = BTreeMap::new();
            for child in children {
                for (fact, derivation) in
                    evaluate_query(child, facts, resolver, universe, cancellation)?
                {
                    check_cancelled(cancellation)?;
                    admitted.entry(fact).or_default().push(derivation);
                }
            }
            Ok(admitted
                .into_iter()
                .map(|(fact, derivations)| {
                    (
                        fact,
                        Derivation {
                            rule: "or",
                            premises: merged_premises(fact, derivations.iter()),
                        },
                    )
                })
                .collect())
        }
        Query::Not(child) => {
            let excluded = evaluate_query(child, facts, resolver, universe, cancellation)?;
            Ok(universe
                .iter()
                .copied()
                .filter(|fact| !cancellation.is_cancelled() && !excluded.contains_key(fact))
                .map(|fact| {
                    (
                        fact,
                        Derivation {
                            rule: "not",
                            premises: vec![fact],
                        },
                    )
                })
                .collect())
        }
    }
}

fn check_cancelled(cancellation: &dyn QueryCancellation) -> Result<(), QueryError> {
    if cancellation.is_cancelled() {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}

fn merged_premises<'a>(
    fact: FactRef,
    derivations: impl IntoIterator<Item = &'a Derivation>,
) -> Vec<FactRef> {
    let mut premises = derivations
        .into_iter()
        .flat_map(|derivation| derivation.premises.iter().copied())
        .collect::<Vec<_>>();
    if premises.is_empty() {
        premises.push(fact);
    }
    premises.sort();
    premises.dedup();
    premises
}

fn concrete_overlaps(left: &ConcreteAspect, right: &ConcreteAspect) -> bool {
    left.regions.iter().any(|left| {
        right
            .regions
            .iter()
            .any(|right| left.intersect(*right).is_some())
    })
}
