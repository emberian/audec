# MIDI 1 input foundation

Status: feature-gated control/device foundation. No GPUI surface, MIDI thru,
MIDI output, MPE/MIDI 2, or claim of durable recording is included yet.

Enable the native backend explicitly:

```sh
cargo run --features midi-input --bin audec-midi-inspect
```

The default build does not compile Midir, Wmidi, or Rtrb. The optional path is
split into four responsibilities:

1. `MidirInputBackend` discovers ports and provides a current-generation token
   for exact selection. Only the visible port name may be retained as a relink
   preference. Relinking refuses missing and duplicate names; backend IDs,
   indexes, and handles never become project identity.
2. The native callback validates MIDI 1 packets with Wmidi and admits note-on,
   note-off, CC, and 14-bit pitch bend into one preallocated Rtrb SPSC queue.
   It does not allocate, lock, log, schedule an instrument, or edit a project.
3. `MidiClockMapper` maps connection-stable microsecond timestamps through an
   explicit paired calibration into signed project frames. The controller must
   recalibrate after reconnect, seek, transport discontinuity, or sample-rate
   change. Because Midir deliberately leaves the timestamp origin unspecified,
   `calibrate_at_next_event` pairs the first admitted timestamp with an
   authoritative transport/audio frame. Configured input latency is subtracted
   in frames.
4. `MidiControlIngress::record_into_commands` runs on the control side. A
   caller-owned lowerer can correlate note pairs and construct authoritative
   commands; only a caller-owned command authority can accept them. Queueing an
   observation is not durable recording and there is no second realtime graph.

Diagnostics distinguish malformed packets, valid-but-unsupported packets,
queue-full xruns/drops, peak queue depth, timestamp regressions, saturated frame
mappings, and command refusals. Overflow is drop-newest and never blocks the
device callback.

## Platform refusal and permissions

- macOS/CoreMIDI can return no ports or reject a connection when a device is
  disconnected, unavailable to the process, or captured by another client.
  Audec reports initialization, inspection, disappearance, rename, and connect
  failures instead of silently selecting another port.
- Linux uses ALSA sequencing by default. Distribution packages need ALSA
  development files to compile the feature, and the running user must be able
  to access the sequencer devices. Container/sandbox sessions often expose no
  MIDI ports; that is reported as an empty discovery result.
- Windows uses WinMM by default. Device privacy policy, driver removal, and
  exclusive-client behavior surface as discovery/connect refusal.
- iOS, Android, Web MIDI, JACK, Bluetooth pairing, and application entitlement
  policy need platform-specific product work before being advertised. The
  feature flag alone is not a permission or packaging declaration.

The Midir source is pinned to audited upstream commit
`3406042797e190ae5985e84bbedbc2d475325272`. Published 0.10.3 conflicts with
Audec's current Linux CPAL/ALSA dependency, while published 0.11.0's CoreMIDI
line conflicts with GPUI 0.2.2's exact CoreFoundation pin. The chosen commit
contains the intended 0.11 ALSA range and the compatible CoreMIDI 0.8 adapter.
