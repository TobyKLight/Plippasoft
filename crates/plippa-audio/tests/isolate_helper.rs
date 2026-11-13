//! SECFIX_06 / PAR-01: hostile media cannot abort the process that opens it.
//!
//! Fixtures come from `plippa_audio::hostile` (built in code). Children are started with
//! `RLIMIT_AS` like the review's `ulimit -v 1000000`.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use plippa_audio::hostile::{self, HostileFile};
use plippa_audio::isolate::{Budgets, CrashMarks, Isolation};
use plippa_audio::{AudioError, OverspecPolicy, StreamDecoder};
use plippa_core::Capabilities;

const HELPER: &str = env!("CARGO_BIN_EXE_plippa-media-helper");
/// The review's `ulimit -v 1000000` (kilobytes).
const REVIEW_LIMIT_BYTES: u64 = 1_000_000 * 1024;

fn caps() -> Capabilities {
    Capabilities::desktop_mock()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn isolation(state: &Path) -> Isolation {
    Isolation::new(HELPER, CrashMarks::new(state))
}

fn open(iso: &Isolation, path: &Path) -> Result<plippa_audio::isolate::RemoteDecoder, AudioError> {
    let f = std::fs::File::open(path).unwrap();
    iso.open_decoder(&f, path, &caps(), OverspecPolicy::Downsample)
}

// ---- the old in-process path really aborts -------------------------------------------------

/// Child half: runs the in-process entry point named by `PLIPPA_HOSTILE_CASE` and exits 0 if
/// it returns (an `Err` is fine), so the parent can see a signal when it does not.
#[test]
fn in_process_child() {
    let Ok(case) = std::env::var("PLIPPA_HOSTILE_CASE") else { return };
    let path = PathBuf::from(case);
    let _ = StreamDecoder::open(&path, &caps(), OverspecPolicy::Downsample);
}

fn run_in_process_child(path: &Path) -> std::process::ExitStatus {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "in_process_child", "--nocapture"])
        .env("PLIPPA_HOSTILE_CASE", path);
    // SAFETY: setrlimit is async-signal-safe.
    unsafe {
        c.pre_exec(|| {
            let l = libc::rlimit { rlim_cur: REVIEW_LIMIT_BYTES, rlim_max: REVIEW_LIMIT_BYTES };
            if libc::setrlimit(libc::RLIMIT_AS, &l) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        });
    }
    c.output().unwrap().status
}

/// Documents the proof: the FLAC and MP3 review files abort the in-process decoder (SIGABRT)
/// under the review's limit. The MP4 file no longer reaches an MP4 reader (trimmed away), so
/// it is an ordinary error; its abort was shown on the pre-trim build (SECFIX_06 decision log).
/// The production entry points (engine, server) no longer use this path for untrusted files:
/// `hostile_files_do_not_abort_the_production_entry_points` below.
#[test]
fn hostile_files_abort_the_in_process_decoder() {
    let d = tempfile::tempdir().unwrap();
    for f in hostile::review_cases() {
        let p = hostile::write_all(d.path(), std::slice::from_ref(&f)).remove(0);
        let st = run_in_process_child(&p);
        match f.name {
            "mp4_stts.mp3" => assert!(st.success(), "{}: {st:?}", f.name),
            _ => assert_eq!(st.signal(), Some(libc::SIGABRT), "{} must abort in-process: {st:?}", f.name),
        }
    }
}

// ---- the helper path survives --------------------------------------------------------------

fn write(dir: &Path, f: &HostileFile) -> PathBuf {
    hostile::write_all(dir, std::slice::from_ref(f)).remove(0)
}

#[test]
fn hostile_files_do_not_abort_the_production_entry_points() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    for f in hostile::review_cases() {
        let p = write(d.path(), &f);
        // The test process itself is the witness: it is still here after every file.
        let err = open(&iso, &p).expect_err(f.name);
        match f.name {
            "mp4_stts.mp3" => assert!(
                !matches!(err, AudioError::Unplayable { .. }),
                "an MP4 is an ordinary probe error on the trimmed build: {err}"
            ),
            _ => assert!(matches!(err, AudioError::Unplayable { .. }), "{}: {err}", f.name),
        }
        // The same file through the validate path.
        let f2 = std::fs::File::open(&p).unwrap();
        assert!(iso.validate_for_stream(&f2, &p, &caps(), OverspecPolicy::Downsample).is_err());
        // A valid file still works right after.
        open(&iso, &fixture("tone_48k.flac")).expect("a valid file after a hostile one");
    }
}

