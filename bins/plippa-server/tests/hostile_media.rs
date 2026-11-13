//! SECFIX_06 / PAR-01 at the server: a hostile media file cannot abort `plippa-server`.
//!
//! The children run under the review's `ulimit -v 1000000`, so a Symphonia allocation abort
//! anywhere in the child's own process would show up as `SIGABRT`; the isolated helper has its
//! own (tighter) limits and absorbs the abort. Fixtures are built in code
//! (`plippa_audio::hostile`).

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plippa_audio::hostile;
use plippa_audio::isolate::Isolation;
use plippa_noubin::{LibraryRoot, LibraryWriter};
use plippa_server::waveform::{spawn_waveform_worker, WaveformState};
use plippa_server::{RecordingIpc, ServerService};

/// The review's `ulimit -v 1000000` (kilobytes), in bytes.
const REVIEW_LIMIT_BYTES: u64 = 1_000_000 * 1024;

fn wav_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures/tone_48k_16.wav")
}

/// Re-run this test binary for the child entry named `entry` under the review's limit.
fn run_child(entry: &str, env: &[(&str, &Path)]) -> std::process::Output {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", entry, "--nocapture", "--test-threads=1"]);
    for (k, v) in env {
        c.env(k, v);
    }
    // SAFETY: setrlimit is async-signal-safe.
    unsafe {
        c.pre_exec(|| {
            let l = libc::rlimit { rlim_cur: REVIEW_LIMIT_BYTES, rlim_max: REVIEW_LIMIT_BYTES };
            if libc::setrlimit(libc::RLIMIT_AS, &l) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        });
    }
    c.output().unwrap()
}

// ---- probe_duration_ms -----------------------------------------------------------------------

/// Child entry: probe every file in `PLIPPA_CASES_DIR` plus the valid WAV, report on stdout.
#[test]
fn child_probe_every_file() {
    let Ok(dir) = std::env::var("PLIPPA_CASES_DIR") else { return };
    let mut names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    for p in names {
        let ms = plippa_server::probe_duration_ms(&p);
        println!("PROBE {} {:?}", p.file_name().unwrap().to_string_lossy(), ms);
    }
    println!("HELPERS {}", Isolation::global().spawn_count());
    println!("CHILD-DONE");
}

#[test]
fn probe_duration_ms_survives_every_hostile_file() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cases = d.path().join("cases");
    fs::create_dir(&cases).unwrap();
    hostile::write_all(&cases, &hostile::review_cases());
    fs::copy(wav_fixture(), cases.join("valid.wav")).unwrap();

    let out = run_child(
        "child_probe_every_file",
        &[("PLIPPA_CASES_DIR", &cases), ("PLIPPA_STATE_DIR", state.path())],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "child died ({:?}): {text}\n{}", out.status.signal(), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("CHILD-DONE"), "{text}");
    for hostile in ["mp4_stts.mp3", "flac_vorbis.flac", "mp3_id3.mp3"] {
        assert!(text.contains(&format!("PROBE {hostile} None")), "{hostile}: {text}");
    }
    assert!(text.contains("PROBE valid.wav Some("), "a valid file is still measured: {text}");
    // The probes ran in helpers (the old code opened every file in-process and spawned none).
    let spawned: u64 = text
        .lines()
        .find_map(|l| l.find("HELPERS ").map(|i| l[i + 8..].trim().to_string()))
        .and_then(|n| n.parse().ok())
        .expect("HELPERS line");
    assert!(spawned >= 4, "every file is probed in a helper: {text}");
}

// ---- the waveform worker ---------------------------------------------------------------------

