//! Backend-independent mixer and routing state.
//!
//! This module describes a non-destructive signal-flow graph. It deliberately
//! contains no audio backend, plugin loader, realtime callback, or sample data.
//! A renderer can consume its routing and latency metadata, while project code
//! can use [`MixerCommand`] as an undo/redo integration seam.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

typed_id!(BusId);
typed_id!(NodeId);
typed_id!(SendId);
typed_id!(ProcessorId);
typed_id!(ParameterId);

/// The semantic role of a mixer bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BusKind {
    Source,
    Component,
    Group,
    Master,
}

/// Where an auxiliary send taps its source bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SendTap {
    /// After inserts, but before the source bus's gain and pan controls.
    PreFader,
    /// After inserts and the source bus's gain and pan controls.
    PostFader,
}

/// Serializable-looking plugin metadata without any plugin-hosting behavior.
///
/// `opaque_state` is owned by the eventual host implementation. The mixer does
/// not interpret it, instantiate a plugin, or promise that `format` is
/// supported by the running application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDescriptor {
    pub format: String,
    pub identifier: String,
    pub display_name: String,
    pub vendor: Option<String>,
    pub opaque_state: Vec<u8>,
}

impl PluginDescriptor {
    pub fn new(
        format: impl Into<String>,
        identifier: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            format: format.into(),
            identifier: identifier.into(),
            display_name: display_name.into(),
            vendor: None,
            opaque_state: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessorParameter {
    id: ParameterId,
    key: String,
    name: String,
    normalized_value: f32,
}

impl ProcessorParameter {
    pub fn id(&self) -> ParameterId {
        self.id
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn normalized_value(&self) -> f32 {
        self.normalized_value
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Processor {
    id: ProcessorId,
    node_id: NodeId,
    descriptor: PluginDescriptor,
    latency_samples: u32,
    parameters: BTreeMap<ParameterId, ProcessorParameter>,
}

impl Processor {
    pub fn id(&self) -> ProcessorId {
        self.id
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    pub fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    pub fn parameters(&self) -> impl Iterator<Item = &ProcessorParameter> {
        self.parameters.values()
    }

    pub fn parameter(&self, id: ParameterId) -> Option<&ProcessorParameter> {
        self.parameters.get(&id)
    }
}

/// One position in a bus's ordered insert chain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InsertSlot {
    processor_id: ProcessorId,
    bypassed: bool,
    wet: f32,
}

impl InsertSlot {
    pub fn processor_id(self) -> ProcessorId {
        self.processor_id
    }

    pub fn bypassed(self) -> bool {
        self.bypassed
    }

    pub fn wet(self) -> f32 {
        self.wet
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Send {
    id: SendId,
    target: BusId,
    tap: SendTap,
    level_db: f32,
    muted: bool,
}

impl Send {
    pub fn id(&self) -> SendId {
        self.id
    }

    pub fn target(&self) -> BusId {
        self.target
    }

    pub fn tap(&self) -> SendTap {
        self.tap
    }

    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    pub fn muted(&self) -> bool {
        self.muted
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaderState {
    gain_db: f32,
    pan: f32,
    muted: bool,
    soloed: bool,
}

impl Default for FaderState {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            soloed: false,
        }
    }
}

impl FaderState {
    pub fn gain_db(self) -> f32 {
        self.gain_db
    }

    pub fn gain_linear(self) -> f32 {
        db_to_linear(self.gain_db)
    }

    /// Constant-power pan coordinate in the inclusive range `-1.0..=1.0`.
    pub fn pan(self) -> f32 {
        self.pan
    }

    pub fn muted(self) -> bool {
        self.muted
    }

    pub fn soloed(self) -> bool {
        self.soloed
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bus {
    id: BusId,
    node_id: NodeId,
    name: String,
    kind: BusKind,
    output: Option<BusId>,
    inserts: Vec<InsertSlot>,
    sends: Vec<Send>,
    fader: FaderState,
}

impl Bus {
    pub fn id(&self) -> BusId {
        self.id
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> BusKind {
        self.kind
    }

    pub fn output(&self) -> Option<BusId> {
        self.output
    }

    pub fn inserts(&self) -> &[InsertSlot] {
        &self.inserts
    }

    pub fn sends(&self) -> &[Send] {
        &self.sends
    }

    pub fn fader(&self) -> FaderState {
        self.fader
    }
}

/// Identifies the object assigned to a generic graph node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeOwner {
    Bus(BusId),
    Processor(ProcessorId),
}

/// A concrete graph edge. Main routes and parallel sends remain distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteEdge {
    pub from: BusId,
    pub to: BusId,
    pub kind: RouteKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteKind {
    Main,
    Send(SendId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveBusState {
    pub explicitly_muted: bool,
    pub soloed: bool,
    pub in_solo_path: bool,
    pub solo_suppressed: bool,
    pub audible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusLatency {
    /// Longest upstream arrival before compensation at this bus.
    pub input_latency_samples: u64,
    /// Sum of enabled insert latencies on this bus.
    pub insert_latency_samples: u64,
    /// Compensated input latency plus this bus's inserts.
    pub output_latency_samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteLatency {
    pub arrival_latency_samples: u64,
    pub compensation_delay_samples: u64,
}

/// Deterministic delay-compensation metadata for a validated graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatencyPlan {
    pub buses: BTreeMap<BusId, BusLatency>,
    pub routes: BTreeMap<RouteEdge, RouteLatency>,
    pub master_output_latency_samples: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MixerGraph {
    buses: BTreeMap<BusId, Bus>,
    processors: BTreeMap<ProcessorId, Processor>,
    master: BusId,
    /// Ephemeral optimistic-concurrency token for semantic control-surface
    /// intents. Project revisions remain the durable persistence authority.
    revision: u64,
    next_bus_id: u64,
    next_node_id: u64,
    next_send_id: u64,
    next_processor_id: u64,
    next_parameter_id: u64,
}

impl Default for MixerGraph {
    fn default() -> Self {
        Self::new("Master")
    }
}

impl MixerGraph {
    pub fn new(master_name: impl Into<String>) -> Self {
        let master = BusId::from_raw(1);
        let master_bus = Bus {
            id: master,
            node_id: NodeId::from_raw(1),
            name: master_name.into(),
            kind: BusKind::Master,
            output: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            fader: FaderState::default(),
        };
        Self {
            buses: BTreeMap::from([(master, master_bus)]),
            processors: BTreeMap::new(),
            master,
            revision: 0,
            next_bus_id: 2,
            next_node_id: 2,
            next_send_id: 1,
            next_processor_id: 1,
            next_parameter_id: 1,
        }
    }

    pub fn master(&self) -> BusId {
        self.master
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn buses(&self) -> impl Iterator<Item = &Bus> {
        self.buses.values()
    }

    pub fn bus(&self, id: BusId) -> Option<&Bus> {
        self.buses.get(&id)
    }

    pub fn processors(&self) -> impl Iterator<Item = &Processor> {
        self.processors.values()
    }

    pub fn processor(&self, id: ProcessorId) -> Option<&Processor> {
        self.processors.get(&id)
    }

    pub fn node_owner(&self, node_id: NodeId) -> Option<NodeOwner> {
        self.buses
            .values()
            .find(|bus| bus.node_id == node_id)
            .map(|bus| NodeOwner::Bus(bus.id))
            .or_else(|| {
                self.processors
                    .values()
                    .find(|processor| processor.node_id == node_id)
                    .map(|processor| NodeOwner::Processor(processor.id))
            })
    }

    /// Adds a bus routed directly to the master by default.
    pub fn add_bus(&mut self, kind: BusKind, name: impl Into<String>) -> Result<BusId, MixerError> {
        if kind == BusKind::Master {
            return Err(MixerError::MasterAlreadyExists);
        }
        let id = BusId::from_raw(take_id(&mut self.next_bus_id, "bus")?);
        let node_id = NodeId::from_raw(take_id(&mut self.next_node_id, "node")?);
        self.buses.insert(
            id,
            Bus {
                id,
                node_id,
                name: name.into(),
                kind,
                output: Some(self.master),
                inserts: Vec::new(),
                sends: Vec::new(),
                fader: FaderState::default(),
            },
        );
        Ok(id)
    }

    /// Removes an unreferenced non-master bus and its owned processors.
    pub fn remove_bus(&mut self, id: BusId) -> Result<Bus, MixerError> {
        self.require_bus(id)?;
        if id == self.master {
            return Err(MixerError::CannotRemoveMaster);
        }
        if self.routes().iter().any(|route| route.to == id) {
            return Err(MixerError::BusStillReferenced(id));
        }
        let bus = self.buses.remove(&id).expect("bus checked above");
        for insert in &bus.inserts {
            self.processors.remove(&insert.processor_id);
        }
        Ok(bus)
    }

    pub fn rename_bus(&mut self, id: BusId, name: impl Into<String>) -> Result<(), MixerError> {
        self.bus_mut(id)?.name = name.into();
        Ok(())
    }

    /// Changes a bus's main output atomically, rejecting cycles.
    pub fn set_output(&mut self, from: BusId, to: BusId) -> Result<(), MixerError> {
        self.require_bus(from)?;
        self.require_bus(to)?;
        if from == self.master {
            return Err(MixerError::MasterCannotRoute);
        }
        let old = self.bus_mut(from)?.output.replace(to);
        if let Err(error) = self.validate() {
            self.bus_mut(from)?.output = old;
            return Err(error);
        }
        Ok(())
    }

    pub fn add_send(
        &mut self,
        from: BusId,
        to: BusId,
        tap: SendTap,
        level_db: f32,
    ) -> Result<SendId, MixerError> {
        validate_finite("send level", level_db)?;
        self.require_bus(from)?;
        self.require_bus(to)?;
        if from == self.master {
            return Err(MixerError::MasterCannotSend);
        }
        let id = SendId::from_raw(peek_id(self.next_send_id, "send")?);
        self.bus_mut(from)?.sends.push(Send {
            id,
            target: to,
            tap,
            level_db,
            muted: false,
        });
        if let Err(error) = self.validate() {
            self.bus_mut(from)?.sends.pop();
            return Err(error);
        }
        take_id(&mut self.next_send_id, "send")?;
        Ok(id)
    }

    pub fn remove_send(&mut self, id: SendId) -> Result<Send, MixerError> {
        let (bus_id, position) = self.find_send(id).ok_or(MixerError::MissingSend(id))?;
        Ok(self.bus_mut(bus_id)?.sends.remove(position))
    }

    pub fn set_send_tap(&mut self, id: SendId, tap: SendTap) -> Result<(), MixerError> {
        self.send_mut(id)?.tap = tap;
        Ok(())
    }

    pub fn set_send_level(&mut self, id: SendId, level_db: f32) -> Result<(), MixerError> {
        validate_finite("send level", level_db)?;
        self.send_mut(id)?.level_db = level_db;
        Ok(())
    }

    pub fn set_send_muted(&mut self, id: SendId, muted: bool) -> Result<(), MixerError> {
        self.send_mut(id)?.muted = muted;
        Ok(())
    }

    pub fn set_gain_db(&mut self, id: BusId, gain_db: f32) -> Result<(), MixerError> {
        validate_finite("gain", gain_db)?;
        self.bus_mut(id)?.fader.gain_db = gain_db;
        Ok(())
    }

    pub fn set_pan(&mut self, id: BusId, pan: f32) -> Result<(), MixerError> {
        validate_unit("pan", pan, -1.0, 1.0)?;
        self.bus_mut(id)?.fader.pan = pan;
        Ok(())
    }

    pub fn set_muted(&mut self, id: BusId, muted: bool) -> Result<(), MixerError> {
        self.bus_mut(id)?.fader.muted = muted;
        Ok(())
    }

    pub fn set_soloed(&mut self, id: BusId, soloed: bool) -> Result<(), MixerError> {
        self.bus_mut(id)?.fader.soloed = soloed;
        Ok(())
    }

    /// Inserts a processor at `index`; `None` appends to the chain.
    pub fn insert_processor(
        &mut self,
        bus_id: BusId,
        index: Option<usize>,
        descriptor: PluginDescriptor,
        latency_samples: u32,
    ) -> Result<ProcessorId, MixerError> {
        self.require_bus(bus_id)?;
        validate_descriptor(&descriptor)?;
        let len = self.buses[&bus_id].inserts.len();
        let index = index.unwrap_or(len);
        if index > len {
            return Err(MixerError::InvalidInsertIndex { index, len });
        }
        let id = ProcessorId::from_raw(take_id(&mut self.next_processor_id, "processor")?);
        let node_id = NodeId::from_raw(take_id(&mut self.next_node_id, "node")?);
        self.processors.insert(
            id,
            Processor {
                id,
                node_id,
                descriptor,
                latency_samples,
                parameters: BTreeMap::new(),
            },
        );
        self.bus_mut(bus_id)?.inserts.insert(
            index,
            InsertSlot {
                processor_id: id,
                bypassed: false,
                wet: 1.0,
            },
        );
        Ok(id)
    }

    pub fn remove_processor(&mut self, id: ProcessorId) -> Result<Processor, MixerError> {
        let (bus_id, position) = self
            .find_insert(id)
            .ok_or(MixerError::MissingProcessor(id))?;
        self.bus_mut(bus_id)?.inserts.remove(position);
        Ok(self
            .processors
            .remove(&id)
            .expect("insert ownership was checked"))
    }

    pub fn move_processor(
        &mut self,
        bus_id: BusId,
        id: ProcessorId,
        new_index: usize,
    ) -> Result<(), MixerError> {
        let (owner, old_index) = self
            .find_insert(id)
            .ok_or(MixerError::MissingProcessor(id))?;
        if owner != bus_id {
            return Err(MixerError::ProcessorNotOnBus { id, bus_id });
        }
        let len_after_removal = self.buses[&bus_id].inserts.len() - 1;
        if new_index > len_after_removal {
            return Err(MixerError::InvalidInsertIndex {
                index: new_index,
                len: len_after_removal,
            });
        }
        let slot = self.bus_mut(bus_id)?.inserts.remove(old_index);
        self.bus_mut(bus_id)?.inserts.insert(new_index, slot);
        Ok(())
    }

    pub fn set_insert_bypassed(
        &mut self,
        id: ProcessorId,
        bypassed: bool,
    ) -> Result<(), MixerError> {
        self.insert_mut(id)?.bypassed = bypassed;
        Ok(())
    }

    pub fn set_insert_wet(&mut self, id: ProcessorId, wet: f32) -> Result<(), MixerError> {
        validate_unit("insert wet", wet, 0.0, 1.0)?;
        self.insert_mut(id)?.wet = wet;
        Ok(())
    }

    pub fn set_processor_latency(
        &mut self,
        id: ProcessorId,
        latency_samples: u32,
    ) -> Result<(), MixerError> {
        self.processor_mut(id)?.latency_samples = latency_samples;
        Ok(())
    }

    pub fn add_parameter(
        &mut self,
        processor_id: ProcessorId,
        key: impl Into<String>,
        name: impl Into<String>,
        normalized_value: f32,
    ) -> Result<ParameterId, MixerError> {
        validate_unit("parameter value", normalized_value, 0.0, 1.0)?;
        self.require_processor(processor_id)?;
        let key = key.into();
        if key.is_empty() {
            return Err(MixerError::EmptyField("parameter key"));
        }
        if self.processors[&processor_id]
            .parameters
            .values()
            .any(|parameter| parameter.key == key)
        {
            return Err(MixerError::DuplicateParameterKey { processor_id, key });
        }
        let id = ParameterId::from_raw(take_id(&mut self.next_parameter_id, "parameter")?);
        self.processor_mut(processor_id)?.parameters.insert(
            id,
            ProcessorParameter {
                id,
                key,
                name: name.into(),
                normalized_value,
            },
        );
        Ok(id)
    }

    pub fn set_parameter_value(
        &mut self,
        processor_id: ProcessorId,
        parameter_id: ParameterId,
        normalized_value: f32,
    ) -> Result<(), MixerError> {
        validate_unit("parameter value", normalized_value, 0.0, 1.0)?;
        let parameter = self
            .processor_mut(processor_id)?
            .parameters
            .get_mut(&parameter_id)
            .ok_or(MixerError::MissingParameter(parameter_id))?;
        parameter.normalized_value = normalized_value;
        Ok(())
    }

    pub fn remove_parameter(
        &mut self,
        processor_id: ProcessorId,
        parameter_id: ParameterId,
    ) -> Result<ProcessorParameter, MixerError> {
        self.processor_mut(processor_id)?
            .parameters
            .remove(&parameter_id)
            .ok_or(MixerError::MissingParameter(parameter_id))
    }

    pub fn routes(&self) -> Vec<RouteEdge> {
        let mut routes = Vec::new();
        for bus in self.buses.values() {
            if let Some(to) = bus.output {
                routes.push(RouteEdge {
                    from: bus.id,
                    to,
                    kind: RouteKind::Main,
                });
            }
            routes.extend(bus.sends.iter().map(|send| RouteEdge {
                from: bus.id,
                to: send.target,
                kind: RouteKind::Send(send.id),
            }));
        }
        routes.sort_unstable();
        routes
    }

    /// Computes mute/solo state using stable ID ordering.
    ///
    /// For each explicitly soloed bus, its upstream contributors and its
    /// downstream path are retained. Traversal starts from the explicit solos
    /// in each direction, so a source solo does not admit sibling sources that
    /// happen to share a group. Explicit mute always wins over solo.
    pub fn effective_states(&self) -> BTreeMap<BusId, EffectiveBusState> {
        let soloed: Vec<_> = self
            .buses
            .values()
            .filter(|bus| bus.fader.soloed)
            .map(|bus| bus.id)
            .collect();
        let any_solo = !soloed.is_empty();
        let routes = self.routes();
        let mut solo_path = BTreeSet::new();
        for root in soloed {
            collect_reachable(root, &routes, true, &mut solo_path);
            collect_reachable(root, &routes, false, &mut solo_path);
        }
        self.buses
            .values()
            .map(|bus| {
                let in_solo_path = !any_solo || solo_path.contains(&bus.id);
                let solo_suppressed = any_solo && !in_solo_path;
                (
                    bus.id,
                    EffectiveBusState {
                        explicitly_muted: bus.fader.muted,
                        soloed: bus.fader.soloed,
                        in_solo_path,
                        solo_suppressed,
                        audible: !bus.fader.muted && !solo_suppressed,
                    },
                )
            })
            .collect()
    }

    /// Scalar send gain. Pan remains separate routing metadata.
    ///
    /// Both tap positions honor effective mute/solo state. A pre-fader send
    /// omits source fader gain; a post-fader send includes it.
    pub fn effective_send_gain(&self, id: SendId) -> Result<f32, MixerError> {
        let (bus_id, position) = self.find_send(id).ok_or(MixerError::MissingSend(id))?;
        let bus = &self.buses[&bus_id];
        let send = &bus.sends[position];
        if send.muted || !self.effective_states()[&bus_id].audible {
            return Ok(0.0);
        }
        let fader_gain = match send.tap {
            SendTap::PreFader => 1.0,
            SendTap::PostFader => bus.fader.gain_linear(),
        };
        Ok(db_to_linear(send.level_db) * fader_gain)
    }

    pub fn latency_plan(&self) -> Result<LatencyPlan, MixerError> {
        self.validate()?;
        let routes = self.routes();
        let mut indegree: BTreeMap<_, usize> =
            self.buses.keys().copied().map(|id| (id, 0)).collect();
        for route in &routes {
            *indegree.get_mut(&route.to).expect("validated target") += 1;
        }
        let mut ready: BTreeSet<_> = indegree
            .iter()
            .filter_map(|(&id, &degree)| (degree == 0).then_some(id))
            .collect();
        let mut bus_latencies: BTreeMap<BusId, BusLatency> = BTreeMap::new();
        let mut route_latencies: BTreeMap<RouteEdge, RouteLatency> = BTreeMap::new();

        while let Some(id) = ready.pop_first() {
            let arrivals: Vec<_> = routes
                .iter()
                .filter(|route| route.to == id)
                .map(|route| {
                    let latency = bus_latencies
                        .get(&route.from)
                        .expect("predecessor visited")
                        .output_latency_samples;
                    (*route, latency)
                })
                .collect();
            let input_latency = arrivals
                .iter()
                .map(|(_, latency)| *latency)
                .max()
                .unwrap_or(0);
            for (route, arrival) in arrivals {
                route_latencies.insert(
                    route,
                    RouteLatency {
                        arrival_latency_samples: arrival,
                        compensation_delay_samples: input_latency - arrival,
                    },
                );
            }
            let insert_latency = self.bus_insert_latency(id)?;
            bus_latencies.insert(
                id,
                BusLatency {
                    input_latency_samples: input_latency,
                    insert_latency_samples: insert_latency,
                    output_latency_samples: input_latency.saturating_add(insert_latency),
                },
            );

            for route in routes.iter().filter(|route| route.from == id) {
                let degree = indegree.get_mut(&route.to).expect("validated target");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(route.to);
                }
            }
        }

        let master_output_latency_samples = bus_latencies[&self.master].output_latency_samples;
        Ok(LatencyPlan {
            buses: bus_latencies,
            routes: route_latencies,
            master_output_latency_samples,
        })
    }

    /// Checks all identities, references, value domains, ownership, and routes.
    pub fn validate(&self) -> Result<(), MixerError> {
        let master = self
            .buses
            .get(&self.master)
            .ok_or(MixerError::MissingMaster(self.master))?;
        if master.kind != BusKind::Master {
            return Err(MixerError::MissingMaster(self.master));
        }
        if self
            .buses
            .values()
            .filter(|bus| bus.kind == BusKind::Master)
            .count()
            != 1
        {
            return Err(MixerError::MasterAlreadyExists);
        }
        if master.output.is_some() {
            return Err(MixerError::MasterCannotRoute);
        }
        if !master.sends.is_empty() {
            return Err(MixerError::MasterCannotSend);
        }

        let mut nodes = BTreeSet::new();
        let mut sends = BTreeSet::new();
        let mut used_processors = BTreeSet::new();
        let mut parameters = BTreeSet::new();
        for (&id, bus) in &self.buses {
            if id != bus.id {
                return Err(MixerError::IdentityMismatch("bus"));
            }
            if !nodes.insert(bus.node_id) {
                return Err(MixerError::DuplicateNode(bus.node_id));
            }
            validate_finite("gain", bus.fader.gain_db)?;
            validate_unit("pan", bus.fader.pan, -1.0, 1.0)?;
            if id != self.master && bus.output.is_none() {
                return Err(MixerError::MissingOutput(id));
            }
            if let Some(target) = bus.output {
                self.require_bus(target)?;
            }
            for send in &bus.sends {
                if !sends.insert(send.id) {
                    return Err(MixerError::DuplicateSend(send.id));
                }
                self.require_bus(send.target)?;
                validate_finite("send level", send.level_db)?;
            }
            for insert in &bus.inserts {
                validate_unit("insert wet", insert.wet, 0.0, 1.0)?;
                if !used_processors.insert(insert.processor_id) {
                    return Err(MixerError::ProcessorUsedTwice(insert.processor_id));
                }
                self.require_processor(insert.processor_id)?;
            }
        }
        for (&id, processor) in &self.processors {
            if id != processor.id {
                return Err(MixerError::IdentityMismatch("processor"));
            }
            if !used_processors.contains(&id) {
                return Err(MixerError::OrphanProcessor(id));
            }
            if !nodes.insert(processor.node_id) {
                return Err(MixerError::DuplicateNode(processor.node_id));
            }
            validate_descriptor(&processor.descriptor)?;
            let mut keys = BTreeSet::new();
            for (&parameter_id, parameter) in &processor.parameters {
                if parameter_id != parameter.id {
                    return Err(MixerError::IdentityMismatch("parameter"));
                }
                if !parameters.insert(parameter_id) {
                    return Err(MixerError::DuplicateParameter(parameter_id));
                }
                if !keys.insert(parameter.key.as_str()) {
                    return Err(MixerError::DuplicateParameterKey {
                        processor_id: id,
                        key: parameter.key.clone(),
                    });
                }
                validate_unit("parameter value", parameter.normalized_value, 0.0, 1.0)?;
            }
        }

        if let Some(cycle) = find_cycle(&self.buses, &self.routes()) {
            return Err(MixerError::CycleDetected(cycle));
        }
        Ok(())
    }

    fn bus_insert_latency(&self, bus_id: BusId) -> Result<u64, MixerError> {
        let bus = self
            .buses
            .get(&bus_id)
            .ok_or(MixerError::MissingBus(bus_id))?;
        Ok(bus
            .inserts
            .iter()
            .filter(|insert| !insert.bypassed)
            .map(|insert| self.processors[&insert.processor_id].latency_samples as u64)
            .fold(0_u64, u64::saturating_add))
    }

    fn require_bus(&self, id: BusId) -> Result<(), MixerError> {
        self.buses
            .contains_key(&id)
            .then_some(())
            .ok_or(MixerError::MissingBus(id))
    }

    fn require_processor(&self, id: ProcessorId) -> Result<(), MixerError> {
        self.processors
            .contains_key(&id)
            .then_some(())
            .ok_or(MixerError::MissingProcessor(id))
    }

    fn bus_mut(&mut self, id: BusId) -> Result<&mut Bus, MixerError> {
        self.buses.get_mut(&id).ok_or(MixerError::MissingBus(id))
    }

    fn processor_mut(&mut self, id: ProcessorId) -> Result<&mut Processor, MixerError> {
        self.processors
            .get_mut(&id)
            .ok_or(MixerError::MissingProcessor(id))
    }

    fn find_insert(&self, id: ProcessorId) -> Option<(BusId, usize)> {
        self.buses.values().find_map(|bus| {
            bus.inserts
                .iter()
                .position(|insert| insert.processor_id == id)
                .map(|position| (bus.id, position))
        })
    }

    fn insert_mut(&mut self, id: ProcessorId) -> Result<&mut InsertSlot, MixerError> {
        let (bus_id, position) = self
            .find_insert(id)
            .ok_or(MixerError::MissingProcessor(id))?;
        Ok(&mut self.bus_mut(bus_id)?.inserts[position])
    }

    fn find_send(&self, id: SendId) -> Option<(BusId, usize)> {
        self.buses.values().find_map(|bus| {
            bus.sends
                .iter()
                .position(|send| send.id == id)
                .map(|position| (bus.id, position))
        })
    }

    fn send_mut(&mut self, id: SendId) -> Result<&mut Send, MixerError> {
        let (bus_id, position) = self.find_send(id).ok_or(MixerError::MissingSend(id))?;
        Ok(&mut self.bus_mut(bus_id)?.sends[position])
    }
}

/// A complete reversible mixer state transition suitable for session history.
///
/// Commands compare the expected state before applying or reverting, so stale
/// history cannot silently overwrite unrelated mixer edits.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerCommand {
    label: String,
    before: MixerGraph,
    after: MixerGraph,
}

impl MixerCommand {
    pub fn build<F>(
        label: impl Into<String>,
        current: &MixerGraph,
        edit: F,
    ) -> Result<Self, MixerError>
    where
        F: FnOnce(&mut MixerGraph) -> Result<(), MixerError>,
    {
        Self::build_at_revision(label, current.revision(), current, edit)
    }

    /// Build a semantic intent against the revision observed at gesture start.
    pub fn build_at_revision<F>(
        label: impl Into<String>,
        expected_revision: u64,
        current: &MixerGraph,
        edit: F,
    ) -> Result<Self, MixerError>
    where
        F: FnOnce(&mut MixerGraph) -> Result<(), MixerError>,
    {
        if current.revision != expected_revision {
            return Err(MixerError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        let mut after = current.clone();
        edit(&mut after)?;
        after.validate()?;
        after.revision = current
            .revision
            .checked_add(1)
            .ok_or(MixerError::RevisionExhausted)?;
        Ok(Self {
            label: label.into(),
            before: current.clone(),
            after,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn before(&self) -> &MixerGraph {
        &self.before
    }

    pub fn after(&self) -> &MixerGraph {
        &self.after
    }

    /// Rebase only the graph's ephemeral optimistic token while proving the
    /// durable graph content still exactly matches this command's `before`.
    /// Project journal recovery uses this after repository decoding, because
    /// project/domain revisions are durable while `MixerGraph::revision` is
    /// intentionally not part of the file format.
    pub(crate) fn rebase_ephemeral_revision_for_replay(
        &self,
        current: &MixerGraph,
    ) -> Result<Self, MixerError> {
        let mut before = self.before.clone();
        before.revision = current.revision;
        if &before != current {
            return Err(MixerError::CommandConflict);
        }
        let mut after = self.after.clone();
        after.revision = current
            .revision
            .checked_add(1)
            .ok_or(MixerError::RevisionExhausted)?;
        Ok(Self {
            label: self.label.clone(),
            before,
            after,
        })
    }

    pub fn inverse(&self) -> Self {
        let mut after = self.before.clone();
        after.revision = self.after.revision.saturating_add(1);
        Self {
            label: self.label.clone(),
            before: self.after.clone(),
            after,
        }
    }

    pub fn apply(&self, graph: &mut MixerGraph) -> Result<(), MixerError> {
        if graph.revision != self.before.revision {
            return Err(MixerError::RevisionConflict {
                expected: self.before.revision,
                actual: graph.revision,
            });
        }
        if graph != &self.before {
            return Err(MixerError::CommandConflict);
        }
        *graph = self.after.clone();
        Ok(())
    }

    pub fn revert(&self, graph: &mut MixerGraph) -> Result<(), MixerError> {
        self.inverse().apply(graph)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MixerError {
    MissingBus(BusId),
    MissingMaster(BusId),
    MissingSend(SendId),
    MissingProcessor(ProcessorId),
    MissingParameter(ParameterId),
    MissingOutput(BusId),
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    RevisionExhausted,
    MasterAlreadyExists,
    MasterCannotRoute,
    MasterCannotSend,
    CannotRemoveMaster,
    BusStillReferenced(BusId),
    CycleDetected(Vec<BusId>),
    InvalidValue {
        field: &'static str,
        value: f32,
        minimum: Option<f32>,
        maximum: Option<f32>,
    },
    EmptyField(&'static str),
    InvalidInsertIndex {
        index: usize,
        len: usize,
    },
    ProcessorNotOnBus {
        id: ProcessorId,
        bus_id: BusId,
    },
    ProcessorUsedTwice(ProcessorId),
    OrphanProcessor(ProcessorId),
    DuplicateNode(NodeId),
    DuplicateSend(SendId),
    DuplicateParameter(ParameterId),
    DuplicateParameterKey {
        processor_id: ProcessorId,
        key: String,
    },
    IdentityMismatch(&'static str),
    IdExhausted(&'static str),
    CommandConflict,
}

impl fmt::Display for MixerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBus(id) => write!(f, "mixer bus {id} does not exist"),
            Self::MissingMaster(id) => write!(f, "master bus {id} does not exist"),
            Self::MissingSend(id) => write!(f, "send {id} does not exist"),
            Self::MissingProcessor(id) => write!(f, "processor {id} does not exist"),
            Self::MissingParameter(id) => write!(f, "parameter {id} does not exist"),
            Self::MissingOutput(id) => write!(f, "non-master bus {id} has no main output"),
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "mixer revision conflict: expected {expected}, found {actual}"
            ),
            Self::RevisionExhausted => write!(f, "mixer revision exhausted"),
            Self::MasterAlreadyExists => write!(f, "a mixer graph has exactly one master bus"),
            Self::MasterCannotRoute => write!(f, "the master bus cannot have a main output"),
            Self::MasterCannotSend => write!(f, "the master bus cannot create sends"),
            Self::CannotRemoveMaster => write!(f, "the master bus cannot be removed"),
            Self::BusStillReferenced(id) => write!(f, "bus {id} is still a routing target"),
            Self::CycleDetected(path) => write!(f, "routing cycle detected: {path:?}"),
            Self::InvalidValue {
                field,
                value,
                minimum,
                maximum,
            } => write!(
                f,
                "invalid {field} value {value} (range {minimum:?}..={maximum:?})"
            ),
            Self::EmptyField(field) => write!(f, "{field} cannot be empty"),
            Self::InvalidInsertIndex { index, len } => {
                write!(f, "insert index {index} exceeds chain length {len}")
            }
            Self::ProcessorNotOnBus { id, bus_id } => {
                write!(f, "processor {id} is not inserted on bus {bus_id}")
            }
            Self::ProcessorUsedTwice(id) => write!(f, "processor {id} is inserted twice"),
            Self::OrphanProcessor(id) => write!(f, "processor {id} is not in an insert chain"),
            Self::DuplicateNode(id) => write!(f, "node id {id} is used twice"),
            Self::DuplicateSend(id) => write!(f, "send id {id} is used twice"),
            Self::DuplicateParameter(id) => write!(f, "parameter id {id} is used twice"),
            Self::DuplicateParameterKey { processor_id, key } => {
                write!(
                    f,
                    "processor {processor_id} has duplicate parameter key {key:?}"
                )
            }
            Self::IdentityMismatch(kind) => write!(f, "{kind} map key does not match its id"),
            Self::IdExhausted(kind) => write!(f, "{kind} id space is exhausted"),
            Self::CommandConflict => write!(f, "mixer command no longer matches current state"),
        }
    }
}

impl Error for MixerError {}

fn validate_descriptor(descriptor: &PluginDescriptor) -> Result<(), MixerError> {
    if descriptor.format.is_empty() {
        return Err(MixerError::EmptyField("plugin format"));
    }
    if descriptor.identifier.is_empty() {
        return Err(MixerError::EmptyField("plugin identifier"));
    }
    Ok(())
}

fn validate_finite(field: &'static str, value: f32) -> Result<(), MixerError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(MixerError::InvalidValue {
            field,
            value,
            minimum: None,
            maximum: None,
        })
    }
}

fn validate_unit(
    field: &'static str,
    value: f32,
    minimum: f32,
    maximum: f32,
) -> Result<(), MixerError> {
    if value.is_finite() && value >= minimum && value <= maximum {
        Ok(())
    } else {
        Err(MixerError::InvalidValue {
            field,
            value,
            minimum: Some(minimum),
            maximum: Some(maximum),
        })
    }
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn peek_id(next: u64, kind: &'static str) -> Result<u64, MixerError> {
    (next != u64::MAX)
        .then_some(next)
        .ok_or(MixerError::IdExhausted(kind))
}

fn take_id(next: &mut u64, kind: &'static str) -> Result<u64, MixerError> {
    let id = peek_id(*next, kind)?;
    *next += 1;
    Ok(id)
}

fn collect_reachable(
    root: BusId,
    routes: &[RouteEdge],
    forward: bool,
    collected: &mut BTreeSet<BusId>,
) {
    let mut pending = vec![root];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        collected.insert(id);
        let mut neighbors: Vec<_> = routes
            .iter()
            .filter_map(|route| {
                if forward && route.from == id {
                    Some(route.to)
                } else if !forward && route.to == id {
                    Some(route.from)
                } else {
                    None
                }
            })
            .collect();
        neighbors.sort_unstable_by(|a, b| b.cmp(a));
        pending.extend(neighbors);
    }
}

fn find_cycle(buses: &BTreeMap<BusId, Bus>, routes: &[RouteEdge]) -> Option<Vec<BusId>> {
    fn visit(
        id: BusId,
        routes: &[RouteEdge],
        states: &mut BTreeMap<BusId, u8>,
        stack: &mut Vec<BusId>,
    ) -> Option<Vec<BusId>> {
        states.insert(id, 1);
        stack.push(id);
        let neighbors: BTreeSet<_> = routes
            .iter()
            .filter_map(|route| (route.from == id).then_some(route.to))
            .collect();
        for neighbor in neighbors {
            match states.get(&neighbor).copied().unwrap_or(0) {
                0 => {
                    if let Some(cycle) = visit(neighbor, routes, states, stack) {
                        return Some(cycle);
                    }
                }
                1 => {
                    let start = stack
                        .iter()
                        .position(|candidate| *candidate == neighbor)
                        .expect("visiting node is on stack");
                    let mut cycle = stack[start..].to_vec();
                    cycle.push(neighbor);
                    return Some(cycle);
                }
                _ => {}
            }
        }
        stack.pop();
        states.insert(id, 2);
        None
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    for &id in buses.keys() {
        if states.get(&id).copied().unwrap_or(0) == 0 {
            if let Some(cycle) = visit(id, routes, &mut states, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str) -> PluginDescriptor {
        PluginDescriptor::new("test-opaque", format!("test.{name}"), name)
    }

    #[test]
    fn graph_has_one_master_and_validates_values_and_references() {
        let mut graph = MixerGraph::default();
        let source = graph.add_bus(BusKind::Source, "Dialogue").unwrap();
        assert_eq!(graph.bus(source).unwrap().output(), Some(graph.master()));
        assert!(graph.validate().is_ok());
        assert!(matches!(
            graph.add_bus(BusKind::Master, "Other master"),
            Err(MixerError::MasterAlreadyExists)
        ));
        assert!(matches!(
            graph.set_pan(source, 1.01),
            Err(MixerError::InvalidValue { field: "pan", .. })
        ));
        assert!(matches!(
            graph.set_output(source, BusId::from_raw(999)),
            Err(MixerError::MissingBus(_))
        ));
    }

    #[test]
    fn main_routes_and_sends_reject_cycles_without_mutating_graph() {
        let mut graph = MixerGraph::default();
        let source = graph.add_bus(BusKind::Source, "Source").unwrap();
        let group = graph.add_bus(BusKind::Group, "Group").unwrap();
        graph.set_output(source, group).unwrap();
        let before = graph.clone();
        assert!(matches!(
            graph.set_output(group, source),
            Err(MixerError::CycleDetected(_))
        ));
        assert_eq!(graph, before);
        assert!(matches!(
            graph.add_send(group, source, SendTap::PreFader, -6.0),
            Err(MixerError::CycleDetected(_))
        ));
        assert_eq!(graph, before);
    }

    #[test]
    fn solo_paths_keep_contributors_and_targets_but_not_siblings() {
        let mut graph = MixerGraph::default();
        let lead = graph.add_bus(BusKind::Source, "Lead").unwrap();
        let sibling = graph.add_bus(BusKind::Source, "Sibling").unwrap();
        let group = graph.add_bus(BusKind::Group, "Group").unwrap();
        graph.set_output(lead, group).unwrap();
        graph.set_output(sibling, group).unwrap();

        graph.set_soloed(lead, true).unwrap();
        let states = graph.effective_states();
        assert!(states[&lead].audible);
        assert!(states[&group].audible);
        assert!(states[&graph.master()].audible);
        assert!(!states[&sibling].audible);

        graph.set_soloed(lead, false).unwrap();
        graph.set_soloed(group, true).unwrap();
        let states = graph.effective_states();
        assert!(states[&lead].audible && states[&sibling].audible);
        graph.set_muted(group, true).unwrap();
        assert!(!graph.effective_states()[&group].audible);
    }

    #[test]
    fn insert_order_bypass_wet_and_parameters_are_preserved() {
        let mut graph = MixerGraph::default();
        let bus = graph.add_bus(BusKind::Component, "Harmonic").unwrap();
        let first = graph.insert_processor(bus, None, plugin("EQ"), 12).unwrap();
        let second = graph
            .insert_processor(bus, Some(0), plugin("Compressor"), 4)
            .unwrap();
        assert_eq!(
            graph
                .bus(bus)
                .unwrap()
                .inserts()
                .iter()
                .map(|slot| slot.processor_id())
                .collect::<Vec<_>>(),
            vec![second, first]
        );
        graph.set_insert_bypassed(second, true).unwrap();
        graph.set_insert_wet(first, 0.25).unwrap();
        let parameter = graph
            .add_parameter(first, "frequency", "Frequency", 0.5)
            .unwrap();
        graph.set_parameter_value(first, parameter, 0.75).unwrap();
        assert_eq!(
            graph
                .processor(first)
                .unwrap()
                .parameter(parameter)
                .unwrap()
                .normalized_value(),
            0.75
        );
        assert_eq!(
            graph.latency_plan().unwrap().buses[&bus].insert_latency_samples,
            12
        );
    }

    #[test]
    fn pre_and_post_fader_sends_have_explicit_gain_semantics() {
        let mut graph = MixerGraph::default();
        let source = graph.add_bus(BusKind::Source, "Source").unwrap();
        let reverb = graph.add_bus(BusKind::Group, "Reverb").unwrap();
        graph.set_gain_db(source, -6.0).unwrap();
        let pre = graph
            .add_send(source, reverb, SendTap::PreFader, -3.0)
            .unwrap();
        let post = graph
            .add_send(source, reverb, SendTap::PostFader, -3.0)
            .unwrap();
        let ratio =
            graph.effective_send_gain(post).unwrap() / graph.effective_send_gain(pre).unwrap();
        assert!((ratio - db_to_linear(-6.0)).abs() < 1.0e-6);
        graph.set_send_muted(pre, true).unwrap();
        assert_eq!(graph.effective_send_gain(pre).unwrap(), 0.0);
    }

    #[test]
    fn latency_plan_aligns_each_target_and_ignores_bypassed_inserts() {
        let mut graph = MixerGraph::default();
        let fast = graph.add_bus(BusKind::Source, "Fast").unwrap();
        let slow = graph.add_bus(BusKind::Source, "Slow").unwrap();
        let group = graph.add_bus(BusKind::Group, "Group").unwrap();
        graph.set_output(fast, group).unwrap();
        graph.set_output(slow, group).unwrap();
        let fast_fx = graph
            .insert_processor(fast, None, plugin("fast"), 10)
            .unwrap();
        graph
            .insert_processor(slow, None, plugin("slow"), 30)
            .unwrap();
        graph
            .insert_processor(group, None, plugin("group"), 5)
            .unwrap();

        let plan = graph.latency_plan().unwrap();
        let fast_route = RouteEdge {
            from: fast,
            to: group,
            kind: RouteKind::Main,
        };
        let slow_route = RouteEdge {
            from: slow,
            to: group,
            kind: RouteKind::Main,
        };
        assert_eq!(plan.routes[&fast_route].compensation_delay_samples, 20);
        assert_eq!(plan.routes[&slow_route].compensation_delay_samples, 0);
        assert_eq!(plan.buses[&group].output_latency_samples, 35);
        assert_eq!(plan.master_output_latency_samples, 35);

        graph.set_insert_bypassed(fast_fx, true).unwrap();
        let plan = graph.latency_plan().unwrap();
        assert_eq!(plan.routes[&fast_route].arrival_latency_samples, 0);
        assert_eq!(plan.routes[&fast_route].compensation_delay_samples, 30);
    }

    #[test]
    fn ids_are_typed_stable_and_never_reused_after_removal() {
        let mut graph = MixerGraph::default();
        let first = graph.add_bus(BusKind::Source, "First").unwrap();
        graph.remove_bus(first).unwrap();
        let second = graph.add_bus(BusKind::Source, "Second").unwrap();
        assert!(second.get() > first.get());

        let first_send = graph
            .add_send(second, graph.master(), SendTap::PreFader, 0.0)
            .unwrap();
        graph.remove_send(first_send).unwrap();
        let second_send = graph
            .add_send(second, graph.master(), SendTap::PreFader, 0.0)
            .unwrap();
        assert!(second_send.get() > first_send.get());

        let processor = graph
            .insert_processor(second, None, plugin("one"), 0)
            .unwrap();
        let parameter = graph.add_parameter(processor, "p", "P", 0.0).unwrap();
        // Each typed namespace is stable and may independently use the same raw value.
        assert_eq!(processor.get(), 1);
        assert_eq!(parameter.get(), 1);
        assert_eq!(
            graph.node_owner(graph.bus(second).unwrap().node_id()),
            Some(NodeOwner::Bus(second))
        );
    }

    #[test]
    fn mixer_commands_are_exactly_reversible_and_conflict_safe() {
        let mut graph = MixerGraph::default();
        let command = MixerCommand::build("Add music bus", &graph, |draft| {
            draft.add_bus(BusKind::Source, "Music")?;
            Ok(())
        })
        .unwrap();
        let original = graph.clone();
        command.apply(&mut graph).unwrap();
        assert_ne!(graph, original);
        command.revert(&mut graph).unwrap();
        assert_eq!(graph.buses, original.buses);
        assert_eq!(graph.processors, original.processors);
        assert_eq!(graph.revision(), original.revision() + 2);
        graph.add_bus(BusKind::Group, "Unrelated").unwrap();
        assert!(matches!(
            command.apply(&mut graph),
            Err(MixerError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn replay_rebase_changes_only_ephemeral_revision_and_rejects_content_drift() {
        let mut original = MixerGraph::default();
        let first = MixerCommand::build("First", &original, |draft| {
            draft.add_bus(BusKind::Source, "First")?;
            Ok(())
        })
        .unwrap();
        first.apply(&mut original).unwrap();
        let second = MixerCommand::build("Second", &original, |draft| {
            draft.add_bus(BusKind::Source, "Second")?;
            Ok(())
        })
        .unwrap();

        // Repository decoding reconstructs identical graph content through
        // public APIs, so its deliberately ephemeral token starts at zero.
        let mut decoded = MixerGraph::default();
        decoded.add_bus(BusKind::Source, "First").unwrap();
        assert_eq!(decoded.revision(), 0);
        let rebased = second
            .rebase_ephemeral_revision_for_replay(&decoded)
            .unwrap();
        rebased.apply(&mut decoded).unwrap();
        assert!(decoded.buses().any(|bus| bus.name() == "Second"));

        let mut divergent = MixerGraph::default();
        divergent.add_bus(BusKind::Source, "Different").unwrap();
        assert!(matches!(
            second.rebase_ephemeral_revision_for_replay(&divergent),
            Err(MixerError::CommandConflict)
        ));
    }
}
