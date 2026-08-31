//! Shared control-plane contract for audio initiated by workspace panes.
//!
//! A pane may either replace a project-frame-aligned signal inside the sole
//! project renderer, or request a finite clip on the independent preview bus.
//! It never owns an [`AudioHost`], transport, playhead, or loop.  Analytical
//! source/construction/residual signals use [`PaneTimelineEffect`]; browser
//! samples, pads, family medoids, and Loom templates use [`PreviewController`].
//! The latter assigns a total session-local generation so an obsolete worker
//! completion or pointer release cannot replace/stop a newer preview.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::audio::AudioFormat;
use crate::audio_host::{AudioHost, AudioHostError, AuditionClip};
use crate::live_project::LiveProjectSnapshot;
use crate::project_audio_controller::{
    AuditionAlignment, ProjectAudioController, ProjectAudioControllerError,
};
use crate::project_controller::{
    ConstructivePublication, ConstructivePublishedFocus, SampleActionOutcome,
};
use crate::render_plan::{RenderFormat, RenderSpan};
use crate::render_runtime::{
    canonical_pcm_digest, AuditionMix, AuditionOwner, AuditionSubject, TimelineAudition,
    TimelineAuditionId,
};
use crate::sample_actions::{
    resolve_sample_audition, SampleAction, SampleActionKind, SampleActionResult,
    SampleAuditionIntent, SamplePreviewClipRef, SamplePreviewCommand, SamplePreviewError,
    SamplePreviewToken, SamplePublishedResult, SampleResultFocus, SampleViewOutcome, SamplerTarget,
};
use crate::workspace_items::WorkspaceViewId;

pub const PANE_AUDITION_OWNER_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-paneaudio1");

/// Derive stable control-plane ownership from the persisted workspace view,
/// not from a transient GPUI entity or lens kind. Floating/docking therefore
/// preserves ownership while duplicate panes remain distinct.
pub fn workspace_audition_owner(view: WorkspaceViewId) -> Result<AuditionOwner, PaneAudioError> {
    if view.0 == 0 {
        return Err(PaneAudioError::ZeroWorkspaceView);
    }
    Ok(AuditionOwner {
        namespace: PANE_AUDITION_OWNER_NAMESPACE,
        local: view.0,
    })
}

// This is deliberately a child of the UI-neutral audio bridge instead of a
// GPUI test. It exercises the complete musician path with a headless instance
// of the same one-transport/one-preview-bus topology used by `AudioHost`.
#[cfg(test)]
#[path = "musician_gate.rs"]
mod musician_gate;

/// Semantic origin of an audible request from a pane.
///
/// This is intentionally smaller than the pane/view type hierarchy.  It is a
/// routing proof: each visible sound has exactly one of two playback classes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PaneAudioKind {
    HpssSource,
    HpssHarmonic,
    HpssTransient,
    HpssResidual,
    LoomSource,
    LoomConstruction,
    LoomResidual,
    ComparisonSource,
    ComparisonConstruction,
    ComparisonResidual,
    RhythmConstruction,
    RhythmFamilyMedoid,
    LoomTemplate,
    AssetOneShot,
    PadGate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneAudioRoute {
    /// Immutable PCM occupies an exact project span and replaces/overlays the
    /// active cohort without creating a transport.
    TimelineAligned,
    /// A finite, position-independent sound uses the independent preview bus.
    ShortPreview,
}

impl PaneAudioKind {
    pub const fn route(self) -> PaneAudioRoute {
        match self {
            Self::HpssSource
            | Self::HpssHarmonic
            | Self::HpssTransient
            | Self::HpssResidual
            | Self::LoomSource
            | Self::LoomConstruction
            | Self::LoomResidual
            | Self::ComparisonSource
            | Self::ComparisonConstruction
            | Self::ComparisonResidual
            | Self::RhythmConstruction => PaneAudioRoute::TimelineAligned,
            Self::RhythmFamilyMedoid | Self::LoomTemplate | Self::AssetOneShot | Self::PadGate => {
                PaneAudioRoute::ShortPreview
            }
        }
    }

    pub const fn audition_subject(self) -> Option<AuditionSubject> {
        match self {
            Self::HpssSource | Self::LoomSource | Self::ComparisonSource => {
                Some(AuditionSubject::Source)
            }
            Self::HpssHarmonic => Some(AuditionSubject::Harmonic),
            Self::HpssTransient => Some(AuditionSubject::Transient),
            Self::HpssResidual | Self::LoomResidual | Self::ComparisonResidual => {
                Some(AuditionSubject::Residual)
            }
            Self::LoomConstruction | Self::ComparisonConstruction | Self::RhythmConstruction => {
                Some(AuditionSubject::Construction)
            }
            Self::RhythmFamilyMedoid | Self::LoomTemplate | Self::AssetOneShot | Self::PadGate => {
                None
            }
        }
    }
}

/// One pane request ready for publication through `ProjectAudioController`.
/// Applying it may seek or adopt a loop, but only on the shared project
/// transport owned by the caller.
#[derive(Clone, Debug)]
pub struct PaneTimelineEffect {
    pub kind: PaneAudioKind,
    pub audition: Arc<TimelineAudition>,
    pub alignment: AuditionAlignment,
}

