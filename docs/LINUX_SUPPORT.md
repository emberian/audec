# Linux support status

This document distinguishes code that is portable from desktop runtime support.
`Linux` below means a native `x86_64-unknown-linux-gnu` Ubuntu build. The
current tier is **developer preview**: CI compiles the complete application,
opens its real GPUI main window under virtual X11, and executes isolated worker
DSP. There is still no downloadable Linux package or physical audio-device /
Wayland certification.

| Area | Status | Evidence / constraint |
| --- | --- | --- |
| Domain, project codecs, persistence, analysis, deterministic rendering | **compile-gated** | These modules are still owned by the `audec` binary rather than a separate core crate. Ubuntu CI compiles the app test harness with `cargo test --bin audec --no-run`; it does not assert a GUI-free core runtime. |
| Fake worker, model/worker protocol, plugin-host crash isolation | **CI-gated** | Ubuntu CI builds all three worker binaries and runs `plugin_host_process` and `clap_worker_process`. Those tests use disposable subprocesses and do not need a desktop or audio device. |
| Real CLAP worker | **process-runtime-gated** | A permissively licensed CLAP fixture is compiled as a native Linux shared object, loaded in `audec-clap-worker`, and processes sample-accurate parameter changes over named shared memory in a subprocess. This proves the isolated worker/transport slice, not arbitrary third-party-plugin compatibility or complete in-app plugin routing. |
| GPUI on X11 | **virtual-runtime-gated** | GPUI 0.2.2 enables X11 upstream. Ubuntu CI starts the real `audec` binary under Xvfb with Mesa software Vulkan, finds its visible window by the GPUI-published process ID, checks the Audec title/window resource, and requires the event/render process to remain alive after mapping. |
| GPUI on Wayland | **compile-gated** | The same dependency build enables Wayland, but CI has no nested Wayland compositor and does not open a Wayland window. |
| Native Open/Save dialogs | **build-gated only** | The application calls GPUI's `prompt_for_paths` / `prompt_for_new_path`. GPUI's Linux implementation uses the XDG desktop portal. A real session needs `xdg-desktop-portal` and a desktop-specific portal backend; CI does not exercise either. |
| Window decorations, menus, fonts, clipboard, multiwindow layout | **startup only on X11** | The smoke gate verifies one visible main window, not pixels, keyboard/IME, clipboard, menus, dialogs, close behavior, or floating-window interaction. The workspace reserves macOS traffic-light geometry only behind `cfg(target_os = "macos")`, but floating windows still request transparent titlebars. Test both X11 and Wayland compositors before advertising desktop support. |
| Audio output | **build- and model-gated only** | Default builds use Rodio; `cpal-device` builds replace it behind the same application audio-host contract with one direct CPAL stream for project and preview audio. The direct callback and recovery service have backend-model regressions, but no hosted runner supplies a real ALSA/PipeWire device, so opening, latency, XRUN behavior, and routing remain unverified. |
| Packaging, desktop entry, sandboxing | **not provided** | The X11 smoke runs a development binary. There is no icon set, `.desktop` metadata, install layout, Flatpak/Snap manifest, or packaged-install smoke. The repository intentionally does not ship a package skeleton that would imply distribution support. |

## Ubuntu build prerequisites

For the current GPUI 0.2.2 dependency graph, install Rust stable and the same
development packages as the `gpui-build` CI job. At minimum, ALSA requires
`pkg-config libasound2-dev`; GPUI enables X11 and Wayland and uses Vulkan,
Fontconfig, XKB, and XDG portal APIs on Linux.

```sh
sudo apt-get update
sudo apt-get install --yes \
  build-essential pkg-config libasound2-dev libdbus-1-dev libegl1-mesa-dev \
  libfontconfig1-dev libssl-dev libvulkan-dev libwayland-dev libx11-dev \
  libx11-xcb-dev libxcb1-dev libxcb-icccm4-dev libxcb-image0-dev \
  libxcb-keysyms1-dev libxcb-randr0-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-util0-dev libxcb-xfixes0-dev libxkbcommon-dev libxkbcommon-x11-dev
cargo build --locked --release --bin audec
```

For a developer desktop session, run `scripts/launch-linux.sh` after the
build. The launcher resolves the binary relative to the checkout while leaving
the caller's working directory intact, so relative project/audio paths retain
their expected meaning. It refuses a headless shell; it does not select an
audio backend or modify compositor, portal, or GPU settings.

The runtime packages depend on the desktop. A typical native installation also
needs a Vulkan driver, `xdg-desktop-portal` plus one suitable portal backend,
and working ALSA devices (directly or through PipeWire's ALSA compatibility).
Audec intentionally continues to use Rodio/CPAL as its sole device engine.

## CI contract

`scripts/linux-ci.sh workers` is the portable executable/process gate.
`scripts/linux-ci.sh app` is the whole GPUI compile gate.
`scripts/linux-ci.sh desktop-smoke` consumes that exact debug binary inside a
caller-provided X11 session. GitHub Actions supplies Xvfb, a D-Bus session, and
Mesa's software Vulkan driver; the bounded smoke requires a visible Audec
window and then terminates only the process it launched. It never loads source
audio, opens an audio device, or exercises a portal.

Before changing the tier from developer preview to supported, add hardware or
VM runners that cover at least one named X11 window manager and one named
Wayland compositor; verify file dialogs, text/key input, clipboard, multiwindow
behavior, clean close/quit, and audio-device selection/fallback separately; and
ship a reproducible package with desktop metadata.

## What the X11 smoke refuses to claim

Xvfb plus Mesa lavapipe is useful because it catches missing shared libraries,
GPUI platform initialization failures, Vulkan/swapchain failures, main-window
construction panics, and immediate event-loop/render crashes. It is not a
visual regression test and it cannot certify compositor decorations, GPU
drivers, HiDPI, portals, PipeWire/ALSA routing, latency, or XRUN behavior.
