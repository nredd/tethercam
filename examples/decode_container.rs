//! Build a PTP `OpenSession` command and decode it back.
//!
//! Run with `cargo run --example decode_container`.

use std::error::Error;
use std::io::{self, Write};

use tethercam::ptp::Container;

/// PTP `OpenSession` operation code.
const OPEN_SESSION: u16 = 0x1002;

fn main() -> Result<(), Box<dyn Error>> {
    let wire = Container::command(OPEN_SESSION, 0, &[1])?;
    let (container, _) = Container::decode(&wire)?;

    let mut out = io::stdout().lock();
    writeln!(out, "wire:   {wire:02x?}")?;
    writeln!(out, "kind:   {:?}", container.kind)?;
    writeln!(out, "code:   {:#06x}", container.code)?;
    writeln!(out, "params: {:?}", container.params().collect::<Vec<_>>())?;
    Ok(())
}
