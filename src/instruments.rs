//! Deterministic built-in instruments for audec's constructive audio path.
//!
//! The engines in this module consume [`crate::sequencer::ScheduledEvent`]s
//! directly, preserving their exact half-open frame offsets. They render into
//! caller-owned interleaved stereo buffers and retain voices between calls, so
//! the same event stream produces the same samples regardless of block splits.
//!
//! Realtime note: constructors and sample loading allocate, but
//! `render_scheduled_block` does not allocate after voice capacity is reserved.
//! Voice stealing replaces an existing slot in place. A realtime graph should
//! construct/configure an instrument on its control thread, then move the ready
//! instance into the audio graph instead of changing parameters in a callback.

use std::error::Error;
use std::f32::consts::{FRAC_PI_4, PI, TAU};
use std::fmt;
use std::sync::Arc;

use crate::sequencer::{ExpressionDimension, ScheduledEvent, ScheduledKind, TriggerTarget};

const MIN_GAIN_DB: f32 = -120.0;
const MAX_GAIN_DB: f32 = 24.0;
const MAX_VOICES: usize = 256;

/// Stable identity for matching note-on, expression, and note-off events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct VoiceKey {
    clip: u64,
    event: u64,
    channel: u8,
}

impl VoiceKey {
    fn note(clip: u64, note: u64, channel: u8) -> Self {
        Self {
            clip,
            event: note,
            channel,
        }
    }

