//! Atomic execution state for a content-addressed deprojection plan.
//!
//! Native analyzers and isolated workers may finish in any order permitted by
//! the DAG. Their products become publishable only when every declared node has
//! completed under the current generation. Late completions may remain in the
//! artifact cache, but this coordinator refuses to attach them to a superseded
//! project run.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::artifact_catalog::ArtifactId;
use crate::deprojection_program::{
    DeprojectionNodeId, DeprojectionPlan, DeprojectionRunGuard, DeprojectionRunToken,
};

#[path = "deprojection_promotion.rs"]
pub mod promotion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeLease {
    pub run: DeprojectionRunToken,
    pub node: DeprojectionNodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageProduct {
    pub kind: String,
    pub artifact: ArtifactId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeExecutionStatus {
    Pending,
    Running,
    Completed(Vec<StageProduct>),
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedDeprojectionRun {
    pub token: DeprojectionRunToken,
    pub products: BTreeMap<DeprojectionNodeId, Vec<StageProduct>>,
}

#[derive(Clone, Debug)]
pub struct DeprojectionExecution {
    plan: DeprojectionPlan,
    guard: DeprojectionRunGuard,
    statuses: BTreeMap<DeprojectionNodeId, NodeExecutionStatus>,
}

impl DeprojectionExecution {
    pub fn new(plan: DeprojectionPlan) -> Self {
        let statuses = plan
            .nodes
            .iter()
            .map(|node| (node.id, NodeExecutionStatus::Pending))
            .collect();
        let guard = DeprojectionRunGuard::new(plan.id);
        Self {
            plan,
            guard,
            statuses,
        }
    }

    pub fn plan(&self) -> &DeprojectionPlan {
        &self.plan
    }

    pub fn token(&self) -> DeprojectionRunToken {
        self.guard.token()
    }

    pub fn cancellation(&self) -> crate::daw_render::RenderCancellation {
        self.guard.cancellation()
    }

    pub fn status(&self, node: DeprojectionNodeId) -> Option<&NodeExecutionStatus> {
        self.statuses.get(&node)
    }

    pub fn ready_nodes(&self) -> Vec<DeprojectionNodeId> {
        let completed = self
            .statuses
            .iter()
            .filter_map(|(id, status)| {
                matches!(status, NodeExecutionStatus::Completed(_)).then_some(*id)
            })
            .collect::<BTreeSet<_>>();
        self.plan
            .ready_nodes(&completed)
            .into_iter()
            .filter(|node| matches!(self.statuses.get(node), Some(NodeExecutionStatus::Pending)))
            .collect()
    }

    pub fn start(&mut self, node: DeprojectionNodeId) -> Result<NodeLease, ExecutionError> {
        if !self.ready_nodes().contains(&node) {
            return Err(ExecutionError::NodeNotReady(node));
        }
        self.statuses.insert(node, NodeExecutionStatus::Running);
        Ok(NodeLease {
            run: self.guard.token(),
            node,
        })
    }

    pub fn complete(
        &mut self,
        lease: NodeLease,
        mut products: Vec<StageProduct>,
    ) -> Result<(), ExecutionError> {
        self.validate_lease(lease)?;
        if !matches!(
            self.statuses.get(&lease.node),
            Some(NodeExecutionStatus::Running)
        ) {
            return Err(ExecutionError::NodeNotRunning(lease.node));
        }
        let node = self
            .plan
            .nodes
            .iter()
            .find(|node| node.id == lease.node)
            .ok_or(ExecutionError::UnknownNode(lease.node))?;
        products.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.artifact.cmp(&right.artifact))
        });
        if products.windows(2).any(|pair| pair[0].kind == pair[1].kind) {
            return Err(ExecutionError::DuplicateOutputKind(lease.node));
        }
        let actual = products
            .iter()
            .map(|product| product.kind.clone())
            .collect::<Vec<_>>();
        if actual != node.output_kinds {
            return Err(ExecutionError::OutputContractMismatch {
                node: lease.node,
                expected: node.output_kinds.clone(),
                actual,
            });
        }
        self.statuses
            .insert(lease.node, NodeExecutionStatus::Completed(products));
        Ok(())
    }

