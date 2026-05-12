//! CLI smoke-test for the FT8 vendor crate. Right now it only proves
//! the static lib is linkable; a follow-up turn fills in real decode
//! against a 12 kHz mono WAV (matching the wsjt-x test corpus shape).

fn main() {
    eprintln!(
        "ferrite-ft8 v{} — decoder wrapper not yet implemented",
        ferrite_ft8::version()
    );
    eprintln!("(see workspace task list — Ft8Demod block lands in the next turn)");
    std::process::exit(2);
}
