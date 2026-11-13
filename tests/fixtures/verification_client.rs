//! Synthetic subprocess fixture for verifier lifecycle tests, never a network client.
//!
//! References:
//! - <https://ffmpeg.org/ffmpeg.html>
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let exe = std::env::current_exe()?;
    let name = exe.file_name().and_then(OsStr::to_str).unwrap_or_default();
    if name.contains("fail")
        && (!name.contains("auth") || args.first().is_some_and(|arg| arg == "--config"))
    {
        return Err(io::Error::other("injected fixture failure"));
    }
    if args.first().is_some_and(|arg| arg == "serve-owned") {
        io::copy(&mut io::stdin().lock(), &mut io::sink())?;
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "run") {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return Ok(());
    }
    if name == "decode-corrupt"
        && let Some([_, path]) = args
            .windows(2)
            .find(|pair| pair.first().is_some_and(|arg| arg == "-i"))
        && std::fs::read(path)?.ends_with(&[0xff, 0xd8, 0xff, 0xd9])
        && args.iter().any(|arg| arg == "-xerror")
    {
        return Err(io::Error::other("injected corrupt trailing frame"));
    }
    let mut out = io::stdout().lock();
    if args.iter().any(|arg| arg == "%{http_code}") {
        let status = if args.first().is_some_and(|arg| arg == "--config") {
            if name.contains("bad200") {
                "401"
            } else {
                "200"
            }
        } else if name.contains("bad401") {
            "200"
        } else {
            "401"
        };
        for pair in args.windows(2) {
            if let [flag, path] = pair
                && flag == "-o"
                && path != "/dev/null"
            {
                std::fs::write(Path::new(path), [1, 2, 3])?;
            }
        }
        write!(out, "{status}")?;
        let remove = exe.with_extension("remove");
        if remove.is_file() {
            let target = std::fs::read_to_string(remove)?;
            let target = Path::new(&target);
            if target.is_file() {
                let root = exe
                    .parent()
                    .ok_or_else(|| io::Error::other("missing fixture root"))?
                    .canonicalize()?;
                let canonical = target.canonicalize()?;
                if !canonical.starts_with(root)
                    || !matches!(
                        target.file_name().and_then(OsStr::to_str),
                        Some("unauthenticated-status.txt" | "authenticated-status.txt")
                    )
                {
                    return Err(io::Error::other(
                        "fixture deletion outside owned status outputs",
                    ));
                }
                std::fs::remove_file(target)?;
            }
        }
    } else if args.iter().any(|arg| arg == "-show_packets") {
        write!(
            out,
            "{{\"packets\":[{{\"pts_time\":\"0\"}},{{\"pts_time\":\"1\"}}]}}"
        )?;
    } else if args.iter().any(|arg| arg == "-show_entries") {
        write!(out, "{{\"streams\":[{{\"codec_name\":\"synthetic\"}}]}}")?;
    } else {
        for pair in args.windows(2) {
            if let [flag, path] = pair
                && flag == "-y"
            {
                std::fs::write(Path::new(path), [0; 16])?;
            }
        }
    }
    Ok(())
}