    pub fn fail(&mut self, lease: NodeLease, detail: String) -> Result<(), ExecutionError> {
        self.validate_lease(lease)?;
        if detail.trim().is_empty() {
            return Err(ExecutionError::EmptyFailureDetail);
        }
        if !matches!(
            self.statuses.get(&lease.node),
            Some(NodeExecutionStatus::Running)
        ) {
            return Err(ExecutionError::NodeNotRunning(lease.node));
        }
        self.statuses
            .insert(lease.node, NodeExecutionStatus::Failed(detail));
        Ok(())
    }

    /// Cancel the current generation. No current or later completion can make
    /// this run publishable after this call.
    pub fn cancel(&mut self) {
        self.guard.cancellation().cancel();
        for status in self.statuses.values_mut() {
            if matches!(
                status,
                NodeExecutionStatus::Pending | NodeExecutionStatus::Running
            ) {
                *status = NodeExecutionStatus::Cancelled;
            }
        }
    }

    /// Replace the entire plan and generation. Existing artifacts are left to
    /// the content cache; no status crosses the semantic boundary.
    pub fn supersede(&mut self, plan: DeprojectionPlan) -> DeprojectionRunToken {
        let token = self.guard.supersede(plan.id);
        self.statuses = plan
            .nodes
            .iter()
            .map(|node| (node.id, NodeExecutionStatus::Pending))
            .collect();
        self.plan = plan;
        token
    }

    /// Publication is all-or-nothing at the semantic layer. Individual node
    /// artifacts are immutable cache entries, but a partial run is not a
    /// deprojection result set.
    pub fn completed_run(&self) -> Result<CompletedDeprojectionRun, ExecutionError> {
        let token = self.guard.token();
        if !self.guard.accepts(token) {
            return Err(ExecutionError::CancelledRun);
        }
        let mut products = BTreeMap::new();
        for node in &self.plan.nodes {
            match self.statuses.get(&node.id) {
                Some(NodeExecutionStatus::Completed(node_products)) => {
                    products.insert(node.id, node_products.clone());
                }
                Some(NodeExecutionStatus::Failed(detail)) => {
                    return Err(ExecutionError::FailedNode {
                        node: node.id,
                        detail: detail.clone(),
                    });
                }
                _ => return Err(ExecutionError::IncompleteRun),
            }
        }
        Ok(CompletedDeprojectionRun { token, products })
    }

    fn validate_lease(&self, lease: NodeLease) -> Result<(), ExecutionError> {
        // Token freshness is the authority boundary. A superseding plan will
        // commonly have different node identities, but that must not turn a
        // late completion into the weaker/misleading "unknown node" case.
        if !self.guard.accepts(lease.run) {
            return Err(ExecutionError::StaleLease(lease.node));
        }
        if !self.statuses.contains_key(&lease.node) {
            return Err(ExecutionError::UnknownNode(lease.node));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionError {
    UnknownNode(DeprojectionNodeId),
    NodeNotReady(DeprojectionNodeId),
    NodeNotRunning(DeprojectionNodeId),
    StaleLease(DeprojectionNodeId),
    DuplicateOutputKind(DeprojectionNodeId),
    OutputContractMismatch {
        node: DeprojectionNodeId,
        expected: Vec<String>,
        actual: Vec<String>,
    },
    EmptyFailureDetail,
    IncompleteRun,
    CancelledRun,
    FailedNode {
        node: DeprojectionNodeId,
        detail: String,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownNode(node) => write!(formatter, "unknown deprojection node {node:?}"),
            Self::NodeNotReady(node) => {
                write!(formatter, "deprojection node {node:?} is not ready")
            }
            Self::NodeNotRunning(node) => {
                write!(formatter, "deprojection node {node:?} is not running")
            }
            Self::StaleLease(node) => write!(formatter, "stale lease for node {node:?}"),
            Self::DuplicateOutputKind(node) => {
                write!(
                    formatter,
                    "node {node:?} returned one output kind more than once"
                )
            }
            Self::OutputContractMismatch {
                node,
                expected,
                actual,
            } => write!(
                formatter,
                "node {node:?} output contract mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::EmptyFailureDetail => formatter.write_str("node failure detail is empty"),
            Self::IncompleteRun => formatter.write_str("deprojection run is incomplete"),
            Self::CancelledRun => formatter.write_str("deprojection run is cancelled"),
            Self::FailedNode { node, detail } => {
                write!(formatter, "deprojection node {node:?} failed: {detail}")
            }
        }
    }
}

impl std::error::Error for ExecutionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
    use crate::deprojection_program::{
        DeprojectionNode, DeprojectionStage, MaterialSpan, SourceClaimId,
    };

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [byte; 32])
    }

