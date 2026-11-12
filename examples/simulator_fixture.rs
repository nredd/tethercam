//! Generate a synthetic Sony-shaped JPEG without USB or network access.
//!
//! Run with `cargo run --example simulator_fixture -- synthetic.jpg`.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pass one output JPEG path"))?;
    if args.next().is_some() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "pass one output JPEG path").into(),
        );
    }
    let mut rgb = Vec::with_capacity(1024 * 680 * 3);
    for y in 0..680_u16 {
        for x in 0..1024_u16 {
            rgb.extend_from_slice(&[
                u8::try_from(x % 255)?,
                u8::try_from(y % 255)?,
                u8::try_from((x + y) % 255)?,
            ]);
        }
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 85);
    encoder.set_sampling_factor(SamplingFactor::F_2_1);
    encoder.encode(&rgb, 1024, 680, ColorType::Rgb)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    writeln!(
        io::stdout().lock(),
        "Wrote a synthetic 1024x680 JPEG; no camera accessed."
    )?;
    Ok(())
}
