//! Deterministic nonnegative component hypotheses for time-frequency matrices.
//!
//! This module deliberately stops short of naming instruments. Nonnegative
//! matrix factorization (NMF) finds recurring spectral shapes and their
//! activity over time; those shapes may correspond to an instrument, a room
//! tail, a production gesture, or several sounds that tend to occur together.
//! Consumers should therefore present them as editable hypotheses.

use std::error::Error;
use std::fmt;

const DEFAULT_EPSILON: f32 = 1.0e-9;
const ERROR_CHECK_INTERVAL: usize = 5;

/// Controls the deterministic NMF solver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecompositionParams {
    /// Number of recurring component hypotheses to produce.
    pub rank: usize,
    /// Maximum number of alternating multiplicative-update iterations.
    pub iterations: usize,
    /// L1 penalty applied to temporal activations in scale-normalized space.
    /// `0.0` disables the penalty; values around `0.001..0.05` are useful.
    pub activation_sparsity: f32,
    /// Seed for deterministic positive initialization.
    pub seed: u64,
    /// Stop after successive checked relative errors improve by less than this.
    /// `0.0` always runs all requested iterations.
    pub convergence_tolerance: f32,
}

impl Default for DecompositionParams {
    fn default() -> Self {
        Self {
            rank: 6,
            iterations: 80,
            activation_sparsity: 0.002,
            seed: 0x41_55_44_45_43,
            convergence_tolerance: 1.0e-5,
        }
    }
}

/// One rank-one spectral/time hypothesis.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentHypothesis {
    /// L1-normalized spectrum, one value per input frequency bin.
    pub spectral_template: Vec<f32>,
    /// Nonnegative activity, one value per input frame, in input amplitude units.
    pub activation: Vec<f32>,
    /// Fraction of the summed independent component energies attributable here.
    /// Component energies overlap, so this is a ranking aid rather than a stem mix.
    pub energy_share: f32,
    /// `1 - max cosine similarity` to another returned template.
    pub spectral_distinctness: f32,
    /// Heuristic confidence combining global fit, energy share, and distinctness.
    pub confidence: f32,
}

/// Result of decomposing a frequency-by-time nonnegative matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentDecomposition {
    pub frequency_bins: usize,
    pub frames: usize,
    pub components: Vec<ComponentHypothesis>,
    pub iterations_run: usize,
    /// Root-mean-square reconstruction error, in input amplitude units.
    pub reconstruction_rmse: f32,
    /// Frobenius reconstruction error divided by input Frobenius norm.
    pub relative_error: f32,
    /// Energy explained by the reconstruction, clamped to `[0, 1]`.
    pub explained_energy: f32,
    /// Conservative overall hypothesis confidence based on fit and separation.
    pub confidence: f32,
    /// True when the input contained no positive energy.
    pub silent: bool,
}

impl ComponentDecomposition {
    /// Reconstructs the frequency-by-time matrix in row-major order.
    pub fn reconstruct(&self) -> Vec<f32> {
        let mut reconstructed = vec![0.0; self.frequency_bins * self.frames];
        for component in &self.components {
            for frequency in 0..self.frequency_bins {
                let spectral = component.spectral_template[frequency];
                let row =
                    &mut reconstructed[frequency * self.frames..(frequency + 1) * self.frames];
                for (sample, activation) in row.iter_mut().zip(&component.activation) {
                    *sample += spectral * activation;
                }
            }
        }
        reconstructed
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecompositionError {
    EmptyDimensions,
    InvalidRank,
    RankExceedsDimensions { rank: usize, maximum: usize },
    ZeroIterations,
    ShapeMismatch { expected: usize, actual: usize },
    InvalidMatrixValue { index: usize, value: f32 },
    InvalidSparsity(f32),
    InvalidTolerance(f32),
}

impl fmt::Display for DecompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDimensions => write!(formatter, "matrix dimensions must both be positive"),
            Self::InvalidRank => write!(formatter, "decomposition rank must be positive"),
            Self::RankExceedsDimensions { rank, maximum } => write!(
                formatter,
                "decomposition rank {rank} exceeds the matrix's maximum useful rank {maximum}"
            ),
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
            Self::InvalidTolerance(value) => write!(
                formatter,
                "convergence tolerance must be finite and nonnegative, got {value}"
            ),
        }
    }
}

