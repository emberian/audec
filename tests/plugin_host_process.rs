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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plugin::{PluginFormat, PluginKey, ProcessingContract, TailReport};
use plugin_wire::{
    ParameterKeyDto, ParameterValueDto, SharedMemoryAccessDto, SharedMemoryBindingDto,
    SharedMemoryRegionDto, TokenDto,
};
use plugin_worker::{
    HostError, HostErrorKind, HostHealth, InstanceRecipe, OutOfProcessPluginHost, WorkerLaunch,
    FAKE_CLAP_ID,
};

fn temporary_root(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("audec-host-{label}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn launch(root: &PathBuf, mode: Option<&str>) -> WorkerLaunch {
    let mut launch = WorkerLaunch::new(
        PathBuf::from(env!("CARGO_BIN_EXE_audec-fake-plugin")),
        root.clone(),
    );
    launch.arguments = vec!["--session-root".into(), root.to_string_lossy().into_owned()];
    if let Some(mode) = mode {
        launch.arguments.push(mode.into());
    }
    launch.startup_timeout = Duration::from_secs(2);
    launch.request_timeout = Duration::from_millis(200);
    launch
}

fn region(token: u128, access: SharedMemoryAccessDto) -> SharedMemoryRegionDto {
    SharedMemoryRegionDto {
        token: TokenDto::new(token),
        byte_len: 4096,
        access,
    }
}

fn recipe() -> InstanceRecipe {
    InstanceRecipe {
        instance: 1,
        artifact_lease: 99,
        plugin: PluginKey {
            format: PluginFormat::Clap,
            identifier: FAKE_CLAP_ID.into(),
        },
        contract: ProcessingContract {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 512,
            audio_ports: vec![],
            note_inputs: BTreeMap::new(),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 16,
            initial_tail: TailReport::FiniteFrames(64),
            offline: false,
        },
        state: None,
        shared_memory: SharedMemoryBindingDto {
            instance: TokenDto::new(1),
            audio_inputs: region(10, SharedMemoryAccessDto::HostWrites),
            audio_outputs: region(11, SharedMemoryAccessDto::WorkerWrites),
            events_to_worker: region(12, SharedMemoryAccessDto::HostWrites),
            events_from_worker: region(13, SharedMemoryAccessDto::WorkerWrites),
        },
        parameters: vec![ParameterValueDto {
            key: ParameterKeyDto::Clap { id: 1 },
            normalized: 0.25,
        }],
        activate: true,
    }
}

#[test]
fn isolated_lifecycle_process_and_state_are_end_to_end() {
    let root = temporary_root("lifecycle");
    let mut host = OutOfProcessPluginHost::launch(launch(&root, None)).unwrap();
    let status = host.create_instance(recipe()).unwrap();
    assert!(status.active);
    assert_eq!(status.latency_frames, 16);
    assert_eq!(status.tail, TailReport::FiniteFrames(64));
    assert_eq!(host.process_block(1, 128, 0).unwrap(), 0);

    host.set_parameters(
        1,
        vec![ParameterValueDto {
            key: ParameterKeyDto::Clap { id: 1 },
            normalized: 0.75,
        }],
    )
    .unwrap();
    let state = host
        .save_state(1, "states/fixture.bin".into(), 1024)
        .unwrap();
    assert!(String::from_utf8(state.bytes)
        .unwrap()
        .contains("gain=0.75000000000000000"));
    assert_eq!(host.diagnostics().completed_process_blocks, 1);
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn process_crash_is_isolated_and_recipe_can_be_recovered() {
    let root = temporary_root("crash");
    let mut host =
        OutOfProcessPluginHost::launch(launch(&root, Some("--crash-on-process"))).unwrap();
    host.create_instance(recipe()).unwrap();
    let error = host.process_block(1, 128, 0).unwrap_err();
    assert!(matches!(
        error,
        HostError::ProcessFailure {
            kind: HostErrorKind::Crashed,
            ..
        }
    ));
    assert_eq!(host.health(), HostHealth::Failed);
    assert_eq!(host.diagnostics().crashes, 1);

    host.recover().unwrap();
    assert_eq!(host.health(), HostHealth::Ready);
    assert_eq!(host.instance(1).unwrap().recovery_count, 1);
    assert!(host.instance(1).unwrap().active);
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn process_deadline_kills_a_hung_worker_without_blocking_recovery() {
    let root = temporary_root("timeout");
    let mut host =
        OutOfProcessPluginHost::launch(launch(&root, Some("--hang-on-process"))).unwrap();
    host.create_instance(recipe()).unwrap();
    let error = host.process_block(1, 128, 0).unwrap_err();
    assert!(matches!(
        error,
        HostError::ProcessFailure {
            kind: HostErrorKind::Timeout,
            ..
        }
    ));
    assert_eq!(host.diagnostics().timeouts, 1);
    host.recover().unwrap();
    assert_eq!(host.instance(1).unwrap().recovery_count, 1);
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}
