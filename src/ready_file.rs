//! Atomic creation of an out-of-band readiness signal file.
//!
//! A follower (`tendr events --follow`, `tendr watch`) creates this file once
//! its baseline is established and its initial output is flushed, so
//! orchestrators — and tests — can observe "safe to perform live mutations now"
//! without contaminating the NDJSON stdout stream. The file is an empty
//! lifecycle marker, not a resumable cursor.

use std::io;
use std::path::Path;

/// Atomically publish an empty readiness marker at `path`.
///
/// Uses `create_new` (`O_EXCL` on Unix, `CREATE_NEW` on Windows), so publishing
/// is a genuine no-clobber, cross-platform race with exactly one winner: every
/// other concurrent creator gets [`io::ErrorKind::AlreadyExists`], and an
/// existing marker is never overwritten. The marker is empty, so there is no
/// partially written state a watcher could observe — the file's existence alone
/// is the signal.
///
/// # Errors
/// Returns [`io::ErrorKind::AlreadyExists`] if `path` already exists, or any
/// other I/O error from creating the file (e.g. a missing parent directory).
pub fn create_ready_file(path: &Path) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?; // empty marker; the handle drops (closes) here
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};

    #[test]
    fn creates_empty_marker_at_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready");
        create_ready_file(&path).unwrap();
        assert!(path.exists(), "ready file should exist after creation");
        assert_eq!(
            fs::read(&path).unwrap().len(),
            0,
            "ready file is an empty lifecycle marker"
        );
    }

    #[test]
    fn fails_if_destination_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready");
        fs::write(&path, b"pre-existing").unwrap();
        let err = create_ready_file(&path).expect_err("must reject an existing destination");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ready");
        create_ready_file(&path).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "only the ready file should remain: {names:?}"
        );
    }

    /// Publishing is a no-clobber race with exactly one winner: every other
    /// concurrent creator observes `AlreadyExists`, never a silent overwrite.
    #[test]
    fn concurrent_creators_have_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("ready"));
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait(); // maximise contention on the create
                    create_ready_file(&path)
                })
            })
            .collect();

        let results: Vec<io::Result<()>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.is_ok()).count();
        let already_exists = results
            .iter()
            .filter(|r| matches!(r, Err(e) if e.kind() == io::ErrorKind::AlreadyExists))
            .count();

        assert_eq!(winners, 1, "exactly one creator publishes the marker");
        assert_eq!(
            already_exists,
            n - 1,
            "every other concurrent creator must get AlreadyExists"
        );
    }
}
