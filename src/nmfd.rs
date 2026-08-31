//! Deterministic convolutional nonnegative component hypotheses.
//!
//! Nonnegative matrix factor deconvolution (NMFD), also called convolutional
//! NMF, models a frequency-by-time matrix as recurring, temporally extended
//! templates. A component is only a recurrence hypothesis: it may describe a
//! production gesture, a rhythm shared by several sources, ambience, or an
//! instrument. This module intentionally does not attach source labels.

use std::error::Error;
use std::fmt;

const EPSILON: f32 = 1.0e-12;

/// Controls deterministic convolutional nonnegative factorization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NmfdParams {
    /// Number of recurring component hypotheses.
    pub component_count: usize,
    /// Number of frames (lags) in each frequency-by-lag template.
    pub temporal_template_length: usize,
    /// Maximum number of alternating multiplicative-update iterations.
    pub iterations: usize,
    /// Seed for deterministic positive initialization.
    pub seed: u64,
    /// L1 penalty on activations after input peak normalization.
    pub activation_sparsity: f32,
    /// Quadratic penalty on differences between adjacent activations.
    pub activation_smoothness: f32,
    /// Stop when relative objective improvement is no larger than this value.
    /// A value of zero always runs all requested iterations.
    pub convergence_tolerance: f32,
}

impl Default for NmfdParams {
    fn default() -> Self {
        Self {
            component_count: 6,
            temporal_template_length: 8,
            iterations: 120,
            seed: 0x4e_4d_46_44_41_55_44,
            activation_sparsity: 0.001,
            activation_smoothness: 0.0,
            convergence_tolerance: 1.0e-6,
        }
    }
}

/// Result of convolutional factorization.
#[derive(Clone, Debug, PartialEq)]
pub struct NmfdResult {
    pub frequency_bins: usize,
    pub frames: usize,
    pub component_count: usize,
    pub temporal_template_length: usize,
    /// Frequency-by-lag templates in component-major row-major order.
    ///
    /// `templates[(component * frequency_bins + frequency)
    /// * temporal_template_length + lag]` is conventionally called `W`.
    /// Every non-silent component template is normalized to a peak of one.
    pub templates: Vec<f32>,
    /// Activations in component-by-frame row-major order, conventionally `H`.
    /// Values are returned in the input matrix's amplitude scale.
    pub activations: Vec<f32>,
    /// Reconstructed frequency-by-time matrix in input amplitude units.
    pub reconstruction: Vec<f32>,
    /// Penalized objectives, including the initialization at index zero.
    ///
    /// These values use peak-normalized input and are therefore comparable
    /// across simple changes of input gain. Values beyond the `f32` range are
    /// reported as `f32::MAX`; full-precision values still govern convergence.
    pub objective_history: Vec<f32>,
    /// Scale-independent Frobenius reconstruction error at the same points as
    /// [`Self::objective_history`].
    pub relative_error_history: Vec<f32>,
    pub iterations_run: usize,
    pub converged: bool,
    /// True when the input had no positive energy.
    pub silent: bool,
    /// True if restoring the input scale saturated any output at `f32::MAX`.
    /// At that numeric limit, the returned factors and reconstruction may no
    /// longer agree exactly because their mathematically required value is not
    /// representable as `f32`.
    pub output_saturated: bool,
}

impl NmfdResult {
    /// Returns one component's frequency-by-lag row-major template.
    pub fn template(&self, component: usize) -> Option<&[f32]> {
        if component >= self.component_count {
            return None;
        }
        let size = self
            .frequency_bins
            .checked_mul(self.temporal_template_length)?;
        let start = component.checked_mul(size)?;
        let end = start.checked_add(size)?;
        self.templates.get(start..end)
    }

    /// Returns one component's frame-by-frame activation.
    pub fn activation(&self, component: usize) -> Option<&[f32]> {
        if component >= self.component_count {
            return None;
        }
        let start = component.checked_mul(self.frames)?;
        let end = start.checked_add(self.frames)?;
        self.activations.get(start..end)
    }

