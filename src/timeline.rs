//! Sample-coordinate viewport mechanics for arrangement and lens timelines.
//!
//! The viewport is deliberately independent from transport. `ensure_visible`
//! implements follow mode, while `pan_fraction` and `zoom_around` are local
//! navigation operations that callers can use to disengage follow.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineViewport {
    pub start_sample: u64,
    pub end_sample: u64,
    pub total_samples: u64,
    pub minimum_span: u64,
}

impl TimelineViewport {
    pub fn fit(total_samples: u64) -> Self {
        Self {
            start_sample: 0,
            end_sample: total_samples,
            total_samples,
            minimum_span: 1,
        }
    }

    pub fn around(total_samples: u64, center_sample: u64, span_samples: u64) -> Self {
        let mut viewport = Self::fit(total_samples);
        viewport.set_span_around(center_sample, span_samples);
        viewport
    }

    pub fn span(self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }

    pub fn is_fit(self) -> bool {
        self.start_sample == 0 && self.end_sample == self.total_samples
    }

    pub fn contains(self, sample: u64) -> bool {
        sample >= self.start_sample && sample <= self.end_sample
    }

    pub fn set_total_samples(&mut self, total_samples: u64) {
        self.total_samples = total_samples;
        let span = self.span().clamp(self.minimum_span, total_samples.max(1));
        self.start_sample = self.start_sample.min(total_samples.saturating_sub(span));
        self.end_sample = (self.start_sample + span).min(total_samples);
    }

    pub fn set_span_around(&mut self, center_sample: u64, span_samples: u64) {
        if self.total_samples == 0 {
            self.start_sample = 0;
            self.end_sample = 0;
            return;
        }
        let span = span_samples.clamp(self.minimum_span, self.total_samples);
        let center = center_sample.min(self.total_samples);
        let start = center
            .saturating_sub(span / 2)
            .min(self.total_samples - span);
        self.start_sample = start;
        self.end_sample = start + span;
    }

    pub fn sample_at_fraction(self, fraction: f64) -> u64 {
        if self.span() == 0 {
            return self.start_sample;
        }
        let offset = (fraction.clamp(0.0, 1.0) * self.span() as f64).round() as u64;
        (self.start_sample + offset).min(self.end_sample)
    }

    pub fn fraction_of(self, sample: u64) -> f32 {
        if self.span() == 0 {
            return 0.0;
        }
        ((sample.clamp(self.start_sample, self.end_sample) - self.start_sample) as f64
            / self.span() as f64) as f32
    }

    pub fn zoom_around(&mut self, anchor_sample: u64, scale: f64) {
        if self.total_samples == 0 || !scale.is_finite() || scale <= 0.0 {
            return;
        }
        let old_span = self.span().max(1);
        let new_span =
            ((old_span as f64 * scale).round() as u64).clamp(self.minimum_span, self.total_samples);
        let anchor = anchor_sample.clamp(self.start_sample, self.end_sample);
        let anchor_fraction = (anchor - self.start_sample) as f64 / old_span as f64;
        let desired_left = (anchor_fraction * new_span as f64).round() as u64;
        let start = anchor
            .saturating_sub(desired_left)
            .min(self.total_samples - new_span);
        self.start_sample = start;
        self.end_sample = start + new_span;
    }

    pub fn pan_fraction(&mut self, fraction_of_span: f64) {
        if self.is_fit() || !fraction_of_span.is_finite() {
            return;
        }
        let delta = (fraction_of_span * self.span() as f64).round() as i128;
        let maximum_start = self.total_samples.saturating_sub(self.span()) as i128;
        let start = (self.start_sample as i128 + delta).clamp(0, maximum_start) as u64;
        let span = self.span();
        self.start_sample = start;
        self.end_sample = start + span;
    }

    /// Move the viewport only when `sample` leaves its inner safe region.
    /// This gives transport follow a stable, page-like motion rather than
    /// continuously dragging the arrangement under the pointer.
    pub fn ensure_visible(&mut self, sample: u64, margin_fraction: f64) -> bool {
        if self.contains(sample) && self.is_fit() {
            return false;
        }
        let margin_fraction = margin_fraction.clamp(0.0, 0.49);
        let margin = (self.span() as f64 * margin_fraction).round() as u64;
        let safe_start = self.start_sample.saturating_add(margin);
        let safe_end = self.end_sample.saturating_sub(margin);
        if sample >= safe_start && sample <= safe_end {
            return false;
        }
        self.set_span_around(sample, self.span());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn around_clamps_at_material_edges() {
        assert_eq!(
            TimelineViewport::around(1_000, 20, 200),
            TimelineViewport {
                start_sample: 0,
                end_sample: 200,
                total_samples: 1_000,
                minimum_span: 1,
            }
        );
        let end = TimelineViewport::around(1_000, 980, 200);
        assert_eq!((end.start_sample, end.end_sample), (800, 1_000));
    }

    #[test]
    fn conversions_are_relative_to_the_visible_span() {
        let viewport = TimelineViewport::around(1_000, 500, 200);
        assert_eq!(viewport.sample_at_fraction(0.0), 400);
        assert_eq!(viewport.sample_at_fraction(0.5), 500);
        assert_eq!(viewport.sample_at_fraction(1.0), 600);
        assert!((viewport.fraction_of(550) - 0.75).abs() < 1.0e-6);
    }

    #[test]
    fn zoom_preserves_the_anchor_position() {
        let mut viewport = TimelineViewport::around(1_000, 500, 400);
        viewport.zoom_around(400, 0.5);
        assert_eq!((viewport.start_sample, viewport.end_sample), (350, 550));
        assert!((viewport.fraction_of(400) - 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn panning_is_local_and_clamped() {
        let mut viewport = TimelineViewport::around(1_000, 500, 200);
        viewport.pan_fraction(0.5);
        assert_eq!((viewport.start_sample, viewport.end_sample), (500, 700));
        viewport.pan_fraction(-10.0);
        assert_eq!((viewport.start_sample, viewport.end_sample), (0, 200));
    }

    #[test]
    fn follow_moves_only_outside_the_safe_region() {
        let mut viewport = TimelineViewport::around(1_000, 500, 200);
        assert!(!viewport.ensure_visible(450, 0.15));
        assert!(viewport.ensure_visible(590, 0.15));
        assert!(viewport.contains(590));
        assert_eq!(viewport.span(), 200);
    }

    #[test]
    fn empty_material_is_well_defined() {
        let mut viewport = TimelineViewport::around(0, 100, 200);
        viewport.zoom_around(10, 0.5);
        viewport.pan_fraction(1.0);
        assert_eq!(viewport.sample_at_fraction(0.5), 0);
        assert_eq!(viewport.fraction_of(10), 0.0);
    }
}
