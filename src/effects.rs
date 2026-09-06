//! In-tree effects a mixer insert runs, and the honest history bound each one
//! needs before a tile can start in the middle of a project.
//!
//! The mixer owns *which* effect an insert is ([`crate::mixer::NativeEffectKind`])
//! and stores its settings as the normalized `Processor` parameters a project
//! already persists and automation already addresses
//! (`ParameterAddress::Plugin { processor_id, key }`). This module owns what
//! those numbers mean, the DSP that reads them, and how far back a tile must
//! render before the effect's retained state is the state a whole bounce would
//! have had.
//!
//! ## What "the same bits" costs, measured
//!
//! Every effect here is an IIR: its state at a frame is a response to all
//! earlier input, not to a bounded window of it. A tile that prerolls `N`
//! frames does not *reconstruct* that state, it *re-converges* to it, and the
//! convergence is not the clean exponential the pole radius suggests. Two
//! `f32` trajectories differing by `d` decay toward each other at the pole
//! radius `r` per frame, but every frame in which they still differ can round
//! differently, and one such rounding re-injects roughly one ulp, which a
//! resonant pole pair then amplifies by `1 / sin(pole angle)` before it decays
//! again. The merge is therefore a *race*, and the frames it takes is a random
//! variable, not a bound.
//!
//! Measured on this kernel (3,000,000 random boundaries at 18 kHz, plus
//! 300,000 at 1 kHz / 0.85 resonance, both with the state floor below), the
//! number of frames to merge, expressed as bits of pole decay
//! (`frames * -log2(r)`), is:
//!
//! | quantile | p50 | p99 | p99.9 | p99.99 | p99.999 | max of 3e6 |
//! |---|---|---|---|---|---|---|
//! | bits | 36 | 84 | 113 | 141 | 174 | 232 |
//!
//! The tail is exponential with a scale near 13 bits per e-fold, so
//! [`HISTORY_MERGE_BITS`] = 512 sits about thirty e-folds past the worst of
//! three million boundaries. That is the whole budget:
//!
//! - 24 bits so a state as large as 2^24 can decay to [`STATE_FLOOR`];
//! - 40 bits of floor, so a preroll spent inside digital silence reaches the
//!   *same exact zero* the whole bounce reaches (see [`STATE_FLOOR`]);
//! - 232 bits, the worst merge in three million measured boundaries;
//! - the rest is margin.
//!
//! This is a measured tail with margin, not a proof. The exact alternative is
//! a state checkpoint, which `tile_contract` still refuses
//! (`TileRefusal::CheckpointImplementationPending`). Where even the tail is
//! not enough the effect says so instead of claiming otherwise: the EQ's
//! direct-form-I biquads were measured *never* to merge (see
//! [`history_bound_frames`]), so an EQ insert declares an unbounded history
//! and the controller renders a whole bounce with its named diagnostic
//! ("incremental bounce fallback: graph needs N context frames; policy allows
//! M"). A compressor's envelope pole does the same at every release it can
//! reach, and so does a filter whose cutoff an automation lane can drag down
//! to 40 Hz.

use std::f32::consts::PI;

use crate::mixer::{BusId, MixerError, MixerGraph, NativeEffectKind, ProcessorId};

/// Bits of pole decay a tile's preroll must buy before the tile's retained
/// state and the whole bounce's are the same `f32` bits.
///
/// The module doc has the measurement this number comes from and the budget it
/// spends. It is deliberately far larger than the mean merge (36 bits): the
/// merge is a race against `f32` rounding, and a bound set near the mean is a
/// coin flip per tile boundary, not a contract.
pub const HISTORY_MERGE_BITS: f64 = 512.0;

/// Magnitude below which an effect's retained state is exactly zero.
///
/// Without this, a tile whose preroll falls inside digital silence and a whole
/// bounce carrying a decaying tail through the same silence never meet: both
/// decay at the pole radius, so the *ratio* between them never improves, and
/// the tile renders exact zeros against a tail of denormal dust for the rest
/// of the project. Measured on a 10,000-frame noise burst followed by silence,
/// a tile starting 10,000 frames into the silence differed from the whole
/// render in 39,999 of 40,000 frames; with this floor it differs in none,
/// because both runs reach the same exact zero (frame 13,502 in that fixture).
///
/// 2^-40 is about -241 dBFS. It costs nothing audible and, measured, nothing
/// at all: on a 200,000-frame half-scale noise render through the resonant
/// filter the floored and unfloored outputs were bit-identical, because a
/// state that small only ever occurs in a tail. The floor is applied inside
/// the kernels, so it is the same arithmetic on the whole-bounce path and the
/// tile path; a rule applied on one path only would be the bug it is meant to
/// fix.
pub const STATE_FLOOR: f32 = 1.0 / (1_u64 << 40) as f32;

/// Most parameters any one effect has.
pub const MAX_EFFECT_PARAMETERS: usize = 5;

/// A sixth parameter on any effect is a compile error here rather than a panic
/// in `EffectRuntime::refresh`, which sizes its coefficient cache by this.
const _: () = {
    let mut index = 0;
    while index < NativeEffectKind::ALL.len() {
        assert!(
            parameters(NativeEffectKind::ALL[index]).len() <= MAX_EFFECT_PARAMETERS,
            "an effect parameter table outgrew MAX_EFFECT_PARAMETERS"
        );
        index += 1;
    }
};