    /// Final scale-independent Frobenius reconstruction error.
    pub fn relative_error(&self) -> f32 {
        self.relative_error_history.last().copied().unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NmfdError {
    EmptyDimensions,
    InvalidComponentCount,
    InvalidTemplateLength,
    TemplateLongerThanInput {
        template_length: usize,
        frames: usize,
    },
    FactorShapeOverflow,
    ZeroIterations,
    ShapeMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidMatrixValue {
        index: usize,
        value: f32,
    },
    InvalidSparsity(f32),
    InvalidSmoothness(f32),
    InvalidTolerance(f32),
}

impl fmt::Display for NmfdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDimensions => write!(formatter, "matrix dimensions must both be positive"),
            Self::InvalidComponentCount => write!(formatter, "component count must be positive"),
            Self::InvalidTemplateLength => write!(formatter, "template length must be positive"),
            Self::TemplateLongerThanInput {
                template_length,
                frames,
            } => write!(
                formatter,
                "template length {template_length} exceeds the input length of {frames} frames"
            ),
            Self::FactorShapeOverflow => write!(formatter, "requested factor shape is too large"),
            Self::ZeroIterations => write!(formatter, "iteration count must be positive"),
            Self::ShapeMismatch { expected, actual } => write!(
                formatter,
                "matrix shape requires {expected} values but received {actual}"
            ),
            Self::InvalidMatrixValue { index, value } => write!(
                formatter,
                "matrix value at index {index} must be finite and nonnegative, got {value}"
            ),
            Self::InvalidSparsity(value) => write!(
                formatter,
                "activation sparsity must be finite and nonnegative, got {value}"
            ),
            Self::InvalidSmoothness(value) => write!(
                formatter,
                "activation smoothness must be finite and nonnegative, got {value}"
            ),
            Self::InvalidTolerance(value) => write!(
                formatter,
                "convergence tolerance must be finite and nonnegative, got {value}"
            ),
        }
    }
}

impl Error for NmfdError {}