impl Error for DecompositionError {}

/// Decomposes a nonnegative `frequency_bins × frames` row-major matrix.
///
/// Input should be a linear magnitude or power representation, never raw dB.
/// The returned templates are normalized to sum to one. Their removed scale is
/// transferred into activations, so [`ComponentDecomposition::reconstruct`]
/// remains in the same units as `matrix`.
pub fn decompose_nonnegative(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    params: DecompositionParams,
) -> Result<ComponentDecomposition, DecompositionError> {
    validate_input(matrix, frequency_bins, frames, params)?;

    let maximum = matrix.iter().copied().fold(0.0_f32, f32::max);
    if maximum == 0.0 {
        return Ok(silent_decomposition(frequency_bins, frames, params.rank));
    }

    // Scale invariance keeps the sparsity coefficient and numerical epsilon
    // meaningful for both normalized spectrograms and large power spectra.
    let input_scale = maximum;
    let normalized: Vec<f32> = matrix.iter().map(|value| value / input_scale).collect();
    let mut generator = DeterministicRng::new(params.seed);
    let mut templates = initialize_templates(frequency_bins, params.rank, &mut generator);
    let mut activations = initialize_activations(
        &normalized,
        frequency_bins,
        frames,
        params.rank,
        &mut generator,
    );

    let mut numerator_h = vec![0.0; params.rank * frames];
    let mut gram_w = vec![0.0; params.rank * params.rank];
    let mut numerator_w = vec![0.0; frequency_bins * params.rank];
    let mut gram_h = vec![0.0; params.rank * params.rank];
    let mut previous_error = f32::INFINITY;
    let mut iterations_run = 0;

    for iteration in 0..params.iterations {
        update_activations(
            &normalized,
            &templates,
            &mut activations,
            frequency_bins,
            frames,
            params.rank,
            params.activation_sparsity,
            &mut numerator_h,
            &mut gram_w,
        );
        update_templates(
            &normalized,
            &mut templates,
            &mut activations,
            frequency_bins,
            frames,
            params.rank,
            &mut numerator_w,
            &mut gram_h,
        );
        iterations_run = iteration + 1;

        let should_check =
            iterations_run % ERROR_CHECK_INTERVAL == 0 || iterations_run == params.iterations;
        if should_check && params.convergence_tolerance > 0.0 {
            let error = relative_error(
                &normalized,
                &templates,
                &activations,
                frequency_bins,
                frames,
                params.rank,
            );
            let improvement = previous_error - error;
            if previous_error.is_finite()
                && improvement >= 0.0
                && improvement < params.convergence_tolerance
            {
                break;
            }
            previous_error = error;
        }
    }

    // Initialization and updates use normalized input. Transfer the scale back
    // to H after optimization; W is already L1 normalized.
    for value in &mut activations {
        *value *= input_scale;
    }

    Ok(build_result(
        matrix,
        templates,
        activations,
        frequency_bins,
        frames,
        params.rank,
        iterations_run,
    ))
}

fn validate_input(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    params: DecompositionParams,
) -> Result<(), DecompositionError> {
    if frequency_bins == 0 || frames == 0 {
        return Err(DecompositionError::EmptyDimensions);
    }
    if params.rank == 0 {
        return Err(DecompositionError::InvalidRank);
    }
    let maximum_rank = frequency_bins.min(frames);
    if params.rank > maximum_rank {
        return Err(DecompositionError::RankExceedsDimensions {
            rank: params.rank,
            maximum: maximum_rank,
        });
    }
    if params.iterations == 0 {
        return Err(DecompositionError::ZeroIterations);
    }
    if !params.activation_sparsity.is_finite() || params.activation_sparsity < 0.0 {
        return Err(DecompositionError::InvalidSparsity(
            params.activation_sparsity,
        ));
    }
    if !params.convergence_tolerance.is_finite() || params.convergence_tolerance < 0.0 {
        return Err(DecompositionError::InvalidTolerance(
            params.convergence_tolerance,
        ));
    }
    let expected = frequency_bins
        .checked_mul(frames)
        .ok_or(DecompositionError::ShapeMismatch {
            expected: usize::MAX,
            actual: matrix.len(),
        })?;
    if matrix.len() != expected {
        return Err(DecompositionError::ShapeMismatch {
            expected,
            actual: matrix.len(),
        });
    }
    for (index, &value) in matrix.iter().enumerate() {
        if !value.is_finite() || value < 0.0 {
            return Err(DecompositionError::InvalidMatrixValue { index, value });
        }
    }
    Ok(())
}