impl PaneTimelineEffect {
    pub fn from_mono(
        kind: PaneAudioKind,
        owner: AuditionOwner,
        project_revision: u64,
        span: RenderSpan,
        format: RenderFormat,
        mono: Arc<[f32]>,
        alignment: AuditionAlignment,
    ) -> Result<Self, PaneAudioError> {
        require_timeline_kind(kind)?;
        let frames = usize::try_from(span.len()).map_err(|_| PaneAudioError::SignalTooLarge)?;
        if mono.len() != frames {
            return Err(PaneAudioError::MonoFrameCount {
                expected: frames,
                actual: mono.len(),
            });
        }
        if mono.iter().any(|sample| !sample.is_finite()) {
            return Err(PaneAudioError::NonFinitePcm);
        }
        let channels = usize::from(format.channels.get());
        let capacity = frames
            .checked_mul(channels)
            .ok_or(PaneAudioError::SignalTooLarge)?;
        let mut interleaved = Vec::with_capacity(capacity);
        for sample in mono.iter().copied() {
            interleaved.extend(std::iter::repeat_n(sample, channels));
        }
        Self::from_interleaved(
            kind,
            owner,
            project_revision,
            span,
            format,
            Arc::from(interleaved),
            alignment,
        )
    }

    pub fn from_interleaved(
        kind: PaneAudioKind,
        owner: AuditionOwner,
        project_revision: u64,
        span: RenderSpan,
        format: RenderFormat,
        interleaved: Arc<[f32]>,
        alignment: AuditionAlignment,
    ) -> Result<Self, PaneAudioError> {
        require_timeline_kind(kind)?;
        let subject = kind
            .audition_subject()
            .expect("timeline kinds have an audition subject");
        let content = canonical_pcm_digest(&interleaved);
        let audition = TimelineAudition::new(
            TimelineAuditionId {
                owner,
                revision: project_revision,
                content,
            },
            subject,
            AuditionMix::Replace,
            span,
            format,
            interleaved,
        )?;
        Ok(Self {
            kind,
            audition: Arc::new(audition),
            alignment,
        })
    }

    /// The adapter has no transport parameter of its own: all aligned panes
    /// necessarily cross the same `ProjectAudioController` and `AudioHost`.
    pub fn apply(
        &self,
        controller: &mut ProjectAudioController,
        host: &AudioHost,
    ) -> Result<(), PaneAudioError> {
        controller.start_scoped_audition(host, Arc::clone(&self.audition), self.alignment)?;
        Ok(())
    }
}

fn require_timeline_kind(kind: PaneAudioKind) -> Result<(), PaneAudioError> {
    if kind.route() == PaneAudioRoute::TimelineAligned {
        Ok(())
    } else {
        Err(PaneAudioError::WrongRoute {
            kind,
            expected: PaneAudioRoute::TimelineAligned,
        })
    }
}

/// A preview request token is returned synchronously when a pane begins
/// resolving PCM. The same token must accompany completion and release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewRequest {
    pub token: SamplePreviewToken,
    pub kind: PaneAudioKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreviewStatus {
    pub desired: Option<PreviewRequest>,
    pub active: Option<PreviewRequest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewOutcome {
    Played(PreviewRequest),
    Stopped(PreviewRequest),
    IgnoredStale(PreviewRequest),
}

/// Minimal bus interface used to keep ownership/generation policy testable
/// without opening hardware. It deliberately exposes no project transport.
pub trait PreviewBus {
    fn play_preview(&self, clip: AuditionClip);
    fn stop_preview(&self);
}

impl PreviewBus for AudioHost {
    fn play_preview(&self, clip: AuditionClip) {
        self.audition(clip);
    }

    fn stop_preview(&self) {
        AudioHost::stop_preview(self);
    }
}

/// Sole control-plane arbiter for the host's one finite-preview bus.
///
/// `desired` advances before PCM resolution. Therefore a late completion from
/// any previous pane is rejected even when its owner differs from the newest
/// pane. `release` is exact-token based, so a stale pad key-up cannot stop a
/// newer strike from the same pad or a browser preview from another pane.
#[derive(Clone, Debug, Default)]
pub struct PreviewController {
    next_generation: u64,
    status: PreviewStatus,
}

/// Token allocated when a pane submits a semantic audition action. GPUI keeps
/// this beside its SampleAction request and supplies the same ticket when the
/// controller outcome arrives or the originating key/pointer is released.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleAuditionTicket {
    pub request: PreviewRequest,
    pub intent: SampleAuditionIntent,
}

