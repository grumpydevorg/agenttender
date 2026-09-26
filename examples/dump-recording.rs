//! Print a PTY recording as plain text, one line per record, with terminal
//! queries and mode changes flagged (see `tendr::recording::dump`).
//!
//! ```text
//! cargo run --example dump-recording -- <path>
//! ```
//!
//! `<path>` is one segment file (`seg-00000000.tndrrec`), a run's recording
//! directory (`<session>/recording/<run_id>/`, whose segments are decoded
//! together in order), or a directory of runs (`<session>/recording/`, or the
//! session directory itself), each run dumped in turn.
//!
//! A development aid: examples are not in the published crate, and the release
//! workflow builds only `--bin tendr`.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tendr::recorder::DirectoryStore;
use tendr::recording::{DecodedRecording, decode_recording, decode_segment, dump};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // `dump-recording … | head` closes the pipe early; that is not a failure.
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dump-recording: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        bail!("usage: dump-recording <segment file | recording directory>");
    };
    let path = PathBuf::from(path);
    let mut out = BufWriter::new(io::stdout().lock());

    if path.is_file() {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let segment =
            decode_segment(&bytes).with_context(|| format!("decoding {}", path.display()))?;
        let recording = DecodedRecording {
            header: segment.header,
            segment_count: 1,
            records: segment.records,
            end: segment.end,
        };
        dump(&recording, &mut out)?;
    } else {
        let runs = runs(&path)?;
        if runs.is_empty() {
            bail!("no recording segments at or under {}", path.display());
        }
        for (i, (dir, segments)) in runs.iter().enumerate() {
            if runs.len() > 1 {
                if i > 0 {
                    writeln!(out)?;
                }
                writeln!(out, "== {}", dir.display())?;
            }
            let data = segments
                .iter()
                .map(|p| std::fs::read(p).with_context(|| format!("reading {}", p.display())))
                .collect::<Result<Vec<_>>>()?;
            let recording = decode_recording(data.iter().map(Vec::as_slice))
                .with_context(|| format!("decoding {}", dir.display()))?;
            dump(&recording, &mut out)?;
        }
    }
    out.flush()?;
    Ok(())
}

/// The recording directories at `path`, each with its segments in order:
/// `path` itself if it holds segments, otherwise every subdirectory of
/// `path/recording` (or of `path`) that does, in name order. Run ids are
/// UUIDv7, so name order is start order.
fn runs(path: &Path) -> Result<Vec<(PathBuf, Vec<PathBuf>)>> {
    let list = |dir: &Path| {
        DirectoryStore::segments(dir).with_context(|| format!("listing {}", dir.display()))
    };
    let segments = list(path)?;
    if !segments.is_empty() {
        return Ok(vec![(path.to_path_buf(), segments)]);
    }
    let nested = path.join("recording");
    let root = if nested.is_dir() {
        nested
    } else {
        path.to_path_buf()
    };
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("listing {}", root.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    let mut runs = Vec::new();
    for dir in dirs {
        let segments = list(&dir)?;
        if !segments.is_empty() {
            runs.push((dir, segments));
        }
    }
    Ok(runs)
}