fn silent_decomposition(
    frequency_bins: usize,
    frames: usize,
    rank: usize,
) -> ComponentDecomposition {
    let components = (0..rank)
        .map(|_| ComponentHypothesis {
            spectral_template: vec![0.0; frequency_bins],
            activation: vec![0.0; frames],
            energy_share: 0.0,
            spectral_distinctness: 0.0,
            confidence: 0.0,
        })
        .collect();
    ComponentDecomposition {
        frequency_bins,
        frames,
        components,
        iterations_run: 0,
        reconstruction_rmse: 0.0,
        relative_error: 0.0,
        explained_energy: 0.0,
        confidence: 0.0,
        silent: true,
    }
}

fn initialize_templates(
    frequency_bins: usize,
    rank: usize,
    generator: &mut DeterministicRng,
) -> Vec<f32> {
    let mut templates = vec![0.0; frequency_bins * rank];
    for component in 0..rank {
        let mut sum = 0.0;
        for frequency in 0..frequency_bins {
            let value = 0.5 + generator.next_unit_f32();
            templates[frequency * rank + component] = value;
            sum += value;
        }
        for frequency in 0..frequency_bins {
            templates[frequency * rank + component] /= sum;
        }
    }
    templates
}

fn initialize_activations(
    matrix: &[f32],
    frequency_bins: usize,
    frames: usize,
    rank: usize,
    generator: &mut DeterministicRng,
) -> Vec<f32> {
    let mut activations = vec![0.0; rank * frames];
    for frame in 0..frames {
        let spectral_mass: f32 = (0..frequency_bins)
            .map(|frequency| matrix[frequency * frames + frame])
            .sum();
        let baseline = (spectral_mass / rank as f32).max(DEFAULT_EPSILON);
        for component in 0..rank {
            activations[component * frames + frame] = baseline * (0.5 + generator.next_unit_f32());
        }
    }
    activations
}

#[allow(clippy::too_many_arguments)]
fn update_activations(
    matrix: &[f32],
    templates: &[f32],
    activations: &mut [f32],
    frequency_bins: usize,
    frames: usize,
    rank: usize,
    sparsity: f32,
    numerator: &mut [f32],
    gram: &mut [f32],
) {
    numerator.fill(0.0);
    gram.fill(0.0);

    // W^T V
    for frequency in 0..frequency_bins {
        let matrix_row = &matrix[frequency * frames..(frequency + 1) * frames];
        let template_row = &templates[frequency * rank..(frequency + 1) * rank];
        for component in 0..rank {
            let weight = template_row[component];
            let numerator_row = &mut numerator[component * frames..(component + 1) * frames];
            for frame in 0..frames {
                numerator_row[frame] += weight * matrix_row[frame];
            }
        }
    }

    // W^T W
    for frequency in 0..frequency_bins {
        let row = &templates[frequency * rank..(frequency + 1) * rank];
        for left in 0..rank {
            for right in 0..rank {
                gram[left * rank + right] += row[left] * row[right];
            }
        }
    }

    // Compute all ratios from the same H, then apply them together. Updating
    // H in place while computing denominators would create an unintended
    // order-dependent Gauss-Seidel variant of the multiplicative rule.
    for component in 0..rank {
        for frame in 0..frames {
            let mut denominator = sparsity;
            for other in 0..rank {
                denominator += gram[component * rank + other] * activations[other * frames + frame];
            }
            let index = component * frames + frame;
            numerator[index] /= denominator.max(DEFAULT_EPSILON);
        }
    }
    for (activation, ratio) in activations.iter_mut().zip(numerator.iter()) {
        *activation = (*activation * *ratio).max(DEFAULT_EPSILON);
    }
}

