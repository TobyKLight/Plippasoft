//! SECFIX_05: a peer that is slowly opening the same index (it holds `.plippa/.vet.lock`)
//! must not freeze the server: opens under the service mutex wait only the short interactive
//! time, report the library as temporarily unavailable once, and a later poll attaches it.

use plippa_ipc_proto::{IpcMessage, NOTICE_NO_MEDIA_STORAGE};
use plippa_noubin::{LibraryRoot, LibraryWriter};
use plippa_server::{RecordingIpc, ServerService};
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ROLE: &str = "PLIPPA_SERVER_TEST_ROLE";
const ROOT: &str = "PLIPPA_SERVER_TEST_ROOT";

/// Child role: hold the vet lock like a peer that is slowly opening a big index.
#[test]
fn child_holds_the_vet_lock() {
    if std::env::var(ROLE).as_deref() != Ok("hold") {
        return;
    }
    use plippa_core::safe_fs::{lock_file_exclusive, FileLock};
    let root = PathBuf::from(std::env::var(ROOT).unwrap());
    let held = lock_file_exclusive(&root.join(".plippa/.vet.lock"), 0, 1, Duration::from_secs(5)).unwrap();
    assert!(matches!(held, FileLock::Held(_)));
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

#[test]
fn a_peer_holding_the_index_lock_never_freezes_the_service_and_is_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    fs::create_dir_all(root.join("Music")).unwrap();
    fs::write(root.join("Music/Good.wav"), b"RIFF").unwrap();
    fs::write(
        root.join("Music/_good.noudata"),
        r#"{"playableItemTitle":"Good Song","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();

    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["child_holds_the_vet_lock", "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE, "hold")
        .env(ROOT, &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "holder exited early");
        if line.contains("READY") {
            break;
        }
    }

    let ipc = RecordingIpc::new();
    let view = ipc.clone();
    let started = Instant::now();
    let mut svc = ServerService::start(Some(root.clone()), Box::new(ipc), ServerService::default_mock_associate());
    assert!(started.elapsed() < Duration::from_secs(3), "start-up blocked: {:?}", started.elapsed());
    // status() (called by the HTTP API on every request) and the hot-plug poll stay responsive
    // while the peer holds the lock, and do not repeat the notice.
    for _ in 0..4 {
        let t = Instant::now();
        let _ = svc.status();
        svc.poll_mounts();
        assert!(t.elapsed() < Duration::from_secs(3), "a poll blocked: {:?}", t.elapsed());
    }
    let notices = view
        .messages()
        .iter()
        .filter(|m| matches!(m, IpcMessage::UserNotice(n) if n.code == NOTICE_NO_MEDIA_STORAGE))
        .count();
    assert!(notices <= 2, "notice spam: {notices}");
    assert!(svc.list_playables().is_err(), "the library is not attached while busy");

    // The peer finishes: the next poll attaches the library, without any mount change.
    drop(holder.stdin.take());
    let _ = holder.wait();
    svc.poll_mounts();
    let list = svc.list_playables().expect("retried and attached");
    assert!(list.iter().any(|p| p.title.as_deref() == Some("Good Song")));
}
