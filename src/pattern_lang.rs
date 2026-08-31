//! Pattern mini-notation: parser, printer, and step evaluator.
//!
//! Normative design: `docs/LANGUAGES.md` §2 (deviations recorded in the
//! module tests and the NOTATION lane report). Wiring into live patterns and
//! `PatternDefinition` provenance is the separate NOTEWIRE workstream.
//!
//! A pattern term is not a recording, and a name binding is not an
//! instrument identity. Evaluation is pure and total on parseable input:
//! exact rational placement over PPQ ticks, typed diagnostics wherever the
//! grid forces rounding, and no dice — probabilistic constructs compile to
//! `StepEvent::probability` for the sequencer's seeded scheduler to realize.
//!
//! Semantics fixed by this implementation (matching `sequencer.rs` truth):
//!
//! * `*n` on a leaf is always a ratchet (`StepEvent::ratchets = n`, gate =
//!   the slot width), never a structural expansion. The sequencer spaces
//!   ratchets by `gate / n` (truncating); a nonzero remainder is reported as
//!   [`PatternEvalDiagnostic::RatchetSpacingTruncated`], not hidden.
//! * `Swing` is only meaningful at the root and compiles to
//!   `StepPattern::swing`; the sequencer applies it to odd grid indices at
//!   schedule time, additive with `micro_offset`. Baking swing into offsets
//!   would double-apply under later user edits, so nesting is an error.
//! * The output grid is the coarsest regular grid hitting every event
//!   exactly when that needs at most [`MAX_EXACT_STEPS`] steps; otherwise a
//!   sixteen-step fallback with residues in `micro_offset`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::reconstruction::ReconstructionProposalId;
use crate::sequencer::{BeatDuration, StepEvent, StepLane, StepLaneId, StepPattern, TriggerTarget};

/// Largest exact grid the evaluator will choose before falling back.
pub const MAX_EXACT_STEPS: u64 = 64;
const FALLBACK_STEPS: u64 = 16;

/// FNV-1a 128 over the canonical printed form (the `assets` fingerprint
/// idiom): stable, deterministic, and explicitly non-cryptographic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermHash(pub u128);

/// Exact width as a fraction of the parent extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ratio {
    pub numerator: u32,
    pub denominator: u32,
}

impl Ratio {
    pub const ONE: Self = Self {
        numerator: 1,
        denominator: 1,
    };
}

/// One step in a sequence. Modifiers from the surface syntax land here:
/// `@` sets `width` (rational: `@3` or `@3/2`), `!` sets `replicate`,
/// `*` sets `repeat`, `?` sets `probability`.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub element: Element,
    pub width: Ratio,
    pub replicate: u32,
    pub repeat: u32,
    pub probability: Option<f32>,
}

impl Step {
    fn plain(element: Element) -> Self {
        Self {
            element,
            width: Ratio::ONE,
            replicate: 1,
            repeat: 1,
            probability: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Element {
    Rest,
    Name {
        binding: String,
        variant: Option<u32>,
    },
    Group(Vec<Step>),
    /// One member per cycle, round-robin by cycle index.
    Alternate(Vec<Step>),
}

/// Transforms applicable under `Every`.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternTransform {
    Rotate(i32),
    Gain(f32),
    Degrade(f32),
}

/// The pattern term. Combinator calls and mini-notation parse into the same
/// type; there are no strings-as-code anywhere below this boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternExpr {
    /// One cycle of steps whose widths partition the cycle.
    Seq(Vec<Step>),
    /// Simultaneous patterns merged into shared lanes.
    Stack(Vec<PatternExpr>),
    Every {
        period: u32,
        offset: u32,
        transform: PatternTransform,
        inner: Box<PatternExpr>,
    },
    Rotate {
        steps: i32,
        inner: Box<PatternExpr>,
    },
    Euclid {
        hits: u32,
        slots: u32,
        rotation: i32,
        element: Element,
    },
    Fast {
        factor: u32,
        inner: Box<PatternExpr>,
    },
    Slow {
        factor: u32,
        inner: Box<PatternExpr>,
    },
    Swing {
        amount: f32,
        inner: Box<PatternExpr>,
    },
    Gain {
        linear: f32,
        inner: Box<PatternExpr>,
    },
    Degrade {
        probability: f32,
        inner: Box<PatternExpr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternParseError {
    /// Byte offset into the source string.
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for PatternParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for PatternParseError {}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternEvalError {
    /// An identifier had no entry in the binding table. Variants look up the
    /// composite key `name:variant`.
    UnboundName(String),
    /// Two events landed on the same lane and grid step; a `BTreeMap<u32,
    /// StepEvent>` lane cannot hold both and last-writer-wins would lie.
    StepCollision { binding: String, tick: i64 },
    EmptyPattern,
    InvalidRatio { numerator: u32, denominator: u32 },
    /// A finite/positive/unit-range parameter constraint was violated.
    InvalidParameter(&'static str),
    /// `Swing` below the root cannot map onto the pattern-global swing field.
    NestedSwing,
    /// Exact rational arithmetic exceeded the representable range.
    SpanOverflow,
    InvalidCycle,
}

impl fmt::Display for PatternEvalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundName(name) => write!(formatter, "unbound name: {name}"),
            Self::StepCollision { binding, tick } => {
                write!(formatter, "step collision on lane {binding} at tick {tick}")
            }
            Self::EmptyPattern => formatter.write_str("pattern has no steps"),
            Self::InvalidRatio {
                numerator,
                denominator,
            } => write!(formatter, "invalid ratio {numerator}/{denominator}"),
            Self::InvalidParameter(what) => write!(formatter, "invalid parameter: {what}"),
            Self::NestedSwing => formatter.write_str("swing must be the outermost combinator"),
            Self::SpanOverflow => formatter.write_str("pattern arithmetic overflowed"),
            Self::InvalidCycle => formatter.write_str("cycle length must be positive ticks"),
        }
    }
}

impl std::error::Error for PatternEvalError {}

/// Non-fatal evaluation facts: lossy boundaries are reported, not hidden.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PatternEvalDiagnostic {
    /// An exact rational position or gate did not land on an integer tick;
    /// the error is reported in thousandths of a tick.
    RoundedToTick {
        at_tick: i64,
        error_milliticks: i32,
    },
    /// The sequencer spaces ratchets by `gate / ratchets` with truncation;
    /// this event's gate leaves a nonzero remainder.
    RatchetSpacingTruncated {
        at_tick: i64,
        remainder_ticks: u32,
    },
}

pub struct EvalContext<'a> {
    /// Name → trigger binding. One step lane per distinct bound key.
    pub bindings: &'a BTreeMap<String, TriggerTarget>,
    /// One cycle's musical length.
    pub cycle: BeatDuration,
    /// Reserved for future stochastic combinators. Present evaluation rolls
    /// no dice: probabilities compile into `StepEvent::probability`.
    pub seed: u64,
    /// Which cycle index alternations and `every`/`slow` select from.
    pub cycle_index: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalOutput {
    pub pattern: StepPattern,
    pub diagnostics: Vec<PatternEvalDiagnostic>,
}

/// Provenance for generated patterns. Wiring this into
/// `sequencer::PatternDefinition` (plus codec round-trip and old-file
/// defaulting to `Authored`) is NOTEWIRE's shared-struct change.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternOrigin {
    Authored,
    Expression {
        source: String,
        term_hash: TermHash,
        bindings_hash: TermHash,
        /// Set — never silently cleared — when the realized pattern is
        /// edited by hand after generation.
        diverged: bool,
    },
    Deprojected {
        proposal: ReconstructionProposalId,
        diverged: bool,
    },
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    source: &'a [u8],
    offset: usize,
}

