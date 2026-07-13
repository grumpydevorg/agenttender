use std::num::NonZeroU32;
use tempfile::TempDir;
use tender::model::ids::{
    EpochTimestamp, Generation, Namespace, ProcessIdentity, RunId, SessionName,
};
use tender::model::meta::Meta;
use tender::model::spec::LaunchSpec;
#[cfg(unix)]
use tender::session::LockGuard;
use tender::session::{self, SessionError, SessionRoot};

fn tmp_root() -> (TempDir, SessionRoot) {
    let dir = TempDir::new().unwrap();
    let root = SessionRoot::new(dir.path().to_path_buf());
    (dir, root)
}

fn default_ns() -> Namespace {
    Namespace::default_namespace()
}

fn test_sidecar() -> ProcessIdentity {
    ProcessIdentity {
        pid: NonZeroU32::new(100).unwrap(),
        start_time_ns: 1000,
    }
}

fn test_meta(name: &str) -> Meta {
    Meta::new_starting(
        SessionName::new(name).unwrap(),
        RunId::new(),
        Generation::first(),
        LaunchSpec::new(vec!["echo".into(), "hello".into()]).unwrap(),
        test_sidecar(),
        EpochTimestamp::now(),
    )
}

// === Create ===

#[test]
fn create_session_dir() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();
    assert!(session.path().exists());
    assert!(session.path().is_dir());
}

#[test]
fn create_already_exists() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    session::create(&root, &default_ns(), &name).unwrap();
    let err = session::create(&root, &default_ns(), &name).unwrap_err();
    assert!(matches!(err, SessionError::AlreadyExists(_)));
}

#[test]
fn create_nested_root() {
    let dir = TempDir::new().unwrap();
    let root = SessionRoot::new(dir.path().join("deep").join("nested").join("sessions"));
    let name = SessionName::new("job").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();
    assert!(session.path().exists());
}

// === Open ===

#[test]
fn open_existing_with_meta() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();
    // Must write meta before open will accept it
    session::write_meta_atomic(&session, &test_meta("upload")).unwrap();

    let opened = session::open(&root, &default_ns(), &name).unwrap();
    assert!(opened.is_some());
}

#[test]
fn open_dir_without_meta_returns_corrupt() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("empty").unwrap();
    session::create(&root, &default_ns(), &name).unwrap();
    // Don't write meta
    let err = session::open(&root, &default_ns(), &name).unwrap_err();
    assert!(matches!(err, SessionError::Corrupt { .. }));
}

#[test]
fn open_nonexistent_returns_none() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("nope").unwrap();
    let session = session::open(&root, &default_ns(), &name).unwrap();
    assert!(session.is_none());
}

// === List ===

#[test]
fn list_empty_root() {
    let (_dir, root) = tmp_root();
    let sessions = session::list(&root, None).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn list_nonexistent_root() {
    let root = SessionRoot::new("/tmp/tender-nonexistent-test-root".into());
    let sessions = session::list(&root, None).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn list_returns_created_sessions_sorted() {
    let (_dir, root) = tmp_root();
    let names = ["charlie", "alpha", "bravo"];
    for n in &names {
        let name = SessionName::new(n).unwrap();
        session::create(&root, &default_ns(), &name).unwrap();
    }
    let sessions = session::list(&root, None).unwrap();
    let result: Vec<&str> = sessions.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(result, vec!["alpha", "bravo", "charlie"]);
}

#[test]
fn list_skips_invalid_entries() {
    let (_dir, root) = tmp_root();
    // Create a valid session
    let name = SessionName::new("valid").unwrap();
    session::create(&root, &default_ns(), &name).unwrap();
    // Create an invalid entry (starts with dot)
    std::fs::create_dir(root.path().join(".hidden")).unwrap();
    // Create a file (not a directory)
    std::fs::write(root.path().join("not-a-dir"), "").unwrap();

    let sessions = session::list(&root, None).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].1.as_str(), "valid");
}

