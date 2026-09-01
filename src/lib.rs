//! Audec's application, project, render, and analysis implementation.
//!
//! This library target is the first dependency boundary in the former
//! all-in-one executable crate. It deliberately preserves the existing module
//! topology while allowing the desktop executable and future headless tools to
//! consume Audec through narrow public entry points. Stable public APIs should
//! be added as explicit facades; the implementation modules below remain
//! crate-private until a real downstream contract needs them.

mod air_query;
mod analysis;
mod app_controller;
mod arrangement;
mod arrangement_interaction;
mod arrangement_view;
mod artifact_catalog;
mod artifact_promotion_bridge;
mod aspect;
mod asset_view;
mod assets;
mod audio;
mod audio_host;
mod automation;
mod beat_this;
mod change_set;
mod command;
mod command_journal;
mod command_record;
pub mod compiled_audio_graph;
mod comparison;
mod comparison_controller;
mod comparison_runtime;
mod constructive;
mod control_views;
mod coverage;
mod cqt;
mod curve_lang;
#[cfg(test)]
mod cycle10_acceptance;
#[cfg(test)]
mod cycle9_acceptance;
mod daw_engine;
mod daw_project;
mod daw_render;
mod decomposition;
mod deprojection_evaluation;
mod deprojection_execution;
mod deprojection_program;
pub mod device_service;
#[cfg(test)]
mod engine_regression;
mod explanation;
mod explanation_adapters;
mod explanation_pane_model;
mod explanation_workbench_view;
mod explorer_model;
mod export;
mod file_actions;
mod generative_lowering;
mod generative_ontology;
mod hpss;
mod inference_recipe;
mod instruments;
mod interpretation;
mod interpretation_navigation;
mod lens;
mod live_project;
mod loom;
mod media_resolver;
#[cfg(feature = "midi-input")]
pub mod midi_input;
mod mixer;
mod model_claim;
mod model_registry;
mod model_store;
mod model_supervisor;
mod model_task_service;
mod model_wire;
mod model_worker;
mod nmfd;
mod ontology;
mod pane_audio;
mod pane_session_binding;
mod pattern_actions;
mod pattern_authoring;
mod pattern_controller;
mod pattern_lang;
mod pattern_use_graph;
mod persistence;
mod pitch;
mod plugin;
mod product_action_router;
mod product_input;
mod project;
mod project_audio_controller;
mod project_codecs;
mod project_controller;
mod project_format;
mod project_io;
mod project_repository;
mod project_selection;
mod project_session;
mod project_store;
mod pyramid;
mod reading;
mod reading_codec;
mod reading_query_view;
mod reconstruction;
mod reconstruction_apply;
mod render;
mod render_dependencies;
mod render_dependency_runtime;
mod render_plan;
mod render_products;
mod render_runtime;
mod render_service;
mod render_tiles;
mod render_validation;
mod reverse_navigation;
mod reverse_surface;
mod reverse_surface_view;
mod rhythm;
mod rhythm_explanation;
mod runtime_command_codec;
mod sample_actions;
mod sample_kit;
mod sample_material;
mod sampler_runtime;
mod sampler_view;
mod selection_aspect_service;
mod sequencer;
mod sequencer_view;
mod session;
mod settings;
mod spectral_tiles;
pub mod streaming_media;
pub mod task_coordinator;
mod timeline;
pub mod timeline_scene_index;
mod transport_handoff_controller;
mod ui;
mod ui_actions;
mod ui_drag;
mod ui_platform;
mod view_links;
mod waveform_proxy;
pub mod worker_runtime;
mod workspace;
mod workspace_document;
mod workspace_items;
mod workspace_presenter;
mod workspace_session_layout;
mod workspace_ui;

/// Desktop application facade. GPUI and Guise remain implementation details
/// behind this module rather than leaking into executable entry points.
pub mod audec_app;