#[derive(Clone, Debug)]
pub enum SamplePanePreviewEffect {
    Play {
        request: PreviewRequest,
        clip: AuditionClip,
    },
    Release {
        request: PreviewRequest,
    },
    CancelOwner {
        owner: AuditionOwner,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplePanePreviewOutcome {
    Preview(PreviewOutcome),
    OwnerCancelled(bool),
}

impl SamplePanePreviewEffect {
    pub fn apply<B: PreviewBus>(
        self,
        previews: &mut PreviewController,
        bus: &B,
    ) -> SamplePanePreviewOutcome {
        match self {
            Self::Play { request, clip } => {
                SamplePanePreviewOutcome::Preview(previews.complete(bus, request, clip))
            }
            Self::Release { request } => {
                SamplePanePreviewOutcome::Preview(previews.release(bus, request))
            }
            Self::CancelOwner { owner } => {
                SamplePanePreviewOutcome::OwnerCancelled(previews.cancel_owner(bus, owner))
            }
        }
    }
}

#[derive(Debug)]
pub struct SamplePaneOutcome {
    pub result: SampleActionResult,
    pub focus: Option<SampleResultFocus>,
    pub preview: Option<SamplePanePreviewEffect>,
}

/// UI-neutral bridge owned by one sampler/browser pane. It allocates preview
/// generations through the shared PreviewController but never activates panes
/// or touches AudioHost itself.
#[derive(Clone, Copy, Debug)]
pub struct SamplePaneBridge {
    owner: AuditionOwner,
}

impl SamplePaneBridge {
    pub fn new(view: WorkspaceViewId) -> Result<Self, PaneAudioError> {
        Ok(Self {
            owner: workspace_audition_owner(view)?,
        })
    }

    pub const fn owner(self) -> AuditionOwner {
        self.owner
    }

    pub fn begin_audition(
        self,
        previews: &mut PreviewController,
        intent: SampleAuditionIntent,
    ) -> Result<SampleAuditionTicket, PaneAudioError> {
        let kind = match intent {
            SampleAuditionIntent::MaterialOneShot { .. } => PaneAudioKind::AssetOneShot,
            SampleAuditionIntent::PadGate { .. } => PaneAudioKind::PadGate,
        };
        Ok(SampleAuditionTicket {
            request: previews.begin(self.owner, kind)?,
            intent,
        })
    }

    /// Convert one controller outcome into view feedback, optional navigation,
    /// and an optional finite-preview effect. A release must receive the press
    /// ticket it closes; this is what makes stale releases generation-safe.
    pub fn resolve_outcome(
        self,
        snapshot: &LiveProjectSnapshot,
        action: &SampleAction,
        outcome: SampleActionOutcome,
        ticket: Option<SampleAuditionTicket>,
    ) -> Result<SamplePaneOutcome, PaneAudioError> {
        let preview = match &outcome {
            SampleActionOutcome::Audition(intent) => {
                let ticket = ticket.ok_or(PaneAudioError::MissingSampleAuditionTicket)?;
                if !audition_ticket_matches(ticket.intent, *intent) {
                    return Err(PaneAudioError::MismatchedSampleAuditionTicket);
                }
                match intent {
                    SampleAuditionIntent::MaterialOneShot { .. }
                    | SampleAuditionIntent::PadGate { pressed: true, .. } => {
                        let resolved =
                            resolve_sample_audition(snapshot, ticket.request.token, *intent)?;
                        let SamplePreviewCommand::Start { clip, .. } = resolved.command else {
                            return Err(PaneAudioError::MismatchedSampleAuditionTicket);
                        };
                        Some(SamplePanePreviewEffect::Play {
                            request: ticket.request,
                            clip: sample_preview_clip(&clip)?,
                        })
                    }
                    SampleAuditionIntent::PadGate { pressed: false, .. } => {
                        Some(SamplePanePreviewEffect::Release {
                            request: ticket.request,
                        })
                    }
                }
            }
            _ => None,
        };
        let result = sample_action_result(action, outcome);
        let focus = result.as_ref().ok().and_then(|outcome| match outcome {
            SampleViewOutcome::Published(receipt) if receipt.focus != SampleResultFocus::Stay => {
                Some(receipt.focus)
            }
            _ => None,
        });
        Ok(SamplePaneOutcome {
            result,
            focus,
            preview,
        })
    }

    /// Pair with pane/entity disposal. Applying this effect cancels only this
    /// persisted view owner and cannot silence a surviving pane.
    pub const fn dispose_effect(self) -> SamplePanePreviewEffect {
        SamplePanePreviewEffect::CancelOwner { owner: self.owner }
    }
}

fn audition_ticket_matches(ticket: SampleAuditionIntent, outcome: SampleAuditionIntent) -> bool {
    match (ticket, outcome) {
        (
            SampleAuditionIntent::MaterialOneShot { material: left, .. },
            SampleAuditionIntent::MaterialOneShot {
                material: right, ..
            },
        ) => left == right,
        (
            SampleAuditionIntent::PadGate {
                kit: left_kit,
                pad: left_pad,
                ..
            },
            SampleAuditionIntent::PadGate {
                kit: right_kit,
                pad: right_pad,
                ..
            },
        ) => left_kit == right_kit && left_pad == right_pad,
        _ => false,
    }
}

/// Canonical conversion previously duplicated by GPUI. Provenance comes from
/// the submitted action, and a new-pad publication retains the exact pad in a
/// sampler focus instead of degrading to kit-only focus.
pub fn sample_action_result(
    action: &SampleAction,
    outcome: SampleActionOutcome,
) -> SampleActionResult {
    Ok(match outcome {
        SampleActionOutcome::Published(outcome) => {
            SampleViewOutcome::Published(sample_publication_result(action, outcome.publication))
        }
        SampleActionOutcome::Audition(intent) => SampleViewOutcome::Audition(intent),
        SampleActionOutcome::Preview(preview) => SampleViewOutcome::ChopPreview(preview),
        SampleActionOutcome::Inspect(_) => SampleViewOutcome::Acknowledged {
            kind: SampleActionKind::Inspect,
            message: "Inspection target accepted".into(),
            provenance: action.result_provenance(),
        },
        SampleActionOutcome::Workspace(_) => SampleViewOutcome::Acknowledged {
            kind: SampleActionKind::Workspace,
            message: "Workspace target accepted".into(),
            provenance: action.result_provenance(),
        },
        SampleActionOutcome::ForwardZoneEdit(_) | SampleActionOutcome::ForwardDrop(_) => {
            SampleViewOutcome::Acknowledged {
                kind: SampleActionKind::Edit,
                message: "Edit retained for its owning surface".into(),
                provenance: action.result_provenance(),
            }
        }
    })
}

/// Build the durable musician-facing receipt from the controller's immutable
/// publication. Keeping this pure lets a session adapter preserve exact focus
/// and provenance when a background result reaches GPUI later.
pub fn sample_publication_result(
    action: &SampleAction,
    publication: ConstructivePublication,
) -> SamplePublishedResult {
    let focus = match publication.focus {
        ConstructivePublishedFocus::Stay | ConstructivePublishedFocus::Arrangement(_) => {
            SampleResultFocus::Stay
        }
        ConstructivePublishedFocus::Kit(kit) => SampleResultFocus::Kit(kit),
        ConstructivePublishedFocus::Pad { kit, pad } => SampleResultFocus::Pad { kit, pad },
        ConstructivePublishedFocus::Pattern(pattern) => SampleResultFocus::Pattern(pattern),
        ConstructivePublishedFocus::Sampler { kit, disposition } => {
            let target =
                publication
                    .pad
                    .map_or(SamplerTarget::Kit(kit), |pad| SamplerTarget::Pad {
                        kit,
                        pad,
                    });
            SampleResultFocus::Sampler {
                target,
                disposition,
            }
        }
    };
    SamplePublishedResult {
        revision: publication.revision,
        kit: publication.kit,
        pad: publication.pad,
        pattern: publication.pattern,
        focus,
        provenance: action.result_provenance(),
    }
}

impl PreviewController {
    pub const fn status(&self) -> PreviewStatus {
        self.status
    }

    pub fn begin(
        &mut self,
        owner: AuditionOwner,
        kind: PaneAudioKind,
    ) -> Result<PreviewRequest, PaneAudioError> {
        if kind.route() != PaneAudioRoute::ShortPreview {
            return Err(PaneAudioError::WrongRoute {
                kind,
                expected: PaneAudioRoute::ShortPreview,
            });
        }
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(PaneAudioError::PreviewGenerationExhausted)?;
        let request = PreviewRequest {
            token: SamplePreviewToken {
                owner,
                generation: self.next_generation,
            },
            kind,
        };
        self.status.desired = Some(request);
        Ok(request)
    }

    /// Publish a resolved clip only if it is still the globally desired
    /// preview. The prior clip remains audible while newer PCM is resolving.
    pub fn complete<B: PreviewBus>(
        &mut self,
        bus: &B,
        request: PreviewRequest,
        clip: AuditionClip,
    ) -> PreviewOutcome {
        if self.status.desired != Some(request) {
            return PreviewOutcome::IgnoredStale(request);
        }
        bus.play_preview(clip);
        self.status.active = Some(request);
        PreviewOutcome::Played(request)
    }

    /// Release/cancel one exact generation. It cannot affect a newer desired
    /// or active request, including a newer strike by the same owner.
    pub fn release<B: PreviewBus>(&mut self, bus: &B, request: PreviewRequest) -> PreviewOutcome {
        if self.status.desired == Some(request) {
            self.status.desired = None;
        }
        if self.status.active != Some(request) {
            return PreviewOutcome::IgnoredStale(request);
        }
        bus.stop_preview();
        self.status.active = None;
        PreviewOutcome::Stopped(request)
    }

    /// Explicit global transport actions (play, locate, project close) may
    /// cancel the finite preview bus. Clearing `desired` here is essential: a
    /// worker completion already in flight must not restart preview afterward.
    pub fn cancel_all<B: PreviewBus>(&mut self, bus: &B) -> bool {
        self.status.desired = None;
        if self.status.active.take().is_some() {
            bus.stop_preview();
            true
        } else {
            false
        }
    }

    /// Pane teardown invalidates only that pane's pending/active preview. A
    /// closed stale pane cannot silence a preview owned by a surviving pane.
    pub fn cancel_owner<B: PreviewBus>(&mut self, bus: &B, owner: AuditionOwner) -> bool {
        if self
            .status
            .desired
            .is_some_and(|request| request.token.owner == owner)
        {
            self.status.desired = None;
        }
        if !self
            .status
            .active
            .is_some_and(|request| request.token.owner == owner)
        {
            return false;
        }
        self.status.active = None;
        bus.stop_preview();
        true
    }

    /// Reconcile natural one-shot completion observed from `AudioHost`.
    pub fn observe_bus_idle(&mut self) {
        let Some(active) = self.status.active.take() else {
            return;
        };
        if self.status.desired == Some(active) {
            self.status.desired = None;
        }
    }
}

/// Materialize the resolver's zero-copy PCM view for the host preview bus.
/// This matches the sampler's equal-power pan law. Tuning is represented by
/// the clip's declared source rate, leaving device-rate conversion to Rodio;
/// no second resampler or render graph is introduced.
pub fn sample_preview_clip(clip: &SamplePreviewClipRef) -> Result<AuditionClip, PaneAudioError> {
    if !clip.gain.is_finite()
        || !clip.pan.is_finite()
        || !clip.tuning_cents.is_finite()
        || !(-1.0..=1.0).contains(&clip.pan)
        || !(-9_600.0..=9_600.0).contains(&clip.tuning_cents)
    {
        return Err(PaneAudioError::InvalidPreviewParameters);
    }
    let channels = usize::from(clip.pcm.format.channels.get());
    if !(1..=2).contains(&channels) {
        return Err(PaneAudioError::UnsupportedPreviewChannels(channels as u16));
    }
    let start = usize::try_from(clip.source_range.start.0)
        .ok()
        .and_then(|frame| frame.checked_mul(channels))
        .ok_or(PaneAudioError::SignalTooLarge)?;
    let end = usize::try_from(clip.source_range.end.0)
        .ok()
        .and_then(|frame| frame.checked_mul(channels))
        .ok_or(PaneAudioError::SignalTooLarge)?;
    let source = clip
        .pcm
        .samples
        .get(start..end)
        .ok_or(PaneAudioError::PreviewRangeOutsidePcm)?;
    if source.iter().any(|sample| !sample.is_finite()) {
        return Err(PaneAudioError::NonFinitePcm);
    }
    let angle = (clip.pan + 1.0) * std::f32::consts::FRAC_PI_4;
    let left_gain = clip.gain * angle.cos();
    let right_gain = clip.gain * angle.sin();
    let mut stereo = Vec::with_capacity((source.len() / channels).saturating_mul(2));
    if channels == 1 {
        for sample in source.iter().copied() {
            stereo.push(sample * left_gain);
            stereo.push(sample * right_gain);
        }
    } else {
        for frame in source.chunks_exact(2) {
            stereo.push(frame[0] * left_gain);
            stereo.push(frame[1] * right_gain);
        }
    }
    let base_rate = f64::from(clip.pcm.format.sample_rate.get());
    let rate = (base_rate * 2.0_f64.powf(f64::from(clip.tuning_cents) / 1_200.0)).round();
    if !rate.is_finite() || !(1.0..=f64::from(u32::MAX)).contains(&rate) {
        return Err(PaneAudioError::InvalidPreviewSampleRate);
    }
    let format = AudioFormat::new(rate as u32, 2).map_err(AudioHostError::from)?;
    AuditionClip::from_interleaved(format, stereo).map_err(Into::into)
}

#[derive(Debug)]
pub enum PaneAudioError {
    WrongRoute {
        kind: PaneAudioKind,
        expected: PaneAudioRoute,
    },
    MonoFrameCount {
        expected: usize,
        actual: usize,
    },
    NonFinitePcm,
    SignalTooLarge,
    PreviewGenerationExhausted,
    ZeroWorkspaceView,
    InvalidPreviewParameters,
    UnsupportedPreviewChannels(u16),
    PreviewRangeOutsidePcm,
    InvalidPreviewSampleRate,
    MissingSampleAuditionTicket,
    MismatchedSampleAuditionTicket,
    SamplePreview(SamplePreviewError),
    Runtime(crate::render_runtime::RenderRuntimeError),
    ProjectAudio(ProjectAudioControllerError),
    AudioHost(AudioHostError),
}

impl fmt::Display for PaneAudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongRoute { kind, expected } => {
                write!(formatter, "{kind:?} is not a {expected:?} audio request")
            }
            Self::MonoFrameCount { expected, actual } => write!(
                formatter,
                "aligned mono PCM has {actual} frames, expected {expected}"
            ),
            Self::NonFinitePcm => formatter.write_str("aligned PCM contains a non-finite sample"),
            Self::SignalTooLarge => formatter.write_str("aligned signal is too large"),
            Self::PreviewGenerationExhausted => {
                formatter.write_str("preview generation counter exhausted")
            }
            Self::ZeroWorkspaceView => {
                formatter.write_str("workspace view zero cannot own an audition")
            }
            Self::InvalidPreviewParameters => {
                formatter.write_str("preview gain, pan, or tuning is invalid")
            }
            Self::UnsupportedPreviewChannels(channels) => {
                write!(
                    formatter,
                    "preview PCM has {channels} channels; expected mono or stereo"
                )
            }
            Self::PreviewRangeOutsidePcm => {
                formatter.write_str("preview source range lies outside decoded PCM")
            }
            Self::InvalidPreviewSampleRate => {
                formatter.write_str("preview tuning produces an invalid source sample rate")
            }
            Self::MissingSampleAuditionTicket => {
                formatter.write_str("sample audition outcome has no preview request ticket")
            }
            Self::MismatchedSampleAuditionTicket => {
                formatter.write_str("sample audition outcome does not match its preview ticket")
            }
            Self::SamplePreview(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
            Self::ProjectAudio(error) => error.fmt(formatter),
            Self::AudioHost(error) => error.fmt(formatter),
        }
    }
}

