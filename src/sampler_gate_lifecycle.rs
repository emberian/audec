//! Pure ownership rules for momentary sampler-pad audition.
//!
//! A pad can be held simultaneously by a pointer and one or more keyboard
//! inputs.  The audio boundary must still receive exactly one semantic press
//! when the first owner arrives and one release when the last owner leaves.
//! This state machine deliberately returns transitions instead of invoking a
//! callback so the GPUI surface can preserve project/session authority.

use std::collections::BTreeMap;

use crate::sample_actions::SamplerGatePress;
use crate::sample_kit::PadId;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SamplerGateTransition {
    Press(SamplerGatePress),
    Release(SamplerGatePress),
}

#[derive(Clone, Debug, Default)]
pub struct SamplerGateLifecycle {
    pointer: Option<SamplerGatePress>,
    keys: BTreeMap<String, SamplerGatePress>,
}

impl SamplerGateLifecycle {
    pub fn pointer(&self) -> Option<SamplerGatePress> {
        self.pointer
    }

    pub fn key(&self, key: &str) -> Option<SamplerGatePress> {
        self.keys.get(key).copied()
    }

    pub fn holds_pad(&self, pad: PadId) -> bool {
        self.pointer.is_some_and(|gate| gate.pad == pad)
            || self.keys.values().any(|gate| gate.pad == pad)
    }

    pub fn press_pointer(&mut self, gate: SamplerGatePress) -> Vec<SamplerGateTransition> {
        if self.pointer.is_some_and(|held| held.pad == gate.pad) {
            return Vec::new();
        }
        let mut transitions = Vec::with_capacity(2);
        if let Some(previous) = self.pointer.replace(gate) {
            if !self.keys.values().any(|held| held.pad == previous.pad) {
                transitions.push(SamplerGateTransition::Release(previous));
            }
        }
        if !self.keys.values().any(|held| held.pad == gate.pad) {
            transitions.push(SamplerGateTransition::Press(gate));
        }
        transitions
    }

    pub fn release_pointer(&mut self) -> Vec<SamplerGateTransition> {
        let Some(gate) = self.pointer.take() else {
            return Vec::new();
        };
        if self.keys.values().any(|held| held.pad == gate.pad) {
            Vec::new()
        } else {
            vec![SamplerGateTransition::Release(gate)]
        }
    }

    pub fn press_key(
        &mut self,
        key: impl Into<String>,
        gate: SamplerGatePress,
    ) -> Vec<SamplerGateTransition> {
        let key = key.into();
        if self.keys.contains_key(&key) {
            return Vec::new();
        }
        let already_held = self.holds_pad(gate.pad);
        self.keys.insert(key, gate);
        if already_held {
            Vec::new()
        } else {
            vec![SamplerGateTransition::Press(gate)]
        }
    }

    pub fn release_key(&mut self, key: &str) -> Vec<SamplerGateTransition> {
        let Some(gate) = self.keys.remove(key) else {
            return Vec::new();
        };
        if self.holds_pad(gate.pad) {
            Vec::new()
        } else {
            vec![SamplerGateTransition::Release(gate)]
        }
    }

    pub fn release_pad(&mut self, pad: PadId) -> Vec<SamplerGateTransition> {
        let mut representative = None;
        if self.pointer.is_some_and(|gate| gate.pad == pad) {
            representative = self.pointer.take();
        }
        self.keys.retain(|_, gate| {
            if gate.pad == pad {
                if representative.is_none() {
                    representative = Some(*gate);
                }
                false
            } else {
                true
            }
        });
        representative
            .map(SamplerGateTransition::Release)
            .into_iter()
            .collect()
    }

    /// Release each audibly held pad once. The representative gate identity
    /// is stable (pointer first, then lexical key order) but remains an
    /// ephemeral UI correlation token; the semantic action retains kit/pad.
    pub fn drain(&mut self) -> Vec<SamplerGateTransition> {
        let mut audible = BTreeMap::<PadId, SamplerGatePress>::new();
        if let Some(gate) = self.pointer.take() {
            audible.insert(gate.pad, gate);
        }
        for gate in std::mem::take(&mut self.keys).into_values() {
            audible.entry(gate.pad).or_insert(gate);
        }
        audible
            .into_values()
            .map(SamplerGateTransition::Release)
            .collect()
    }