    fn trigger(clip: u64, lane: u64, ratchet: u8) -> Self {
        Self {
            clip,
            event: lane.rotate_left(17) ^ u64::from(ratchet),
            channel: u8::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Adsr {
    pub attack_seconds: f32,
    pub decay_seconds: f32,
    pub sustain: f32,
    pub release_seconds: f32,
}

impl Default for Adsr {
    fn default() -> Self {
        Self {
            attack_seconds: 0.005,
            decay_seconds: 0.12,
            sustain: 0.72,
            release_seconds: 0.18,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterParams {
    /// Base low-pass cutoff. It is clamped below Nyquist while rendering.
    pub cutoff_hz: f32,
    /// Zero is lightly damped; one approaches self-oscillation.
    pub resonance: f32,
    /// Envelope modulation depth in octaves.
    pub envelope_octaves: f32,
}

impl Default for FilterParams {
    fn default() -> Self {
        Self {
            cutoff_hz: 8_000.0,
            resonance: 0.1,
            envelope_octaves: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FmParams {
    /// Modulator/carrier frequency ratio.
    pub ratio: f32,
    /// Phase-modulation index in radians.
    pub index: f32,
}

impl Default for FmParams {
    fn default() -> Self {
        Self {
            ratio: 2.0,
            index: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SynthParams {
    pub waveform: Waveform,
    pub envelope: Adsr,
    pub filter: FilterParams,
    pub fm: FmParams,
    pub gain_db: f32,
    pub pan: f32,
    pub maximum_voices: usize,
}

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            waveform: Waveform::Saw,
            envelope: Adsr::default(),
            filter: FilterParams::default(),
            fm: FmParams::default(),
            gain_db: -9.0,
            pan: 0.0,
            maximum_voices: 32,
        }
    }
}

impl SynthParams {
    pub fn validate(&self, sample_rate: u32) -> Result<(), InstrumentError> {
        if sample_rate == 0 {
            return Err(InstrumentError::InvalidSampleRate);
        }
        if !non_negative(self.envelope.attack_seconds)
            || !non_negative(self.envelope.decay_seconds)
            || !unit(self.envelope.sustain)
            || !non_negative(self.envelope.release_seconds)
        {
            return Err(InstrumentError::InvalidEnvelope);
        }
        if !self.filter.cutoff_hz.is_finite()
            || self.filter.cutoff_hz <= 0.0
            || !unit(self.filter.resonance)
            || !self.filter.envelope_octaves.is_finite()
            || !(-12.0..=12.0).contains(&self.filter.envelope_octaves)
        {
            return Err(InstrumentError::InvalidFilter);
        }
        if !self.fm.ratio.is_finite()
            || self.fm.ratio <= 0.0
            || self.fm.ratio > 32.0
            || !self.fm.index.is_finite()
            || !(0.0..=32.0).contains(&self.fm.index)
        {
            return Err(InstrumentError::InvalidFm);
        }
        validate_output(self.gain_db, self.pan, self.maximum_voices)
    }
}

/// A compact polyphonic subtractive synth with a band-limited saw/square,
/// ADSR, optional phase modulation, and a stable TPT state-variable low-pass.
pub struct SubtractiveSynth {
    sample_rate: u32,
    instrument_id: u64,
    params: SynthParams,
    voices: Vec<SynthVoice>,
    next_age: u64,
}

impl SubtractiveSynth {
    pub fn new(
        sample_rate: u32,
        instrument_id: u64,
        params: SynthParams,
    ) -> Result<Self, InstrumentError> {
        params.validate(sample_rate)?;
        let maximum_voices = params.maximum_voices;
        Ok(Self {
            sample_rate,
            instrument_id,
            params,
            voices: Vec::with_capacity(maximum_voices),
            next_age: 0,
        })
    }

    pub fn params(&self) -> &SynthParams {
        &self.params
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.len()
    }

    /// Adds this instrument's output to a caller-owned interleaved stereo block.
    pub fn render_scheduled_block(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        let frames = validate_block(events, output)?;
        let mut event_index = 0;
        for frame in 0..frames {
            let absolute_frame = project_start.saturating_add(frame as i64);
            self.release_due_voices(absolute_frame);
            while event_index < events.len() && events[event_index].block_offset as usize == frame {
                self.handle_event(absolute_frame, &events[event_index]);
                event_index += 1;
            }
            let (mut left, mut right) = (0.0, 0.0);
            for voice in &mut self.voices {
                let (voice_left, voice_right) = voice.next_sample(self.sample_rate, &self.params);
                left += voice_left;
                right += voice_right;
            }
            self.voices.retain(|voice| !voice.envelope.finished());
            output[frame * 2] += finite_or_zero(left);
            output[frame * 2 + 1] += finite_or_zero(right);
        }
        Ok(())
    }

    pub fn render_scheduled_block_replace(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        output.fill(0.0);
        self.render_scheduled_block(project_start, events, output)
    }

    fn handle_event(&mut self, absolute_frame: i64, event: &ScheduledEvent) {
        match &event.kind {
            ScheduledKind::LoopBoundary => self.voices.clear(),
            ScheduledKind::NoteOn {
                clip,
                note,
                instrument: Some(instrument),
                pitch,
                velocity,
                pan,
                channel,
                ..
            } if *instrument == self.instrument_id => {
                let key = VoiceKey::note(clip.get(), note.get(), *channel);
                self.start_voice(key, pitch.midi_key, pitch.cents, *velocity, *pan, None);
            }
            ScheduledKind::NoteOff {
                clip,
                note,
                instrument: Some(instrument),
                release_velocity,
                channel,
            } if *instrument == self.instrument_id => {
                let key = VoiceKey::note(clip.get(), note.get(), *channel);
                for voice in self.voices.iter_mut().filter(|voice| voice.key == key) {
                    voice.envelope.release(
                        *release_velocity,
                        &self.params.envelope,
                        self.sample_rate,
                    );
                }
            }
            ScheduledKind::NoteExpression {
                clip,
                note,
                instrument: Some(instrument),
                dimension,
                value,
                channel,
            } if *instrument == self.instrument_id && value.is_finite() => {
                let key = VoiceKey::note(clip.get(), note.get(), *channel);
                for voice in self.voices.iter_mut().filter(|voice| voice.key == key) {
                    match dimension {
                        ExpressionDimension::PitchCents => voice.expression_pitch_cents = *value,
                        ExpressionDimension::Pressure => voice.pressure = value.clamp(0.0, 1.0),
                        ExpressionDimension::Timbre => voice.timbre = value.clamp(-1.0, 1.0),
                    }
                }
            }
            ScheduledKind::Trigger {
                clip,
                lane,
                target: TriggerTarget::InstrumentNote { instrument, key },
                velocity,
                pan,
                pitch_semitones,
                gate_frames,
                ratchet,
                ..
            } if *instrument == self.instrument_id => {
                let voice_key = VoiceKey::trigger(clip.get(), lane.get(), *ratchet);
                let auto_off =
                    absolute_frame.saturating_add((*gate_frames).min(i64::MAX as u64) as i64);
                self.start_voice(
                    voice_key,
                    *key,
                    *pitch_semitones * 100.0,
                    *velocity,
                    *pan,
                    Some(auto_off),
                );
            }
            _ => {}
        }
    }

    fn start_voice(
        &mut self,
        key: VoiceKey,
        midi_key: u8,
        cents: f32,
        velocity: f32,
        pan: f32,
        auto_off: Option<i64>,
    ) {
        if !cents.is_finite() || !velocity.is_finite() || velocity <= 0.0 {
            return;
        }
        // Retriggering an identity replaces it rather than leaving an
        // unreachable duplicate behind.
        if let Some(index) = self.voices.iter().position(|voice| voice.key == key) {
            self.voices.swap_remove(index);
        }
        let voice = SynthVoice::new(
            key,
            midi_key,
            cents,
            velocity.clamp(0.0, 1.0),
            sane_pan(pan),
            auto_off,
            self.next_age,
        );
        self.next_age = self.next_age.wrapping_add(1);
        if self.voices.len() < self.params.maximum_voices {
            self.voices.push(voice);
        } else {
            let victim = self
                .voices
                .iter()
                .enumerate()
                .min_by(|(_, left), (_, right)| {
                    left.steal_score()
                        .total_cmp(&right.steal_score())
                        .then_with(|| left.age.cmp(&right.age))
                })
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.voices[victim] = voice;
        }
    }

    fn release_due_voices(&mut self, absolute_frame: i64) {
        for voice in self.voices.iter_mut().filter(|voice| {
            voice
                .auto_off
                .is_some_and(|auto_off| auto_off <= absolute_frame)
        }) {
            voice.auto_off = None;
            voice
                .envelope
                .release(1.0, &self.params.envelope, self.sample_rate);
        }
    }
}

struct SynthVoice {
    key: VoiceKey,
    midi_key: u8,
    base_cents: f32,
    expression_pitch_cents: f32,
    velocity: f32,
    pressure: f32,
    timbre: f32,
    pan: f32,
    phase: f32,
    mod_phase: f32,
    envelope: Envelope,
    filter: StateVariableFilter,
    auto_off: Option<i64>,
    age: u64,
}

impl SynthVoice {
    fn new(
        key: VoiceKey,
        midi_key: u8,
        cents: f32,
        velocity: f32,
        pan: f32,
        auto_off: Option<i64>,
        age: u64,
    ) -> Self {
        Self {
            key,
            midi_key,
            base_cents: cents,
            expression_pitch_cents: 0.0,
            velocity,
            pressure: 1.0,
            timbre: 0.0,
            pan,
            phase: 0.0,
            mod_phase: 0.0,
            envelope: Envelope::new(),
            filter: StateVariableFilter::default(),
            auto_off,
            age,
        }
    }

    fn next_sample(&mut self, sample_rate: u32, params: &SynthParams) -> (f32, f32) {
        let envelope = self.envelope.next(params.envelope, sample_rate);
        let frequency = midi_frequency(
            self.midi_key,
            self.base_cents + self.expression_pitch_cents,
            sample_rate,
        );
        let dt = frequency / sample_rate as f32;
        let phase_mod = self.mod_phase.sin() * params.fm.index / TAU;
        let oscillator = oscillator(params.waveform, wrap_phase(self.phase + phase_mod), dt);
        self.phase = wrap_phase(self.phase + dt);
        self.mod_phase = wrap_phase(self.mod_phase + dt * params.fm.ratio);

        let cutoff = (params.filter.cutoff_hz
            * 2.0_f32.powf(params.filter.envelope_octaves * envelope + self.timbre * 2.0))
        .clamp(10.0, sample_rate as f32 * 0.45);
        let filtered =
            self.filter
                .low_pass(oscillator, cutoff, params.filter.resonance, sample_rate);
        let amplitude = envelope * self.velocity * self.pressure * db_to_linear(params.gain_db);
        pan_mono(
            filtered * amplitude,
            (params.pan + self.pan).clamp(-1.0, 1.0),
        )
    }

    fn steal_score(&self) -> f32 {
        let releasing = if self.envelope.stage == EnvelopeStage::Release {
            0.0
        } else {
            2.0
        };
        releasing + self.envelope.level
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplerMode {
    /// The sample always plays through; note-off is ignored.
    OneShot,
    /// A note-off stops the sample at its exact event frame.
    Gated,
}

#[derive(Clone, Debug)]
pub struct SampleData {
    pub sample_rate: u32,
    pub channels: u8,
    pub interleaved: Arc<[f32]>,
    pub root_key: u8,
    pub tuning_cents: f32,
}

impl SampleData {
    pub fn from_interleaved(
        sample_rate: u32,
        channels: u8,
        interleaved: impl Into<Arc<[f32]>>,
        root_key: u8,
        tuning_cents: f32,
    ) -> Result<Self, InstrumentError> {
        let value = Self {
            sample_rate,
            channels,
            interleaved: interleaved.into(),
            root_key,
            tuning_cents,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn frame_count(&self) -> usize {
        self.interleaved.len() / usize::from(self.channels)
    }

    fn validate(&self) -> Result<(), InstrumentError> {
        if self.sample_rate == 0 {
            return Err(InstrumentError::InvalidSampleRate);
        }
        if !(1..=2).contains(&self.channels)
            || self.interleaved.is_empty()
            || self.interleaved.len() % usize::from(self.channels) != 0
        {
            return Err(InstrumentError::InvalidSample);
        }
        if !self.tuning_cents.is_finite()
            || !(-9_600.0..=9_600.0).contains(&self.tuning_cents)
            || self.interleaved.iter().any(|sample| !sample.is_finite())
        {
            return Err(InstrumentError::InvalidSample);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SamplerParams {
    pub mode: SamplerMode,
    pub gain_db: f32,
    pub pan: f32,
    pub maximum_voices: usize,
    /// A sequencer `TriggerTarget::Sample` must match this raw asset id.
    /// `None` disables sample-target triggers while MIDI note events still play.
    pub trigger_asset: Option<u64>,
    /// Default choke group for a routed pad. An explicitly authored event
    /// group takes precedence; runtime route normalization supplies this
    /// value when the sequencer lane itself has no group.
    pub choke_group: Option<u32>,
}

impl Default for SamplerParams {
    fn default() -> Self {
        Self {
            mode: SamplerMode::OneShot,
            gain_db: 0.0,
            pan: 0.0,
            maximum_voices: 32,
            trigger_asset: None,
            choke_group: None,
        }
    }
}

impl SamplerParams {
    pub fn validate(&self) -> Result<(), InstrumentError> {
        validate_output(self.gain_db, self.pan, self.maximum_voices)
    }
}

/// Pitchable mono/stereo one-shot sampler with linear interpolation.
pub struct Sampler {
    output_sample_rate: u32,
    sample: SampleData,
    params: SamplerParams,
    voices: Vec<SampleVoice>,
    next_age: u64,
}

impl Sampler {
    pub fn new(
        output_sample_rate: u32,
        sample: SampleData,
        params: SamplerParams,
    ) -> Result<Self, InstrumentError> {
        if output_sample_rate == 0 {
            return Err(InstrumentError::InvalidSampleRate);
        }
        sample.validate()?;
        params.validate()?;
        let maximum_voices = params.maximum_voices;
        Ok(Self {
            output_sample_rate,
            sample,
            params,
            voices: Vec::with_capacity(maximum_voices),
            next_age: 0,
        })
    }

    pub fn sample(&self) -> &SampleData {
        &self.sample
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.len()
    }

    pub fn render_scheduled_block(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        let frames = validate_block(events, output)?;
        let mut event_index = 0;
        for frame in 0..frames {
            let absolute_frame = project_start.saturating_add(frame as i64);
            self.voices
                .retain(|voice| voice.auto_off.is_none_or(|off| off > absolute_frame));
            while event_index < events.len() && events[event_index].block_offset as usize == frame {
                self.handle_event(absolute_frame, &events[event_index]);
                event_index += 1;
            }
            let mut mixed = (0.0, 0.0);
            for voice in &mut self.voices {
                let (left, right) = voice.next_sample(&self.sample, &self.params);
                mixed.0 += left;
                mixed.1 += right;
            }
            let sample_frames = self.sample.frame_count() as f64;
            self.voices.retain(|voice| voice.position < sample_frames);
            output[frame * 2] += finite_or_zero(mixed.0);
            output[frame * 2 + 1] += finite_or_zero(mixed.1);
        }
        Ok(())
    }

    pub fn render_scheduled_block_replace(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        output.fill(0.0);
        self.render_scheduled_block(project_start, events, output)
    }

    fn handle_event(&mut self, absolute_frame: i64, event: &ScheduledEvent) {
        match &event.kind {
            ScheduledKind::LoopBoundary => self.voices.clear(),
            ScheduledKind::NoteOn {
                clip,
                note,
                instrument: Some(_),
                pitch,
                velocity,
                pan,
                channel,
                ..
            } => self.start_voice(
                VoiceKey::note(clip.get(), note.get(), *channel),
                pitch.midi_key,
                pitch.cents,
                *velocity,
                *pan,
                None,
                None,
            ),
            ScheduledKind::NoteOff {
                clip,
                note,
                instrument: Some(_),
                channel,
                ..
            } if self.params.mode == SamplerMode::Gated => {
                let key = VoiceKey::note(clip.get(), note.get(), *channel);
                self.voices.retain(|voice| voice.key != key);
            }
            ScheduledKind::Trigger {
                clip,
                lane,
                target: TriggerTarget::Sample(asset),
                velocity,
                pan,
                pitch_semitones,
                gate_frames,
                choke_group,
                ratchet,
                ..
            } => {
                if let Some(group) = choke_group {
                    self.voices
                        .retain(|voice| voice.choke_group != Some(*group));
                }
                if self.params.trigger_asset == Some(asset.get()) {
                    let auto_off = (self.params.mode == SamplerMode::Gated).then(|| {
                        absolute_frame.saturating_add((*gate_frames).min(i64::MAX as u64) as i64)
                    });
                    self.start_voice(
                        VoiceKey::trigger(clip.get(), lane.get(), *ratchet),
                        self.sample.root_key,
                        *pitch_semitones * 100.0,
                        *velocity,
                        *pan,
                        auto_off,
                        *choke_group,
                    );
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_voice(
        &mut self,
        key: VoiceKey,
        midi_key: u8,
        cents: f32,
        velocity: f32,
        pan: f32,
        auto_off: Option<i64>,
        choke_group: Option<u32>,
    ) {
        if !cents.is_finite() || !velocity.is_finite() || velocity <= 0.0 {
            return;
        }
        let semitones = midi_key as f32 - self.sample.root_key as f32
            + (cents + self.sample.tuning_cents) / 100.0;
        let rate = 2.0_f64.powf(semitones as f64 / 12.0) * f64::from(self.sample.sample_rate)
            / f64::from(self.output_sample_rate);
        if !rate.is_finite() || rate <= 0.0 {
            return;
        }
        let voice = SampleVoice {
            key,
            position: 0.0,
            rate,
            velocity: velocity.clamp(0.0, 1.0),
            pan: sane_pan(pan),
            auto_off,
            choke_group,
            age: self.next_age,
        };
        self.next_age = self.next_age.wrapping_add(1);
        if self.voices.len() < self.params.maximum_voices {
            self.voices.push(voice);
        } else {
            let victim = self
                .voices
                .iter()
                .enumerate()
                .min_by_key(|(_, voice)| voice.age)
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.voices[victim] = voice;
        }
    }
}

struct SampleVoice {
    key: VoiceKey,
    position: f64,
    rate: f64,
    velocity: f32,
    pan: f32,
    auto_off: Option<i64>,
    choke_group: Option<u32>,
    age: u64,
}

impl SampleVoice {
    fn next_sample(&mut self, sample: &SampleData, params: &SamplerParams) -> (f32, f32) {
        let frame = self.position.floor() as usize;
        if frame >= sample.frame_count() {
            return (0.0, 0.0);
        }
        let next = (frame + 1).min(sample.frame_count() - 1);
        let fraction = (self.position - frame as f64) as f32;
        let (left, right) = if sample.channels == 1 {
            let value = lerp(
                sample.interleaved[frame],
                sample.interleaved[next],
                fraction,
            );
            (value, value)
        } else {
            (
                lerp(
                    sample.interleaved[frame * 2],
                    sample.interleaved[next * 2],
                    fraction,
                ),
                lerp(
                    sample.interleaved[frame * 2 + 1],
                    sample.interleaved[next * 2 + 1],
                    fraction,
                ),
            )
        };
        self.position += self.rate;
        let gain = self.velocity * db_to_linear(params.gain_db);
        pan_stereo(
            left * gain,
            right * gain,
            (params.pan + self.pan).clamp(-1.0, 1.0),
        )
    }
}

/// Convenient graph-facing tagged node.
pub enum BuiltInInstrument {
    Sampler(Sampler),
    Subtractive(SubtractiveSynth),
}

impl BuiltInInstrument {
    pub fn render_scheduled_block(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        match self {
            Self::Sampler(instrument) => {
                instrument.render_scheduled_block(project_start, events, output)
            }
            Self::Subtractive(instrument) => {
                instrument.render_scheduled_block(project_start, events, output)
            }
        }
    }

    pub fn render_scheduled_block_replace(
        &mut self,
        project_start: i64,
        events: &[ScheduledEvent],
        output: &mut [f32],
    ) -> Result<(), InstrumentError> {
        output.fill(0.0);
        self.render_scheduled_block(project_start, events, output)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstrumentError {
    InvalidSampleRate,
    InvalidEnvelope,
    InvalidFilter,
    InvalidFm,
    InvalidGain,
    InvalidPan,
    InvalidVoiceLimit,
    InvalidSample,
    StereoOutputRequired { samples: usize },
    EventOutsideBlock { offset: u32, frames: usize },
    EventsOutOfOrder { preceding: u32, following: u32 },
}

impl fmt::Display for InstrumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => write!(formatter, "sample rate must be non-zero"),
            Self::InvalidEnvelope => write!(formatter, "ADSR parameters are invalid"),
            Self::InvalidFilter => write!(formatter, "filter parameters are invalid"),
            Self::InvalidFm => write!(formatter, "FM parameters are invalid"),
            Self::InvalidGain => {
                write!(formatter, "gain must be finite and between -120 and +24 dB")
            }
            Self::InvalidPan => write!(formatter, "pan must be finite and in [-1, 1]"),
            Self::InvalidVoiceLimit => {
                write!(formatter, "voice limit must be in [1, {MAX_VOICES}]")
            }
            Self::InvalidSample => write!(formatter, "sample PCM or metadata is invalid"),
            Self::StereoOutputRequired { samples } => write!(
                formatter,
                "instrument output must contain complete stereo frames, got {samples} samples"
            ),
            Self::EventOutsideBlock { offset, frames } => write!(
                formatter,
                "event offset {offset} is outside a {frames}-frame half-open block"
            ),
            Self::EventsOutOfOrder {
                preceding,
                following,
            } => write!(
                formatter,
                "event offsets are not ordered: {following} follows {preceding}"
            ),
        }
    }
}

impl Error for InstrumentError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvelopeStage {
    Attack,
    Decay,
    Sustain,
    Release,
    Finished,
}

struct Envelope {
    stage: EnvelopeStage,
    level: f32,
    release_step: f32,
}

impl Envelope {
    fn new() -> Self {
        Self {
            stage: EnvelopeStage::Attack,
            level: 0.0,
            release_step: 0.0,
        }
    }

    fn next(&mut self, params: Adsr, sample_rate: u32) -> f32 {
        match self.stage {
            EnvelopeStage::Attack => {
                let frames = seconds_to_frames(params.attack_seconds, sample_rate);
                if frames == 0 {
                    self.level = 1.0;
                    self.stage = EnvelopeStage::Decay;
                } else {
                    self.level = (self.level + 1.0 / frames as f32).min(1.0);
                    if self.level >= 1.0 {
                        self.stage = EnvelopeStage::Decay;
                    }
                }
            }
            EnvelopeStage::Decay => {
                let frames = seconds_to_frames(params.decay_seconds, sample_rate);
                if frames == 0 {
                    self.level = params.sustain;
                    self.stage = EnvelopeStage::Sustain;
                } else {
                    self.level =
                        (self.level - (1.0 - params.sustain) / frames as f32).max(params.sustain);
                    if self.level <= params.sustain {
                        self.stage = EnvelopeStage::Sustain;
                    }
                }
            }
            EnvelopeStage::Sustain => self.level = params.sustain,
            EnvelopeStage::Release => {
                self.level = (self.level - self.release_step).max(0.0);
                if self.level <= 0.0 {
                    self.stage = EnvelopeStage::Finished;
                }
            }
            EnvelopeStage::Finished => self.level = 0.0,
        }
        self.level
    }

    fn release(&mut self, release_velocity: f32, params: &Adsr, sample_rate: u32) {
        if self.stage == EnvelopeStage::Finished || self.stage == EnvelopeStage::Release {
            return;
        }
        let velocity = if release_velocity.is_finite() {
            release_velocity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let seconds = params.release_seconds * (1.25 - velocity * 0.5);
        let frames = seconds_to_frames(seconds, sample_rate);
        if frames == 0 {
            self.level = 0.0;
            self.stage = EnvelopeStage::Finished;
        } else {
            self.release_step = self.level / frames as f32;
            self.stage = EnvelopeStage::Release;
        }
    }

    fn finished(&self) -> bool {
        self.stage == EnvelopeStage::Finished
    }
}

#[derive(Default)]
struct StateVariableFilter {
    integrator_one: f32,
    integrator_two: f32,
}

impl StateVariableFilter {
    fn low_pass(&mut self, input: f32, cutoff: f32, resonance: f32, sample_rate: u32) -> f32 {
        let g = (PI * cutoff / sample_rate as f32).tan();
        let damping = 2.0 - resonance.clamp(0.0, 1.0) * 1.9;
        let a1 = 1.0 / (1.0 + g * (g + damping));
        let v1 = (self.integrator_one + g * (input - self.integrator_two)) * a1;
        let v2 = self.integrator_two + g * v1;
        self.integrator_one = finite_or_zero(2.0 * v1 - self.integrator_one);
        self.integrator_two = finite_or_zero(2.0 * v2 - self.integrator_two);
        finite_or_zero(v2)
    }
}

fn validate_block(events: &[ScheduledEvent], output: &[f32]) -> Result<usize, InstrumentError> {
    if output.len() % 2 != 0 {
        return Err(InstrumentError::StereoOutputRequired {
            samples: output.len(),
        });
    }
    let frames = output.len() / 2;
    let mut preceding = None;
    for event in events {
        if event.block_offset as usize >= frames {
            return Err(InstrumentError::EventOutsideBlock {
                offset: event.block_offset,
                frames,
            });
        }
        if let Some(previous) = preceding {
            if event.block_offset < previous {
                return Err(InstrumentError::EventsOutOfOrder {
                    preceding: previous,
                    following: event.block_offset,
                });
            }
        }
        preceding = Some(event.block_offset);
    }
    Ok(frames)
}

fn validate_output(gain_db: f32, pan: f32, maximum_voices: usize) -> Result<(), InstrumentError> {
    if !gain_db.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&gain_db) {
        return Err(InstrumentError::InvalidGain);
    }
    if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
        return Err(InstrumentError::InvalidPan);
    }
    if !(1..=MAX_VOICES).contains(&maximum_voices) {
        return Err(InstrumentError::InvalidVoiceLimit);
    }
    Ok(())
}

fn oscillator(waveform: Waveform, phase: f32, dt: f32) -> f32 {
    match waveform {
        Waveform::Sine => (phase * TAU).sin(),
        Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
        Waveform::Saw => {
            let mut value = phase * 2.0 - 1.0;
            value -= poly_blep(phase, dt);
            value
        }
        Waveform::Square => {
            let mut value = if phase < 0.5 { 1.0 } else { -1.0 };
            value += poly_blep(phase, dt);
            value -= poly_blep(wrap_phase(phase + 0.5), dt);
            value
        }
    }
}

fn poly_blep(phase: f32, dt: f32) -> f32 {
    if dt <= 0.0 {
        return 0.0;
    }
    if phase < dt {
        let t = phase / dt;
        t + t - t * t - 1.0
    } else if phase > 1.0 - dt {
        let t = (phase - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

fn midi_frequency(key: u8, cents: f32, sample_rate: u32) -> f32 {
    let semitones = key as f32 - 69.0 + cents / 100.0;
    (440.0 * 2.0_f32.powf(semitones / 12.0)).clamp(1.0, sample_rate as f32 * 0.45)
}

fn seconds_to_frames(seconds: f32, sample_rate: u32) -> u64 {
    (seconds as f64 * f64::from(sample_rate))
        .round()
        .clamp(0.0, u64::MAX as f64) as u64
}

fn wrap_phase(phase: f32) -> f32 {
    phase - phase.floor()
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn pan_mono(value: f32, pan: f32) -> (f32, f32) {
    let angle = (pan + 1.0) * FRAC_PI_4;
    (value * angle.cos(), value * angle.sin())
}

fn pan_stereo(left: f32, right: f32, pan: f32) -> (f32, f32) {
    let angle = (pan + 1.0) * FRAC_PI_4;
    (left * angle.cos(), right * angle.sin())
}

fn lerp(left: f32, right: f32, amount: f32) -> f32 {
    left + (right - left) * amount
}

fn sane_pan(pan: f32) -> f32 {
    if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::{
        Articulation, NoteId, NotePitch, PatternClipId, ProjectFrame, SampleAssetId, StepLaneId,
    };

    fn note_event(offset: u32, on: bool, note: u64) -> ScheduledEvent {
        note_event_for(Some(7), offset, on, note)
    }

    fn note_event_for(instrument: Option<u64>, offset: u32, on: bool, note: u64) -> ScheduledEvent {
        ScheduledEvent {
            block_offset: offset,
            project_frame: ProjectFrame(i64::from(offset)),
            kind: if on {
                ScheduledKind::NoteOn {
                    clip: PatternClipId::from_raw(1),
                    note: NoteId::from_raw(note),
                    instrument,
                    pitch: NotePitch {
                        midi_key: 69,
                        cents: 0.0,
                    },
                    velocity: 1.0,
                    pan: 0.0,
                    channel: 0,
                    articulation: Articulation::Normal,
                }
            } else {
                ScheduledKind::NoteOff {
                    clip: PatternClipId::from_raw(1),
                    note: NoteId::from_raw(note),
                    instrument,
                    release_velocity: 1.0,
                    channel: 0,
                }
            },
        }
    }

    #[test]
    fn synth_starts_on_the_exact_frame_and_stays_finite() {
        let mut params = SynthParams::default();
        params.waveform = Waveform::Square;
        params.envelope.attack_seconds = 0.0;
        params.envelope.decay_seconds = 0.0;
        params.filter.cutoff_hz = 20_000.0;
        let mut synth = SubtractiveSynth::new(48_000, 7, params).unwrap();
        let mut output = vec![0.0; 32];
        synth
            .render_scheduled_block(0, &[note_event(5, true, 1)], &mut output)
            .unwrap();
        assert!(output[..10].iter().all(|sample| *sample == 0.0));
        assert!(output[10..].iter().any(|sample| sample.abs() > 1.0e-6));
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn synth_does_not_guess_a_route_for_legacy_unrouted_notes() {
        let mut params = SynthParams::default();
        params.envelope.attack_seconds = 0.0;
        params.envelope.decay_seconds = 0.0;
        let mut synth = SubtractiveSynth::new(48_000, 7, params).unwrap();
        let mut output = vec![0.0; 32];

        synth
            .render_scheduled_block(0, &[note_event_for(None, 0, true, 1)], &mut output)
            .unwrap();

        assert_eq!(synth.active_voice_count(), 0);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn synth_is_invariant_to_block_partitioning() {
        let mut params = SynthParams::default();
        params.waveform = Waveform::Sine;
        params.envelope.attack_seconds = 0.0;
        params.envelope.decay_seconds = 0.0;
        params.envelope.release_seconds = 0.0;
        params.filter.cutoff_hz = 18_000.0;
        let mut whole = SubtractiveSynth::new(48_000, 7, params.clone()).unwrap();
        let mut split = SubtractiveSynth::new(48_000, 7, params).unwrap();
        let events = [note_event(2, true, 1), note_event(19, false, 1)];
        let mut whole_output = vec![0.0; 64];
        whole
            .render_scheduled_block(0, &events, &mut whole_output)
            .unwrap();

        let mut first = vec![0.0; 24];
        split
            .render_scheduled_block(0, &[note_event(2, true, 1)], &mut first)
            .unwrap();
        let mut second = vec![0.0; 40];
        split
            .render_scheduled_block(12, &[note_event(7, false, 1)], &mut second)
            .unwrap();
        first.extend(second);
        assert_eq!(whole_output, first);
    }

    #[test]
    fn trigger_gate_releases_across_a_block_boundary() {
        let mut params = SynthParams::default();
        params.envelope.attack_seconds = 0.0;
        params.envelope.decay_seconds = 0.0;
        params.envelope.release_seconds = 0.0;
        let mut synth = SubtractiveSynth::new(1_000, 42, params).unwrap();
        let trigger = ScheduledEvent {
            block_offset: 3,
            project_frame: ProjectFrame(103),
            kind: ScheduledKind::Trigger {
                clip: PatternClipId::from_raw(1),
                lane: StepLaneId::from_raw(2),
                target: TriggerTarget::InstrumentNote {
                    instrument: 42,
                    key: 60,
                },
                choke_group: None,
                velocity: 1.0,
                pan: 0.0,
                pitch_semitones: 0.0,
                gate_frames: 5,
                ratchet: 0,
            },
        };
        let mut first = vec![0.0; 12];
        synth
            .render_scheduled_block(100, &[trigger], &mut first)
            .unwrap();
        let mut second = vec![0.0; 12];
        synth.render_scheduled_block(106, &[], &mut second).unwrap();
        assert!(second[..4].iter().any(|sample| sample.abs() > 0.0));
        assert!(second[4..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn sampler_interpolates_and_honors_half_open_event_boundary() {
        let sample =
            SampleData::from_interleaved(4, 1, vec![1.0, 0.5, 0.0, -0.5], 69, 0.0).unwrap();
        let mut sampler = Sampler::new(4, sample, SamplerParams::default()).unwrap();
        let mut output = vec![0.0; 8];
        sampler
            .render_scheduled_block(0, &[note_event(3, true, 1)], &mut output)
            .unwrap();
        assert!(output[..6].iter().all(|sample| *sample == 0.0));
        assert!(output[6].abs() > 0.0 && output[7].abs() > 0.0);

        let error = sampler
            .render_scheduled_block(4, &[note_event(4, true, 2)], &mut output)
            .unwrap_err();
        assert!(matches!(error, InstrumentError::EventOutsideBlock { .. }));
    }

    #[test]
    fn sampler_trigger_binding_and_voice_limit_are_deterministic() {
        let sample = SampleData::from_interleaved(8, 1, vec![1.0; 16], 60, 0.0).unwrap();
        let params = SamplerParams {
            maximum_voices: 2,
            trigger_asset: Some(9),
            ..SamplerParams::default()
        };
        let mut sampler = Sampler::new(8, sample, params).unwrap();
        let trigger = |offset, lane, asset| ScheduledEvent {
            block_offset: offset,
            project_frame: ProjectFrame(i64::from(offset)),
            kind: ScheduledKind::Trigger {
                clip: PatternClipId::from_raw(1),
                lane: StepLaneId::from_raw(lane),
                target: TriggerTarget::Sample(SampleAssetId::from_raw(asset)),
                choke_group: None,
                velocity: 1.0,
                pan: 0.0,
                pitch_semitones: 0.0,
                gate_frames: 1,
                ratchet: 0,
            },
        };
        let mut output = vec![0.0; 16];
        sampler
            .render_scheduled_block(
                0,
                &[
                    trigger(0, 1, 99),
                    trigger(1, 2, 9),
                    trigger(2, 3, 9),
                    trigger(3, 4, 9),
                ],
                &mut output,
            )
            .unwrap();
        assert_eq!(sampler.active_voice_count(), 2);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn sampler_trigger_gates_and_cross_target_chokes_are_frame_exact() {
        let sample = SampleData::from_interleaved(8, 1, vec![1.0; 16], 60, 0.0).unwrap();
        let event = |offset, asset, choke_group, gate_frames| ScheduledEvent {
            block_offset: offset,
            project_frame: ProjectFrame(i64::from(offset)),
            kind: ScheduledKind::Trigger {
                clip: PatternClipId::from_raw(1),
                lane: StepLaneId::from_raw(asset),
                target: TriggerTarget::Sample(SampleAssetId::from_raw(asset)),
                choke_group,
                velocity: 1.0,
                pan: 0.0,
                pitch_semitones: 0.0,
                gate_frames,
                ratchet: 0,
            },
        };

        let mut gated = Sampler::new(
            8,
            sample.clone(),
            SamplerParams {
                mode: SamplerMode::Gated,
                trigger_asset: Some(9),
                ..SamplerParams::default()
            },
        )
        .unwrap();
        let mut gated_output = vec![0.0; 16];
        gated
            .render_scheduled_block(0, &[event(0, 9, None, 3)], &mut gated_output)
            .unwrap();
        assert!(gated_output[..6].iter().all(|sample| sample.abs() > 0.0));
        assert!(gated_output[6..].iter().all(|sample| *sample == 0.0));

        let mut choked = Sampler::new(
            8,
            sample,
            SamplerParams {
                trigger_asset: Some(9),
                choke_group: Some(4),
                ..SamplerParams::default()
            },
        )
        .unwrap();
        let mut choked_output = vec![0.0; 16];
        choked
            .render_scheduled_block(
                0,
                &[event(0, 9, Some(4), 8), event(2, 10, Some(4), 8)],
                &mut choked_output,
            )
            .unwrap();
        assert!(choked_output[..4].iter().all(|sample| sample.abs() > 0.0));
        assert!(choked_output[4..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn invalid_parameters_and_pcm_are_rejected() {
        let mut params = SynthParams::default();
        params.filter.cutoff_hz = f32::NAN;
        assert!(matches!(
            SubtractiveSynth::new(48_000, 0, params),
            Err(InstrumentError::InvalidFilter)
        ));
        assert!(matches!(
            SampleData::from_interleaved(48_000, 1, vec![0.0, f32::NAN], 60, 0.0),
            Err(InstrumentError::InvalidSample)
        ));
        let mut params = SamplerParams::default();
        params.maximum_voices = 0;
        assert!(matches!(
            params.validate(),
            Err(InstrumentError::InvalidVoiceLimit)
        ));
    }

    #[test]
    fn event_validation_precedes_render_mutation() {
        let mut synth = SubtractiveSynth::new(48_000, 0, SynthParams::default()).unwrap();
        let mut output = vec![0.25; 8];
        let events = [note_event(3, true, 1), note_event(2, false, 1)];
        assert!(matches!(
            synth.render_scheduled_block(0, &events, &mut output),
            Err(InstrumentError::EventsOutOfOrder { .. })
        ));
        assert_eq!(output, vec![0.25; 8]);
        assert_eq!(synth.active_voice_count(), 0);
    }
}