#[allow(clippy::too_many_arguments)]
fn update_templates(
    matrix: &[f32],
    templates: &mut [f32],
    activations: &mut [f32],
    frequency_bins: usize,
    frames: usize,
    rank: usize,
    numerator: &mut [f32],
    gram: &mut [f32],
) {
    numerator.fill(0.0);
    gram.fill(0.0);

    // V H^T
    for frequency in 0..frequency_bins {
        let matrix_row = &matrix[frequency * frames..(frequency + 1) * frames];
        for component in 0..rank {
            let activation_row = &activations[component * frames..(component + 1) * frames];
            let mut sum = 0.0;
            for frame in 0..frames {
                sum += matrix_row[frame] * activation_row[frame];
            }
            numerator[frequency * rank + component] = sum;
        }
    }

    // H H^T
    for left in 0..rank {
        let left_row = &activations[left * frames..(left + 1) * frames];
        for right in 0..rank {
            let right_row = &activations[right * frames..(right + 1) * frames];
            let mut sum = 0.0;
            for frame in 0..frames {
                sum += left_row[frame] * right_row[frame];
            }
            gram[left * rank + right] = sum;
        }
    }

    // As with H, compute all ratios before changing W.
    for frequency in 0..frequency_bins {
        for component in 0..rank {
            let mut denominator = 0.0;
            for other in 0..rank {
                denominator += templates[frequency * rank + other] * gram[other * rank + component];
            }
            let index = frequency * rank + component;
            numerator[index] /= denominator.max(DEFAULT_EPSILON);
        }
    }
    for (template, ratio) in templates.iter_mut().zip(numerator.iter()) {
        *template = (*template * *ratio).max(DEFAULT_EPSILON);
    }
    normalize_templates(templates, activations, frequency_bins, frames, rank);
}

fn normalize_templates(
    templates: &mut [f32],
    activations: &mut [f32],
    frequency_bins: usize,
    frames: usize,
    rank: usize,
) {
    for component in 0..rank {
        let sum: f32 = (0..frequency_bins)
            .map(|frequency| templates[frequency * rank + component])
            .sum();
        if sum > DEFAULT_EPSILON {
            for frequency in 0..frequency_bins {
                templates[frequency * rank + component] /= sum;
            }
            for activation in &mut activations[component * frames..(component + 1) * frames] {
                *activation *= sum;
            }
        }
    }
}

fn relative_error(
    matrix: &[f32],
    templates: &[f32],
    activations: &[f32],
    frequency_bins: usize,
    frames: usize,
    rank: usize,
) -> f32 {
    let mut squared_error = 0.0_f64;
    let mut squared_input = 0.0_f64;
    for frequency in 0..frequency_bins {
        for frame in 0..frames {
            let mut estimate = 0.0;
            for component in 0..rank {
                estimate += templates[frequency * rank + component]
                    * activations[component * frames + frame];
            }
            let observed = matrix[frequency * frames + frame];
            let residual = f64::from(observed - estimate);
            squared_error += residual * residual;
            squared_input += f64::from(observed) * f64::from(observed);
        }
    }
    if squared_input == 0.0 {
        0.0
    } else {
        (squared_error / squared_input).sqrt() as f32
    }
}