    fn artifact(byte: u8) -> ArtifactId {
        ArtifactId(digest(byte))
    }

    fn plan(recipe: u8) -> DeprojectionPlan {
        let source = MaterialSpan {
            material_sha256: "11".repeat(32),
            start_frame: 0,
            frame_count: 100,
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let claim = SourceClaimId(digest(3));
        let analysis = DeprojectionNode::new(
            DeprojectionStage::NativeRhythm {
                recipe: digest(recipe),
            },
            Vec::new(),
            vec![claim],
            vec!["rhythm".into()],
        )
        .unwrap();
        let patterns = DeprojectionNode::new(
            DeprojectionStage::SynthesizePatterns {
                recipe: digest(recipe.saturating_add(1)),
            },
            vec![analysis.id],
            vec![claim],
            vec!["patterns".into()],
        )
        .unwrap();
        DeprojectionPlan::new(source, vec![analysis, patterns]).unwrap()
    }

    #[test]
    fn dependencies_open_frontiers_and_complete_atomically() {
        let mut run = DeprojectionExecution::new(plan(10));
        let first = run.ready_nodes();
        assert_eq!(first.len(), 1);
        let first_lease = run.start(first[0]).unwrap();
        run.complete(
            first_lease,
            vec![StageProduct {
                kind: "rhythm".into(),
                artifact: artifact(20),
            }],
        )
        .unwrap();
        let second = run.ready_nodes();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            run.completed_run(),
            Err(ExecutionError::IncompleteRun)
        ));
        let second_lease = run.start(second[0]).unwrap();
        run.complete(
            second_lease,
            vec![StageProduct {
                kind: "patterns".into(),
                artifact: artifact(21),
            }],
        )
        .unwrap();
        assert_eq!(run.completed_run().unwrap().products.len(), 2);
    }

    #[test]
    fn stale_generation_completion_is_refused() {
        let mut run = DeprojectionExecution::new(plan(10));
        let lease = run.start(run.ready_nodes()[0]).unwrap();
        run.supersede(plan(30));
        assert!(matches!(
            run.complete(
                lease,
                vec![StageProduct {
                    kind: "rhythm".into(),
                    artifact: artifact(22),
                }]
            ),
            Err(ExecutionError::StaleLease(_))
        ));
    }

    #[test]
    fn wrong_output_schema_never_completes_a_node() {
        let mut run = DeprojectionExecution::new(plan(10));
        let lease = run.start(run.ready_nodes()[0]).unwrap();
        assert!(matches!(
            run.complete(
                lease,
                vec![StageProduct {
                    kind: "not-rhythm".into(),
                    artifact: artifact(23),
                }]
            ),
            Err(ExecutionError::OutputContractMismatch { .. })
        ));
        assert!(matches!(
            run.status(lease.node),
            Some(NodeExecutionStatus::Running)
        ));
    }

    #[test]
    fn cancellation_prevents_semantic_publication() {
        let mut run = DeprojectionExecution::new(plan(10));
        let lease = run.start(run.ready_nodes()[0]).unwrap();
        run.cancel();
        assert!(matches!(
            run.complete(
                lease,
                vec![StageProduct {
                    kind: "rhythm".into(),
                    artifact: artifact(24),
                }]
            ),
            Err(ExecutionError::StaleLease(_))
        ));
        assert!(matches!(
            run.completed_run(),
            Err(ExecutionError::CancelledRun)
        ));
    }
}