pub fn parse(source: &str) -> Result<PatternExpr, PatternParseError> {
    let mut parser = Parser {
        source: source.as_bytes(),
        offset: 0,
    };
    parser.skip_space();
    let expr = parser.expr()?;
    parser.skip_space();
    if parser.offset != parser.source.len() {
        return Err(parser.error("unexpected trailing input"));
    }
    Ok(expr)
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> PatternParseError {
        PatternParseError {
            offset: self.offset,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.offset += 1;
        Some(byte)
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
            self.offset += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), PatternParseError> {
        if self.peek() == Some(byte) {
            self.offset += 1;
            Ok(())
        } else {
            Err(self.error(format!("expected '{}'", byte as char)))
        }
    }

    /// Either a combinator call (`ident(...)`) or a mini-notation sequence.
    fn expr(&mut self) -> Result<PatternExpr, PatternParseError> {
        if let Some(call) = self.try_call()? {
            return Ok(call);
        }
        let steps = self.steps()?;
        if steps.is_empty() {
            return Err(self.error("expected a pattern"));
        }
        Ok(PatternExpr::Seq(steps))
    }

    /// A call is an identifier immediately followed by `(`. A bare
    /// identifier is a step name, so bindings may shadow combinator names.
    fn try_call(&mut self) -> Result<Option<PatternExpr>, PatternParseError> {
        let start = self.offset;
        let Some(word) = self.ident() else {
            return Ok(None);
        };
        if self.peek() != Some(b'(') {
            self.offset = start;
            return Ok(None);
        }
        let known = matches!(
            word.as_str(),
            "seq" | "stack" | "every" | "rot" | "e" | "fast" | "slow" | "swing" | "gain"
                | "degrade"
        );
        if !known {
            self.offset = start;
            return Ok(None);
        }
        self.expect(b'(')?;
        self.skip_space();
        let call = match word.as_str() {
            "seq" => self.expr()?,
            "stack" => {
                let mut members = vec![self.expr()?];
                while self.comma()? {
                    members.push(self.expr()?);
                }
                PatternExpr::Stack(members)
            }
            "every" => {
                let period = self.integer()? as u32;
                self.require_comma()?;
                let transform = self.transform()?;
                self.require_comma()?;
                let inner = Box::new(self.expr()?);
                PatternExpr::Every {
                    period,
                    offset: 0,
                    transform,
                    inner,
                }
            }
            "rot" => {
                let steps = self.signed_integer()? as i32;
                self.require_comma()?;
                let inner = Box::new(self.expr()?);
                PatternExpr::Rotate { steps, inner }
            }
            "e" => {
                let hits = self.integer()? as u32;
                self.require_comma()?;
                let slots = self.integer()? as u32;
                self.skip_space();
                let mut rotation = 0;
                let mut element = None;
                while self.comma()? {
                    // Third argument is a rotation integer or the element.
                    if element.is_none() && rotation == 0 && self.peek_signed_integer() {
                        rotation = self.signed_integer()? as i32;
                    } else {
                        element = Some(self.element()?);
                    }
                }
                let element = element.ok_or_else(|| self.error("e(...) needs an element"))?;
                PatternExpr::Euclid {
                    hits,
                    slots,
                    rotation,
                    element,
                }
            }
            "fast" | "slow" => {
                let factor = self.integer()? as u32;
                self.require_comma()?;
                let inner = Box::new(self.expr()?);
                if word == "fast" {
                    PatternExpr::Fast { factor, inner }
                } else {
                    PatternExpr::Slow { factor, inner }
                }
            }
            "swing" | "gain" | "degrade" => {
                let value = self.number()?;
                self.require_comma()?;
                let inner = Box::new(self.expr()?);
                match word.as_str() {
                    "swing" => PatternExpr::Swing {
                        amount: value,
                        inner,
                    },
                    "gain" => PatternExpr::Gain {
                        linear: value,
                        inner,
                    },
                    _ => PatternExpr::Degrade {
                        probability: value,
                        inner,
                    },
                }
            }
            _ => unreachable!("known combinator list is exhaustive"),
        };
        self.skip_space();
        self.expect(b')')?;
        Ok(Some(call))
    }

    fn comma(&mut self) -> Result<bool, PatternParseError> {
        self.skip_space();
        if self.peek() == Some(b',') {
            self.offset += 1;
            self.skip_space();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn require_comma(&mut self) -> Result<(), PatternParseError> {
        if self.comma()? {
            Ok(())
        } else {
            Err(self.error("expected ','"))
        }
    }

    fn transform(&mut self) -> Result<PatternTransform, PatternParseError> {
        let Some(word) = self.ident() else {
            return Err(self.error("expected rot/gain/degrade transform"));
        };
        self.expect(b'(')?;
        self.skip_space();
        let transform = match word.as_str() {
            "rot" => PatternTransform::Rotate(self.signed_integer()? as i32),
            "gain" => PatternTransform::Gain(self.number()?),
            "degrade" => PatternTransform::Degrade(self.number()?),
            other => return Err(self.error(format!("unknown transform: {other}"))),
        };
        self.skip_space();
        self.expect(b')')?;
        Ok(transform)
    }

    fn steps(&mut self) -> Result<Vec<Step>, PatternParseError> {
        let mut steps = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                Some(b']') | Some(b'>') | Some(b')') | Some(b',') | None => break,
                _ => steps.push(self.step()?),
            }
        }
        Ok(steps)
    }

    fn step(&mut self) -> Result<Step, PatternParseError> {
        let element = self.element()?;
        let mut step = Step::plain(element);
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.offset += 1;
                    step.repeat = self.integer()? as u32;
                    if step.repeat == 0 {
                        return Err(self.error("'*0' is not a repeat"));
                    }
                }
                Some(b'!') => {
                    self.offset += 1;
                    step.replicate = self.integer()? as u32;
                    if step.replicate == 0 {
                        return Err(self.error("'!0' is not a replication"));
                    }
                }
                Some(b'@') => {
                    self.offset += 1;
                    let numerator = self.integer()? as u32;
                    let denominator = if self.peek() == Some(b'/') {
                        self.offset += 1;
                        self.integer()? as u32
                    } else {
                        1
                    };
                    if numerator == 0 || denominator == 0 {
                        return Err(self.error("width must be a positive rational"));
                    }
                    step.width = Ratio {
                        numerator,
                        denominator,
                    };
                }
                Some(b'?') => {
                    self.offset += 1;
                    step.probability = Some(if self.peek_number() {
                        self.number()?
                    } else {
                        0.5
                    });
                }
                _ => break,
            }
        }
        Ok(step)
    }

    fn element(&mut self) -> Result<Element, PatternParseError> {
        self.skip_space();
        match self.peek() {
            Some(b'~') => {
                self.offset += 1;
                Ok(Element::Rest)
            }
            Some(b'[') => {
                self.offset += 1;
                let steps = self.steps()?;
                self.expect(b']')?;
                if steps.is_empty() {
                    return Err(self.error("empty group"));
                }
                Ok(Element::Group(steps))
            }
            Some(b'<') => {
                self.offset += 1;
                let steps = self.steps()?;
                self.expect(b'>')?;
                if steps.is_empty() {
                    return Err(self.error("empty alternation"));
                }
                Ok(Element::Alternate(steps))
            }
            Some(byte) if byte.is_ascii_lowercase() => {
                let binding = self
                    .ident()
                    .expect("lowercase start guarantees an identifier");
                let variant = if self.peek() == Some(b':') {
                    self.offset += 1;
                    Some(self.integer()? as u32)
                } else {
                    None
                };
                Ok(Element::Name { binding, variant })
            }
            _ => Err(self.error("expected a name, '~', '[', or '<'")),
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.offset;
        match self.peek() {
            Some(byte) if byte.is_ascii_lowercase() => {}
            _ => return None,
        }
        self.offset += 1;
        while matches!(
            self.peek(),
            Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        ) {
            self.offset += 1;
        }
        Some(String::from_utf8_lossy(&self.source[start..self.offset]).into_owned())
    }

    fn peek_number(&self) -> bool {
        matches!(self.peek(), Some(byte) if byte.is_ascii_digit())
    }

    fn peek_signed_integer(&self) -> bool {
        match self.peek() {
            Some(b'-') => matches!(
                self.source.get(self.offset + 1),
                Some(byte) if byte.is_ascii_digit()
            ),
            Some(byte) => byte.is_ascii_digit(),
            None => false,
        }
    }

    fn integer(&mut self) -> Result<u64, PatternParseError> {
        let start = self.offset;
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
            self.offset += 1;
        }
        if start == self.offset {
            return Err(self.error("expected an integer"));
        }
        std::str::from_utf8(&self.source[start..self.offset])
            .expect("digits are UTF-8")
            .parse()
            .map_err(|_| self.error("integer out of range"))
    }

    fn signed_integer(&mut self) -> Result<i64, PatternParseError> {
        let negative = self.peek() == Some(b'-');
        if negative {
            self.offset += 1;
        }
        let magnitude = self.integer()? as i64;
        Ok(if negative { -magnitude } else { magnitude })
    }

    fn number(&mut self) -> Result<f32, PatternParseError> {
        let start = self.offset;
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
            self.offset += 1;
        }
        if self.peek() == Some(b'.') {
            self.offset += 1;
            while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
                self.offset += 1;
            }
        }
        if start == self.offset {
            return Err(self.error("expected a number"));
        }
        std::str::from_utf8(&self.source[start..self.offset])
            .expect("digits are UTF-8")
            .parse()
            .map_err(|_| self.error("number out of range"))
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// Canonical form; `parse(print(t)) == t` for every term this module can
/// construct. Combinators print as calls, sequences as mini-notation.
pub fn print(expr: &PatternExpr) -> String {
    let mut output = String::new();
    print_expr(expr, &mut output);
    output
}

