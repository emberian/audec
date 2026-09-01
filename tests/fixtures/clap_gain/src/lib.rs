// SPDX-License-Identifier: MIT OR Apache-2.0
// Adapted from Clack's gain example; provenance is in ../README.md.

use clack_extensions::audio_ports::*;
use clack_extensions::params::*;
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::sync::atomic::{AtomicU32, Ordering};

const PARAM_GAIN: ClapId = ClapId::new(1);

pub struct FixturePlugin;

impl Plugin for FixturePlugin {
    type AudioProcessor<'a> = FixtureAudio<'a>;
    type Shared<'a> = FixtureShared;
    type MainThread<'a> = FixtureMain<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _: Option<&FixtureShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for FixturePlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new("dev.audec.fixture.gain", "Audec CLAP Gain Fixture")
            .with_vendor("Audec tests")
            .with_version("1")
            .with_features([AUDIO_EFFECT, STEREO])
    }

    fn new_shared(_: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(FixtureShared {
            gain: AtomicF32::new(1.0),
        })
    }

    fn new_main_thread<'a>(
        _: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(FixtureMain { shared })
    }
}

pub struct FixtureShared {
    gain: AtomicF32,
}
impl PluginShared<'_> for FixtureShared {}

pub struct FixtureMain<'a> {
    shared: &'a FixtureShared,
}
impl<'a> PluginMainThread<'a, FixtureShared> for FixtureMain<'a> {}

pub struct FixtureAudio<'a> {
    shared: &'a FixtureShared,
}

impl<'a> PluginAudioProcessor<'a, FixtureShared, FixtureMain<'a>> for FixtureAudio<'a> {
    fn activate(
        _: HostAudioProcessorHandle<'a>,
        _: &mut FixtureMain<'a>,
        shared: &'a FixtureShared,
        _: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self { shared })
    }

    fn process(
        &mut self,
        _: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut pair = audio
            .port_pair(0)
            .ok_or(PluginError::Message("missing stereo port"))?;
        let mut channels = pair
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("fixture requires f32"))?;
        let mut buffers = [None, None];
        for (pair, output) in channels.iter_mut().zip(&mut buffers) {
            *output = match pair {
                ChannelPair::InPlace(buffer) => Some(buffer),
                ChannelPair::InputOutput(input, output) => {
                    output.copy_from_slice(input);
                    Some(output)
                }
                ChannelPair::InputOnly(_) | ChannelPair::OutputOnly(_) => None,
            };
        }
        for batch in events.input.batch() {
            for event in batch.events() {
                self.shared.handle_event(event);
            }
            let gain = self.shared.gain.load();
            let start = batch.first_sample();
            let end = batch
                .next_batch_first_sample()
                .unwrap_or_else(|| buffers.iter().flatten().next().map_or(0, |b| b.len()));
            for buffer in buffers.iter_mut().flatten() {
                for sample in &mut buffer[start..end] {
                    *sample *= gain;
                }
            }
        }
        Ok(ProcessStatus::ContinueIfNotQuiet)
    }
}

impl FixtureShared {
    fn handle_event(&self, event: &UnknownEvent) {
        if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
            if event.param_id() == PARAM_GAIN {
                self.gain.store(event.value() as f32);
            }
        }
    }
}

impl PluginAudioPortsImpl for FixtureMain<'_> {
    fn count(&mut self, _: bool) -> u32 {
        1
    }
    fn get(&mut self, index: u32, _: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginMainThreadParams for FixtureMain<'_> {
    fn count(&mut self) -> u32 {
        1
    }
    fn get_info(&mut self, index: u32, writer: &mut ParamInfoWriter) {
        if index == 0 {
            writer.set(&ParamInfo {
                id: PARAM_GAIN,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Gain",
                module: b"",
                min_value: 0.0,
                max_value: 1.0,
                default_value: 1.0,
            });
        }
    }
    fn get_value(&mut self, id: ClapId) -> Option<f64> {
        (id == PARAM_GAIN).then(|| self.shared.gain.load() as f64)
    }
    fn value_to_text(
        &mut self,
        id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        if id == PARAM_GAIN {
            write!(writer, "{value:.3}")
        } else {
            Err(std::fmt::Error)
        }
    }
    fn text_to_value(&mut self, id: ClapId, text: &CStr) -> Option<f64> {
        (id == PARAM_GAIN)
            .then(|| text.to_str().ok()?.parse().ok())
            .flatten()
    }
    fn flush(&mut self, input: &InputEvents, _: &mut OutputEvents) {
        for event in input {
            self.shared.handle_event(event);
        }
    }
}

impl PluginAudioProcessorParams for FixtureAudio<'_> {
    fn flush(&mut self, input: &InputEvents, _: &mut OutputEvents) {
        for event in input {
            self.shared.handle_event(event);
        }
    }
}

impl PluginStateImpl for FixtureMain<'_> {
    fn save(&mut self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(&self.shared.gain.load().to_le_bytes())?;
        Ok(())
    }
    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut bytes = [0; 4];
        input.read_exact(&mut bytes)?;
        self.shared.gain.store(f32::from_le_bytes(bytes));
        Ok(())
    }
}

struct AtomicF32(AtomicU32);
impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }
    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

clack_export_entry!(SinglePluginEntry<FixturePlugin>);