#[test]
fn a_crashing_file_is_marked_and_never_retried() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    let bad = write(d.path(), &hostile::review_cases()[1]); // flac_vorbis.flac

    assert_eq!(iso.spawn_count(), 0);
    assert!(matches!(open(&iso, &bad), Err(AudioError::Unplayable { .. })));
    assert_eq!(iso.spawn_count(), 1, "the first attempt costs one helper");
    for _ in 0..5 {
        assert!(matches!(open(&iso, &bad), Err(AudioError::Unplayable { .. })));
    }
    assert_eq!(iso.spawn_count(), 1, "a marked file costs no further spawn");
    assert!(iso.is_marked_by_path(&bad));

    // A new store instance on the same file (a restart) still refuses without a spawn.
    let again = isolation(state.path());
    assert!(matches!(open(&again, &bad), Err(AudioError::Unplayable { .. })));
    assert_eq!(again.spawn_count(), 0);

    // Changing size (and mtime) makes it eligible again: it is tried, and crashes, once more.
    let mut bytes = std::fs::read(&bad).unwrap();
    bytes.push(0);
    std::fs::write(&bad, &bytes).unwrap();
    assert!(!again.is_marked_by_path(&bad));
    assert!(matches!(open(&again, &bad), Err(AudioError::Unplayable { .. })));
    assert_eq!(again.spawn_count(), 1, "an edited file is tried again");

    // A corrupt mark file is ignored (never a crash): the file is simply tried again.
    std::fs::write(state.path().join("media-crash-marks.bin"), b"\xff\xff garbage").unwrap();
    let third = isolation(state.path());
    assert!(matches!(open(&third, &bad), Err(AudioError::Unplayable { .. })));
    assert_eq!(third.spawn_count(), 1);
}