#[allow(clippy::too_many_arguments)]
fn build_result(
    matrix: &[f32],
    templates: Vec<f32>,
    activations: Vec<f32>,
    frequency_bins: usize,
    frames: usize,
    rank: usize,
    iterations_run: usize,
) -> ComponentDecomposition {
    let mut component_energies = vec![0.0_f64; rank];
    for component in 0..rank {
        let spectral_energy: f64 = (0..frequency_bins)
            .map(|frequency| {
                let value = f64::from(templates[frequency * rank + component]);
                value * value
            })
            .sum();
        let activation_energy: f64 = activations[component * frames..(component + 1) * frames]
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum();
        component_energies[component] = spectral_energy * activation_energy;
    }
    let summed_component_energy: f64 = component_energies.iter().sum();

    let relative_error = relative_error(
        matrix,
        &templates,
        &activations,
        frequency_bins,
        frames,
        rank,
    );
    let explained_energy = (1.0 - relative_error * relative_error).clamp(0.0, 1.0);
    let reconstruction_rmse = {
        let input_rms = (matrix
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            / matrix.len() as f64)
            .sqrt() as f32;
        relative_error * input_rms
    };

    let mut distinctness = vec![1.0_f32; rank];
    for component in 0..rank {
        let mut maximum_similarity = 0.0_f32;
        for other in 0..rank {
            if component != other {
                maximum_similarity = maximum_similarity.max(template_cosine(
                    &templates,
                    frequency_bins,
                    rank,
                    component,
                    other,
                ));
            }
        }
        distinctness[component] = if rank == 1 {
            1.0
        } else {
            (1.0 - maximum_similarity).clamp(0.0, 1.0)
        };
    }

    let fit = (1.0 - relative_error).clamp(0.0, 1.0);
    let mean_distinctness = distinctness.iter().sum::<f32>() / rank as f32;
    let overall_confidence = fit * (0.35 + 0.65 * mean_distinctness);
    let mut components: Vec<_> = (0..rank)
        .map(|component| {
            let energy_share = if summed_component_energy > 0.0 {
                (component_energies[component] / summed_component_energy) as f32
            } else {
                0.0
            };
            ComponentHypothesis {
                spectral_template: (0..frequency_bins)
                    .map(|frequency| templates[frequency * rank + component])
                    .collect(),
                activation: activations[component * frames..(component + 1) * frames].to_vec(),
                energy_share,
                spectral_distinctness: distinctness[component],
                confidence: fit * (0.25 + 0.75 * distinctness[component]) * energy_share.sqrt(),
            }
        })
        .collect();

    // Stable energy ordering gives the UI consistent component lanes without
    // changing reconstruction or pretending the ordering carries semantics.
    components.sort_by(|left, right| {
        right
            .energy_share
            .total_cmp(&left.energy_share)
            .then_with(|| {
                right
                    .spectral_distinctness
                    .total_cmp(&left.spectral_distinctness)
            })
    });

    ComponentDecomposition {
        frequency_bins,
        frames,
        components,
        iterations_run,
        reconstruction_rmse,
        relative_error,
        explained_energy,
        confidence: overall_confidence,
        silent: false,
    }
}

