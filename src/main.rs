//! Audec desktop executable.
//!
//! Process argument discovery stays here; GPUI/Guise setup and Audec service
//! installation live behind the library's desktop application boundary.

fn main() {
    audec::audec_app::run(audec::audec_app::LaunchOptions::from_process_arguments());
}
