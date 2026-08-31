//! Explicit cross-view link groups for semantic navigation and selection.
//!
//! Transport is global and is not linked here. Viewports, frequency ranges,
//! selections, edit cursors, and follow policy remain local unless both the
//! publishing and receiving views opt into the corresponding facet. Linking a
//! hypothesis selection shares attention, not acceptance or project truth.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::aspect::{BandSpan, FrameSpan};
use crate::project_selection::{EditCursor, ProjectSelection};
pub use crate::workspace_document::{LinkFacets, LinkGroupId, ViewLinkMembership, WorkspaceViewId};

/// A patch must distinguish "leave local value alone" from "clear the linked
/// value". `Option<T>` alone cannot express that distinction.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum FacetPatch<T> {
    #[default]
    Unchanged,
    Clear,
    Set(T),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkedViewPatch {
    pub time: FacetPatch<FrameSpan>,
    pub frequency: FacetPatch<BandSpan>,
    pub selection: FacetPatch<ProjectSelection>,
    pub edit_cursor: FacetPatch<EditCursor>,
    pub follow: FacetPatch<bool>,
}

impl LinkedViewPatch {
    pub fn touched_facets(&self) -> LinkFacets {
        let mut facets = LinkFacets::NONE;
        if !matches!(self.time, FacetPatch::Unchanged) {
            facets = facets.union(LinkFacets::TIME);
        }
        if !matches!(self.frequency, FacetPatch::Unchanged) {
            facets = facets.union(LinkFacets::FREQUENCY);
        }
        if !matches!(self.selection, FacetPatch::Unchanged) {
            facets = facets.union(LinkFacets::SELECTION);
        }
        if !matches!(self.edit_cursor, FacetPatch::Unchanged) {
            facets = facets.union(LinkFacets::EDIT_CURSOR);
        }
        if !matches!(self.follow, FacetPatch::Unchanged) {
            facets = facets.union(LinkFacets::FOLLOW);
        }
        facets
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkedViewState {
    pub time: Option<FrameSpan>,
    pub frequency: Option<BandSpan>,
    pub selection: Option<ProjectSelection>,
    pub edit_cursor: Option<EditCursor>,
    pub follow: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ViewLinkDelivery {
    pub recipient: WorkspaceViewId,
    pub group: LinkGroupId,
    pub revision: u64,
    pub facets: LinkFacets,
    pub state: LinkedViewState,
}

#[derive(Clone, Debug, Default)]
struct GroupState {
    revision: u64,
    state: LinkedViewState,
}

/// Pure router. A GPUI `ProjectSession` entity can emit one event per returned
/// delivery, while a headless host can consume the same deterministic list.
#[derive(Clone, Debug, Default)]
pub struct ViewLinkRegistry {
    memberships: BTreeMap<WorkspaceViewId, ViewLinkMembership>,
    groups: BTreeMap<LinkGroupId, GroupState>,
}

impl ViewLinkRegistry {
    pub fn register(
        &mut self,
        view: WorkspaceViewId,
        membership: ViewLinkMembership,
    ) -> Result<(), ViewLinkError> {
        if view.0 == 0 {
            return Err(ViewLinkError::ZeroViewId);
        }
        if membership.group == LinkGroupId::UNLINKED && !membership.facets.is_empty() {
            return Err(ViewLinkError::UnlinkedFacets(view));
        }
        self.memberships.insert(view, membership);
        if membership.group != LinkGroupId::UNLINKED {
            self.groups.entry(membership.group).or_default();
        }
        Ok(())
    }

    pub fn unregister(&mut self, view: WorkspaceViewId) -> bool {
        self.memberships.remove(&view).is_some()
    }

    pub fn membership(&self, view: WorkspaceViewId) -> Option<ViewLinkMembership> {
        self.memberships.get(&view).copied()
    }

    pub fn group_state(&self, group: LinkGroupId) -> Option<(u64, &LinkedViewState)> {
        self.groups
            .get(&group)
            .map(|state| (state.revision, &state.state))
    }

    pub fn publish(
        &mut self,
        source: WorkspaceViewId,
        patch: LinkedViewPatch,
    ) -> Result<Vec<ViewLinkDelivery>, ViewLinkError> {
        let membership = self
            .memberships
            .get(&source)
            .copied()
            .ok_or(ViewLinkError::UnknownView(source))?;
        if membership.group == LinkGroupId::UNLINKED {
            return Ok(Vec::new());
        }
        let touched = patch.touched_facets();
        if !membership.facets.contains(touched) {
            return Err(ViewLinkError::FacetNotPublished {
                view: source,
                attempted: touched,
                allowed: membership.facets,
            });
        }
        if touched.is_empty() {
            return Ok(Vec::new());
        }

        let group = self.groups.entry(membership.group).or_default();
        apply(&mut group.state.time, patch.time);
        apply(&mut group.state.frequency, patch.frequency);
        apply(&mut group.state.selection, patch.selection);
        apply(&mut group.state.edit_cursor, patch.edit_cursor);
        apply(&mut group.state.follow, patch.follow);
        group.revision = group.revision.wrapping_add(1);

        let revision = group.revision;
        let state = group.state.clone();
        Ok(self
            .memberships
            .iter()
            .filter_map(|(view, recipient)| {
                (*view != source
                    && recipient.group == membership.group
                    && !recipient.facets.intersection(touched).is_empty())
                .then_some(ViewLinkDelivery {
                    recipient: *view,
                    group: membership.group,
                    revision,
                    facets: recipient.facets.intersection(touched),
                    state: state.clone(),
                })
            })
            .collect())
    }
}

fn apply<T>(slot: &mut Option<T>, patch: FacetPatch<T>) {
    match patch {
        FacetPatch::Unchanged => {}
        FacetPatch::Clear => *slot = None,
        FacetPatch::Set(value) => *slot = Some(value),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewLinkError {
    ZeroViewId,
    UnknownView(WorkspaceViewId),
    UnlinkedFacets(WorkspaceViewId),
    FacetNotPublished {
        view: WorkspaceViewId,
        attempted: LinkFacets,
        allowed: LinkFacets,
    },
}

impl fmt::Display for ViewLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroViewId => formatter.write_str("workspace view ID zero is reserved"),
            Self::UnknownView(view) => write!(formatter, "workspace view {} is not linked", view.0),
            Self::UnlinkedFacets(view) => write!(
                formatter,
                "workspace view {} declares facets without a link group",
                view.0
            ),
            Self::FacetNotPublished { view, .. } => write!(
                formatter,
                "workspace view {} attempted to publish an unlinked facet",
                view.0
            ),
        }
    }
}

impl Error for ViewLinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutually_linked_facets_are_delivered() {
        let mut links = ViewLinkRegistry::default();
        links
            .register(
                WorkspaceViewId(7),
                ViewLinkMembership {
                    group: LinkGroupId(1),
                    facets: LinkFacets::TIME.union(LinkFacets::FREQUENCY),
                },
            )
            .unwrap();
        links
            .register(
                WorkspaceViewId(8),
                ViewLinkMembership {
                    group: LinkGroupId(1),
                    facets: LinkFacets::TIME,
                },
            )
            .unwrap();
        links
            .register(
                WorkspaceViewId(9),
                ViewLinkMembership {
                    group: LinkGroupId(2),
                    facets: LinkFacets::TIME,
                },
            )
            .unwrap();

        let deliveries = links
            .publish(
                WorkspaceViewId(7),
                LinkedViewPatch {
                    time: FacetPatch::Set(FrameSpan { start: 4, end: 12 }),
                    ..LinkedViewPatch::default()
                },
            )
            .unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].recipient, WorkspaceViewId(8));
        assert_eq!(deliveries[0].facets, LinkFacets::TIME);
    }

    #[test]
    fn clear_is_distinct_from_no_change() {
        let mut links = ViewLinkRegistry::default();
        for view in [WorkspaceViewId(7), WorkspaceViewId(8)] {
            links
                .register(
                    view,
                    ViewLinkMembership {
                        group: LinkGroupId(1),
                        facets: LinkFacets::TIME,
                    },
                )
                .unwrap();
        }
        links
            .publish(
                WorkspaceViewId(7),
                LinkedViewPatch {
                    time: FacetPatch::Set(FrameSpan { start: 1, end: 2 }),
                    ..LinkedViewPatch::default()
                },
            )
            .unwrap();
        links
            .publish(
                WorkspaceViewId(7),
                LinkedViewPatch {
                    time: FacetPatch::Clear,
                    ..LinkedViewPatch::default()
                },
            )
            .unwrap();
        assert_eq!(links.group_state(LinkGroupId(1)).unwrap().1.time, None);
    }
}