/// A library whose release lists two valid WAVs and the three hostile files.
fn hostile_library(root: &Path) -> Vec<(&'static str, bool)> {
    let mut rels: Vec<(&'static str, bool)> = vec![("good1.wav", true), ("good2.wav", true)];
    for f in hostile::review_cases() {
        rels.push((f.name, false));
    }
    hostile::write_all(root, &hostile::review_cases());
    for good in ["good1.wav", "good2.wav"] {
        fs::copy(wav_fixture(), root.join(good)).unwrap();
    }
    let items: Vec<String> = rels
        .iter()
        .map(|(r, _)| format!(r#"{{"mediaTitle": "{r}", "localFilePaths": ["{r}"]}}"#))
        .collect();
    fs::write(
        root.join("_hostile.noudata"),
        format!(
            r#"{{"noudataVersion": "0.4.0", "noudataType": "release",
                 "playableItemTitle": "Hostile", "mediaList": [{}]}}"#,
            items.join(",")
        ),
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
    rels
}

/// Child entry: the real waveform worker over the hostile library.
#[test]
fn child_waveform_sweeps() {
    let Ok(root) = std::env::var("PLIPPA_LIBRARY") else { return };
    let root = PathBuf::from(root);
    println!("PID {}", std::process::id());
    let svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let registry = svc.waveform_registry();
    let svc = Arc::new(std::sync::Mutex::new(svc));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_waveform_worker(Arc::clone(&svc), Duration::from_millis(100), Arc::clone(&stop));
    let rels = ["good1.wav", "good2.wav", "mp4_stts.mp3", "flac_vorbis.flac", "mp3_id3.mp3"];
    let deadline = Instant::now() + Duration::from_secs(90);
    let settled = |r: &str| matches!(registry.state(r), WaveformState::Analysed | WaveformState::Failed);
    while !rels.iter().all(|r| settled(r)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let after_first = Isolation::global().spawn_count();
    // More sweeps happen (100 ms interval): a marked or failed file must not cost a helper again.
    std::thread::sleep(Duration::from_millis(1500));
    let after_more = Isolation::global().spawn_count();
    for r in rels {
        println!("STATE {r} {:?}", registry.state(r));
    }
    println!("SPAWNS {after_first} {after_more}");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handle.join().unwrap();
    println!("PID-STILL {}", std::process::id());
    println!("CHILD-DONE");
}

#[test]
fn the_waveform_worker_survives_and_keeps_analysing() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let root = d.path().join("stick");
    fs::create_dir(&root).unwrap();
    hostile_library(&root);

    let out = run_child(
        "child_waveform_sweeps",
        &[("PLIPPA_LIBRARY", &root), ("PLIPPA_STATE_DIR", state.path())],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the server process died ({:?}): {text}\n{}",
        out.status.signal(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("CHILD-DONE"), "{text}");
    // libtest prefixes the first line of a captured child with the test name: look anywhere.
    let pid = |tag: &str| {
        text.lines()
            .find_map(|l| l.find(tag).map(|i| l[i + tag.len()..].trim().to_string()))
            .unwrap_or_else(|| panic!("no {tag} line in {text}"))
    };
    assert_eq!(pid("PID "), pid("PID-STILL "), "same process throughout");
    for good in ["good1.wav", "good2.wav"] {
        assert!(text.contains(&format!("STATE {good} Analysed")), "{good}: {text}");
        assert!(plippa_noubin::waveform_path(&root, good).exists(), "{good} has a sidecar");
    }
    for bad in ["mp4_stts.mp3", "flac_vorbis.flac", "mp3_id3.mp3"] {
        assert!(text.contains(&format!("STATE {bad} Failed")), "{bad}: {text}");
        assert!(!plippa_noubin::waveform_path(&root, bad).exists());
    }
    let spawns: Vec<u64> = pid("SPAWNS ").split(' ').map(|n| n.parse().unwrap()).collect();
    assert_eq!(spawns[0], spawns[1], "no helper is started again for a marked or failed file: {text}");
    // The two crashing files are marked on the device (not on the stick).
    assert!(state.path().join("media-crash-marks.bin").exists());
    assert!(!root.join(".plippa").join("media-crash-marks.bin").exists());
}