/// Factors a nonnegative `frequency_bins × frames` row-major matrix.
///
/// The causal model is
/// `V[frequency, frame] ~= sum(W[component, frequency, lag]
/// * H[component, frame - lag])` over valid nonnegative `frame - lag`.
/// Input should be linear magnitude or power, not decibels.
pub fn decompose_nonnegative(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    params: NmfdParams,
) -> Result<NmfdResult, NmfdError> {
    validate_input(matrix, frequency_bins, frames, params)?;

    let input_scale = matrix.iter().copied().fold(0.0_f32, f32::max);
    if input_scale == 0.0 {
        return Ok(silent_result(frequency_bins, frames, params));
    }

    let normalized: Vec<f32> = matrix.iter().map(|value| value / input_scale).collect();
    let mut generator = DeterministicRng::new(params.seed);
    let mut templates = initialize_templates(
        &normalized,
        frequency_bins,
        frames,
        params.component_count,
        params.temporal_template_length,
        &mut generator,
    );
    let mut activations = initialize_activations(
        &normalized,
        frequency_bins,
        frames,
        params.component_count,
        &mut generator,
    );

    let mut reconstruction = reconstruct(
        &templates,
        &activations,
        frequency_bins,
        frames,
        params.component_count,
        params.temporal_template_length,
    );
    let (initial_objective, initial_error) = metrics(
        &normalized,
        &reconstruction,
        &activations,
        frames,
        params.component_count,
        params.activation_sparsity,
        params.activation_smoothness,
    );
    let mut control_objective = initial_objective;
    let mut objective_history = vec![bounded_history_value(initial_objective)];
    let mut relative_error_history = vec![initial_error];
    let mut iterations_run = 0;
    let mut converged = false;

    for _ in 0..params.iterations {
        let old_templates = templates.clone();
        let old_activations = activations.clone();
        let previous_objective = control_objective;
        let previous_error = *relative_error_history.last().unwrap_or(&initial_error);

        update_activations(
            &normalized,
            &reconstruction,
            &templates,
            &mut activations,
            frequency_bins,
            frames,
            params.component_count,
            params.temporal_template_length,
            params.activation_sparsity,
            params.activation_smoothness,
        );
        reconstruction = reconstruct(
            &templates,
            &activations,
            frequency_bins,
            frames,
            params.component_count,
            params.temporal_template_length,
        );
        let (mut block_objective, mut block_error) = metrics(
            &normalized,
            &reconstruction,
            &activations,
            frames,
            params.component_count,
            params.activation_sparsity,
            params.activation_smoothness,
        );

        // Keep the previous finite point if a multiplicative activation step
        // is spoiled by floating-point roundoff. With tolerance zero we still
        // perform the requested number of iterations.
        if !block_objective.is_finite() || block_objective > previous_objective {
            activations = old_activations;
            reconstruction = reconstruct(
                &templates,
                &activations,
                frequency_bins,
                frames,
                params.component_count,
                params.temporal_template_length,
            );
            block_objective = previous_objective;
            block_error = previous_error;
        }

        update_templates(
            &normalized,
            &reconstruction,
            &mut templates,
            &activations,
            frequency_bins,
            frames,
            params.component_count,
            params.temporal_template_length,
        );
        reconstruction = reconstruct(
            &templates,
            &activations,
            frequency_bins,
            frames,
            params.component_count,
            params.temporal_template_length,
        );
        let (mut objective, mut relative_error) = metrics(
            &normalized,
            &reconstruction,
            &activations,
            frames,
            params.component_count,
            params.activation_sparsity,
            params.activation_smoothness,
        );

        // Peak normalization is a projected template update rather than an
        // unconstrained multiplicative step. If it is not an improvement,
        // retain the valid H-only point instead of discarding both blocks.
        if !objective.is_finite() || objective > block_objective {
            templates = old_templates;
            reconstruction = reconstruct(
                &templates,
                &activations,
                frequency_bins,
                frames,
                params.component_count,
                params.temporal_template_length,
            );
            objective = block_objective;
            relative_error = block_error;
        }

        control_objective = objective;
        objective_history.push(bounded_history_value(objective));
        relative_error_history.push(relative_error);
        iterations_run += 1;

        let relative_improvement = if previous_objective <= f64::from(EPSILON) {
            0.0
        } else {
            ((previous_objective - objective) / previous_objective).max(0.0)
        };
        if params.convergence_tolerance > 0.0
            && relative_improvement <= f64::from(params.convergence_tolerance)
        {
            converged = true;
            break;
        }
    }

    let output_saturated = activations
        .iter()
        .chain(&reconstruction)
        .any(|&value| f64::from(value) * f64::from(input_scale) > f64::from(f32::MAX));
    for value in &mut activations {
        *value = finite_product(*value, input_scale);
    }
    for value in &mut reconstruction {
        *value = finite_product(*value, input_scale);
    }

    Ok(NmfdResult {
        frequency_bins,
        frames,
        component_count: params.component_count,
        temporal_template_length: params.temporal_template_length,
        templates,
        activations,
        reconstruction,
        objective_history,
        relative_error_history,
        iterations_run,
        converged,
        silent: false,
        output_saturated,
    })
}