fn alive(pid: u32) -> bool {
    // A zombie still has a /proc entry; a reaped process does not.
    Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn the_helper_enforces_its_own_limits() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    // 1. Allocation past RLIMIT_AS kills only the helper: the reason names the signal.
    let iso = isolation(state.path());
    let bad = write(d.path(), &hostile::review_cases()[2]); // mp3_id3.mp3
    match open(&iso, &bad) {
        Err(AudioError::Unplayable { reason, .. }) => assert!(reason.contains("signal"), "{reason}"),
        other => panic!("{other:?}"),
    }

    // 2. A helper that stops answering is killed by the stall watchdog and reaped.
    let tight = iso.with_budgets(Budgets {
        chunk: Duration::from_millis(400),
        ..Budgets::default()
    });
    let mut dec = open(&tight, &fixture("tone_48k.flac")).unwrap();
    let pid = dec.helper_pid().unwrap();
    assert!(dec.next_chunk(1024).unwrap().is_some());
    // SAFETY: stopping our own child.
    unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
    let started = Instant::now();
    let err = dec.next_chunk(1024).unwrap_err();
    assert!(matches!(err, AudioError::Unplayable { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(dec.failure().unwrap().contains("no answer"), "{:?}", dec.failure());
    assert!(!alive(pid), "the hung helper was killed and reaped (no zombie)");
    // and the file is now marked
    assert!(tight.is_marked_by_path(&fixture("tone_48k.flac")));
}

#[test]
fn every_helper_is_reaped_when_its_decoder_goes_away() {
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    let dec = open(&iso, &fixture("tone_48k.mp3")).unwrap();
    let pid = dec.helper_pid().unwrap();
    assert!(alive(pid));
    drop(dec);
    assert!(!alive(pid));
}

#[test]
fn the_helper_refuses_to_run_without_its_limits() {
    // Started by hand: no RLIMIT_AS etc. in place, nothing to decode with.
    let out = Command::new(HELPER)
        .args(["--as", "1073741824", "--cpu", "10", "--nofile", "8"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(plippa_audio::isolate::server::EXIT_NOT_CONFINED));
    assert!(out.stdout.is_empty(), "no protocol output without confinement");
    let out = Command::new(HELPER).output().unwrap();
    assert_eq!(out.status.code(), Some(plippa_audio::isolate::server::EXIT_BAD_ARGS));
}

#[test]
fn a_missing_or_wrong_helper_is_unavailable_not_a_verdict_on_the_file() {
    let state = tempfile::tempdir().unwrap();
    let iso = Isolation::new("/nonexistent/plippa-media-helper", CrashMarks::new(state.path()));
    let p = fixture("tone_48k.mp3");
    assert!(matches!(open(&iso, &p), Err(AudioError::HelperUnavailable { .. })));
    assert!(!iso.is_marked_by_path(&p), "an infrastructure failure never marks");
    // A program that is not a helper at all (exits at once).
    let iso = Isolation::new("/bin/true", CrashMarks::new(state.path()));
    assert!(matches!(open(&iso, &p), Err(AudioError::HelperUnavailable { .. })));
    assert!(!iso.is_marked_by_path(&p));
}

// ---- streams are bounded by progress, not by total time ------------------------------------

#[test]
fn a_long_file_is_not_killed_by_a_total_time_limit() {
    // A sparse 8-bit mono WAV of 32 MiB of silence: tens of millions of frames, so decoding it
    // takes far longer than the stall budget, while every chunk arrives within it.
    const DATA_BYTES: u64 = 32 * 1024 * 1024;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("long.wav");
    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&((36 + DATA_BYTES) as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&1u16.to_le_bytes()); // mono
    header.extend_from_slice(&8_000u32.to_le_bytes());
    header.extend_from_slice(&8_000u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&8u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(DATA_BYTES as u32).to_le_bytes());
    std::fs::write(&path, &header).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(header.len() as u64 + DATA_BYTES).unwrap();

    let state = tempfile::tempdir().unwrap();
    let stall = Duration::from_millis(500);
    let iso = isolation(state.path()).with_budgets(Budgets { chunk: stall, ..Budgets::default() });
    let mut dec = open(&iso, &path).unwrap();
    let started = Instant::now();
    let mut frames = 0u64;
    while let Some(pcm) = dec.next_chunk(plippa_audio::DEFAULT_CHUNK_FRAMES).unwrap() {
        frames += pcm.frame_count() as u64;
    }
    assert_eq!(frames, DATA_BYTES);
    // Headroom: the helper's peak address space over a whole long stream stays far below its
    // RLIMIT_AS (512 MiB), so the limit only ever bites on abuse.
    let status = std::fs::read_to_string(format!("/proc/{}/status", dec.helper_pid().unwrap())).unwrap();
    let peak_kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmPeak:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    eprintln!("helper VmPeak over a 32 MiB stream: {peak_kb} kB");
    assert!(peak_kb * 1024 < plippa_audio::isolate::limits::STREAM_ADDRESS_SPACE / 2, "VmPeak {peak_kb} kB");
    assert!(
        started.elapsed() > stall * 2,
        "the stream took {:?}, which must exceed the stall budget {stall:?} for this to prove anything",
        started.elapsed()
    );
    assert!(!iso.is_marked_by_path(&path));
}

// ---- the helper reads the descriptor it was given, never a path -----------------------------

#[test]
fn the_helper_never_opens_a_path_it_was_not_given() {
    let d = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    let original = d.path().join("song.flac");
    std::fs::copy(fixture("tone_48k.flac"), &original).unwrap();
    let mut expected = StreamDecoder::open(&original, &caps(), OverspecPolicy::Downsample).unwrap();
    let mut want = Vec::new();
    while let Some(p) = expected.next_chunk(4096).unwrap() {
        want.extend(p.samples);
    }

    let mut dec = open(&iso, &original).unwrap();
    // After the hand-over the path now names a different file (a symlink swap).
    let other = d.path().join("other.flac");
    std::fs::copy(fixture("tone_48k_3s.mp3"), &other).unwrap();
    std::fs::remove_file(&original).unwrap();
    std::os::unix::fs::symlink(&other, &original).unwrap();
    // Seeks re-open the source inside the helper: they must still read the original object.
    dec.seek_frame(0).unwrap();
    let mut got = Vec::new();
    while let Some(p) = dec.next_chunk(4096).unwrap() {
        got.extend(p.samples);
    }
    assert_eq!(got.len(), want.len());
    assert!(got == want, "the stream is the original file, whatever the path names now");
}

// ---- the remote decoder behaves like the in-process one ----------------------------------------

fn drain_remote(dec: &mut plippa_audio::isolate::RemoteDecoder) -> Vec<f32> {
    let mut out = Vec::new();
    while let Some(p) = dec.next_chunk(4096).unwrap() {
        out.extend(p.samples);
    }
    out
}

fn drain_local(dec: &mut StreamDecoder) -> Vec<f32> {
    let mut out = Vec::new();
    while let Some(p) = dec.next_chunk(4096).unwrap() {
        out.extend(p.samples);
    }
    out
}

#[test]
fn the_remote_decoder_matches_the_in_process_decoder_for_every_format() {
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    for name in ["tone_48k.mp3", "tone_48k.flac", "tone_48k_16.wav", "burst_44k_lame.mp3", "headerless_vbr_44k.mp3"] {
        let p = fixture(name);
        let mut local = StreamDecoder::open(&p, &caps(), OverspecPolicy::Downsample).unwrap();
        let mut remote = open(&iso, &p).unwrap();
        assert_eq!(remote.channels(), local.channels(), "{name}");
        assert_eq!(remote.sample_rate_hz(), local.sample_rate_hz(), "{name}");
        assert_eq!(remote.duration_ms(), local.duration_ms(), "{name}");
        assert_eq!(remote.duration_frames(), local.duration_frames(), "{name}");
        assert_eq!(remote.duration_known(), local.duration_known(), "{name}");
        assert_eq!(drain_remote(&mut remote), drain_local(&mut local), "{name}: gapless-trimmed PCM");
        assert!(remote.is_eof());
        // Seeks (backward after EOF, then forward) land and decode identically.
        for frame in [0u64, 12_345, 1_000, 30_000] {
            let l = local.seek_frame(frame);
            let r = remote.seek_frame(frame);
            assert_eq!(l.is_ok(), r.is_ok(), "{name} seek {frame}");
            if l.is_ok() {
                assert_eq!(remote.position_frame(), local.position_frame(), "{name} landing {frame}");
                let a = local.next_chunk(2048).unwrap().map(|p| p.samples);
                let b = remote.next_chunk(2048).unwrap().map(|p| p.samples);
                assert_eq!(a, b, "{name} after seek {frame}");
            }
        }
    }
}

#[test]
fn the_helper_builds_the_mp3_index_and_uses_it_for_backward_seeks() {
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    let p = fixture("tone_48k_3s.mp3");
    let mut remote = open(&iso, &p).unwrap();
    remote.start_index_build(8).unwrap();
    // The walk is a background thread in the helper; the status says when it has landed.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !remote.has_index() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        remote.next_chunk(1024).unwrap();
    }
    assert!(remote.has_index(), "the helper installed its index");
    assert!(remote.index_entries() > 0);
    remote.seek_frame(120_000).unwrap();
    let indexed = remote.indexed_seeks();
    remote.seek_frame(40_000).unwrap();
    assert!(remote.indexed_seeks() > indexed, "a backward seek went through the index");
}

#[test]
fn a_bad_request_order_or_file_is_an_ordinary_error_not_a_crash() {
    let state = tempfile::tempdir().unwrap();
    let iso = isolation(state.path());
    // A file the board does not enable: refused by the gate, not marked, no verdict of crash.
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("song.m4a");
    std::fs::write(&p, b"\0\0\0\x18ftypM4A \0\0\0\0M4A mp42isom").unwrap();
    let mut c = caps();
    c.audio_formats.aac = true;
    let f = std::fs::File::open(&p).unwrap();
    let err = iso.open_decoder(&f, &p, &c, OverspecPolicy::Downsample).unwrap_err();
    assert!(matches!(err, AudioError::FormatNotEnabled { .. }), "{err}");
    assert!(!iso.is_marked_by_path(&p));
    // Garbage with a supported extension: a probe error, not marked.
    let g = d.path().join("junk.mp3");
    std::fs::write(&g, vec![7u8; 2048]).unwrap();
    let err = open(&iso, &g).unwrap_err();
    assert!(matches!(err, AudioError::Probe { .. } | AudioError::Decode { .. }), "{err}");
    assert!(!iso.is_marked_by_path(&g));
}
