//! Stable module boundary for the UI-independent project controller.
//!
//! The implementation currently lives beside `LiveProject` while the legacy
//! lock façade is retired. Consumers should import through this module so the
//! implementation can move without changing GPUI, render, or persistence.

#![allow(unused_imports)]

#[path = "constructive_controller.rs"]
mod constructive_controller;
#[path = "workbench_sampling.rs"]
mod workbench_sampling;

pub use constructive_controller::{
    ConstructiveControllerError, ConstructiveOutcome, ConstructivePublication,
    ConstructivePublishedFocus, ConstructiveSourceSnapshot, SampleActionOutcome,
};
pub use workbench_sampling::{
    WorkbenchSampleIntent, WorkbenchSampleOutcome, WorkbenchSamplingError,
};

pub use crate::live_project::{
    ProjectController, ProjectControllerConfig, ProjectControllerError, ProjectControllerUpdate,
    ProjectDomainOwnership,
};

pub use crate::command_journal::{
    decode_runtime_frame, encode_runtime_record, encode_runtime_records, CommandJournalRecord,
    CommandOperation, RuntimeCommandCodec, RuntimeJournalEncodeError,
};
