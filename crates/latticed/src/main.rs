//! Composition root: wires implementations to traits and runs the server.
//!
//! Layer L7. Must not: Contain logic that belongs in a library crate.
#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout
)]

fn main() {
    if std::env::args()
        .nth(1)
        .is_some_and(|a| a == "--version" || a == "-V")
    {
        println!("latticed {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    println!("latticed: Phase 0 skeleton (layer 7)");
}
