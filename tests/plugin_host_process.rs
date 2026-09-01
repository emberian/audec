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

use plugin::{
    ClapDiscoveryLimits, PluginFormat, PluginIndex, PluginKey, ProcessingContract, ScanCacheEntry,
    TailReport,
};
use plugin_wire::{
    ParameterKeyDto, ParameterValueDto, SharedMemoryAccessDto, SharedMemoryBindingDto,
    SharedMemoryRegionDto, TokenDto,
};
use plugin_worker::transport::{binding_for, SharedBlockTransport, DEFAULT_MAX_EVENTS};
use plugin_worker::{
    HostError, HostErrorKind, HostHealth, InstanceRecipe, OutOfProcessPluginHost,
    PluginCatalogRefreshOutcome, PluginCatalogRefreshPolicy, ProcessBlockOutcome, WorkerLaunch,
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
        artifact: None,
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
    let mut recipe = recipe();
    let nonce = (std::process::id() as u128) << 64
        | SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            & u64::MAX as u128;
    recipe.shared_memory = binding_for(
        TokenDto::new(recipe.instance),
        &recipe.contract,
        DEFAULT_MAX_EVENTS,
        nonce,
    )
    .unwrap();
    let mut transport = SharedBlockTransport::create(
        &recipe.contract,
        recipe.shared_memory.clone(),
        DEFAULT_MAX_EVENTS,
    )
    .unwrap();
    host.create_instance(recipe).unwrap();
    let outcome = host.process_block_or_silence(1, 128, 0, &mut transport);
    assert!(matches!(outcome, ProcessBlockOutcome::Silenced { .. }));
    assert_eq!(host.health(), HostHealth::Failed);
    assert_eq!(host.diagnostics().crashes, 1);
    assert_eq!(host.diagnostics().silenced_process_blocks, 1);

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

#[test]
fn catalog_refresh_discovers_scans_and_reuses_verified_artifacts() {
    let root = temporary_root("catalog");
    let plugins = root.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(plugins.join("first.clap"), b"first plugin bytes").unwrap();
    fs::write(plugins.join("second.clap"), b"second plugin bytes").unwrap();
    let mut host = OutOfProcessPluginHost::launch(launch(&root, None)).unwrap();
    let mut index = PluginIndex::default();
    let policy = PluginCatalogRefreshPolicy {
        discovery: ClapDiscoveryLimits {
            maximum_depth: 2,
            maximum_entries: 16,
            maximum_candidates: 4,
        },
        maximum_artifact_bytes: 1024,
        scan_timeout_millis: 1_000,
        maximum_descriptors: 8,
        maximum_parameters_per_plugin: 128,
        quarantine_after: 2,
    };

    let first = host
        .refresh_clap_catalog(&mut index, [plugins.clone()], policy)
        .unwrap();
    assert_eq!(first.discovery.candidates.len(), 2);
    assert_eq!(first.entries.len(), 2);
    assert!(first.entries.iter().all(|entry| matches!(
        &entry.outcome,
        PluginCatalogRefreshOutcome::ScannedReady { descriptors: 1 }
    )));
    assert_eq!(index.entries().len(), 2);

    let second = host
        .refresh_clap_catalog(&mut index, [plugins], policy)
        .unwrap();
    assert!(second.entries.iter().all(|entry| matches!(
        &entry.outcome,
        PluginCatalogRefreshOutcome::CachedReady { descriptors: 1 }
    )));
    assert_eq!(second.worker_recoveries, 0);
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn catalog_refresh_recovers_crashed_scanner_and_quarantines_identical_bytes() {
    let root = temporary_root("catalog-crash");
    let plugins = root.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let artifact = plugins.join("hostile.clap");
    fs::write(&artifact, b"hostile plugin bytes").unwrap();
    let artifact = fs::canonicalize(artifact).unwrap();
    let mut host = OutOfProcessPluginHost::launch(launch(&root, Some("--crash-on-scan"))).unwrap();
    let mut index = PluginIndex::default();
    let policy = PluginCatalogRefreshPolicy {
        discovery: ClapDiscoveryLimits {
            maximum_depth: 2,
            maximum_entries: 8,
            maximum_candidates: 2,
        },
        maximum_artifact_bytes: 1024,
        scan_timeout_millis: 500,
        maximum_descriptors: 8,
        maximum_parameters_per_plugin: 128,
        quarantine_after: 2,
    };

    for expected_failures in [1, 2] {
        let report = host
            .refresh_clap_catalog(&mut index, [plugins.clone()], policy)
            .unwrap();
        assert_eq!(report.worker_recoveries, 1);
        assert!(matches!(
            &report.entries[0].outcome,
            PluginCatalogRefreshOutcome::ScannedFailed {
                consecutive_failures,
                quarantined,
                ..
            } if *consecutive_failures == expected_failures && *quarantined == (expected_failures == 2)
        ));
    }
    assert!(matches!(
        index.entries().get(&artifact),
        Some(ScanCacheEntry::Quarantined {
            consecutive_failures: 2,
            ..
        })
    ));
    let cached = host
        .refresh_clap_catalog(&mut index, [plugins], policy)
        .unwrap();
    assert!(matches!(
        &cached.entries[0].outcome,
        PluginCatalogRefreshOutcome::CachedQuarantined {
            consecutive_failures: 2
        }
    ));
    host.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}