// === Meta read/write ===

#[test]
fn write_then_read_meta() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    let meta = test_meta("upload");
    session::write_meta_atomic(&session, &meta).unwrap();

    let back = session::read_meta(&session).unwrap();
    assert_eq!(back.session().as_str(), "upload");
    assert_eq!(back.schema_version(), Meta::SCHEMA_VERSION);
}

#[test]
fn read_meta_missing_returns_corrupt() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("empty").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();
    let err = session::read_meta(&session).unwrap_err();
    assert!(matches!(err, SessionError::Corrupt { .. }));
}

#[test]
fn read_meta_invalid_json_returns_corrupt() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("bad").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();
    std::fs::write(session.meta_path(), "not json").unwrap();
    let err = session::read_meta(&session).unwrap_err();
    assert!(matches!(err, SessionError::Corrupt { .. }));
}

#[test]
fn atomic_write_no_tmp_left_behind() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    let meta = test_meta("upload");
    session::write_meta_atomic(&session, &meta).unwrap();

    // .tmp file should not exist after successful write
    assert!(!session.path().join("meta.json.tmp").exists());
    // meta.json should exist
    assert!(session.meta_path().exists());
}

#[test]
fn atomic_write_overwrites_existing() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    // Write first version
    let meta1 = test_meta("upload");
    session::write_meta_atomic(&session, &meta1).unwrap();
    let run_id_1 = session::read_meta(&session).unwrap().run_id();

    // Write second version (different run_id)
    let meta2 = test_meta("upload");
    session::write_meta_atomic(&session, &meta2).unwrap();
    let run_id_2 = session::read_meta(&session).unwrap().run_id();

    assert_ne!(run_id_1, run_id_2);
}

// === Lock ===

#[cfg(unix)]
#[test]
fn lock_acquire_and_drop() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    let guard = LockGuard::try_acquire(&session).unwrap();
    assert!(session.lock_path().exists());
    drop(guard);
}

#[cfg(unix)]
#[test]
fn lock_exclusivity_across_try_acquire() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    let _guard = LockGuard::try_acquire(&session).unwrap();
    // Second try_acquire should fail with Locked
    let err = LockGuard::try_acquire(&session).unwrap_err();
    assert!(matches!(err, SessionError::Locked(_)));
}

#[cfg(unix)]
#[test]
fn lock_released_on_drop() {
    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    {
        let _guard = LockGuard::try_acquire(&session).unwrap();
    }
    // After drop, should be able to acquire again
    let _guard2 = LockGuard::try_acquire(&session).unwrap();
}

#[cfg(unix)]
#[test]
fn lock_exclusivity_across_processes() {
    use std::process::Command;

    let (_dir, root) = tmp_root();
    let name = SessionName::new("upload").unwrap();
    let session = session::create(&root, &default_ns(), &name).unwrap();

    let lock_path = session.lock_path();

    // Hold lock via perl (available on macOS and Linux, unlike flock CLI)
    let mut child = Command::new("perl")
        .arg("-e")
        .arg(format!(
            "use Fcntl qw(:flock); $| = 1; open(my $fh, '>', '{}') or die; \
             flock($fh, LOCK_EX) or die; print \"R\"; sleep 10;",
            lock_path.display()
        ))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for perl to signal it holds the lock (a readiness byte on stdout)
    // instead of sleeping a fixed window — a slow subprocess launch must not
    // race our try_acquire.
    let mut ready = [0u8; 1];
    {
        use std::io::Read;
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .expect("perl should emit its readiness byte after acquiring the lock");
    }
    assert_eq!(&ready, b"R");

    // Our try_acquire should now fail — perl holds the exclusive lock.
    let err = LockGuard::try_acquire(&session).unwrap_err();
    assert!(matches!(err, SessionError::Locked(_)));

    child.kill().unwrap();
    child.wait().unwrap();

    // After child dies, lock should be available
    let _guard = LockGuard::try_acquire(&session).unwrap();
}