impl Error for PaneAudioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SamplePreview(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::ProjectAudio(error) => Some(error),
            Self::AudioHost(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::render_runtime::RenderRuntimeError> for PaneAudioError {
    fn from(error: crate::render_runtime::RenderRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<ProjectAudioControllerError> for PaneAudioError {
    fn from(error: ProjectAudioControllerError) -> Self {
        Self::ProjectAudio(error)
    }
}

impl From<AudioHostError> for PaneAudioError {
    fn from(error: AudioHostError) -> Self {
        Self::AudioHost(error)
    }
}

impl From<SamplePreviewError> for PaneAudioError {
    fn from(error: SamplePreviewError) -> Self {
        Self::SamplePreview(error)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::daw_project::DawProject;
    use crate::daw_render::PcmAsset;
    use crate::sample_actions::{
        MakeBeatIntent, MakeBeatResultFocus, SampleChopIntent, SampleKitDestination,
        SampleResultProvenance, SampleSelection, SamplerViewDisposition,
    };
    use crate::sample_kit::{KitId, PadId};
    use crate::sample_material::SourceMaterialRef;

    #[derive(Default)]
    struct FakePreviewBus {
        played: RefCell<Vec<Vec<f32>>>,
        stops: RefCell<usize>,
    }

    impl PreviewBus for FakePreviewBus {
        fn play_preview(&self, clip: AuditionClip) {
            self.played.borrow_mut().push(clip.interleaved().to_vec());
        }

        fn stop_preview(&self) {
            *self.stops.borrow_mut() += 1;
        }
    }

    fn owner(local: u64) -> AuditionOwner {
        AuditionOwner {
            namespace: 0x6175_6465_632d_7061_6e65,
            local,
        }
    }

    fn clip(value: f32) -> AuditionClip {
        AuditionClip::mono(48_000, vec![value, 0.0]).unwrap()
    }

    #[test]
    fn analytical_panes_have_one_timeline_route_and_ephemera_have_one_preview_route() {
        for kind in [
            PaneAudioKind::HpssSource,
            PaneAudioKind::HpssHarmonic,
            PaneAudioKind::HpssTransient,
            PaneAudioKind::HpssResidual,
            PaneAudioKind::LoomSource,
            PaneAudioKind::LoomConstruction,
            PaneAudioKind::LoomResidual,
            PaneAudioKind::ComparisonSource,
            PaneAudioKind::ComparisonConstruction,
            PaneAudioKind::ComparisonResidual,
            PaneAudioKind::RhythmConstruction,
        ] {
            assert_eq!(kind.route(), PaneAudioRoute::TimelineAligned, "{kind:?}");
            assert!(kind.audition_subject().is_some(), "{kind:?}");
        }
        for kind in [
            PaneAudioKind::RhythmFamilyMedoid,
            PaneAudioKind::LoomTemplate,
            PaneAudioKind::AssetOneShot,
            PaneAudioKind::PadGate,
        ] {
            assert_eq!(kind.route(), PaneAudioRoute::ShortPreview, "{kind:?}");
            assert_eq!(kind.audition_subject(), None, "{kind:?}");
        }
    }

    #[test]
    fn persisted_workspace_identity_is_the_audition_owner() {
        let view = WorkspaceViewId(44);
        let first = workspace_audition_owner(view).unwrap();
        let recreated = workspace_audition_owner(view).unwrap();
        let duplicate = workspace_audition_owner(WorkspaceViewId(45)).unwrap();
        assert_eq!(first, recreated);
        assert_ne!(first, duplicate);
        assert!(matches!(
            workspace_audition_owner(WorkspaceViewId(0)),
            Err(PaneAudioError::ZeroWorkspaceView)
        ));
    }

    #[test]
    fn hpss_loom_comparison_and_rhythm_compile_to_the_same_shared_transport_effect() {
        let span = RenderSpan::new(120, 124).unwrap();
        let format = RenderFormat::new(48_000, 2).unwrap();
        let mono: Arc<[f32]> = Arc::from([0.1, 0.2, 0.3, 0.4]);
        for (local, kind, subject) in [
            (1, PaneAudioKind::HpssHarmonic, AuditionSubject::Harmonic),
            (
                2,
                PaneAudioKind::LoomConstruction,
                AuditionSubject::Construction,
            ),
            (
                3,
                PaneAudioKind::ComparisonResidual,
                AuditionSubject::Residual,
            ),
            (
                4,
                PaneAudioKind::RhythmConstruction,
                AuditionSubject::Construction,
            ),
        ] {
            let effect = PaneTimelineEffect::from_mono(
                kind,
                owner(local),
                77,
                span,
                format,
                Arc::clone(&mono),
                AuditionAlignment::PreserveTransport,
            )
            .unwrap();
            assert_eq!(effect.kind, kind);
            assert_eq!(effect.audition.subject, subject);
            assert_eq!(effect.audition.span, span);
            assert_eq!(effect.audition.format, format);
            assert_eq!(effect.audition.id.revision, 77);
            assert_eq!(effect.audition.interleaved().len(), 8);
            assert!(matches!(
                effect.alignment,
                AuditionAlignment::PreserveTransport
            ));
        }
    }

    #[test]
    fn late_preview_completion_and_release_cannot_replace_or_stop_newer_owner() {
        let bus = FakePreviewBus::default();
        let mut controller = PreviewController::default();
        let rhythm = controller
            .begin(owner(1), PaneAudioKind::RhythmFamilyMedoid)
            .unwrap();
        let asset = controller
            .begin(owner(2), PaneAudioKind::AssetOneShot)
            .unwrap();

        assert_eq!(
            controller.complete(&bus, rhythm, clip(0.1)),
            PreviewOutcome::IgnoredStale(rhythm)
        );
        assert_eq!(
            controller.complete(&bus, asset, clip(0.2)),
            PreviewOutcome::Played(asset)
        );
        assert_eq!(
            controller.release(&bus, rhythm),
            PreviewOutcome::IgnoredStale(rhythm)
        );
        assert_eq!(controller.status().active, Some(asset));
        assert_eq!(*bus.stops.borrow(), 0);
        assert_eq!(bus.played.borrow().as_slice(), &[vec![0.2, 0.0]]);
    }

    #[test]
    fn stale_pad_release_cannot_stop_a_newer_strike_from_the_same_owner() {
        let bus = FakePreviewBus::default();
        let mut controller = PreviewController::default();
        let first = controller.begin(owner(3), PaneAudioKind::PadGate).unwrap();
        let second = controller.begin(owner(3), PaneAudioKind::PadGate).unwrap();
        assert!(second.token.generation > first.token.generation);

        assert_eq!(
            controller.complete(&bus, second, clip(0.8)),
            PreviewOutcome::Played(second)
        );
        assert_eq!(
            controller.release(&bus, first),
            PreviewOutcome::IgnoredStale(first)
        );
        assert_eq!(controller.status().active, Some(second));
        assert_eq!(*bus.stops.borrow(), 0);
        assert_eq!(
            controller.release(&bus, second),
            PreviewOutcome::Stopped(second)
        );
        assert_eq!(*bus.stops.borrow(), 1);
    }

    #[test]
    fn preview_and_timeline_routes_cannot_be_accidentally_crossed() {
        let mut controller = PreviewController::default();
        assert!(matches!(
            controller.begin(owner(1), PaneAudioKind::HpssSource),
            Err(PaneAudioError::WrongRoute { .. })
        ));
        assert!(matches!(
            PaneTimelineEffect::from_mono(
                PaneAudioKind::LoomTemplate,
                owner(1),
                1,
                RenderSpan::new(0, 2).unwrap(),
                RenderFormat::new(48_000, 1).unwrap(),
                Arc::from([0.0, 0.0]),
                AuditionAlignment::PreserveTransport,
            ),
            Err(PaneAudioError::WrongRoute { .. })
        ));
    }

    #[test]
    fn global_cancel_invalidates_pending_completion_and_stops_only_active_preview() {
        let bus = FakePreviewBus::default();
        let mut controller = PreviewController::default();
        let active = controller
            .begin(owner(1), PaneAudioKind::LoomTemplate)
            .unwrap();
        controller.complete(&bus, active, clip(0.4));
        let pending = controller
            .begin(owner(2), PaneAudioKind::AssetOneShot)
            .unwrap();
        assert!(controller.cancel_all(&bus));
        assert_eq!(controller.status(), PreviewStatus::default());
        assert_eq!(*bus.stops.borrow(), 1);
        assert_eq!(
            controller.complete(&bus, pending, clip(0.9)),
            PreviewOutcome::IgnoredStale(pending)
        );
        assert_eq!(bus.played.borrow().len(), 1);
    }

    #[test]
    fn closing_stale_pane_cannot_stop_surviving_panes_preview() {
        let bus = FakePreviewBus::default();
        let mut controller = PreviewController::default();
        let closed = controller
            .begin(owner(1), PaneAudioKind::LoomTemplate)
            .unwrap();
        let surviving = controller
            .begin(owner(2), PaneAudioKind::RhythmFamilyMedoid)
            .unwrap();
        controller.complete(&bus, surviving, clip(0.7));

        assert!(!controller.cancel_owner(&bus, closed.token.owner));
        assert_eq!(controller.status().active, Some(surviving));
        assert_eq!(*bus.stops.borrow(), 0);
        assert!(controller.cancel_owner(&bus, surviving.token.owner));
        assert_eq!(controller.status(), PreviewStatus::default());
        assert_eq!(*bus.stops.borrow(), 1);
    }

    #[test]
    fn sample_preview_adapter_honors_exact_range_gain_pan_and_tuning() {
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.1, 0.2, 0.3, 0.4]),
        )
        .unwrap();
        let clip = sample_preview_clip(&SamplePreviewClipRef {
            pcm,
            source_range: AssetFrameRange::new(SampleFrames(1), SampleFrames(3)).unwrap(),
            gain: 0.5,
            pan: -1.0,
            tuning_cents: 1_200.0,
        })
        .unwrap();
        assert_eq!(clip.format().sample_rate.get(), 96_000);
        assert_eq!(clip.format().channels.get(), 2);
        assert_eq!(clip.frame_count().0, 2);
        assert_eq!(clip.interleaved(), &[0.1, 0.0, 0.15, 0.0]);
    }

    #[test]
    fn published_new_pad_focus_and_receipt_keep_exact_identity_and_provenance() {
        let asset = AssetId(19);
        let range = AssetFrameRange::new(SampleFrames(120), SampleFrames(960)).unwrap();
        let chop = SampleChopIntent::EqualSlices { count: 7 };
        let action = SampleAction::MakeBeat(MakeBeatIntent {
            source: SampleSelection {
                asset,
                source_range: Some(range),
            },
            chop: chop.clone(),
            kit: SampleKitDestination::NewKit,
            target_bus: None,
            bars: 2,
            quantize_ticks: 120,
            result_focus: MakeBeatResultFocus::Sampler(SamplerViewDisposition::OpenNew),
        });
        let kit = KitId::from_raw(41);
        let pad = PadId::from_raw(73);
        let receipt = sample_publication_result(
            &action,
            ConstructivePublication {
                revision: 9,
                kit,
                pad: Some(pad),
                pattern: None,
                arrangement_clip: None,
                focus: ConstructivePublishedFocus::Sampler {
                    kit,
                    disposition: SamplerViewDisposition::OpenNew,
                },
            },
        );

        assert_eq!(
            receipt.focus,
            SampleResultFocus::Sampler {
                target: SamplerTarget::Pad { kit, pad },
                disposition: SamplerViewDisposition::OpenNew,
            }
        );
        assert_eq!(receipt.pad, Some(pad));
        assert_eq!(
            receipt.provenance,
            Some(SampleResultProvenance::Selection {
                source: SampleSelection {
                    asset,
                    source_range: Some(range),
                },
                chop: Some(chop),
            })
        );
    }

    #[test]
    fn bridge_plays_the_browser_selection_instead_of_the_primary_asset() {
        let primary = AssetId(1);
        let selected = AssetId(2);
        let format = AudioFormat::new(48_000, 1).unwrap();
        let primary_pcm = PcmAsset::new(format, Arc::from([0.1, 0.2])).unwrap();
        let selected_pcm = PcmAsset::new(format, Arc::from([0.8, 0.6])).unwrap();
        let snapshot = LiveProjectSnapshot {
            project: Arc::new(DawProject::new("sample preview", 48_000, 120.0).unwrap()),
            pcm: Arc::new(BTreeMap::from([
                (primary, primary_pcm),
                (selected, selected_pcm),
            ])),
            sample_pcm: Arc::new(BTreeMap::new()),
        };
        let intent = SampleAuditionIntent::MaterialOneShot {
            material: SourceMaterialRef::Asset(selected),
            velocity: 1.0,
        };
        let action = SampleAction::Audition(intent);
        let bridge = SamplePaneBridge::new(WorkspaceViewId(5)).unwrap();
        let mut previews = PreviewController::default();
        let ticket = bridge.begin_audition(&mut previews, intent).unwrap();
        let result = bridge
            .resolve_outcome(
                &snapshot,
                &action,
                SampleActionOutcome::Audition(intent),
                Some(ticket),
            )
            .unwrap();
        let Some(SamplePanePreviewEffect::Play { clip, .. }) = result.preview else {
            panic!("material audition should produce a play effect")
        };
        let equal_power = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(
            clip.interleaved(),
            &[
                0.8 * equal_power,
                0.8 * equal_power,
                0.6 * equal_power,
                0.6 * equal_power
            ]
        );
    }

    #[test]
    fn bridge_stale_pad_release_does_not_stop_the_newer_press() {
        let bus = FakePreviewBus::default();
        let mut previews = PreviewController::default();
        let bridge = SamplePaneBridge::new(WorkspaceViewId(6)).unwrap();
        let kit = KitId::from_raw(2);
        let pad = PadId::from_raw(3);
        let press = SampleAuditionIntent::PadGate {
            kit,
            pad,
            velocity: 0.9,
            pressed: true,
        };
        let first = bridge.begin_audition(&mut previews, press).unwrap();
        let second = bridge.begin_audition(&mut previews, press).unwrap();
        assert_eq!(
            SamplePanePreviewEffect::Play {
                request: second.request,
                clip: clip(0.75),
            }
            .apply(&mut previews, &bus),
            SamplePanePreviewOutcome::Preview(PreviewOutcome::Played(second.request))
        );
        assert_eq!(
            SamplePanePreviewEffect::Release {
                request: first.request,
            }
            .apply(&mut previews, &bus),
            SamplePanePreviewOutcome::Preview(PreviewOutcome::IgnoredStale(first.request))
        );
        assert_eq!(previews.status().active, Some(second.request));
        assert_eq!(*bus.stops.borrow(), 0);
    }

    #[test]
    fn bridge_disposal_cancels_only_its_persisted_view_owner() {
        let bus = FakePreviewBus::default();
        let mut previews = PreviewController::default();
        let closing = SamplePaneBridge::new(WorkspaceViewId(20)).unwrap();
        let surviving = SamplePaneBridge::new(WorkspaceViewId(21)).unwrap();
        let request = previews
            .begin(surviving.owner(), PaneAudioKind::AssetOneShot)
            .unwrap();
        previews.complete(&bus, request, clip(0.4));

        assert_eq!(
            closing.dispose_effect().apply(&mut previews, &bus),
            SamplePanePreviewOutcome::OwnerCancelled(false)
        );
        assert_eq!(previews.status().active, Some(request));
        assert_eq!(*bus.stops.borrow(), 0);
    }
}