/// How a stored normalized value (0..=1) becomes the number the DSP reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParameterCurve {
    Linear { minimum: f32, maximum: f32 },
    /// Constant ratio per unit of the normalized position: frequencies.
    Exponential { minimum: f32, maximum: f32 },
    /// A discrete choice; the normalized position picks the nearest option.
    Choice(&'static [&'static str]),
}

impl ParameterCurve {
    pub fn value(self, normalized: f32) -> f32 {
        let t = normalized.clamp(0.0, 1.0);
        match self {
            Self::Linear { minimum, maximum } => minimum + (maximum - minimum) * t,
            Self::Exponential { minimum, maximum } => {
                minimum * (maximum / minimum).powf(t)
            }
            Self::Choice(options) => {
                let last = options.len().saturating_sub(1) as f32;
                (t * last).round().clamp(0.0, last)
            }
        }
    }

    pub fn label(self, normalized: f32) -> String {
        match self {
            Self::Choice(options) => options
                .get(self.value(normalized) as usize)
                .copied()
                .unwrap_or("?")
                .to_owned(),
            Self::Linear { .. } | Self::Exponential { .. } => {
                let value = self.value(normalized);
                if value.abs() >= 1000.0 {
                    format!("{:.1}k", value / 1000.0)
                } else if value.abs() >= 100.0 {
                    format!("{value:.0}")
                } else {
                    format!("{value:.2}")
                }
            }
        }
    }
}

/// One parameter of one effect: the key a project persists and automation
/// addresses, the name a musician reads, and what its number means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectParameter {
    pub key: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    pub curve: ParameterCurve,
    pub default_normalized: f32,
}

const FILTER_MODES: &[&str] = &["low-pass", "band-pass", "high-pass"];

const FILTER_PARAMETERS: &[EffectParameter] = &[
    EffectParameter {
        key: "mode",
        name: "mode",
        unit: "",
        curve: ParameterCurve::Choice(FILTER_MODES),
        default_normalized: 0.0,
    },
    EffectParameter {
        key: "cutoff",
        name: "cutoff",
        unit: "Hz",
        // 40 Hz, not 20: the pole of a resonant filter an octave lower needs a
        // preroll longer than a tile, and a cutoff a musician cannot reach is
        // a smaller lie than a tile boundary that is not byte-exact.
        curve: ParameterCurve::Exponential {
            minimum: 40.0,
            maximum: 18_000.0,
        },
        default_normalized: 0.75,
    },
    EffectParameter {
        key: "resonance",
        name: "resonance",
        unit: "",
        curve: ParameterCurve::Linear {
            minimum: 0.0,
            maximum: 0.85,
        },
        default_normalized: 0.12,
    },
];

const EQ_PARAMETERS: &[EffectParameter] = &[
    EffectParameter {
        key: "low_shelf_db",
        name: "low shelf",
        unit: "dB",
        curve: ParameterCurve::Linear {
            minimum: -18.0,
            maximum: 18.0,
        },
        default_normalized: 0.5,
    },
    EffectParameter {
        key: "peak_hz",
        name: "peak frequency",
        unit: "Hz",
        curve: ParameterCurve::Exponential {
            minimum: 60.0,
            maximum: 16_000.0,
        },
        default_normalized: 0.5,
    },
    EffectParameter {
        key: "peak_db",
        name: "peak gain",
        unit: "dB",
        curve: ParameterCurve::Linear {
            minimum: -18.0,
            maximum: 18.0,
        },
        default_normalized: 0.5,
    },
    EffectParameter {
        key: "peak_q",
        name: "peak Q",
        unit: "",
        curve: ParameterCurve::Exponential {
            minimum: 0.3,
            maximum: 4.0,
        },
        default_normalized: 0.5,
    },
    EffectParameter {
        key: "high_shelf_db",
        name: "high shelf",
        unit: "dB",
        curve: ParameterCurve::Linear {
            minimum: -18.0,
            maximum: 18.0,
        },
        default_normalized: 0.5,
    },
];

const COMPRESSOR_PARAMETERS: &[EffectParameter] = &[
    EffectParameter {
        key: "threshold_db",
        name: "threshold",
        unit: "dB",
        curve: ParameterCurve::Linear {
            minimum: -48.0,
            maximum: 0.0,
        },
        default_normalized: 0.6,
    },
    EffectParameter {
        key: "ratio",
        name: "ratio",
        unit: ":1",
        curve: ParameterCurve::Exponential {
            minimum: 1.0,
            maximum: 20.0,
        },
        default_normalized: 0.4,
    },
    EffectParameter {
        key: "attack_ms",
        name: "attack",
        unit: "ms",
        curve: ParameterCurve::Exponential {
            minimum: 0.2,
            maximum: 100.0,
        },
        default_normalized: 0.35,
    },
    EffectParameter {
        key: "release_ms",
        name: "release",
        unit: "ms",
        curve: ParameterCurve::Exponential {
            minimum: 5.0,
            maximum: 1_000.0,
        },
        default_normalized: 0.5,
    },
    EffectParameter {
        key: "makeup_db",
        name: "makeup",
        unit: "dB",
        curve: ParameterCurve::Linear {
            minimum: 0.0,
            maximum: 24.0,
        },
        default_normalized: 0.0,
    },
];

/// The parameters of one effect, in the order a strip shows them and a project
/// stores them.
pub const fn parameters(kind: NativeEffectKind) -> &'static [EffectParameter] {
    match kind {
        NativeEffectKind::Filter => FILTER_PARAMETERS,
        NativeEffectKind::Eq => EQ_PARAMETERS,
        NativeEffectKind::Compressor => COMPRESSOR_PARAMETERS,
    }
}