fn template_cosine(
    templates: &[f32],
    frequency_bins: usize,
    rank: usize,
    left: usize,
    right: usize,
) -> f32 {
    let mut dot = 0.0;
    let mut left_energy = 0.0;
    let mut right_energy = 0.0;
    for frequency in 0..frequency_bins {
        let left_value = templates[frequency * rank + left];
        let right_value = templates[frequency * rank + right];
        dot += left_value * right_value;
        left_energy += left_value * left_value;
        right_energy += right_value * right_value;
    }
    let denominator = (left_energy * right_energy).sqrt();
    if denominator <= DEFAULT_EPSILON {
        0.0
    } else {
        (dot / denominator).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug)]
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        // xorshift64* cannot advance from an all-zero state.
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_unit_f32(&mut self) -> f32 {
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        let random = value.wrapping_mul(0x2545_f491_4f6c_dd1d);
        // The leading 24 random bits map exactly into f32's useful mantissa.
        ((random >> 40) as f32) / ((1_u32 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params(rank: usize) -> DecompositionParams {
        DecompositionParams {
            rank,
            iterations: 400,
            activation_sparsity: 0.0,
            seed: 73,
            convergence_tolerance: 1.0e-7,
        }
    }

    fn cosine(left: &[f32], right: &[f32]) -> f32 {
        let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
        let left_energy: f32 = left.iter().map(|value| value * value).sum();
        let right_energy: f32 = right.iter().map(|value| value * value).sum();
        dot / (left_energy * right_energy).sqrt()
    }

    #[test]
    fn separates_synthetic_recurring_spectra() {
        let frequency_bins = 6;
        let frames = 30;
        let expected_templates = [
            vec![0.72, 0.20, 0.06, 0.02, 0.0, 0.0],
            vec![0.0, 0.0, 0.03, 0.10, 0.25, 0.62],
        ];
        let mut expected_activations = vec![vec![0.0; frames]; 2];
        for frame in 0..frames {
            if frame % 6 <= 2 {
                expected_activations[0][frame] = 0.4 + frame as f32 * 0.03;
            }
            if frame % 10 >= 5 {
                expected_activations[1][frame] = 1.2 - frame as f32 * 0.01;
            }
        }
        // Include pure-component anchor frames so the factorization is separable.
        expected_activations[0][0] = 1.5;
        expected_activations[1][0] = 0.0;
        expected_activations[0][5] = 0.0;
        expected_activations[1][5] = 1.5;

        let mut matrix = vec![0.0; frequency_bins * frames];
        for frequency in 0..frequency_bins {
            for frame in 0..frames {
                matrix[frequency * frames + frame] = expected_templates[0][frequency]
                    * expected_activations[0][frame]
                    + expected_templates[1][frequency] * expected_activations[1][frame];
            }
        }

        let result = decompose_nonnegative(
            &matrix,
            frequency_bins,
            frames,
            test_params(expected_templates.len()),
        )
        .unwrap();

        assert!(!result.silent);
        assert!(result.relative_error < 0.01, "{result:#?}");
        for expected in &expected_templates {
            let best_match = result
                .components
                .iter()
                .map(|component| cosine(expected, &component.spectral_template))
                .fold(0.0_f32, f32::max);
            assert!(best_match > 0.98, "best template cosine was {best_match}");
        }
        let reconstruction = result.reconstruct();
        assert_eq!(reconstruction.len(), matrix.len());
    }

    #[test]
    fn handles_silence_without_nan() {
        let result = decompose_nonnegative(&[0.0; 20], 4, 5, test_params(3)).unwrap();
        assert!(result.silent);
        assert_eq!(result.iterations_run, 0);
        assert_eq!(result.relative_error, 0.0);
        assert_eq!(result.confidence, 0.0);
        assert!(result.components.iter().all(|component| component
            .spectral_template
            .iter()
            .chain(&component.activation)
            .all(|value| value.is_finite() && *value == 0.0)));
    }

    #[test]
    fn handles_rank_one_and_zero_rows() {
        let frequency_bins = 4;
        let frames = 8;
        let mut matrix = vec![0.0; frequency_bins * frames];
        for frame in 0..frames {
            matrix[frames + frame] = 0.25 * (frame + 1) as f32;
            matrix[3 * frames + frame] = 0.75 * (frame + 1) as f32;
        }
        let result =
            decompose_nonnegative(&matrix, frequency_bins, frames, test_params(1)).unwrap();
        assert!(result.relative_error < 1.0e-4, "{result:#?}");
        assert!(result.components[0]
            .spectral_template
            .iter()
            .all(|value| value.is_finite()));
        assert!((result.components[0].spectral_template.iter().sum::<f32>() - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn is_exactly_deterministic_for_a_seed() {
        let matrix: Vec<f32> = (0..63)
            .map(|index| ((index * 17 + 3) % 19) as f32 / 19.0)
            .collect();
        let params = DecompositionParams {
            rank: 3,
            iterations: 35,
            activation_sparsity: 0.01,
            seed: 1_234_567,
            convergence_tolerance: 0.0,
        };
        let first = decompose_nonnegative(&matrix, 7, 9, params).unwrap();
        let second = decompose_nonnegative(&matrix, 7, 9, params).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_invalid_inputs() {
        assert_eq!(
            decompose_nonnegative(&[1.0], 0, 1, test_params(1)),
            Err(DecompositionError::EmptyDimensions)
        );
        assert!(matches!(
            decompose_nonnegative(&[1.0], 1, 2, test_params(1)),
            Err(DecompositionError::ShapeMismatch { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[1.0, 1.0], 1, 2, test_params(2)),
            Err(DecompositionError::RankExceedsDimensions { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[-0.1], 1, 1, test_params(1)),
            Err(DecompositionError::InvalidMatrixValue { .. })
        ));
        assert!(matches!(
            decompose_nonnegative(&[f32::NAN], 1, 1, test_params(1)),
            Err(DecompositionError::InvalidMatrixValue { .. })
        ));
    }
}
