#[allow(dead_code)]
#[path = "../src/plugin.rs"]
mod plugin;
#[allow(dead_code)]
#[path = "../src/plugin_wire.rs"]
mod plugin_wire;
#[allow(dead_code)]
#[path = "../src/plugin_worker.rs"]
mod plugin_worker;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plugin::{
    ChannelLayout, NegotiatedAudioPort, ParameterEvent, PluginFormat, PluginKey,
    PluginParameterKey, PortDirection, ProcessingContract, TailReport,
};
use plugin_wire::{ArtifactDto, ArtifactGrantDto, Message, ScanRequestDto, TokenDto};
use plugin_worker::transport::{binding_for, InputEvent, SharedBlockTransport, DEFAULT_MAX_EVENTS};
use plugin_worker::{
    fingerprint_artifact, HostHealth, InstanceRecipe, OutOfProcessPluginHost, WorkerLaunch,
};

fn temporary_root() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("audec-real-clap-worker-{unique}"));
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn launch(root: &PathBuf) -> WorkerLaunch {
    let mut launch = WorkerLaunch::new(
        PathBuf::from(env!("CARGO_BIN_EXE_audec-clap-worker")),
        root.clone(),
    );
    launch.arguments = vec!["--session-root".into(), root.to_string_lossy().into_owned()];
    launch.startup_timeout = Duration::from_secs(5);
    launch.request_timeout = Duration::from_secs(5);
    launch
}

#[test]
fn real_clap_worker_advertises_runtime_and_rejects_non_clap_bytes() {
    let root = temporary_root();
    let artifact = root.join("not-a-plugin.clap");
    fs::write(&artifact, b"not a native CLAP library").unwrap();
    let artifact = fs::canonicalize(artifact).unwrap();
    let mut host = OutOfProcessPluginHost::launch(launch(&root)).unwrap();
    assert!(host.capabilities().scanning);
    assert!(host.capabilities().realtime);
    assert!(host.capabilities().offline);
    assert!(host.capabilities().shared_memory);

    let response = host
        .scan_candidate(ScanRequestDto {
            request_id: 7,
            candidate_path: artifact.to_string_lossy().into_owned(),
            timeout_millis: 1_000,
            maximum_descriptors: 64,
            maximum_parameters_per_plugin: 65_536,
        })
        .unwrap();
    let Message::ScanFailed { failure, .. } = response else {
        panic!("invalid native bytes must not enter the plugin catalog")
    };
    assert!(failure.detail.contains("could not load CLAP entry"));
    assert_eq!(host.health(), HostHealth::Ready);
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn self_built_clap_processes_sample_accurate_gain_in_subprocess() {
    let root = temporary_root();
    let target = root.join("fixture-target");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--quiet",
            "--locked",
            "--manifest-path",
            "tests/fixtures/clap_gain/Cargo.toml",
            "--target-dir",
        ])
        .arg(&target)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success(), "the permissive CLAP fixture must build");
    #[cfg(target_os = "macos")]
    let built = target.join("debug/libaudec_clap_gain_fixture.dylib");
    #[cfg(target_os = "linux")]
    let built = target.join("debug/libaudec_clap_gain_fixture.so");
    let artifact = root.join("audec-gain.clap");
    fs::copy(built, &artifact).unwrap();
    let artifact = fs::canonicalize(artifact).unwrap();
    let fingerprint = fingerprint_artifact(&artifact, 1024 * 1024 * 1024).unwrap();

    let contract = ProcessingContract {
        sample_rate: 48_000,
        minimum_frames: 1,
        maximum_frames: 64,
        audio_ports: vec![
            NegotiatedAudioPort {
                native_id: 0,
                direction: PortDirection::Input,
                layout: ChannelLayout::Stereo,
                channel_offset: 0,
            },
            NegotiatedAudioPort {
                native_id: 0,
                direction: PortDirection::Output,
                layout: ChannelLayout::Stereo,
                channel_offset: 0,
            },
        ],
        note_inputs: BTreeMap::new(),
        note_outputs: BTreeMap::new(),
        initial_latency_frames: 0,
        initial_tail: TailReport::Unknown,
        offline: false,
    };
    let instance = TokenDto::new(1);
    let token_base = (std::process::id() as u128) << 64
        | SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            & u64::MAX as u128;
    let binding = binding_for(instance, &contract, DEFAULT_MAX_EVENTS, token_base).unwrap();
    let mut transport =
        SharedBlockTransport::create(&contract, binding.clone(), DEFAULT_MAX_EVENTS).unwrap();
    let mut host = OutOfProcessPluginHost::launch(launch(&root)).unwrap();
    let plugin = PluginKey {
        format: PluginFormat::Clap,
        identifier: "dev.audec.fixture.gain".into(),
    };
    host.create_instance(InstanceRecipe {
        instance: 1,
        artifact_lease: 9,
        plugin: plugin.clone(),
        contract: contract.clone(),
        artifact: Some(ArtifactGrantDto {
            canonical_path: artifact.to_string_lossy().into_owned(),
            fingerprint: ArtifactDto::from_domain(&fingerprint).unwrap(),
        }),
        state: None,
        shared_memory: binding,
        parameters: vec![],
        activate: true,
    })
    .unwrap();

    let left = [1.0_f32, 1.0, 1.0, 1.0];
    let right = [0.5_f32, 0.5, 0.5, 0.5];
    let gain = InputEvent::Parameter(ParameterEvent {
        frame_offset: 2,
        key: PluginParameterKey::Clap(1),
        value: plugin::NormalizedValue::new(0.25).unwrap(),
    });
    transport
        .controller_write_inputs(4, &[&left, &right], &[gain])
        .unwrap();
    assert_eq!(host.process_block(1, 4, 1).unwrap(), 0);
    let mut output = vec![Vec::new(), Vec::new()];
    transport.controller_read_outputs(4, &mut output).unwrap();
    assert_eq!(output[0], vec![1.0, 1.0, 0.25, 0.25]);
    assert_eq!(output[1], vec![0.5, 0.5, 0.125, 0.125]);

    let state = host.save_state(1, "states/gain.bin".into(), 64).unwrap();
    assert_eq!(state.bytes, 0.25_f32.to_le_bytes());
    host.shutdown().unwrap();
    drop(transport);
    fs::remove_dir_all(root).unwrap();
}