fn validate_input(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    params: NmfdParams,
) -> Result<(), NmfdError> {
    if frequency_bins == 0 || frames == 0 {
        return Err(NmfdError::EmptyDimensions);
    }
    if params.component_count == 0 {
        return Err(NmfdError::InvalidComponentCount);
    }
    if params.temporal_template_length == 0 {
        return Err(NmfdError::InvalidTemplateLength);
    }
    if params.temporal_template_length > frames {
        return Err(NmfdError::TemplateLongerThanInput {
            template_length: params.temporal_template_length,
            frames,
        });
    }
    if params.iterations == 0 {
        return Err(NmfdError::ZeroIterations);
    }
    if !params.activation_sparsity.is_finite() || params.activation_sparsity < 0.0 {
        return Err(NmfdError::InvalidSparsity(params.activation_sparsity));
    }
    if !params.activation_smoothness.is_finite() || params.activation_smoothness < 0.0 {
        return Err(NmfdError::InvalidSmoothness(params.activation_smoothness));
    }
    if !params.convergence_tolerance.is_finite() || params.convergence_tolerance < 0.0 {
        return Err(NmfdError::InvalidTolerance(params.convergence_tolerance));
    }
    let expected = frequency_bins
        .checked_mul(frames)
        .ok_or(NmfdError::ShapeMismatch {
            expected: usize::MAX,
            actual: matrix.len(),
        })?;
    if matrix.len() != expected {
        return Err(NmfdError::ShapeMismatch {
            expected,
            actual: matrix.len(),
        });
    }
    let template_values = params
        .component_count
        .checked_mul(frequency_bins)
        .and_then(|size| size.checked_mul(params.temporal_template_length))
        .ok_or(NmfdError::FactorShapeOverflow)?;
    let activation_values = params
        .component_count
        .checked_mul(frames)
        .ok_or(NmfdError::FactorShapeOverflow)?;
    let maximum_vec_len = (isize::MAX as usize) / std::mem::size_of::<f32>();
    if template_values > maximum_vec_len || activation_values > maximum_vec_len {
        return Err(NmfdError::FactorShapeOverflow);
    }
    for (index, &value) in matrix.iter().enumerate() {
        if !value.is_finite() || value < 0.0 {
            return Err(NmfdError::InvalidMatrixValue { index, value });
        }
    }
    Ok(())
}

fn silent_result(frequency_bins: usize, frames: usize, params: NmfdParams) -> NmfdResult {
    NmfdResult {
        frequency_bins,
        frames,
        component_count: params.component_count,
        temporal_template_length: params.temporal_template_length,
        templates: vec![
            0.0;
            params.component_count * frequency_bins * params.temporal_template_length
        ],
        activations: vec![0.0; params.component_count * frames],
        reconstruction: vec![0.0; frequency_bins * frames],
        objective_history: vec![0.0],
        relative_error_history: vec![0.0],
        iterations_run: 0,
        converged: true,
        silent: true,
        output_saturated: false,
    }
}

fn finite_product(left: f32, right: f32) -> f32 {
    (f64::from(left) * f64::from(right)).min(f64::from(f32::MAX)) as f32
}

fn bounded_history_value(value: f64) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, f64::from(f32::MAX)) as f32
    } else {
        f32::MAX
    }
}

#[allow(clippy::too_many_arguments)]
fn initialize_templates(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    component_count: usize,
    template_length: usize,
    generator: &mut DeterministicRng,
) -> Vec<f32> {
    let template_size = frequency_bins * template_length;
    let mut templates = vec![0.0; component_count * template_size];
    let start_count = frames - template_length + 1;

    // Data-informed patches converge much faster than unstructured random W,
    // while a positive random floor prevents zero-locking on sparse inputs.
    for component in 0..component_count {
        let start = generator.next_usize(start_count);
        let offset = component * template_size;
        let mut maximum = 0.0_f32;
        for frequency in 0..frequency_bins {
            for lag in 0..template_length {
                let observed = matrix[frequency * frames + start + lag];
                let value = observed + 0.01 * (0.25 + generator.next_unit_f32());
                templates[offset + frequency * template_length + lag] = value;
                maximum = maximum.max(value);
            }
        }
        for value in &mut templates[offset..offset + template_size] {
            *value /= maximum.max(EPSILON);
        }
    }
    templates
}

fn initialize_activations(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    component_count: usize,
    generator: &mut DeterministicRng,
) -> Vec<f32> {
    let mut activations = vec![0.0; component_count * frames];
    for frame in 0..frames {
        let mass: f32 = (0..frequency_bins)
            .map(|frequency| matrix[frequency * frames + frame])
            .sum();
        let baseline = (mass / component_count as f32).max(1.0e-4);
        for component in 0..component_count {
            activations[component * frames + frame] = baseline * (0.5 + generator.next_unit_f32());
        }
    }
    activations
}

