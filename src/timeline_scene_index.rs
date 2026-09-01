//! Revisioned, toolkit-neutral indexing for dense timeline scenes.
//!
//! This module owns no project truth, pixels, viewport, or GPUI entities. It
//! indexes immutable projections of typed project objects so editors visit the
//! objects in their visible time-by-lane rectangle rather than the whole song.
//! Equal raw values in different [`TimelineObjectId`] variants remain distinct.
//! Durations are half-open; an empty [`TimelineSpan`] is a point at `start`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimelineSpace {
    ProjectFrames,
    MusicalTicks { ppq: u32 },
    Custom(u64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineCoordinate(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimelineRange {
    start: TimelineCoordinate,
    end: TimelineCoordinate,
}

impl TimelineRange {
    pub fn new(
        start: TimelineCoordinate,
        end: TimelineCoordinate,
    ) -> Result<Self, SceneIndexError> {
        if start >= end {
            return Err(SceneIndexError::EmptyTimeRange);
        }
        Ok(Self { start, end })
    }
    pub const fn start(self) -> TimelineCoordinate {
        self.start
    }
    pub const fn end(self) -> TimelineCoordinate {
        self.end
    }
    pub fn contains(self, at: TimelineCoordinate) -> bool {
        self.start <= at && at < self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimelineSpan {
    start: TimelineCoordinate,
    end: TimelineCoordinate,
}

impl TimelineSpan {
    /// Creates a half-open duration, or a point when `start == end`.
    pub fn new(
        start: TimelineCoordinate,
        end: TimelineCoordinate,
    ) -> Result<Self, SceneIndexError> {
        if start > end {
            return Err(SceneIndexError::ReversedObjectSpan);
        }
        Ok(Self { start, end })
    }
    pub const fn point(at: TimelineCoordinate) -> Self {
        Self { start: at, end: at }
    }
    pub const fn start(self) -> TimelineCoordinate {
        self.start
    }
    pub const fn end(self) -> TimelineCoordinate {
        self.end
    }
    pub fn is_point(self) -> bool {
        self.start == self.end
    }
    pub fn intersects(self, range: TimelineRange) -> bool {
        self.start_i128() < i128::from(range.end.0)
            && self.effective_end_i128() > i128::from(range.start.0)
    }
    fn start_i128(self) -> i128 {
        i128::from(self.start.0)
    }
    fn effective_end_i128(self) -> i128 {
        if self.is_point() {
            i128::from(self.start.0) + 1
        } else {
            i128::from(self.end.0)
        }
    }
    fn union(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimelineObjectKind {
    Clip,
    Note,
    Event,
    AutomationPoint,
    Finding,
    Custom(u16),
}

/// Heterogeneous identity without raw-ID or index erasure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimelineObjectId<C, N, E, A, F> {
    Clip(C),
    Note(N),
    Event(E),
    AutomationPoint(A),
    Finding(F),
}

pub trait TimelineObjectKey: Clone + Ord {
    fn kind(&self) -> TimelineObjectKind;
}

impl<C: Clone + Ord, N: Clone + Ord, E: Clone + Ord, A: Clone + Ord, F: Clone + Ord>
    TimelineObjectKey for TimelineObjectId<C, N, E, A, F>
{
    fn kind(&self) -> TimelineObjectKind {
        match self {
            Self::Clip(_) => TimelineObjectKind::Clip,
            Self::Note(_) => TimelineObjectKind::Note,
            Self::Event(_) => TimelineObjectKind::Event,
            Self::AutomationPoint(_) => TimelineObjectKind::AutomationPoint,
            Self::Finding(_) => TimelineObjectKind::Finding,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimelineLaneId<T, N, E, A, F> {
    Track(T),
    Note(N),
    Event(E),
    Automation(A),
    Finding(F),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineObjectRecord<K, L, P = ()> {
    pub id: K,
    pub lane: L,
    pub span: TimelineSpan,
    /// Larger values paint later and win hit testing. It is not insertion order.
    pub paint_order: i32,
    pub payload: P,
}

impl<K, L, P> TimelineObjectRecord<K, L, P> {
    pub fn new(id: K, lane: L, span: TimelineSpan, paint_order: i32, payload: P) -> Self {
        Self {
            id,
            lane,
            span,
            paint_order,
            payload,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SceneRevision {
    pub source: u64,
    pub index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineLaneQuery<L> {
    All,
    One(L),
    /// Half-open range in stable lane-ID order.
    Range {
        start: L,
        end: L,
    },
}

impl<L: Ord> TimelineLaneQuery<L> {
    pub fn range(start: L, end: L) -> Result<Self, SceneIndexError> {
        if start >= end {
            return Err(SceneIndexError::EmptyLaneRange);
        }
        Ok(Self::Range { start, end })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineSceneQuery<L> {
    pub time: TimelineRange,
    pub lanes: TimelineLaneQuery<L>,
    pub kinds: Option<BTreeSet<TimelineObjectKind>>,
}

impl<L> TimelineSceneQuery<L> {
    pub fn all_lanes(time: TimelineRange) -> Self {
        Self {
            time,
            lanes: TimelineLaneQuery::All,
            kinds: None,
        }
    }
    pub fn one_lane(time: TimelineRange, lane: L) -> Self {
        Self {
            time,
            lanes: TimelineLaneQuery::One(lane),
            kinds: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SceneQueryStats {
    pub indexed_lanes: u64,
    pub indexed_objects: u64,
    pub lanes_visited: u64,
    /// Interval-tree nodes inspected after pruning.
    pub objects_visited: u64,
    /// Time/lane matches before optional kind filtering.
    pub geometrically_visible: u64,
    pub objects_returned: u64,
}

#[derive(Clone, Debug)]
pub struct TimelineSceneQueryResult<K, L, P> {
    pub objects: Vec<Arc<TimelineObjectRecord<K, L, P>>>,
    pub stats: SceneQueryStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineHitQuery<L> {
    pub lane: L,
    pub at: TimelineCoordinate,
    pub tolerance: u64,
    pub kinds: Option<BTreeSet<TimelineObjectKind>>,
}

impl<L> TimelineHitQuery<L> {
    pub fn new(lane: L, at: TimelineCoordinate, tolerance: u64) -> Self {
        Self {
            lane,
            at,
            tolerance,
            kinds: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TimelineHitCandidate<K, L, P> {
    pub object: Arc<TimelineObjectRecord<K, L, P>>,
    pub contains_pointer: bool,
    pub distance: u64,
}

#[derive(Clone, Debug)]
pub struct TimelineHitResult<K, L, P> {
    pub candidates: Vec<TimelineHitCandidate<K, L, P>>,
    pub stats: SceneQueryStats,
}

/// Explicit aggregate instrumentation; snapshots themselves remain immutable.
#[derive(Debug, Default)]
pub struct SceneQueryMeter {
    queries: AtomicU64,
    lanes_visited: AtomicU64,
    objects_visited: AtomicU64,
    geometrically_visible: AtomicU64,
    objects_returned: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SceneQueryTotals {
    pub queries: u64,
    pub lanes_visited: u64,
    pub objects_visited: u64,
    pub geometrically_visible: u64,
    pub objects_returned: u64,
}

impl SceneQueryMeter {
    pub fn record(&self, stats: SceneQueryStats) {
        self.queries.fetch_add(1, AtomicOrdering::Relaxed);
        self.lanes_visited
            .fetch_add(stats.lanes_visited, AtomicOrdering::Relaxed);
        self.objects_visited
            .fetch_add(stats.objects_visited, AtomicOrdering::Relaxed);
        self.geometrically_visible
            .fetch_add(stats.geometrically_visible, AtomicOrdering::Relaxed);
        self.objects_returned
            .fetch_add(stats.objects_returned, AtomicOrdering::Relaxed);
    }
    pub fn totals(&self) -> SceneQueryTotals {
        SceneQueryTotals {
            queries: self.queries.load(AtomicOrdering::Relaxed),
            lanes_visited: self.lanes_visited.load(AtomicOrdering::Relaxed),
            objects_visited: self.objects_visited.load(AtomicOrdering::Relaxed),
            geometrically_visible: self.geometrically_visible.load(AtomicOrdering::Relaxed),
            objects_returned: self.objects_returned.load(AtomicOrdering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidatedLane<L> {
    pub lane: L,
    pub span: TimelineSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneInvalidation<L> {
    pub before: SceneRevision,
    pub after: SceneRevision,
    pub lanes: Vec<InvalidatedLane<L>>,
}

#[derive(Clone, Debug)]
pub struct TimelineSceneUpdate<K, L, P> {
    pub expected_index_revision: u64,
    pub source_revision: u64,
    pub removals: BTreeSet<K>,
    pub upserts: Vec<TimelineObjectRecord<K, L, P>>,
}

impl<K: Ord, L, P> TimelineSceneUpdate<K, L, P> {
    pub fn new(expected_index_revision: u64, source_revision: u64) -> Self {
        Self {
            expected_index_revision,
            source_revision,
            removals: BTreeSet::new(),
            upserts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SceneUpdateResult<K, L, P> {
    pub snapshot: TimelineSceneSnapshot<K, L, P>,
    pub invalidation: SceneInvalidation<L>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneIndexError {
    EmptyTimeRange,
    ReversedObjectSpan,
    EmptyLaneRange,
    DuplicateObject,
    AmbiguousUpdate,
    RevisionMismatch { expected: u64, actual: u64 },
    StaleSourceRevision { current: u64, incoming: u64 },
    RevisionOverflow,
}

impl fmt::Display for SceneIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTimeRange => f.write_str("timeline query range must be non-empty"),
            Self::ReversedObjectSpan => f.write_str("timeline object span must not be reversed"),
            Self::EmptyLaneRange => f.write_str("timeline lane range must be non-empty"),
            Self::DuplicateObject => f.write_str("timeline object ID is duplicated"),
            Self::AmbiguousUpdate => {
                f.write_str("timeline object cannot be removed and upserted together")
            }
            Self::RevisionMismatch { expected, actual } => write!(
                f,
                "timeline index revision mismatch: expected {expected}, actual {actual}"
            ),
            Self::StaleSourceRevision { current, incoming } => write!(
                f,
                "timeline source revision {incoming} is older than current revision {current}"
            ),
            Self::RevisionOverflow => f.write_str("timeline index revision overflow"),
        }
    }
}

impl Error for SceneIndexError {}

#[derive(Clone, Debug)]
struct IntervalNode {
    record: usize,
    left: Option<usize>,
    right: Option<usize>,
    minimum_start: i128,
    maximum_end: i128,
}

#[derive(Clone, Debug)]
struct LaneIndex<K, L, P> {
    records: Vec<Arc<TimelineObjectRecord<K, L, P>>>,
    nodes: Vec<IntervalNode>,
    root: Option<usize>,
}

impl<K: TimelineObjectKey, L, P> LaneIndex<K, L, P> {
    fn from_records(records: impl IntoIterator<Item = Arc<TimelineObjectRecord<K, L, P>>>) -> Self {
        let mut records: Vec<_> = records.into_iter().collect();
        records.sort_by(record_order);
        let mut nodes = Vec::with_capacity(records.len());
        let root = build_interval_tree(&records, &mut nodes, 0, records.len());
        Self {
            records,
            nodes,
            root,
        }
    }

    fn query(
        &self,
        search: InternalRange,
        kinds: Option<&BTreeSet<TimelineObjectKind>>,
        output: &mut Vec<Arc<TimelineObjectRecord<K, L, P>>>,
        stats: &mut SceneQueryStats,
    ) {
        self.query_node(self.root, search, kinds, output, stats);
    }

    fn query_node(
        &self,
        node_index: Option<usize>,
        search: InternalRange,
        kinds: Option<&BTreeSet<TimelineObjectKind>>,
        output: &mut Vec<Arc<TimelineObjectRecord<K, L, P>>>,
        stats: &mut SceneQueryStats,
    ) {
        let Some(node_index) = node_index else {
            return;
        };
        let node = &self.nodes[node_index];
        stats.objects_visited = stats.objects_visited.saturating_add(1);
        if let Some(left) = node.left {
            let child = &self.nodes[left];
            if child.maximum_end > search.start && child.minimum_start < search.end {
                self.query_node(Some(left), search, kinds, output, stats);
            }
        }
        let record = &self.records[node.record];
        if span_intersects_internal(record.span, search) {
            stats.geometrically_visible = stats.geometrically_visible.saturating_add(1);
            if kinds.map_or(true, |allowed| allowed.contains(&record.id.kind())) {
                output.push(Arc::clone(record));
                stats.objects_returned = stats.objects_returned.saturating_add(1);
            }
        }
        if let Some(right) = node.right {
            let child = &self.nodes[right];
            if child.maximum_end > search.start && child.minimum_start < search.end {
                self.query_node(Some(right), search, kinds, output, stats);
            }
        }
    }
}

fn record_order<K: Ord, L, P>(
    left: &Arc<TimelineObjectRecord<K, L, P>>,
    right: &Arc<TimelineObjectRecord<K, L, P>>,
) -> Ordering {
    left.span
        .start
        .cmp(&right.span.start)
        .then_with(|| left.span.end.cmp(&right.span.end))
        .then_with(|| left.paint_order.cmp(&right.paint_order))
        .then_with(|| left.id.cmp(&right.id))
}

fn build_interval_tree<K, L, P>(
    records: &[Arc<TimelineObjectRecord<K, L, P>>],
    nodes: &mut Vec<IntervalNode>,
    start: usize,
    end: usize,
) -> Option<usize> {
    if start >= end {
        return None;
    }
    let middle = start + (end - start) / 2;
    let node_index = nodes.len();
    nodes.push(IntervalNode {
        record: middle,
        left: None,
        right: None,
        minimum_start: records[middle].span.start_i128(),
        maximum_end: records[middle].span.effective_end_i128(),
    });
    let left = build_interval_tree(records, nodes, start, middle);
    let right = build_interval_tree(records, nodes, middle + 1, end);
    let mut minimum_start = records[middle].span.start_i128();
    let mut maximum_end = records[middle].span.effective_end_i128();
    for child in [left, right].iter().filter_map(|child| *child) {
        minimum_start = minimum_start.min(nodes[child].minimum_start);
        maximum_end = maximum_end.max(nodes[child].maximum_end);
    }
    nodes[node_index] = IntervalNode {
        record: middle,
        left,
        right,
        minimum_start,
        maximum_end,
    };
    Some(node_index)
}

#[derive(Clone, Copy)]
struct InternalRange {
    start: i128,
    end: i128,
}

impl From<TimelineRange> for InternalRange {
    fn from(range: TimelineRange) -> Self {
        Self {
            start: i128::from(range.start.0),
            end: i128::from(range.end.0),
        }
    }
}

fn span_intersects_internal(span: TimelineSpan, range: InternalRange) -> bool {
    span.start_i128() < range.end && span.effective_end_i128() > range.start
}

#[derive(Clone, Debug)]
struct SceneSnapshotData<K, L, P> {
    space: TimelineSpace,
    revision: SceneRevision,
    lanes: BTreeMap<L, Arc<LaneIndex<K, L, P>>>,
    object_count: usize,
}

/// Immutable, revision-pinned scene. Cloning it is constant time.
#[derive(Debug)]
pub struct TimelineSceneSnapshot<K, L, P = ()> {
    data: Arc<SceneSnapshotData<K, L, P>>,
}

impl<K, L, P> Clone for TimelineSceneSnapshot<K, L, P> {
    fn clone(&self) -> Self {
        Self {
            data: Arc::clone(&self.data),
        }
    }
}

impl<K: TimelineObjectKey, L: Clone + Ord, P> TimelineSceneSnapshot<K, L, P> {
    pub fn space(&self) -> TimelineSpace {
        self.data.space
    }
    pub fn revision(&self) -> SceneRevision {
        self.data.revision
    }
    pub fn object_count(&self) -> usize {
        self.data.object_count
    }
    pub fn lane_count(&self) -> usize {
        self.data.lanes.len()
    }

    pub fn query(&self, query: &TimelineSceneQuery<L>) -> TimelineSceneQueryResult<K, L, P> {
        let mut result = TimelineSceneQueryResult {
            objects: Vec::new(),
            stats: SceneQueryStats {
                indexed_lanes: usize_to_u64(self.data.lanes.len()),
                indexed_objects: usize_to_u64(self.data.object_count),
                ..SceneQueryStats::default()
            },
        };
        let search = InternalRange::from(query.time);
        self.for_each_lane(&query.lanes, |lane| {
            result.stats.lanes_visited = result.stats.lanes_visited.saturating_add(1);
            lane.query(
                search,
                query.kinds.as_ref(),
                &mut result.objects,
                &mut result.stats,
            );
        });
        result
    }

    pub fn hit_test(&self, query: &TimelineHitQuery<L>) -> TimelineHitResult<K, L, P> {
        let tolerance = i128::from(query.tolerance);
        let at = i128::from(query.at.0);
        let search = InternalRange {
            start: at - tolerance,
            end: at + tolerance + 1,
        };
        let mut visible = Vec::new();
        let mut stats = SceneQueryStats {
            indexed_lanes: usize_to_u64(self.data.lanes.len()),
            indexed_objects: usize_to_u64(self.data.object_count),
            ..SceneQueryStats::default()
        };
        if let Some(lane) = self.data.lanes.get(&query.lane) {
            stats.lanes_visited = 1;
            lane.query(search, query.kinds.as_ref(), &mut visible, &mut stats);
        }
        let mut candidates: Vec<_> = visible
            .into_iter()
            .map(|object| {
                let (contains_pointer, distance) = hit_distance(object.span, query.at);
                TimelineHitCandidate {
                    object,
                    contains_pointer,
                    distance,
                }
            })
            .collect();
        candidates.sort_by(hit_candidate_order);
        TimelineHitResult { candidates, stats }
    }

    fn for_each_lane(
        &self,
        query: &TimelineLaneQuery<L>,
        mut visit: impl FnMut(&LaneIndex<K, L, P>),
    ) {
        match query {
            TimelineLaneQuery::All => self.data.lanes.values().for_each(|lane| visit(lane)),
            TimelineLaneQuery::One(id) => {
                if let Some(lane) = self.data.lanes.get(id) {
                    visit(lane);
                }
            }
            TimelineLaneQuery::Range { start, end } => {
                for (_, lane) in self.data.lanes.range(start.clone()..end.clone()) {
                    visit(lane);
                }
            }
        }
    }
}

fn hit_distance(span: TimelineSpan, at: TimelineCoordinate) -> (bool, u64) {
    let at = i128::from(at.0);
    let start = span.start_i128();
    let end = span.effective_end_i128();
    if start <= at && at < end {
        return (true, 0);
    }
    let distance = if at < start {
        start - at
    } else {
        at - (end - 1)
    };
    (false, distance.min(i128::from(u64::MAX)) as u64)
}

fn hit_candidate_order<K: Ord, L, P>(
    left: &TimelineHitCandidate<K, L, P>,
    right: &TimelineHitCandidate<K, L, P>,
) -> Ordering {
    right
        .object
        .paint_order
        .cmp(&left.object.paint_order)
        .then_with(|| right.contains_pointer.cmp(&left.contains_pointer))
        .then_with(|| left.distance.cmp(&right.distance))
        .then_with(|| {
            let left_len = left.object.span.effective_end_i128() - left.object.span.start_i128();
            let right_len = right.object.span.effective_end_i128() - right.object.span.start_i128();
            left_len.cmp(&right_len)
        })
        .then_with(|| left.object.span.start.cmp(&right.object.span.start))
        .then_with(|| left.object.id.cmp(&right.object.id))
}

/// Mutable publisher. It rebuilds only lanes touched by an atomic update.
pub struct TimelineSceneIndex<K, L, P = ()> {
    current: TimelineSceneSnapshot<K, L, P>,
    locations: BTreeMap<K, L>,
}

impl<K: TimelineObjectKey, L: Clone + Ord, P> TimelineSceneIndex<K, L, P> {
    pub fn empty(space: TimelineSpace, source_revision: u64) -> Self {
        Self {
            current: TimelineSceneSnapshot {
                data: Arc::new(SceneSnapshotData {
                    space,
                    revision: SceneRevision {
                        source: source_revision,
                        index: 0,
                    },
                    lanes: BTreeMap::new(),
                    object_count: 0,
                }),
            },
            locations: BTreeMap::new(),
        }
    }

    pub fn from_records(
        space: TimelineSpace,
        source_revision: u64,
        records: impl IntoIterator<Item = TimelineObjectRecord<K, L, P>>,
    ) -> Result<Self, SceneIndexError> {
        let mut by_lane: BTreeMap<L, BTreeMap<K, Arc<TimelineObjectRecord<K, L, P>>>> =
            BTreeMap::new();
        let mut locations = BTreeMap::new();
        for record in records {
            if locations
                .insert(record.id.clone(), record.lane.clone())
                .is_some()
            {
                return Err(SceneIndexError::DuplicateObject);
            }
            by_lane
                .entry(record.lane.clone())
                .or_default()
                .insert(record.id.clone(), Arc::new(record));
        }
        let lanes = by_lane
            .into_iter()
            .map(|(lane, records)| {
                (
                    lane,
                    Arc::new(LaneIndex::from_records(records.into_values())),
                )
            })
            .collect();
        let object_count = locations.len();
        Ok(Self {
            current: TimelineSceneSnapshot {
                data: Arc::new(SceneSnapshotData {
                    space,
                    revision: SceneRevision {
                        source: source_revision,
                        index: 0,
                    },
                    lanes,
                    object_count,
                }),
            },
            locations,
        })
    }

    pub fn snapshot(&self) -> TimelineSceneSnapshot<K, L, P> {
        self.current.clone()
    }

    pub fn apply_update(
        &mut self,
        update: TimelineSceneUpdate<K, L, P>,
    ) -> Result<SceneUpdateResult<K, L, P>, SceneIndexError> {
        let before = self.current.revision();
        if update.expected_index_revision != before.index {
            return Err(SceneIndexError::RevisionMismatch {
                expected: update.expected_index_revision,
                actual: before.index,
            });
        }
        if update.source_revision < before.source {
            return Err(SceneIndexError::StaleSourceRevision {
                current: before.source,
                incoming: update.source_revision,
            });
        }
        let next_index = before
            .index
            .checked_add(1)
            .ok_or(SceneIndexError::RevisionOverflow)?;
        let mut upsert_ids = BTreeSet::new();
        for record in &update.upserts {
            if !upsert_ids.insert(record.id.clone()) {
                return Err(SceneIndexError::DuplicateObject);
            }
            if update.removals.contains(&record.id) {
                return Err(SceneIndexError::AmbiguousUpdate);
            }
        }

        let mut affected = BTreeSet::new();
        for id in &update.removals {
            if let Some(lane) = self.locations.get(id) {
                affected.insert(lane.clone());
            }
        }
        for record in &update.upserts {
            if let Some(lane) = self.locations.get(&record.id) {
                affected.insert(lane.clone());
            }
            affected.insert(record.lane.clone());
        }

        let mut staged: BTreeMap<L, BTreeMap<K, Arc<TimelineObjectRecord<K, L, P>>>> =
            BTreeMap::new();
        for lane in &affected {
            let records = self
                .current
                .data
                .lanes
                .get(lane)
                .map(|index| {
                    index
                        .records
                        .iter()
                        .map(|record| (record.id.clone(), Arc::clone(record)))
                        .collect()
                })
                .unwrap_or_default();
            staged.insert(lane.clone(), records);
        }
        let mut invalidated = BTreeMap::new();
        for id in &update.removals {
            let Some(lane) = self.locations.get(id) else {
                continue;
            };
            if let Some(old) = staged.get_mut(lane).and_then(|records| records.remove(id)) {
                merge_invalidation(&mut invalidated, lane.clone(), old.span);
            }
        }
        for record in update.upserts {
            if let Some(old_lane) = self.locations.get(&record.id) {
                if let Some(old) = staged
                    .get_mut(old_lane)
                    .and_then(|records| records.remove(&record.id))
                {
                    merge_invalidation(&mut invalidated, old_lane.clone(), old.span);
                }
            }
            merge_invalidation(&mut invalidated, record.lane.clone(), record.span);
            staged
                .get_mut(&record.lane)
                .expect("upsert lane was staged")
                .insert(record.id.clone(), Arc::new(record));
        }

        let mut lanes = self.current.data.lanes.clone();
        for (lane, records) in staged {
            if records.is_empty() {
                lanes.remove(&lane);
            } else {
                lanes.insert(
                    lane,
                    Arc::new(LaneIndex::from_records(records.into_values())),
                );
            }
        }
        for id in &update.removals {
            self.locations.remove(id);
        }
        // Re-publish locations only for rebuilt lanes; untouched lanes retain
        // their existing locator entries.
        for lane in &affected {
            if let Some(index) = lanes.get(lane) {
                for record in &index.records {
                    self.locations.insert(record.id.clone(), lane.clone());
                }
            }
        }

        let after = SceneRevision {
            source: update.source_revision,
            index: next_index,
        };
        let object_count = lanes.values().map(|lane| lane.records.len()).sum();
        let snapshot = TimelineSceneSnapshot {
            data: Arc::new(SceneSnapshotData {
                space: self.current.space(),
                revision: after,
                lanes,
                object_count,
            }),
        };
        self.current = snapshot.clone();
        Ok(SceneUpdateResult {
            snapshot,
            invalidation: SceneInvalidation {
                before,
                after,
                lanes: invalidated
                    .into_iter()
                    .map(|(lane, span)| InvalidatedLane { lane, span })
                    .collect(),
            },
        })
    }
}

fn merge_invalidation<L: Ord>(map: &mut BTreeMap<L, TimelineSpan>, lane: L, span: TimelineSpan) {
    map.entry(lane)
        .and_modify(|current| *current = current.union(span))
        .or_insert(span);
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! id {
        ($name:ident) => {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            struct $name(u64);
        };
    }
    id!(ClipId);
    id!(NoteId);
    id!(EventId);
    id!(PointId);
    id!(FindingId);
    id!(TrackId);
    id!(NoteLaneId);
    id!(EventLaneId);
    id!(AutomationLaneId);
    id!(FindingLaneId);

    type ObjectId = TimelineObjectId<ClipId, NoteId, EventId, PointId, FindingId>;
    type LaneId = TimelineLaneId<TrackId, NoteLaneId, EventLaneId, AutomationLaneId, FindingLaneId>;
    type Record = TimelineObjectRecord<ObjectId, LaneId, u64>;

    fn span(start: i64, end: i64) -> TimelineSpan {
        TimelineSpan::new(TimelineCoordinate(start), TimelineCoordinate(end)).unwrap()
    }
    fn range(start: i64, end: i64) -> TimelineRange {
        TimelineRange::new(TimelineCoordinate(start), TimelineCoordinate(end)).unwrap()
    }
    fn clip(id: u64, lane: u64, start: i64, end: i64) -> Record {
        Record::new(
            ObjectId::Clip(ClipId(id)),
            LaneId::Track(TrackId(lane)),
            span(start, end),
            10,
            id,
        )
    }

    #[test]
    fn typed_variants_do_not_conflate_equal_raw_ids() {
        assert_ne!(ObjectId::Clip(ClipId(7)), ObjectId::Note(NoteId(7)));
        assert_ne!(
            LaneId::Track(TrackId(3)),
            LaneId::Automation(AutomationLaneId(3))
        );
    }

    #[test]
    fn half_open_edges_and_points_are_exact() {
        let lane = LaneId::Track(TrackId(1));
        let records = vec![
            clip(1, 1, 0, 10),
            clip(2, 1, 10, 20),
            clip(3, 1, 20, 30),
            Record::new(
                ObjectId::AutomationPoint(PointId(4)),
                lane,
                TimelineSpan::point(TimelineCoordinate(10)),
                20,
                4,
            ),
            Record::new(
                ObjectId::Finding(FindingId(5)),
                lane,
                TimelineSpan::point(TimelineCoordinate(20)),
                30,
                5,
            ),
        ];
        let index =
            TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 9, records).unwrap();
        let result = index
            .snapshot()
            .query(&TimelineSceneQuery::one_lane(range(10, 20), lane));
        assert_eq!(
            result
                .objects
                .iter()
                .map(|record| record.payload)
                .collect::<Vec<_>>(),
            vec![4, 2]
        );
        assert_eq!(result.stats.geometrically_visible, 2);
    }

    #[test]
    fn query_order_is_independent_of_insertion_order() {
        let mut records = vec![
            clip(5, 2, 100, 150),
            clip(3, 1, 100, 150),
            clip(2, 1, 90, 110),
            clip(4, 2, 80, 120),
            clip(1, 1, 100, 150),
        ];
        let first =
            TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 1, records.clone())
                .unwrap();
        records.reverse();
        let second =
            TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 1, records).unwrap();
        let query = TimelineSceneQuery::all_lanes(range(0, 1_000));
        let ids = |snapshot: TimelineSceneSnapshot<ObjectId, LaneId, u64>| {
            snapshot
                .query(&query)
                .objects
                .iter()
                .map(|record| record.payload)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(first.snapshot()), ids(second.snapshot()));
        assert_eq!(ids(first.snapshot()), vec![2, 1, 3, 4, 5]);
    }

    #[test]
    fn hit_candidates_use_paint_geometry_and_stable_id_ties() {
        let lane = LaneId::Track(TrackId(1));
        let mut low = clip(2, 1, 10, 30);
        low.paint_order = 1;
        let mut short = clip(1, 1, 18, 22);
        short.paint_order = 5;
        let mut same = clip(3, 1, 18, 22);
        same.paint_order = 5;
        let index = TimelineSceneIndex::from_records(
            TimelineSpace::ProjectFrames,
            0,
            vec![same, low, short],
        )
        .unwrap();
        let hit =
            index
                .snapshot()
                .hit_test(&TimelineHitQuery::new(lane, TimelineCoordinate(20), 0));
        assert_eq!(
            hit.candidates
                .iter()
                .map(|c| c.object.payload)
                .collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
        assert!(hit
            .candidates
            .iter()
            .all(|candidate| candidate.contains_pointer));
    }

    #[test]
    fn updates_preserve_snapshots_and_report_lane_invalidation() {
        let lane_one = LaneId::Track(TrackId(1));
        let lane_two = LaneId::Track(TrackId(2));
        let mut index = TimelineSceneIndex::from_records(
            TimelineSpace::ProjectFrames,
            10,
            vec![clip(1, 1, 0, 10), clip(2, 2, 30, 40)],
        )
        .unwrap();
        let old = index.snapshot();
        let mut update = TimelineSceneUpdate::new(old.revision().index, 11);
        update.upserts.push(clip(1, 2, 100, 120));
        update.removals.insert(ObjectId::Clip(ClipId(2)));
        let applied = index.apply_update(update).unwrap();
        assert_eq!(
            old.query(&TimelineSceneQuery::all_lanes(range(0, 50)))
                .objects
                .len(),
            2
        );
        assert_eq!(applied.snapshot.object_count(), 1);
        assert_eq!(
            applied.invalidation.lanes,
            vec![
                InvalidatedLane {
                    lane: lane_one,
                    span: span(0, 10)
                },
                InvalidatedLane {
                    lane: lane_two,
                    span: span(30, 120)
                },
            ]
        );
        assert_eq!(
            applied.snapshot.revision(),
            SceneRevision {
                source: 11,
                index: 1
            }
        );
    }

    #[test]
    fn kind_filter_instruments_geometry_separately() {
        let lane = LaneId::Track(TrackId(1));
        let records = vec![
            clip(1, 1, 0, 100),
            Record::new(ObjectId::Finding(FindingId(2)), lane, span(10, 20), 2, 2),
        ];
        let index =
            TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 0, records).unwrap();
        let query = TimelineSceneQuery {
            time: range(0, 100),
            lanes: TimelineLaneQuery::One(lane),
            kinds: Some(BTreeSet::from([TimelineObjectKind::Finding])),
        };
        let result = index.snapshot().query(&query);
        assert_eq!(
            (
                result.stats.geometrically_visible,
                result.stats.objects_returned
            ),
            (2, 1)
        );
    }

    #[test]
    fn large_query_visits_neighborhood_not_project() {
        let mut records = Vec::with_capacity(100_000);
        for lane in 0..100 {
            for object in 0..1_000 {
                let id = lane * 1_000 + object;
                records.push(clip(id, lane, object as i64 * 20, object as i64 * 20 + 8));
            }
        }
        let index =
            TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 42, records).unwrap();
        let result = index.snapshot().query(&TimelineSceneQuery::one_lane(
            range(10_000, 10_200),
            LaneId::Track(TrackId(73)),
        ));
        assert_eq!(result.stats.indexed_objects, 100_000);
        assert_eq!(result.stats.objects_returned, 10);
        assert!(result.stats.objects_visited < 80, "{:#?}", result.stats);
        let meter = SceneQueryMeter::default();
        meter.record(result.stats);
        assert_eq!(meter.totals().objects_returned, 10);
    }

    #[test]
    fn extreme_point_coordinate_remains_queryable() {
        let lane = LaneId::Automation(AutomationLaneId(1));
        let record = Record::new(
            ObjectId::AutomationPoint(PointId(1)),
            lane,
            TimelineSpan::point(TimelineCoordinate(i64::MAX)),
            0,
            1,
        );
        let index = TimelineSceneIndex::from_records(TimelineSpace::ProjectFrames, 0, vec![record])
            .unwrap();
        let hit = index.snapshot().hit_test(&TimelineHitQuery::new(
            lane,
            TimelineCoordinate(i64::MAX),
            0,
        ));
        assert_eq!(hit.candidates.len(), 1);
        assert!(hit.candidates[0].contains_pointer);
    }
}