fn print_expr(expr: &PatternExpr, output: &mut String) {
    match expr {
        PatternExpr::Seq(steps) => print_steps(steps, output),
        PatternExpr::Stack(members) => {
            output.push_str("stack(");
            for (index, member) in members.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                print_expr(member, output);
            }
            output.push(')');
        }
        PatternExpr::Every {
            period,
            offset: _,
            transform,
            inner,
        } => {
            output.push_str(&format!("every({period}, "));
            match transform {
                PatternTransform::Rotate(steps) => output.push_str(&format!("rot({steps})")),
                PatternTransform::Gain(gain) => output.push_str(&format!("gain({gain:?})")),
                PatternTransform::Degrade(probability) => {
                    output.push_str(&format!("degrade({probability:?})"))
                }
            }
            output.push_str(", ");
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Rotate { steps, inner } => {
            output.push_str(&format!("rot({steps}, "));
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Euclid {
            hits,
            slots,
            rotation,
            element,
        } => {
            if *rotation == 0 {
                output.push_str(&format!("e({hits}, {slots}, "));
            } else {
                output.push_str(&format!("e({hits}, {slots}, {rotation}, "));
            }
            print_element(element, output);
            output.push(')');
        }
        PatternExpr::Fast { factor, inner } => {
            output.push_str(&format!("fast({factor}, "));
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Slow { factor, inner } => {
            output.push_str(&format!("slow({factor}, "));
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Swing { amount, inner } => {
            output.push_str(&format!("swing({amount:?}, "));
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Gain { linear, inner } => {
            output.push_str(&format!("gain({linear:?}, "));
            print_expr(inner, output);
            output.push(')');
        }
        PatternExpr::Degrade { probability, inner } => {
            output.push_str(&format!("degrade({probability:?}, "));
            print_expr(inner, output);
            output.push(')');
        }
    }
}

fn print_steps(steps: &[Step], output: &mut String) {
    for (index, step) in steps.iter().enumerate() {
        if index > 0 {
            output.push(' ');
        }
        print_element(&step.element, output);
        if step.repeat > 1 {
            output.push_str(&format!("*{}", step.repeat));
        }
        if step.replicate > 1 {
            output.push_str(&format!("!{}", step.replicate));
        }
        if step.width != Ratio::ONE {
            if step.width.denominator == 1 {
                output.push_str(&format!("@{}", step.width.numerator));
            } else {
                output.push_str(&format!(
                    "@{}/{}",
                    step.width.numerator, step.width.denominator
                ));
            }
        }
        if let Some(probability) = step.probability {
            output.push_str(&format!("?{probability:?}"));
        }
    }
}

fn print_element(element: &Element, output: &mut String) {
    match element {
        Element::Rest => output.push('~'),
        Element::Name { binding, variant } => {
            output.push_str(binding);
            if let Some(variant) = variant {
                output.push_str(&format!(":{variant}"));
            }
        }
        Element::Group(steps) => {
            output.push('[');
            print_steps(steps, output);
            output.push(']');
        }
        Element::Alternate(steps) => {
            output.push('<');
            print_steps(steps, output);
            output.push('>');
        }
    }
}

pub fn term_hash(expr: &PatternExpr) -> TermHash {
    TermHash(fnv1a_128(print(expr).as_bytes()))
}

/// Hash a binding table the same way, for `PatternOrigin::bindings_hash`.
pub fn bindings_hash(bindings: &BTreeMap<String, TriggerTarget>) -> TermHash {
    let mut description = String::new();
    for (name, target) in bindings {
        description.push_str(name);
        description.push('=');
        description.push_str(&format!("{target:?};"));
    }
    TermHash(fnv1a_128(description.as_bytes()))
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Exact nonnegative rational with u64 components and u128 intermediates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rational {
    numerator: u64,
    denominator: u64,
}

impl Rational {
    const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };
    const ONE: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    fn new(numerator: u64, denominator: u64) -> Result<Self, PatternEvalError> {
        if denominator == 0 {
            return Err(PatternEvalError::SpanOverflow);
        }
        Ok(Self {
            numerator,
            denominator,
        }
        .reduced())
    }

    fn reduced(self) -> Self {
        let divisor = gcd(self.numerator, self.denominator).max(1);
        Self {
            numerator: self.numerator / divisor,
            denominator: self.denominator / divisor,
        }
    }

    fn checked(numerator: u128, denominator: u128) -> Result<Self, PatternEvalError> {
        if denominator == 0 {
            return Err(PatternEvalError::SpanOverflow);
        }
        let divisor = gcd128(numerator, denominator).max(1);
        let numerator = numerator / divisor;
        let denominator = denominator / divisor;
        if numerator > u64::MAX as u128 || denominator > u64::MAX as u128 {
            return Err(PatternEvalError::SpanOverflow);
        }
        Ok(Self {
            numerator: numerator as u64,
            denominator: denominator as u64,
        })
    }

    fn add(self, other: Self) -> Result<Self, PatternEvalError> {
        Self::checked(
            self.numerator as u128 * other.denominator as u128
                + other.numerator as u128 * self.denominator as u128,
            self.denominator as u128 * other.denominator as u128,
        )
    }

    fn mul(self, other: Self) -> Result<Self, PatternEvalError> {
        Self::checked(
            self.numerator as u128 * other.numerator as u128,
            self.denominator as u128 * other.denominator as u128,
        )
    }

    fn div(self, other: Self) -> Result<Self, PatternEvalError> {
        if other.numerator == 0 {
            return Err(PatternEvalError::SpanOverflow);
        }
        Self::checked(
            self.numerator as u128 * other.denominator as u128,
            self.denominator as u128 * other.numerator as u128,
        )
    }

    fn scale_int(self, factor: u64) -> Result<Self, PatternEvalError> {
        Self::checked(
            self.numerator as u128 * factor as u128,
            self.denominator as u128,
        )
    }

    fn cmp_value(self, other: Self) -> std::cmp::Ordering {
        (self.numerator as u128 * other.denominator as u128)
            .cmp(&(other.numerator as u128 * self.denominator as u128))
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

fn gcd128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

/// One realized event in normalized cycle space `[0, 1)`.
#[derive(Clone, Debug)]
struct AbstractEvent {
    lane: String,
    position: Rational,
    width: Rational,
    velocity: f32,
    probability: f32,
    ratchets: u8,
}

/// Exact placement per `docs/LANGUAGES.md` §2 and the module header.
pub fn eval_steps(
    expr: &PatternExpr,
    context: &EvalContext<'_>,
) -> Result<EvalOutput, PatternEvalError> {
    let cycle_ticks = context.cycle.0;
    if cycle_ticks == 0 || cycle_ticks > i64::MAX as u64 {
        return Err(PatternEvalError::InvalidCycle);
    }

    // Swing is only meaningful at the root; see the module header.
    let (swing, core) = match expr {
        PatternExpr::Swing { amount, inner } => {
            if !amount.is_finite() || !(0.0..=1.0).contains(amount) {
                return Err(PatternEvalError::InvalidParameter("swing amount"));
            }
            (*amount, inner.as_ref())
        }
        other => (0.0, other),
    };

    let events = eval_norm(core, context.cycle_index, context)?;
    if events.is_empty() {
        // A pattern of only rests is legal and yields empty lanes.
    }

    let mut diagnostics = Vec::new();

    // Materialize exact tick positions, rounding to the tick lattice.
    struct Placed {
        lane: String,
        tick: i64,
        gate: u64,
        velocity: f32,
        probability: f32,
        ratchets: u8,
    }
    let mut placed = Vec::with_capacity(events.len());
    for event in &events {
        let tick = round_ticks(event.position, cycle_ticks, &mut diagnostics)?;
        let gate = round_ticks(event.width, cycle_ticks, &mut diagnostics)?.max(1) as u64;
        if event.ratchets > 1 {
            let remainder = gate % event.ratchets as u64;
            if remainder != 0 {
                diagnostics.push(PatternEvalDiagnostic::RatchetSpacingTruncated {
                    at_tick: tick,
                    remainder_ticks: remainder.min(u32::MAX as u64) as u32,
                });
            }
        }
        placed.push(Placed {
            lane: event.lane.clone(),
            tick,
            gate,
            velocity: event.velocity,
            probability: event.probability,
            ratchets: event.ratchets,
        });
    }

    // Grid choice: coarsest exact grid within MAX_EXACT_STEPS, else fallback.
    let mut grid_gcd = cycle_ticks;
    for event in &placed {
        grid_gcd = gcd(grid_gcd, event.tick as u64);
    }
    let grid_gcd = grid_gcd.max(1);
    let (resolution, fallback) = if cycle_ticks / grid_gcd <= MAX_EXACT_STEPS {
        (grid_gcd, false)
    } else {
        ((cycle_ticks / FALLBACK_STEPS).max(1), true)
    };

    // Assemble lanes: one per distinct binding key, deterministic order.
    let mut lanes: BTreeMap<String, BTreeMap<u32, StepEvent>> = BTreeMap::new();
    for event in &placed {
        let mut index = (event.tick as u64 / resolution).min(u32::MAX as u64) as u32;
        if fallback {
            index = index.min(FALLBACK_STEPS as u32 - 1);
        }
        let micro = event.tick - index as i64 * resolution as i64;
        let micro = i32::try_from(micro).map_err(|_| PatternEvalError::SpanOverflow)?;
        let steps = lanes.entry(event.lane.clone()).or_default();
        if steps.contains_key(&index) {
            return Err(PatternEvalError::StepCollision {
                binding: event.lane.clone(),
                tick: event.tick,
            });
        }
        steps.insert(
            index,
            StepEvent {
                velocity: event.velocity,
                probability: event.probability,
                micro_offset: micro,
                gate: BeatDuration(event.gate),
                ratchets: event.ratchets,
                pitch_semitones: 0.0,
                pan: 0.0,
            },
        );
    }

    let mut pattern_lanes = BTreeMap::new();
    for (ordinal, (name, steps)) in lanes.into_iter().enumerate() {
        let target = context
            .bindings
            .get(&name)
            .cloned()
            .ok_or_else(|| PatternEvalError::UnboundName(name.clone()))?;
        let id = StepLaneId::from_raw(ordinal as u64 + 1);
        pattern_lanes.insert(
            id,
            StepLane {
                id,
                name,
                target,
                choke_group: None,
                steps,
            },
        );
    }

    Ok(EvalOutput {
        pattern: StepPattern {
            resolution: BeatDuration(resolution),
            swing,
            lanes: pattern_lanes,
        },
        diagnostics,
    })
}

fn round_ticks(
    fraction: Rational,
    cycle_ticks: u64,
    diagnostics: &mut Vec<PatternEvalDiagnostic>,
) -> Result<i64, PatternEvalError> {
    let numerator = fraction.numerator as u128 * cycle_ticks as u128;
    let denominator = fraction.denominator as u128;
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    // Round to nearest; ties toward the later tick.
    let (tick, error_num) = if remainder * 2 >= denominator {
        (quotient + 1, denominator - remainder)
    } else {
        (quotient, remainder)
    };
    if tick > i64::MAX as u128 {
        return Err(PatternEvalError::SpanOverflow);
    }
    if error_num != 0 {
        let millis = (error_num * 1000 / denominator).min(i32::MAX as u128) as i32;
        diagnostics.push(PatternEvalDiagnostic::RoundedToTick {
            at_tick: tick as i64,
            error_milliticks: millis.max(1),
        });
    }
    Ok(tick as i64)
}

/// Evaluate a term into events over one normalized cycle `[0, 1)`.
fn eval_norm(
    expr: &PatternExpr,
    cycle_index: u64,
    context: &EvalContext<'_>,
) -> Result<Vec<AbstractEvent>, PatternEvalError> {
    match expr {
        PatternExpr::Swing { .. } => Err(PatternEvalError::NestedSwing),
        PatternExpr::Seq(steps) => {
            let mut events = Vec::new();
            eval_seq(
                steps,
                Rational::ZERO,
                Rational::ONE,
                cycle_index,
                context,
                &mut events,
            )?;
            Ok(events)
        }
        PatternExpr::Stack(members) => {
            if members.is_empty() {
                return Err(PatternEvalError::EmptyPattern);
            }
            let mut events = Vec::new();
            for member in members {
                events.extend(eval_norm(member, cycle_index, context)?);
            }
            Ok(events)
        }
        PatternExpr::Every {
            period,
            offset,
            transform,
            inner,
        } => {
            if *period == 0 {
                return Err(PatternEvalError::InvalidParameter("every period"));
            }
            let mut events = eval_norm(inner, cycle_index, context)?;
            if cycle_index % u64::from(*period) == u64::from(*offset) % u64::from(*period) {
                apply_transform(transform, &mut events)?;
            }
            Ok(events)
        }
        PatternExpr::Rotate { steps, inner } => {
            let mut events = eval_norm(inner, cycle_index, context)?;
            rotate_payloads(&mut events, *steps);
            Ok(events)
        }
        PatternExpr::Euclid {
            hits,
            slots,
            rotation,
            element,
        } => {
            if *slots == 0 || *hits > *slots {
                return Err(PatternEvalError::InvalidParameter("euclid hits/slots"));
            }
            let mut events = Vec::new();
            let slot_width = Rational::new(1, u64::from(*slots))?;
            for slot in 0..*slots {
                let rotated = (i64::from(slot) - i64::from(*rotation))
                    .rem_euclid(i64::from(*slots)) as u64;
                if (rotated * u64::from(*hits)) % u64::from(*slots) < u64::from(*hits) {
                    let start = slot_width.scale_int(u64::from(slot))?;
                    eval_element(
                        element,
                        start,
                        slot_width,
                        1,
                        None,
                        cycle_index,
                        context,
                        &mut events,
                    )?;
                }
            }
            Ok(events)
        }
        PatternExpr::Fast { factor, inner } => {
            if *factor == 0 {
                return Err(PatternEvalError::InvalidParameter("fast factor"));
            }
            let mut events = Vec::new();
            let copy_width = Rational::new(1, u64::from(*factor))?;
            for copy in 0..u64::from(*factor) {
                let inner_events =
                    eval_norm(inner, cycle_index * u64::from(*factor) + copy, context)?;
                let start = copy_width.scale_int(copy)?;
                for event in inner_events {
                    let position = start.add(event.position.mul(copy_width)?)?;
                    events.push(AbstractEvent {
                        position,
                        width: event.width.mul(copy_width)?,
                        ..event
                    });
                }
            }
            Ok(events)
        }
        PatternExpr::Slow { factor, inner } => {
            if *factor == 0 {
                return Err(PatternEvalError::InvalidParameter("slow factor"));
            }
            let factor_u64 = u64::from(*factor);
            let window = cycle_index % factor_u64;
            let inner_events = eval_norm(inner, cycle_index / factor_u64, context)?;
            let window_start = Rational::new(window, factor_u64)?;
            let window_end = Rational::new(window + 1, factor_u64)?;
            let mut events = Vec::new();
            for event in inner_events {
                if event.position.cmp_value(window_start) == std::cmp::Ordering::Less
                    || event.position.cmp_value(window_end) != std::cmp::Ordering::Less
                {
                    continue;
                }
                // position' = (position - window_start) * factor
                let offset = Rational::checked(
                    event.position.numerator as u128 * window_start.denominator as u128
                        - window_start.numerator as u128 * event.position.denominator as u128,
                    event.position.denominator as u128 * window_start.denominator as u128,
                )?;
                let position = offset.scale_int(factor_u64)?;
                let width = event.width.scale_int(factor_u64)?;
                // Keep the gate inside the cycle.
                let end = position.add(width)?;
                let width = if end.cmp_value(Rational::ONE) == std::cmp::Ordering::Greater {
                    Rational::checked(
                        (Rational::ONE.numerator as u128 * position.denominator as u128)
                            .saturating_sub(position.numerator as u128),
                        position.denominator as u128,
                    )?
                } else {
                    width
                };
                events.push(AbstractEvent {
                    position,
                    width,
                    ..event
                });
            }
            Ok(events)
        }
        PatternExpr::Gain { linear, inner } => {
            if !linear.is_finite() || *linear < 0.0 {
                return Err(PatternEvalError::InvalidParameter("gain"));
            }
            let mut events = eval_norm(inner, cycle_index, context)?;
            for event in &mut events {
                event.velocity = (event.velocity * linear).clamp(0.0, 1.0);
            }
            Ok(events)
        }
        PatternExpr::Degrade { probability, inner } => {
            if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
                return Err(PatternEvalError::InvalidParameter("degrade probability"));
            }
            let mut events = eval_norm(inner, cycle_index, context)?;
            for event in &mut events {
                event.probability = (event.probability * (1.0 - probability)).clamp(0.0, 1.0);
            }
            Ok(events)
        }
    }
}

fn apply_transform(
    transform: &PatternTransform,
    events: &mut Vec<AbstractEvent>,
) -> Result<(), PatternEvalError> {
    match transform {
        PatternTransform::Rotate(steps) => {
            rotate_payloads(events, *steps);
            Ok(())
        }
        PatternTransform::Gain(gain) => {
            if !gain.is_finite() || *gain < 0.0 {
                return Err(PatternEvalError::InvalidParameter("gain"));
            }
            for event in events {
                event.velocity = (event.velocity * gain).clamp(0.0, 1.0);
            }
            Ok(())
        }
        PatternTransform::Degrade(probability) => {
            if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
                return Err(PatternEvalError::InvalidParameter("degrade probability"));
            }
            for event in events {
                event.probability = (event.probability * (1.0 - probability)).clamp(0.0, 1.0);
            }
            Ok(())
        }
    }
}

/// Tidal-style `rot`: payloads cycle over the fixed position lattice.
fn rotate_payloads(events: &mut [AbstractEvent], steps: i32) {
    let count = events.len();
    if count == 0 || steps.rem_euclid(count as i32) == 0 {
        return;
    }
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|left, right| {
        events[*left]
            .position
            .cmp_value(events[*right].position)
            .then_with(|| events[*left].lane.cmp(&events[*right].lane))
            .then_with(|| left.cmp(right))
    });
    let shift = steps.rem_euclid(count as i32) as usize;
    let payloads: Vec<(String, f32, f32, u8)> = order
        .iter()
        .map(|index| {
            let event = &events[*index];
            (
                event.lane.clone(),
                event.velocity,
                event.probability,
                event.ratchets,
            )
        })
        .collect();
    for (rank, index) in order.iter().enumerate() {
        let source = &payloads[(rank + shift) % count];
        let event = &mut events[*index];
        event.lane = source.0.clone();
        event.velocity = source.1;
        event.probability = source.2;
        event.ratchets = source.3;
    }
}

fn eval_seq(
    steps: &[Step],
    start: Rational,
    width: Rational,
    cycle_index: u64,
    context: &EvalContext<'_>,
    out: &mut Vec<AbstractEvent>,
) -> Result<(), PatternEvalError> {
    if steps.is_empty() {
        return Err(PatternEvalError::EmptyPattern);
    }
    let mut total = Rational::ZERO;
    for step in steps {
        if step.width.numerator == 0 || step.width.denominator == 0 {
            return Err(PatternEvalError::InvalidRatio {
                numerator: step.width.numerator,
                denominator: step.width.denominator,
            });
        }
        total = total.add(Rational::new(
            u64::from(step.width.numerator) * u64::from(step.replicate.max(1)),
            u64::from(step.width.denominator),
        )?)?;
    }

    let mut cursor = Rational::ZERO;
    for step in steps {
        let sibling_weight = Rational::new(
            u64::from(step.width.numerator),
            u64::from(step.width.denominator),
        )?;
        let slot_fraction = sibling_weight.div(total)?;
        for _copy in 0..step.replicate.max(1) {
            let slot_start = start.add(cursor.div(total)?.mul(width)?)?;
            let slot_width = slot_fraction.mul(width)?;
            eval_element(
                &step.element,
                slot_start,
                slot_width,
                step.repeat.max(1),
                step.probability,
                cycle_index,
                context,
                out,
            )?;
            cursor = cursor.add(sibling_weight)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn eval_element(
    element: &Element,
    slot_start: Rational,
    slot_width: Rational,
    repeat: u32,
    probability: Option<f32>,
    cycle_index: u64,
    context: &EvalContext<'_>,
    out: &mut Vec<AbstractEvent>,
) -> Result<(), PatternEvalError> {
    if let Some(probability) = probability {
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(PatternEvalError::InvalidParameter("step probability"));
        }
    }
    match element {
        Element::Rest => Ok(()),
        Element::Name { binding, variant } => {
            if repeat > u8::MAX as u32 {
                return Err(PatternEvalError::InvalidParameter("repeat count"));
            }
            let lane = match variant {
                Some(variant) => format!("{binding}:{variant}"),
                None => binding.clone(),
            };
            out.push(AbstractEvent {
                lane,
                position: slot_start,
                width: slot_width,
                velocity: 1.0,
                probability: probability.unwrap_or(1.0),
                ratchets: repeat.max(1) as u8,
            });
            Ok(())
        }
        Element::Group(steps) => {
            let copies = repeat.max(1);
            let copy_width = slot_width.div(Rational::new(u64::from(copies), 1)?)?;
            let before = out.len();
            for copy in 0..copies {
                let copy_start =
                    slot_start.add(copy_width.scale_int(u64::from(copy))?)?;
                eval_seq(steps, copy_start, copy_width, cycle_index, context, out)?;
            }
            if let Some(probability) = probability {
                for event in &mut out[before..] {
                    event.probability = (event.probability * probability).clamp(0.0, 1.0);
                }
            }
            Ok(())
        }
        Element::Alternate(steps) => {
            if steps.is_empty() {
                return Err(PatternEvalError::EmptyPattern);
            }
            let chosen = &steps[(cycle_index % steps.len() as u64) as usize];
            // The chosen member fills the slot; its own repeat/replicate
            // subdivide it, and probabilities compose multiplicatively.
            let combined = match (probability, chosen.probability) {
                (Some(outer), Some(inner)) => Some(outer * inner),
                (Some(outer), None) => Some(outer),
                (None, inner) => inner,
            };
            let copies = chosen.replicate.max(1);
            let copy_width = slot_width.div(Rational::new(u64::from(copies), 1)?)?;
            for copy in 0..copies {
                let copy_start =
                    slot_start.add(copy_width.scale_int(u64::from(copy))?)?;
                eval_element(
                    &chosen.element,
                    copy_start,
                    copy_width,
                    chosen.repeat.max(1).saturating_mul(repeat.max(1)),
                    combined,
                    cycle_index,
                    context,
                    out,
                )?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CYCLE_4_BEATS: u64 = 3_840; // 4 beats at PPQ 960.

    fn bindings(names: &[&str]) -> BTreeMap<String, TriggerTarget> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                (
                    (*name).to_owned(),
                    TriggerTarget::AnalysisTemplate(index as u64),
                )
            })
            .collect()
    }

    fn eval(
        source: &str,
        names: &[&str],
        cycle_ticks: u64,
        cycle_index: u64,
    ) -> Result<EvalOutput, PatternEvalError> {
        let expr = parse(source).expect("test source parses");
        let table = bindings(names);
        eval_steps(
            &expr,
            &EvalContext {
                bindings: &table,
                cycle: BeatDuration(cycle_ticks),
                seed: 0,
                cycle_index,
            },
        )
    }

    fn lane<'a>(output: &'a EvalOutput, name: &str) -> &'a StepLane {
        output
            .pattern
            .lanes
            .values()
            .find(|lane| lane.name == name)
            .unwrap_or_else(|| panic!("lane {name} missing"))
    }

    #[test]
    fn parses_the_grammar_corpus_structurally() {
        let expr = parse("pen ~ [pen pen] pen*4").unwrap();
        let PatternExpr::Seq(steps) = &expr else {
            panic!("expected a sequence");
        };
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1].element, Element::Rest);
        assert!(matches!(steps[2].element, Element::Group(ref inner) if inner.len() == 2));
        assert_eq!(steps[3].repeat, 4);

        let expr = parse("a!2 b@3/2 c?0.8 <d e>:").unwrap_err();
        assert!(expr.offset > 0);

        let expr = parse("a!2 b@3/2 c?0.8 <d e>").unwrap();
        let PatternExpr::Seq(steps) = &expr else {
            panic!("expected a sequence");
        };
        assert_eq!(steps[0].replicate, 2);
        assert_eq!(
            steps[1].width,
            Ratio {
                numerator: 3,
                denominator: 2
            }
        );
        assert_eq!(steps[2].probability, Some(0.8));
        assert!(matches!(steps[3].element, Element::Alternate(ref inner) if inner.len() == 2));

        assert!(parse("").is_err());
        assert!(parse("[ ]").is_err());
        assert!(parse("A").is_err(), "names are lowercase");
    }

    #[test]
    fn print_round_trips_every_constructor() {
        let sources = [
            "pen ~ [pen pen] pen*4",
            "a!2 b@3/2 c?0.8 <d e?0.25>",
            "a:1 a:2 ~ a",
            "stack(a b, e(3, 8, c))",
            "every(2, rot(1), a b)",
            "every(4, degrade(0.5), fast(2, a ~))",
            "rot(-1, a b c)",
            "slow(2, a b c d)",
            "swing(0.33, a b a b)",
            "gain(0.75, a*3 b)",
            "e(3, 8, 2, x)",
        ];
        for source in sources {
            let term = parse(source).expect(source);
            let printed = print(&term);
            let reparsed = parse(&printed)
                .unwrap_or_else(|error| panic!("reprint of {source} -> {printed}: {error}"));
            assert_eq!(reparsed, term, "{source} -> {printed}");
            assert_eq!(term_hash(&reparsed), term_hash(&term));
        }
    }

    #[test]
    fn four_even_steps_choose_the_quarter_grid() {
        let output = eval("pen ~ pen pen", &["pen"], CYCLE_4_BEATS, 0).unwrap();
        assert_eq!(output.pattern.resolution, BeatDuration(960));
        assert!(output.diagnostics.is_empty());
        let lane = lane(&output, "pen");
        assert_eq!(
            lane.steps.keys().copied().collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
        for event in lane.steps.values() {
            assert_eq!(event.micro_offset, 0);
            assert_eq!(event.gate, BeatDuration(960));
            assert_eq!(event.ratchets, 1);
            assert!(event.validate_for_test());
        }
    }

    #[test]
    fn euclid_three_eight_is_the_tresillo() {
        let output = eval("e(3, 8, pen)", &["pen"], CYCLE_4_BEATS, 0).unwrap();
        assert_eq!(output.pattern.resolution, BeatDuration(480));
        assert_eq!(
            lane(&output, "pen").steps.keys().copied().collect::<Vec<_>>(),
            vec![0, 3, 6]
        );
    }

    #[test]
    fn leaf_repeat_is_a_ratchet_with_full_slot_gate() {
        let output = eval("pen*4", &["pen"], CYCLE_4_BEATS, 0).unwrap();
        let lane = lane(&output, "pen");
        assert_eq!(lane.steps.len(), 1);
        let event = &lane.steps[&0];
        assert_eq!(event.ratchets, 4);
        assert_eq!(event.gate, BeatDuration(CYCLE_4_BEATS));
        // 3840 / 4 divides exactly: no truncation diagnostic.
        assert!(output.diagnostics.is_empty());

        // A gate that does not divide by the ratchet count is reported.
        let lossy = eval("pen*7 ~", &["pen"], CYCLE_4_BEATS, 0).unwrap();
        assert!(lossy
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                PatternEvalDiagnostic::RatchetSpacingTruncated { remainder_ticks, .. }
                    if *remainder_ticks > 0
            )));
    }

    #[test]
    fn group_repeat_expands_structurally() {
        let output = eval("[pen pen]*2 ~", &["pen"], CYCLE_4_BEATS, 0).unwrap();
        let lane = lane(&output, "pen");
        // Four hits in the first half: eighth-note grid.
        assert_eq!(output.pattern.resolution, BeatDuration(480));
        assert_eq!(lane.steps.len(), 4);
        assert_eq!(
            lane.steps.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn exact_quintuplets_get_an_exact_grid() {
        // 3 beats = 2880 ticks; five equal steps of 576 ticks divide exactly.
        let output = eval("a a a a a", &["a"], 2_880, 0).unwrap();
        assert_eq!(output.pattern.resolution, BeatDuration(576));
        assert!(output.diagnostics.is_empty());
        assert_eq!(lane(&output, "a").steps.len(), 5);
    }

    #[test]
    fn septuplets_over_one_beat_fall_back_with_micro_offsets_and_diagnostics() {
        let output = eval("a a a a a a a", &["a"], 960, 0).unwrap();
        // 960 / 7 is not integral: rounding happened and was reported.
        assert!(!output.diagnostics.is_empty());
        let lane = lane(&output, "a");
        assert_eq!(lane.steps.len(), 7);
        assert_eq!(output.pattern.resolution, BeatDuration(60));
        assert!(lane.steps.values().any(|event| event.micro_offset != 0));
    }

    #[test]
    fn alternation_selects_by_cycle_index() {
        for (cycle_index, expected) in [(0_u64, "a"), (1, "b"), (2, "a")] {
            let output = eval("<a b> ~", &["a", "b"], CYCLE_4_BEATS, cycle_index).unwrap();
            assert_eq!(output.pattern.lanes.len(), 1);
            assert_eq!(
                output.pattern.lanes.values().next().unwrap().name,
                expected
            );
        }
    }

    #[test]
    fn fast_squeezes_and_advances_the_cycle_index() {
        let output = eval("fast(2, <a b>)", &["a", "b"], CYCLE_4_BEATS, 0).unwrap();
        // Copy 0 sees cycle 0 (a), copy 1 sees cycle 1 (b).
        let a = lane(&output, "a");
        let b = lane(&output, "b");
        assert_eq!(a.steps.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(b.steps.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(output.pattern.resolution, BeatDuration(1_920));
    }

    #[test]
    fn slow_plays_one_window_per_cycle() {
        let first = eval("slow(2, a b c d)", &["a", "b", "c", "d"], CYCLE_4_BEATS, 0).unwrap();
        assert!(first.pattern.lanes.values().any(|lane| lane.name == "a"));
        assert!(first.pattern.lanes.values().any(|lane| lane.name == "b"));
        assert!(!first.pattern.lanes.values().any(|lane| lane.name == "c"));

        let second = eval("slow(2, a b c d)", &["a", "b", "c", "d"], CYCLE_4_BEATS, 1).unwrap();
        assert!(second.pattern.lanes.values().any(|lane| lane.name == "c"));
        assert!(second.pattern.lanes.values().any(|lane| lane.name == "d"));
        assert!(!second.pattern.lanes.values().any(|lane| lane.name == "a"));
    }

    #[test]
    fn every_applies_its_transform_on_matching_cycles_only() {
        let on = eval("every(2, rot(1), a b)", &["a", "b"], CYCLE_4_BEATS, 0).unwrap();
        let off = eval("every(2, rot(1), a b)", &["a", "b"], CYCLE_4_BEATS, 1).unwrap();
        // Rotation swaps which lane owns which position.
        assert_eq!(lane(&on, "a").steps.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(lane(&on, "b").steps.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(lane(&off, "a").steps.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(lane(&off, "b").steps.keys().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn probability_and_degrade_compose_without_dice() {
        let output = eval(
            "degrade(0.25, a?0.8 b)",
            &["a", "b"],
            CYCLE_4_BEATS,
            0,
        )
        .unwrap();
        let a = &lane(&output, "a").steps[&0];
        let b = &lane(&output, "b").steps[&1];
        assert!((a.probability - 0.8 * 0.75).abs() < 1.0e-6);
        assert!((b.probability - 0.75).abs() < 1.0e-6);
    }

    #[test]
    fn gain_scales_velocity_with_clamping() {
        let output = eval("gain(0.5, a a)", &["a"], CYCLE_4_BEATS, 0).unwrap();
        for event in lane(&output, "a").steps.values() {
            assert!((event.velocity - 0.5).abs() < 1.0e-6);
        }
    }

    #[test]
    fn swing_is_root_only_and_sets_the_pattern_field() {
        let output = eval("swing(0.33, a b a b)", &["a", "b"], CYCLE_4_BEATS, 0).unwrap();
        assert!((output.pattern.swing - 0.33).abs() < 1.0e-6);
        for lane in output.pattern.lanes.values() {
            for event in lane.steps.values() {
                assert_eq!(event.micro_offset, 0, "swing must not bake into offsets");
            }
        }

        // Combinator calls are not steps: `swing(...)` cannot appear inside
        // a sequence, so nested swing must be constructed in code.
        assert!(parse("a swing(0.2, b)").is_err());
        let term = PatternExpr::Seq(vec![Step::plain(Element::Name {
            binding: "a".into(),
            variant: None,
        })]);
        let nested = PatternExpr::Gain {
            linear: 1.0,
            inner: Box::new(PatternExpr::Swing {
                amount: 0.2,
                inner: Box::new(term),
            }),
        };
        let table = bindings(&["a"]);
        let result = eval_steps(
            &nested,
            &EvalContext {
                bindings: &table,
                cycle: BeatDuration(CYCLE_4_BEATS),
                seed: 0,
                cycle_index: 0,
            },
        );
        assert_eq!(result.unwrap_err(), PatternEvalError::NestedSwing);
    }

    #[test]
    fn unbound_names_and_collisions_are_typed_errors() {
        assert_eq!(
            eval("mystery", &["pen"], CYCLE_4_BEATS, 0).unwrap_err(),
            PatternEvalError::UnboundName("mystery".into())
        );
        // Variants form composite lookup keys.
        assert_eq!(
            eval("pen:2", &["pen"], CYCLE_4_BEATS, 0).unwrap_err(),
            PatternEvalError::UnboundName("pen:2".into())
        );
        assert!(matches!(
            eval("stack(pen, pen)", &["pen"], CYCLE_4_BEATS, 0).unwrap_err(),
            PatternEvalError::StepCollision { .. }
        ));
    }

    #[test]
    fn evaluation_is_deterministic_and_seed_free_today() {
        let source = "stack(e(5, 16, a), ~ b?0.3 ~ ~)";
        let first = eval(source, &["a", "b"], CYCLE_4_BEATS, 3).unwrap();
        let second = eval(source, &["a", "b"], CYCLE_4_BEATS, 3).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn widths_partition_exactly() {
        // a@3 b@1: a takes 3/4 of the cycle, b the last quarter.
        let output = eval("a@3 b", &["a", "b"], CYCLE_4_BEATS, 0).unwrap();
        assert_eq!(output.pattern.resolution, BeatDuration(960));
        assert_eq!(lane(&output, "a").steps.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(lane(&output, "b").steps.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(lane(&output, "a").steps[&0].gate, BeatDuration(2_880));
    }

    #[test]
    fn replicate_makes_siblings() {
        let output = eval("a!3 b", &["a", "b"], CYCLE_4_BEATS, 0).unwrap();
        assert_eq!(output.pattern.resolution, BeatDuration(960));
        assert_eq!(
            lane(&output, "a").steps.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            lane(&output, "b").steps.keys().copied().collect::<Vec<_>>(),
            vec![3]
        );
    }

    impl StepEvent {
        fn validate_for_test(&self) -> bool {
            self.velocity.is_finite()
                && (0.0..=1.0).contains(&self.velocity)
                && (0.0..=1.0).contains(&self.probability)
                && self.ratchets > 0
        }
    }
}