/// A small, stable index for one parameter key across every effect, so a
/// coalescing series can name "this parameter of this processor" in a few
/// bits. Keys are unique across the three effects, which is what makes one
/// flat index legal.
pub fn parameter_series_index(key: &str) -> Option<u64> {
    let mut index = 0_u64;
    for kind in NativeEffectKind::ALL {
        for parameter in parameters(kind) {
            if parameter.key == key {
                return Some(index);
            }
            index += 1;
        }
    }
    None
}

/// Add one native effect to the end of `bus`'s insert chain, with the
/// parameters it needs at their defaults.
///
/// Native effects are zero-latency, so the declared latency is 0 and
/// `bus_insert_latency` keeps reporting the truth for a chain that mixes them
/// with a hosted processor that declares its own.
pub fn insert_native_effect(
    mixer: &mut MixerGraph,
    bus: BusId,
    index: Option<usize>,
    kind: NativeEffectKind,
) -> Result<ProcessorId, MixerError> {
    let processor = mixer.insert_processor(bus, index, kind.descriptor(), 0)?;
    for parameter in parameters(kind) {
        mixer.add_parameter(
            processor,
            parameter.key,
            parameter.name,
            parameter.default_normalized,
        )?;
    }
    Ok(processor)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterMode {
    LowPass,
    BandPass,
    HighPass,
}

/// One effect with its parameters read as the numbers the DSP uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeEffect {
    Filter {
        mode: FilterMode,
        cutoff_hz: f32,
        resonance: f32,
    },
    Eq {
        low_shelf_db: f32,
        peak_hz: f32,
        peak_db: f32,
        peak_q: f32,
        high_shelf_db: f32,
    },
    Compressor {
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
    },
}

/// Read normalized parameter positions, in [`parameters`] order, as an effect.
pub fn resolve(kind: NativeEffectKind, normalized: &[f32]) -> NativeEffect {
    let specs = parameters(kind);
    let at = |index: usize| -> f32 {
        let spec = specs[index];
        spec.curve
            .value(normalized.get(index).copied().unwrap_or(spec.default_normalized))
    };
    match kind {
        NativeEffectKind::Filter => NativeEffect::Filter {
            mode: match at(0) as i32 {
                0 => FilterMode::LowPass,
                1 => FilterMode::BandPass,
                _ => FilterMode::HighPass,
            },
            cutoff_hz: at(1),
            resonance: at(2),
        },
        NativeEffectKind::Eq => NativeEffect::Eq {
            low_shelf_db: at(0),
            peak_hz: at(1),
            peak_db: at(2),
            peak_q: at(3),
            high_shelf_db: at(4),
        },
        NativeEffectKind::Compressor => NativeEffect::Compressor {
            threshold_db: at(0),
            ratio: at(1),
            attack_ms: at(2),
            release_ms: at(3),
            makeup_db: at(4),
        },
    }
}

/// Frames for a pole of radius `radius` to decay [`HISTORY_MERGE_BITS`] bits
/// below where it started. `u64::MAX` means "no preroll this module is willing
/// to declare is known to suffice", which reads through `NativeNode::timing`
/// as a history longer than any tile context and lands the render on the
/// whole-bounce path by name.
pub fn frames_to_decay(radius: f64) -> u64 {
    if !(0.0..1.0).contains(&radius) {
        return u64::MAX;
    }
    if radius <= 0.0 {
        return 1;
    }
    let frames = HISTORY_MERGE_BITS * std::f64::consts::LN_2 / -radius.ln();
    if frames.is_finite() && frames >= 0.0 && frames < u64::MAX as f64 {
        frames.ceil() as u64
    } else {
        u64::MAX
    }
}

/// The slowest pole this effect can reach, given which of its parameters an
/// automation lane can move.
///
/// `reachable` says, per parameter in [`parameters`] order, the normalized
/// interval this render can see: a single point for a parameter no lane
/// touches, and the whole 0..=1 range for one a lane drives, because a lane
/// may be redrawn anywhere inside its descriptor without re-compiling.
pub fn history_bound_frames(
    kind: NativeEffectKind,
    reachable: &[(f32, f32)],
    sample_rate: u32,
) -> u64 {
    let specs = parameters(kind);
    let sample_rate = sample_rate.max(1) as f32;
    let end = |index: usize, high: bool| -> f32 {
        let (low, top) = reachable
            .get(index)
            .copied()
            .unwrap_or((specs[index].default_normalized, specs[index].default_normalized));
        specs[index].curve.value(if high { top } else { low })
    };
    let radius = match kind {
        // Lowest cutoff with the highest resonance: the longest-ringing pole
        // the parameters allow.
        NativeEffectKind::Filter => {
            svf_pole_radius(end(1, false), end(2, true), sample_rate)
        }
        // No preroll is known to suffice. The EQ's peaking section is a
        // direct-form-I biquad, and DF1 computes each output as a sum of five
        // terms about twice the size of the result: at a low corner the
        // cancellation puts its rounding noise well above the ulp of the state
        // it keeps, which is exactly the injection the merge has to outrun.
        // Measured against a 100,000-frame preroll of full-scale noise at
        // 60 Hz, the peak's four state words never became the whole render's
        // bits: at +18 dB (109 bits of pole decay) and at 0 dB (308 bits of
        // pole decay), where the same experiment merges the state-variable
        // filter after 36 bits on average and 232 bits at worst. The shelves
        // merge (the +18 dB low shelf took 4,838 frames); the peak does not,
        // and the node is one node.
        //
        // So the EQ declares no bound, `tile_contract` refuses with
        // `PrerollCeilingExceeded`, and the render is a whole bounce that says
        // so. Making the EQ tileable means giving it the filter's topology
        // (trapezoidal state-variable sections, whose state words are the
        // integrators rather than the output history), which is a change to
        // what the EQ sounds like and belongs to whoever makes it deliberately.
        NativeEffectKind::Eq => return u64::MAX,
        // The envelope follower's slower coefficient; the gain is read from it
        // without further smoothing, so it is the only retained state.
        NativeEffectKind::Compressor => {
            let attack = one_pole_coefficient(end(2, true), sample_rate);
            let release = one_pole_coefficient(end(3, true), sample_rate);
            f64::from(attack.max(release))
        }
    };
    frames_to_decay(radius)
}