    #[cfg(test)]
    pub fn held_pads(&self) -> impl Iterator<Item = PadId> + '_ {
        let pointer = self.pointer.map(|gate| gate.pad);
        let mut pads = self.keys.values().map(|gate| gate.pad).collect::<Vec<_>>();
        if let Some(pointer) = pointer {
            pads.push(pointer);
        }
        pads.sort_unstable();
        pads.dedup();
        pads.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample_actions::SamplerGateId;
    use crate::sample_kit::KitId;

    fn gate(id: u64, pad: u64) -> SamplerGatePress {
        SamplerGatePress {
            id: SamplerGateId(id),
            kit: KitId::from_raw(1),
            pad: PadId::from_raw(pad),
            velocity: 1.0,
        }
    }

    #[test]
    fn pointer_drag_across_pads_releases_before_pressing() {
        let mut held = SamplerGateLifecycle::default();
        assert_eq!(
            held.press_pointer(gate(1, 1)),
            vec![SamplerGateTransition::Press(gate(1, 1))]
        );
        assert_eq!(
            held.press_pointer(gate(2, 2)),
            vec![
                SamplerGateTransition::Release(gate(1, 1)),
                SamplerGateTransition::Press(gate(2, 2)),
            ]
        );
        assert_eq!(
            held.held_pads().collect::<Vec<_>>(),
            vec![PadId::from_raw(2)]
        );
    }

    #[test]
    fn pointer_and_key_share_one_audible_gate_until_last_release() {
        let mut held = SamplerGateLifecycle::default();
        assert_eq!(
            held.press_key("q", gate(1, 4)),
            vec![SamplerGateTransition::Press(gate(1, 4))]
        );
        assert!(held.press_pointer(gate(2, 4)).is_empty());
        assert!(held.release_key("q").is_empty());
        assert_eq!(
            held.release_pointer(),
            vec![SamplerGateTransition::Release(gate(2, 4))]
        );
    }

    #[test]
    fn key_repeat_and_same_pad_pointer_reentry_do_not_retrigger() {
        let mut held = SamplerGateLifecycle::default();
        assert_eq!(
            held.press_key("q", gate(1, 4)),
            vec![SamplerGateTransition::Press(gate(1, 4))]
        );
        assert!(held.press_key("q", gate(2, 4)).is_empty());
        assert_eq!(held.key("q"), Some(gate(1, 4)));
        assert!(held.press_pointer(gate(3, 4)).is_empty());
        assert!(held.press_pointer(gate(4, 4)).is_empty());
        assert_eq!(held.pointer(), Some(gate(3, 4)));
        assert!(held.release_pointer().is_empty());
        assert_eq!(
            held.release_key("q"),
            vec![SamplerGateTransition::Release(gate(1, 4))]
        );
    }

    #[test]
    fn focus_drain_releases_each_pad_once_and_clears_all_owners() {
        let mut held = SamplerGateLifecycle::default();
        held.press_pointer(gate(1, 2));
        held.press_key("q", gate(2, 2));
        held.press_key("w", gate(3, 3));
        let transitions = held.drain();
        assert_eq!(transitions.len(), 2);
        assert_eq!(
            transitions,
            vec![
                SamplerGateTransition::Release(gate(1, 2)),
                SamplerGateTransition::Release(gate(3, 3)),
            ]
        );
        assert!(held.pointer().is_none());
        assert!(held.key("q").is_none());
        assert!(held.held_pads().next().is_none());
        assert!(held.drain().is_empty());
    }

    #[test]
    fn reassignment_release_closes_all_owners_of_only_that_pad() {
        let mut held = SamplerGateLifecycle::default();
        held.press_pointer(gate(1, 2));
        held.press_key("q", gate(2, 2));
        held.press_key("w", gate(3, 3));
        assert_eq!(
            held.release_pad(PadId::from_raw(2)),
            vec![SamplerGateTransition::Release(gate(1, 2))]
        );
        assert!(!held.holds_pad(PadId::from_raw(2)));
        assert!(held.holds_pad(PadId::from_raw(3)));
        assert_eq!(held.key("w"), Some(gate(3, 3)));
    }
}