#[allow(clippy::too_many_arguments)]
fn reconstruct(
    templates: &[f32],
    activations: &[f32],
    frequency_bins: usize,
    frames: usize,
    component_count: usize,
    template_length: usize,
) -> Vec<f32> {
    let template_size = frequency_bins * template_length;
    let mut output = vec![0.0; frequency_bins * frames];
    for component in 0..component_count {
        let template_offset = component * template_size;
        let activation_offset = component * frames;
        for frequency in 0..frequency_bins {
            let output_row = &mut output[frequency * frames..(frequency + 1) * frames];
            let template_row =
                &templates[template_offset + frequency * template_length..][..template_length];
            for lag in 0..template_length {
                let weight = template_row[lag];
                for frame in lag..frames {
                    output_row[frame] += weight * activations[activation_offset + frame - lag];
                }
            }
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn update_activations(
    matrix: &[f32],
    reconstruction: &[f32],
    templates: &[f32],
    activations: &mut [f32],
    frequency_bins: usize,
    frames: usize,
    component_count: usize,
    template_length: usize,
    sparsity: f32,
    smoothness: f32,
) {
    let old = activations.to_vec();
    let template_size = frequency_bins * template_length;
    for component in 0..component_count {
        let template_offset = component * template_size;
        for activation_frame in 0..frames {
            let mut numerator = 0.0;
            let mut denominator = sparsity;
            let valid_lags = template_length.min(frames - activation_frame);
            for frequency in 0..frequency_bins {
                let template_row =
                    &templates[template_offset + frequency * template_length..][..template_length];
                let matrix_row = &matrix[frequency * frames..(frequency + 1) * frames];
                let reconstruction_row =
                    &reconstruction[frequency * frames..(frequency + 1) * frames];
                for lag in 0..valid_lags {
                    let output_frame = activation_frame + lag;
                    let weight = template_row[lag];
                    numerator += weight * matrix_row[output_frame];
                    denominator += weight * reconstruction_row[output_frame];
                }
            }

            if smoothness > 0.0 && frames > 1 {
                let mut neighbors = 0.0;
                let mut degree = 0.0;
                if activation_frame > 0 {
                    neighbors += old[component * frames + activation_frame - 1];
                    degree += 1.0;
                }
                if activation_frame + 1 < frames {
                    neighbors += old[component * frames + activation_frame + 1];
                    degree += 1.0;
                }
                numerator += smoothness * neighbors;
                denominator += smoothness * degree * old[component * frames + activation_frame];
            }

            let index = component * frames + activation_frame;
            activations[index] = (old[index] * numerator / denominator.max(EPSILON)).max(EPSILON);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_templates(
    matrix: &[f32],
    reconstruction: &[f32],
    templates: &mut [f32],
    activations: &[f32],
    frequency_bins: usize,
    frames: usize,
    component_count: usize,
    template_length: usize,
) {
    let old = templates.to_vec();
    let template_size = frequency_bins * template_length;
    for component in 0..component_count {
        let template_offset = component * template_size;
        let activation_row = &activations[component * frames..(component + 1) * frames];
        for frequency in 0..frequency_bins {
            let matrix_row = &matrix[frequency * frames..(frequency + 1) * frames];
            let reconstruction_row = &reconstruction[frequency * frames..(frequency + 1) * frames];
            for lag in 0..template_length {
                let mut numerator = 0.0;
                let mut denominator = 0.0;
                for output_frame in lag..frames {
                    let activation = activation_row[output_frame - lag];
                    numerator += matrix_row[output_frame] * activation;
                    denominator += reconstruction_row[output_frame] * activation;
                }
                let index = template_offset + frequency * template_length + lag;
                templates[index] = (old[index] * numerator / denominator.max(EPSILON)).max(EPSILON);
            }
        }
    }
    normalize_templates(templates, frequency_bins, component_count, template_length);
}

fn normalize_templates(
    templates: &mut [f32],
    frequency_bins: usize,
    component_count: usize,
    template_length: usize,
) {
    let template_size = frequency_bins * template_length;
    for component in 0..component_count {
        let template = &mut templates[component * template_size..(component + 1) * template_size];
        let maximum = template.iter().copied().fold(0.0_f32, f32::max);
        if maximum > 0.0 && maximum.is_finite() {
            for value in template {
                *value /= maximum;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn metrics(
    matrix: &[f32],
    reconstruction: &[f32],
    activations: &[f32],
    frames: usize,
    component_count: usize,
    sparsity: f32,
    smoothness: f32,
) -> (f64, f32) {
    let mut squared_error = 0.0_f64;
    let mut squared_input = 0.0_f64;
    for (&observed, &estimated) in matrix.iter().zip(reconstruction) {
        let residual = f64::from(observed) - f64::from(estimated);
        squared_error += residual * residual;
        squared_input += f64::from(observed) * f64::from(observed);
    }
    let l1: f64 = activations.iter().map(|&value| f64::from(value)).sum();
    let mut differences = 0.0_f64;
    for component in 0..component_count {
        let row = &activations[component * frames..(component + 1) * frames];
        for pair in row.windows(2) {
            let difference = f64::from(pair[1]) - f64::from(pair[0]);
            differences += difference * difference;
        }
    }
    let objective =
        0.5 * squared_error + f64::from(sparsity) * l1 + 0.5 * f64::from(smoothness) * differences;
    let relative_error = if squared_input == 0.0 {
        0.0
    } else {
        (squared_error / squared_input).sqrt()
    };
    (objective, relative_error.min(f64::from(f32::MAX)) as f32)
}

#[derive(Clone, Copy, Debug)]
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_unit_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1_u32 << 24) as f32)
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        debug_assert!(upper_bound > 0);
        (self.next_u64() % upper_bound as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params(component_count: usize, template_length: usize) -> NmfdParams {
        NmfdParams {
            component_count,
            temporal_template_length: template_length,
            iterations: 800,
            seed: 0xaced,
            activation_sparsity: 0.0001,
            activation_smoothness: 0.0,
            convergence_tolerance: 0.0,
        }
    }

    fn synthesize(
        templates: &[f32],
        activations: &[f32],
        frequency_bins: usize,
        frames: usize,
        component_count: usize,
        template_length: usize,
    ) -> Vec<f32> {
        reconstruct(
            templates,
            activations,
            frequency_bins,
            frames,
            component_count,
            template_length,
        )
    }

    fn cosine(left: &[f32], right: &[f32]) -> f32 {
        let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
        let left_energy: f32 = left.iter().map(|value| value * value).sum();
        let right_energy: f32 = right.iter().map(|value| value * value).sum();
        dot / (left_energy * right_energy).sqrt().max(EPSILON)
    }

    #[test]
    fn recovers_two_recurring_temporally_extended_hypotheses() {
        let frequency_bins = 7;
        let frames = 72;
        let component_count = 2;
        let template_length = 4;
        let mut expected_templates = vec![0.0; component_count * frequency_bins * template_length];
        let index = |component, frequency, lag| {
            (component * frequency_bins + frequency) * template_length + lag
        };
        expected_templates[index(0, 0, 0)] = 0.36;
        expected_templates[index(0, 1, 0)] = 0.14;
        expected_templates[index(0, 2, 1)] = 0.28;
        expected_templates[index(0, 3, 2)] = 0.15;
        expected_templates[index(0, 4, 3)] = 0.07;
        expected_templates[index(1, 6, 0)] = 0.32;
        expected_templates[index(1, 5, 1)] = 0.28;
        expected_templates[index(1, 4, 2)] = 0.22;
        expected_templates[index(1, 2, 3)] = 0.18;

        let mut expected_activations = vec![0.0; component_count * frames];
        for (event, amplitude) in [2, 18, 34, 50, 66]
            .into_iter()
            .zip([1.0, 0.8, 1.15, 0.9, 0.7])
        {
            expected_activations[event] = amplitude;
        }
        for (event, amplitude) in [10, 26, 42, 58].into_iter().zip([0.9, 1.2, 0.75, 1.05]) {
            expected_activations[frames + event] = amplitude;
        }
        let matrix = synthesize(
            &expected_templates,
            &expected_activations,
            frequency_bins,
            frames,
            component_count,
            template_length,
        );

        let result = decompose_nonnegative(
            &matrix,
            frequency_bins,
            frames,
            test_params(component_count, template_length),
        )
        .unwrap();

        // Silence has relative error one for this nonzero signal. The learned
        // recurrent model must explain nearly all of both extended patterns.
        assert!(result.relative_error() < 0.12, "{result:#?}");
        assert!(result.relative_error() < 0.25 * 1.0);
        for expected_component in 0..component_count {
            let expected = &expected_templates[expected_component * frequency_bins * template_length
                ..(expected_component + 1) * frequency_bins * template_length];
            let best_match = (0..component_count)
                .map(|component| cosine(expected, result.template(component).unwrap()))
                .fold(0.0_f32, f32::max);
            assert!(best_match > 0.80, "template cosine was {best_match}");
        }
        for component in 0..component_count {
            let peak = result
                .template(component)
                .unwrap()
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            assert!((peak - 1.0).abs() < 1.0e-6);
        }
        assert!(result
            .objective_history
            .windows(2)
            .all(|pair| pair[1] <= pair[0] * (1.0 + 3.0e-6)));
    }

    #[test]
    fn is_exactly_deterministic_for_a_seed() {
        let matrix: Vec<f32> = (0..8 * 31)
            .map(|index| ((index * 19 + index / 7 + 3) % 29) as f32 / 29.0)
            .collect();
        let params = NmfdParams {
            iterations: 35,
            convergence_tolerance: 0.0,
            ..test_params(3, 5)
        };
        let first = decompose_nonnegative(&matrix, 8, 31, params).unwrap();
        let second = decompose_nonnegative(&matrix, 8, 31, params).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.objective_history.len(), params.iterations + 1);

        let mut malformed = first.clone();
        malformed.templates.clear();
        malformed.activations.clear();
        assert_eq!(malformed.template(0), None);
        assert_eq!(malformed.activation(0), None);
    }

    #[test]
    fn causal_convolution_has_hand_checked_boundaries() {
        // One component, two frequency rows, three causal lags.
        let templates = [1.0, 0.5, 0.25, 0.0, 2.0, 0.0];
        let activations = [3.0, 4.0, 0.0, 0.0, 0.0];
        assert_eq!(
            reconstruct(&templates, &activations, 2, 5, 1, 3),
            vec![3.0, 5.5, 2.75, 1.0, 0.0, 0.0, 6.0, 8.0, 0.0, 0.0]
        );
    }

    #[test]
    fn silence_and_degenerate_rows_remain_finite() {
        let silence = decompose_nonnegative(&[0.0; 30], 5, 6, test_params(2, 3)).unwrap();
        assert!(silence.silent);
        assert!(silence.converged);
        assert_eq!(silence.iterations_run, 0);
        assert!(silence
            .templates
            .iter()
            .chain(&silence.activations)
            .chain(&silence.reconstruction)
            .chain(&silence.objective_history)
            .all(|value| value.is_finite() && *value == 0.0));

        let mut degenerate = vec![0.0; 4 * 12];
        degenerate[2 * 12 + 5] = f32::MIN_POSITIVE;
        let result = decompose_nonnegative(&degenerate, 4, 12, test_params(2, 3)).unwrap();
        assert!(!result.silent);
        assert!(result
            .templates
            .iter()
            .chain(&result.activations)
            .chain(&result.reconstruction)
            .chain(&result.objective_history)
            .chain(&result.relative_error_history)
            .all(|value| value.is_finite() && *value >= 0.0));
    }

    #[test]
    fn smoothness_reduces_activation_roughness() {
        let frequency_bins = 3;
        let frames = 24;
        let matrix: Vec<f32> = (0..frequency_bins * frames)
            .map(|index| ((index * 11 + 5) % 17) as f32 / 17.0)
            .collect();
        let unsmoothed = decompose_nonnegative(
            &matrix,
            frequency_bins,
            frames,
            NmfdParams {
                iterations: 120,
                activation_sparsity: 0.0,
                activation_smoothness: 0.0,
                ..test_params(2, 3)
            },
        )
        .unwrap();
        let smoothed = decompose_nonnegative(
            &matrix,
            frequency_bins,
            frames,
            NmfdParams {
                iterations: 120,
                activation_sparsity: 0.0,
                activation_smoothness: 0.5,
                ..test_params(2, 3)
            },
        )
        .unwrap();
        let roughness = |values: &[f32]| -> f32 {
            values
                .chunks(frames)
                .flat_map(|row| row.windows(2))
                .map(|pair| (pair[1] - pair[0]).powi(2))
                .sum()
        };
        assert!(roughness(&smoothed.activations) < roughness(&unsmoothed.activations));
    }

    #[test]
    fn sparsity_is_not_undone_by_template_normalization() {
        // With peak-normalized W=[1,1] and V=[1,1], the nonnegative optimum is
        // H=1 without regularization and H=1-lambda/2 with an L1 penalty.
        let matrix = [1.0, 1.0];
        let dense = decompose_nonnegative(
            &matrix,
            2,
            1,
            NmfdParams {
                iterations: 300,
                activation_sparsity: 0.0,
                ..test_params(1, 1)
            },
        )
        .unwrap();
        let sparse = decompose_nonnegative(
            &matrix,
            2,
            1,
            NmfdParams {
                iterations: 300,
                activation_sparsity: 0.2,
                ..test_params(1, 1)
            },
        )
        .unwrap();
        assert!((dense.activations[0] - 1.0).abs() < 1.0e-3);
        assert!((sparse.activations[0] - 0.9).abs() < 1.0e-3);
        assert!(sparse.activations[0] < dense.activations[0]);
    }

    #[test]
    fn extreme_finite_values_remain_finite_and_zero_tolerance_runs_all_iterations() {
        let params = NmfdParams {
            iterations: 7,
            activation_sparsity: f32::MAX,
            activation_smoothness: f32::MAX,
            convergence_tolerance: 0.0,
            ..test_params(1, 1)
        };
        let result = decompose_nonnegative(&[f32::MAX, f32::MAX], 2, 1, params).unwrap();
        assert_eq!(result.iterations_run, params.iterations);
        assert_eq!(result.objective_history.len(), params.iterations + 1);
        assert_eq!(finite_product(2.0, f32::MAX), f32::MAX);
        assert!(result
            .templates
            .iter()
            .chain(&result.activations)
            .chain(&result.reconstruction)
            .chain(&result.objective_history)
            .chain(&result.relative_error_history)
            .all(|value| value.is_finite() && *value >= 0.0));
    }

    #[test]
    fn rejects_invalid_configuration_and_matrix_values() {
        assert_eq!(
            decompose_nonnegative(&[1.0], 0, 1, test_params(1, 1)),
            Err(NmfdError::EmptyDimensions)
        );
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 1, test_params(0, 1)),
            Err(NmfdError::InvalidComponentCount)
        ));
        assert!(matches!(
            decompose_nonnegative(&[1.0, 1.0], 1, 2, test_params(1, 3)),
            Err(NmfdError::TemplateLongerThanInput { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 2, test_params(1, 1)),
            Err(NmfdError::ShapeMismatch { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[f32::NAN], 1, 1, test_params(1, 1)),
            Err(NmfdError::InvalidMatrixValue { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[-0.1], 1, 1, test_params(1, 1)),
            Err(NmfdError::InvalidMatrixValue { .. })
        ));

        let mut invalid = test_params(1, 1);
        invalid.iterations = 0;
        assert_eq!(
            decompose_nonnegative(&[1.0], 1, 1, invalid),
            Err(NmfdError::ZeroIterations)
        );
        invalid = test_params(1, 1);
        invalid.activation_sparsity = f32::INFINITY;
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 1, invalid),
            Err(NmfdError::InvalidSparsity(_))
        ));
        invalid = test_params(1, 1);
        invalid.activation_smoothness = -0.1;
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 1, invalid),
            Err(NmfdError::InvalidSmoothness(_))
        ));
        invalid = test_params(1, 1);
        invalid.convergence_tolerance = f32::NAN;
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 1, invalid),
            Err(NmfdError::InvalidTolerance(_))
        ));
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 1, test_params(usize::MAX, 1)),
            Err(NmfdError::FactorShapeOverflow)
        ));
    }
}
