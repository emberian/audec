//! Headless discovery smoke tool for the optional MIDI 1 backend.

use std::error::Error;

use audec::midi_input::{MidiInputBackend, MidirInputBackend};

fn main() -> Result<(), Box<dyn Error>> {
    let mut backend = MidirInputBackend::new("Audec MIDI inspector")?;
    let ports = backend.discover()?;
    if ports.is_empty() {
        println!("No MIDI input ports are currently available.");
        return Ok(());
    }
    println!(
        "MIDI input ports (discovery generation {}):",
        backend.generation()
    );
    for port in ports {
        println!("  {}: {}", port.token.ordinal(), port.display_name);
    }
    Ok(())
}