const EQ_LOW_SHELF_HZ: f32 = 200.0;
const EQ_HIGH_SHELF_HZ: f32 = 4_000.0;

/// Retained state of one insert, sized for the channel count of its plan.
///
/// Reset is exactly the zero state a whole bounce starts from, so a tile that
/// prerolls its declared bound converges onto the same trajectory.
#[derive(Clone, Debug)]
pub struct EffectRuntime {
    kind: NativeEffectKind,
    sample_rate: u32,
    svf: [Svf; MAX_EFFECT_CHANNELS],
    biquads: [[Biquad; 3]; MAX_EFFECT_CHANNELS],
    envelope: f32,
    /// The normalized positions the current coefficients were computed from.
    /// Coefficients are a pure function of them, so reusing them when they did
    /// not move changes no arithmetic.
    coefficient_source: [f32; MAX_EFFECT_PARAMETERS],
    coefficients: Coefficients,
}

/// Stereo is the widest native format; a mono plan uses the first slot.
pub const MAX_EFFECT_CHANNELS: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Svf {
    integrator_one: f32,
    integrator_two: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Biquad {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BiquadCoefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Coefficients {
    Filter {
        g: f32,
        k: f32,
        mode: FilterMode,
    },
    Eq([BiquadCoefficients; 3]),
    Compressor {
        threshold_db: f32,
        slope: f32,
        attack: f32,
        release: f32,
        makeup: f32,
    },
}

impl EffectRuntime {
    pub fn new(kind: NativeEffectKind, sample_rate: u32) -> Self {
        let mut runtime = Self {
            kind,
            sample_rate: sample_rate.max(1),
            svf: [Svf::default(); MAX_EFFECT_CHANNELS],
            biquads: [[Biquad::default(); 3]; MAX_EFFECT_CHANNELS],
            envelope: 0.0,
            coefficient_source: [f32::NAN; MAX_EFFECT_PARAMETERS],
            coefficients: Coefficients::Filter {
                g: 0.0,
                k: 0.0,
                mode: FilterMode::LowPass,
            },
        };
        let defaults: Vec<f32> = parameters(kind)
            .iter()
            .map(|parameter| parameter.default_normalized)
            .collect();
        runtime.refresh(&defaults);
        runtime
    }

    pub fn reset(&mut self) {
        self.svf = [Svf::default(); MAX_EFFECT_CHANNELS];
        self.biquads = [[Biquad::default(); 3]; MAX_EFFECT_CHANNELS];
        self.envelope = 0.0;
    }

    /// Recompute coefficients when, and only when, the normalized positions
    /// moved. Nothing here allocates or touches the graph.
    fn refresh(&mut self, normalized: &[f32]) {
        // `zip`, not `coefficient_source[index]`: the length here is the
        // caller's parameter table, and indexing a fixed array by it put a
        // panic in the render loop for anyone who added a sixth parameter.
        // The const assertion beside `MAX_EFFECT_PARAMETERS` refuses that at
        // compile time; this reads the same either way.
        let mut moved = [false; MAX_EFFECT_PARAMETERS];
        let mut any = false;
        for ((slot, value), flag) in self
            .coefficient_source
            .iter_mut()
            .zip(normalized)
            .zip(&mut moved)
        {
            *flag = *slot != *value;
            any |= *flag;
            *slot = *value;
        }
        if !any {
            return;
        }
        let specs = parameters(self.kind);
        let at = |index: usize| -> f32 {
            let spec = specs[index];
            spec.curve
                .value(normalized.get(index).copied().unwrap_or(spec.default_normalized))
        };
        let rate = self.sample_rate as f32;
        // Only the EQ pays enough per section to be worth splitting: three
        // `sin_cos` and three `powf` per frame on a lane that moves one knob.
        // Coefficients are a pure function of the normalized positions, so
        // keeping the two sections whose inputs did not move is the same
        // arithmetic as recomputing them, bit for bit.
        if let (NativeEffectKind::Eq, Coefficients::Eq(sections)) = (self.kind, self.coefficients) {
            let mut sections = sections;
            if moved[0] {
                sections[0] = low_shelf(EQ_LOW_SHELF_HZ, at(0), rate);
            }
            if moved[1] || moved[2] || moved[3] {
                sections[1] = peaking(at(1), at(2), at(3), rate);
            }
            if moved[4] {
                sections[2] = high_shelf(EQ_HIGH_SHELF_HZ, at(4), rate);
            }
            self.coefficients = Coefficients::Eq(sections);
            return;
        }
        self.coefficients = match resolve(self.kind, normalized) {
            NativeEffect::Filter {
                mode,
                cutoff_hz,
                resonance,
            } => Coefficients::Filter {
                g: svf_g(cutoff_hz, rate),
                k: svf_damping(resonance),
                mode,
            },
            NativeEffect::Eq {
                low_shelf_db,
                peak_hz,
                peak_db,
                peak_q,
                high_shelf_db,
            } => Coefficients::Eq([
                low_shelf(EQ_LOW_SHELF_HZ, low_shelf_db, rate),
                peaking(peak_hz, peak_db, peak_q, rate),
                high_shelf(EQ_HIGH_SHELF_HZ, high_shelf_db, rate),
            ]),
            NativeEffect::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_db,
            } => Coefficients::Compressor {
                threshold_db,
                slope: 1.0 - 1.0 / ratio.max(1.0),
                attack: one_pole_coefficient(attack_ms, rate),
                release: one_pole_coefficient(release_ms, rate),
                makeup: db_to_linear(makeup_db),
            },
        };
    }

    /// Process one frame in place. `frame` is one interleaved frame of at most
    /// [`MAX_EFFECT_CHANNELS`] samples.
    pub fn process_frame(&mut self, normalized: &[f32], frame: &mut [f32]) {
        self.refresh(normalized);
        match self.coefficients {
            Coefficients::Filter { g, k, mode } => {
                for (channel, sample) in frame.iter_mut().enumerate().take(MAX_EFFECT_CHANNELS) {
                    *sample = self.svf[channel].process(*sample, g, k, mode);
                }
            }
            Coefficients::Eq(sections) => {
                for (channel, sample) in frame.iter_mut().enumerate().take(MAX_EFFECT_CHANNELS) {
                    let mut value = *sample;
                    for (section, coefficients) in
                        self.biquads[channel].iter_mut().zip(sections.iter())
                    {
                        value = section.process(value, *coefficients);
                    }
                    *sample = value;
                }
            }
            Coefficients::Compressor {
                threshold_db,
                slope,
                attack,
                release,
                makeup,
            } => {
                let peak = frame
                    .iter()
                    .take(MAX_EFFECT_CHANNELS)
                    .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                let coefficient = if peak > self.envelope { attack } else { release };
                self.envelope =
                    retained(coefficient * self.envelope + (1.0 - coefficient) * peak);
                let over = linear_to_db(self.envelope) - threshold_db;
                let reduction_db = if over > 0.0 { -over * slope } else { 0.0 };
                let gain = db_to_linear(reduction_db) * makeup;
                for sample in frame.iter_mut().take(MAX_EFFECT_CHANNELS) {
                    *sample = finite_or_zero(*sample * gain);
                }
            }
        }
    }
}

impl Svf {
    /// The topology-preserving state-variable filter the built-in voices use
    /// (`instruments.rs`), with the two other outputs its integrators already
    /// carry.
    fn process(&mut self, input: f32, g: f32, k: f32, mode: FilterMode) -> f32 {
        let a1 = 1.0 / (1.0 + g * (g + k));
        let v1 = (self.integrator_one + g * (input - self.integrator_two)) * a1;
        let v2 = self.integrator_two + g * v1;
        self.integrator_one = retained(2.0 * v1 - self.integrator_one);
        self.integrator_two = retained(2.0 * v2 - self.integrator_two);
        finite_or_zero(match mode {
            FilterMode::LowPass => v2,
            FilterMode::BandPass => v1,
            FilterMode::HighPass => input - k * v1 - v2,
        })
    }
}

impl Biquad {
    fn process(&mut self, input: f32, c: BiquadCoefficients) -> f32 {
        let output = c.b0 * input + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1
            - c.a2 * self.y2;
        let output = finite_or_zero(output);
        self.x2 = retained(self.x1);
        self.x1 = retained(input);
        self.y2 = retained(self.y1);
        self.y1 = retained(output);
        output
    }
}

fn svf_g(cutoff_hz: f32, sample_rate: f32) -> f32 {
    let nyquist = sample_rate * 0.5;
    let cutoff = cutoff_hz.clamp(1.0, nyquist * 0.99);
    (PI * cutoff / sample_rate).tan()
}

/// Matches the voice filter's damping law: 0 is heavily damped, 1 approaches
/// self-oscillation.
fn svf_damping(resonance: f32) -> f32 {
    2.0 - resonance.clamp(0.0, 1.0) * 1.9
}

/// Spectral radius of the state-variable filter's 2x2 state transition, which
/// is what "how long does this ring" means exactly.
fn svf_pole_radius(cutoff_hz: f32, resonance: f32, sample_rate: f32) -> f64 {
    let g = f64::from(svf_g(cutoff_hz, sample_rate));
    let k = f64::from(svf_damping(resonance));
    let a1 = 1.0 / (1.0 + g * (g + k));
    let trace = 2.0 * a1 * (1.0 - g * g);
    let determinant = (2.0 * a1 - 1.0) * (1.0 - 2.0 * a1 * g * g) + 4.0 * a1 * a1 * g * g;
    let discriminant = trace * trace - 4.0 * determinant;
    if discriminant < 0.0 {
        determinant.abs().sqrt()
    } else {
        let root = discriminant.sqrt();
        (0.5 * (trace + root)).abs().max((0.5 * (trace - root)).abs())
    }
}

/// Only the EQ's refusal needs this now, and only to state what it refuses.
#[cfg(test)]
fn biquad_pole_radius(c: BiquadCoefficients) -> f64 {
    let a1 = f64::from(c.a1);
    let a2 = f64::from(c.a2);
    let discriminant = a1 * a1 - 4.0 * a2;
    if discriminant < 0.0 {
        a2.abs().sqrt()
    } else {
        let root = discriminant.sqrt();
        (0.5 * (-a1 + root)).abs().max((0.5 * (-a1 - root)).abs())
    }
}

fn one_pole_coefficient(milliseconds: f32, sample_rate: f32) -> f32 {
    let seconds = (milliseconds.max(0.0) / 1000.0).max(1.0e-6);
    (-1.0 / (seconds * sample_rate)).exp()
}

fn low_shelf(frequency: f32, gain_db: f32, sample_rate: f32) -> BiquadCoefficients {
    let a = (10.0_f32).powf(gain_db / 40.0);
    let w0 = 2.0 * PI * frequency.clamp(1.0, sample_rate * 0.49) / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / 2.0 * ((a + 1.0 / a) * (1.0 / 0.707 - 1.0) + 2.0).max(0.0).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha);
    let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha);
    let a0 = (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha;
    let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
    let a2 = (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha;
    normalize(b0, b1, b2, a0, a1, a2)
}

fn high_shelf(frequency: f32, gain_db: f32, sample_rate: f32) -> BiquadCoefficients {
    let a = (10.0_f32).powf(gain_db / 40.0);
    let w0 = 2.0 * PI * frequency.clamp(1.0, sample_rate * 0.49) / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / 2.0 * ((a + 1.0 / a) * (1.0 / 0.707 - 1.0) + 2.0).max(0.0).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha);
    let a0 = (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
    let a2 = (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha;
    normalize(b0, b1, b2, a0, a1, a2)
}

fn peaking(frequency: f32, gain_db: f32, q: f32, sample_rate: f32) -> BiquadCoefficients {
    let a = (10.0_f32).powf(gain_db / 40.0);
    let w0 = 2.0 * PI * frequency.clamp(1.0, sample_rate * 0.49) / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q.max(0.05));
    normalize(
        1.0 + alpha * a,
        -2.0 * cos,
        1.0 - alpha * a,
        1.0 + alpha / a,
        -2.0 * cos,
        1.0 - alpha / a,
    )
}

fn normalize(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> BiquadCoefficients {
    let a0 = if a0.abs() < 1.0e-12 { 1.0 } else { a0 };
    BiquadCoefficients {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// One retained state word, after the two rules every effect here keeps: an
/// unrepresentable value is zero, and a value below [`STATE_FLOOR`] is exactly
/// zero. Both are pure functions of the word, so the whole bounce and a tile
/// apply them at the same frames and reach the same bits.
fn retained(value: f32) -> f32 {
    let value = finite_or_zero(value);
    if value.abs() < STATE_FLOOR {
        0.0
    } else {
        value
    }
}

fn db_to_linear(db: f32) -> f32 {
    (10.0_f32).powf(db / 20.0)
}

fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(1.0e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::BusKind;

    fn defaults(kind: NativeEffectKind) -> Vec<f32> {
        parameters(kind)
            .iter()
            .map(|parameter| parameter.default_normalized)
            .collect()
    }

    fn render(kind: NativeEffectKind, normalized: &[f32], input: &[f32]) -> Vec<f32> {
        let mut runtime = EffectRuntime::new(kind, 44_100);
        let mut output = Vec::with_capacity(input.len());
        for frame in input.chunks_exact(2) {
            let mut buffer = [frame[0], frame[1]];
            runtime.process_frame(normalized, &mut buffer);
            output.extend_from_slice(&buffer);
        }
        output
    }

    /// Broadband stereo noise, deterministic.
    fn noise(frames: usize) -> Vec<f32> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames * 2)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as f32 / 8_388_608.0) - 1.0
            })
            .collect()
    }

    fn centroid(samples: &[f32]) -> f64 {
        // Zero-crossing rate stands in for the spectral centroid here; the
        // spectral assertion itself lives in engine_regression against a real
        // render.
        let left: Vec<f32> = samples.chunks_exact(2).map(|frame| frame[0]).collect();
        let crossings = left
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        crossings as f64 / left.len() as f64
    }

    #[test]
    fn a_low_pass_insert_removes_high_frequency_motion() {
        let input = noise(8_192);
        let mut normalized = defaults(NativeEffectKind::Filter);
        // 40 Hz * (18000/40)^0 = the bottom of the range: a hard low-pass.
        normalized[1] = 0.0;
        normalized[2] = 0.0;
        let filtered = render(NativeEffectKind::Filter, &normalized, &input);
        assert!(
            centroid(&filtered) < centroid(&input) / 8.0,
            "input {} filtered {}",
            centroid(&input),
            centroid(&filtered)
        );
    }

    #[test]
    fn a_high_pass_insert_keeps_high_frequency_motion() {
        let input = noise(8_192);
        let mut normalized = defaults(NativeEffectKind::Filter);
        normalized[0] = 1.0;
        normalized[1] = 0.5;
        let filtered = render(NativeEffectKind::Filter, &normalized, &input);
        assert!(centroid(&filtered) >= centroid(&input) * 0.9);
    }

    #[test]
    fn a_compressor_insert_narrows_the_distance_between_loud_and_quiet() {
        // Half a second loud, half a second 20 dB below it: compression is
        // exactly the claim that the ratio between those two shrinks.
        let frames = 44_100;
        let mut input = noise(frames);
        for (index, frame) in input.chunks_exact_mut(2).enumerate() {
            let envelope = if index < frames / 2 { 1.0 } else { 0.1 };
            frame[0] *= envelope;
            frame[1] *= envelope;
        }
        let mut normalized = defaults(NativeEffectKind::Compressor);
        normalized[0] = 0.4; // threshold under the loud half, over the quiet
        normalized[1] = 1.0; // 20:1
        normalized[2] = 0.0; // fastest attack, so a transient is caught
        normalized[3] = 0.0; // fastest release, so the quiet half recovers
        let compressed = render(NativeEffectKind::Compressor, &normalized, &input);
        // Skip the release window at the boundary; the claim is about the
        // settled level of each half, not the transition.
        let span = |samples: &[f32], from: usize, to: usize| {
            let slice = &samples[from * 2..to * 2];
            (slice.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>()
                / slice.len() as f64)
                .sqrt()
        };
        let dry_range = span(&input, 2_000, frames / 2) / span(&input, frames / 2 + 4_000, frames);
        let wet_range =
            span(&compressed, 2_000, frames / 2) / span(&compressed, frames / 2 + 4_000, frames);
        assert!(
            wet_range < dry_range * 0.7,
            "dry loud/quiet {dry_range:.2}, compressed {wet_range:.2}"
        );
    }

    #[test]
    fn an_eq_at_its_defaults_is_the_identity_within_rounding() {
        let input = noise(4_096);
        let output = render(NativeEffectKind::Eq, &defaults(NativeEffectKind::Eq), &input);
        let worst = input
            .iter()
            .zip(&output)
            .map(|(a, b)| f64::from((a - b).abs()))
            .fold(0.0_f64, f64::max);
        // Three f32 biquads whose numerator and denominator are equal at unity
        // gain still divide, and the near-unity shelf pole amplifies that last
        // bit: about 3e-5 on a half-scale signal, roughly -85 dB. Flat is
        // inaudible, not bit-exact, and this names which one it is.
        assert!(worst < 1.0e-3, "worst deviation {worst}");
    }

    /// The bound is what makes tiles exact, so it must be small enough for the
    /// tile context ceiling in the cases a musician reaches without automation.
    #[test]
    fn a_static_filter_history_bound_fits_inside_one_tile() {
        let specs = parameters(NativeEffectKind::Filter);
        let reachable: Vec<(f32, f32)> = specs
            .iter()
            .map(|parameter| (parameter.default_normalized, parameter.default_normalized))
            .collect();
        let frames = history_bound_frames(NativeEffectKind::Filter, &reachable, 44_100);
        assert!(frames > 0 && frames < 4_096, "{frames} frames");
    }

    /// A lane that can drag the cutoff to 40 Hz *and* the resonance to its
    /// top asks for more history than a tile context holds, so the project
    /// renders whole bounces and the controller says so. This is the cost of
    /// the bound being a real one; the old 40-bit criterion fit inside a tile
    /// only because it was a coin flip.
    #[test]
    fn a_fully_automated_filter_declares_more_history_than_a_tile_holds() {
        let reachable = [(0.0, 1.0); 3];
        let frames = history_bound_frames(NativeEffectKind::Filter, &reachable, 44_100);
        assert!(
            frames > u64::from(crate::render_tiles::DEFAULT_TILE_FRAMES),
            "{frames} frames"
        );
        // Automating only the cutoff, at the default resonance, is the same
        // verdict for the same reason: 40 Hz is the slow pole either way.
        let cutoff_only = history_bound_frames(
            NativeEffectKind::Filter,
            &[(0.0, 0.0), (0.0, 1.0), (0.12, 0.12)],
            44_100,
        );
        assert!(
            cutoff_only > u64::from(crate::render_tiles::DEFAULT_TILE_FRAMES),
            "{cutoff_only} frames"
        );
        // Automating only the resonance still tiles: the default cutoff is
        // high enough that even self-oscillation decays inside a tile.
        let resonance_only = history_bound_frames(
            NativeEffectKind::Filter,
            &[(0.0, 0.0), (0.75, 0.75), (0.0, 1.0)],
            44_100,
        );
        assert!(
            resonance_only < u64::from(crate::render_tiles::DEFAULT_TILE_FRAMES),
            "{resonance_only} frames"
        );
    }

    /// The EQ declares no bound at all, and the module doc says why: its
    /// direct-form-I peaking section was never observed to reach the whole
    /// render's bits. The next test is that observation, kept runnable.
    #[test]
    fn an_eq_declares_no_history_bound_any_preroll_could_satisfy() {
        let reachable = [(0.0, 1.0); 5];
        assert_eq!(
            history_bound_frames(NativeEffectKind::Eq, &reachable, 44_100),
            u64::MAX
        );
        let flat: Vec<(f32, f32)> = parameters(NativeEffectKind::Eq)
            .iter()
            .map(|parameter| (parameter.default_normalized, parameter.default_normalized))
            .collect();
        assert_eq!(
            history_bound_frames(NativeEffectKind::Eq, &flat, 44_100),
            u64::MAX,
            "the refusal is the topology, not the settings"
        );
    }

    /// The measurement behind that refusal, at a tenth of the frames the
    /// module doc reports so it stays a test and not a benchmark.
    ///
    /// A tile that resets the peaking section and prerolls 20,000 frames of
    /// the same noise still does not hold the whole run's four state words,
    /// while `a_filter_preroll_of_its_declared_bound_reproduces_the_whole_run`
    /// shows the filter merging in far less pole decay than it declares.
    #[test]
    fn an_eq_peaking_section_does_not_reach_the_whole_runs_bits() {
        let coefficients = peaking(60.0, 0.0, 4.0, 44_100.0);
        let radius = biquad_pole_radius(coefficients);
        assert!(radius > 0.998, "the peak pole is near unity: {radius}");
        let input = noise(60_000);
        let left: Vec<f32> = input.chunks_exact(2).map(|frame| frame[0]).collect();
        let mut whole = Biquad::default();
        let reset = 20_000;
        for sample in &left[..reset] {
            whole.process(*sample, coefficients);
        }
        let mut tile = Biquad::default();
        let mut merged = None;
        for (index, sample) in left[reset..].iter().enumerate() {
            whole.process(*sample, coefficients);
            tile.process(*sample, coefficients);
            if whole == tile {
                merged = Some(index);
                break;
            }
        }
        assert_eq!(
            merged, None,
            "the EQ would be tileable after {merged:?} frames and this refusal is stale"
        );
    }

    /// The filter's declared bound is the number a tile prerolls, and it is
    /// enough: after that many frames the tile's state and the whole run's are
    /// the same bits, so every sample after it is identical.
    #[test]
    fn a_filter_preroll_of_its_declared_bound_reproduces_the_whole_run() {
        let mut normalized = defaults(NativeEffectKind::Filter);
        normalized[1] = 0.5;
        normalized[2] = 0.7;
        let reachable: Vec<(f32, f32)> = normalized.iter().map(|v| (*v, *v)).collect();
        let bound =
            history_bound_frames(NativeEffectKind::Filter, &reachable, 44_100) as usize;
        assert!(bound > 4_000 && bound < 20_000, "{bound} frames");
        let core = 4_096;
        let input = noise(bound * 2 + core);
        let whole = render(NativeEffectKind::Filter, &normalized, &input);
        // The tile: reset exactly `bound` frames before the core, and not one
        // frame earlier. Rendering the history twice is what the engine used
        // to do, and it proved a bound twice this size.
        let from = (bound * 2 - bound) * 2;
        let tiled = render(NativeEffectKind::Filter, &normalized, &input[from..]);
        let core_start = (bound * 2) * 2;
        let differing = whole[core_start..]
            .iter()
            .zip(&tiled[(core_start - from)..])
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
        assert_eq!(differing, 0, "{differing} core samples differ after a preroll of {bound}");
    }

    /// A preroll spent entirely inside digital silence is the case a decay
    /// argument cannot reach: both trajectories fall at the same rate, so
    /// their ratio never improves. [`STATE_FLOOR`] is what makes them meet, by
    /// sending both to the same exact zero.
    #[test]
    fn a_state_floor_makes_a_silent_gap_reach_the_same_exact_zero() {
        let normalized = {
            let mut normalized = defaults(NativeEffectKind::Filter);
            normalized[1] = 0.2;
            normalized[2] = 0.7;
            normalized
        };
        let burst = 10_000;
        let total = 60_000;
        let mut input = noise(total);
        for sample in &mut input[burst * 2..] {
            *sample = 0.0;
        }
        let whole = render(NativeEffectKind::Filter, &normalized, &input);
        let silent_from = whole
            .chunks_exact(2)
            .enumerate()
            .skip(burst)
            .find(|(_, frame)| frame[0] == 0.0 && frame[1] == 0.0)
            .map(|(index, _)| index)
            .expect("the floor sends a decaying tail to exact zero");
        assert!(
            silent_from < total,
            "the tail reached zero at {silent_from}, inside the fixture"
        );
        // A tile whose whole preroll is inside the silence starts from the
        // zero state; the whole run must already be there.
        let reset = silent_from + 5_000;
        let tiled = render(NativeEffectKind::Filter, &normalized, &input[reset * 2..]);
        let differing = whole[reset * 2..]
            .iter()
            .zip(&tiled)
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
        assert_eq!(differing, 0, "{differing} samples differ across the silent gap");
    }

    /// A compressor's release is genuinely long memory: it declares a bound
    /// larger than a tile and the controller falls back to a whole bounce,
    /// by name. This test pins that it is a declared consequence, not a
    /// surprise.
    #[test]
    fn a_slow_compressor_declares_more_history_than_a_tile_holds() {
        let specs = parameters(NativeEffectKind::Compressor);
        let reachable: Vec<(f32, f32)> = specs
            .iter()
            .map(|parameter| (parameter.default_normalized, parameter.default_normalized))
            .collect();
        let frames = history_bound_frames(NativeEffectKind::Compressor, &reachable, 44_100);
        assert!(frames > 65_536, "{frames} frames");
    }

    #[test]
    fn installing_a_native_effect_adds_its_parameters_at_their_defaults() {
        let mut mixer = MixerGraph::default();
        let bus = mixer.add_bus(BusKind::Source, "Voice").unwrap();
        let processor =
            insert_native_effect(&mut mixer, bus, None, NativeEffectKind::Filter).unwrap();
        let stored = mixer.processor(processor).unwrap();
        assert_eq!(stored.native_effect(), Some(NativeEffectKind::Filter));
        assert_eq!(stored.latency_samples(), 0);
        for parameter in parameters(NativeEffectKind::Filter) {
            let found = stored
                .parameter_by_key(parameter.key)
                .unwrap_or_else(|| panic!("{} is stored", parameter.key));
            assert_eq!(found.normalized_value(), parameter.default_normalized);
        }
        assert_eq!(mixer.bus(bus).unwrap().inserts().len(), 1);
    }

    #[test]
    fn a_hosted_descriptor_is_not_a_native_effect() {
        let descriptor = crate::mixer::PluginDescriptor::new("clap", "com.example.gain", "Gain");
        assert_eq!(crate::mixer::native_effect_of(&descriptor), None);
        let unknown = crate::mixer::PluginDescriptor::new("native", "chorus", "Chorus");
        assert_eq!(crate::mixer::native_effect_of(&unknown), None);
    }
}
