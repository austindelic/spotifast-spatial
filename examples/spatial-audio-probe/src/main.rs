//! Isolated hardware feasibility test. Does not change Spotifast playback.

#[cfg(any(target_os = "macos", test))]
mod signal;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    macos::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The spatial audio probe requires an Apple-silicon Mac with macOS 12.3 or later.");
    std::process::exit(1);
}
