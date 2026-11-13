//! SECFIX_06 Part C spike: what decoding in the isolated helper costs against decoding
//! in-process. A measurement, not a pass/fail test: run it with
//! `cargo test --release -p plippa-audio --test isolate_costs -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plippa_audio::isolate::{CrashMarks, Isolation};
use plippa_audio::{OverspecPolicy, StreamDecoder};
use plippa_core::Capabilities;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn caps() -> Capabilities {
    Capabilities::desktop_mock()
}

fn stereo_wav(path: &Path, seconds: usize) {
    let frames = seconds * 44_100;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + frames * 4) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    for x in [1u16, 2] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&((frames * 4) as u32).to_le_bytes());
    for i in 0..frames {
        let s = ((i % 200) as i16 - 100) * 100;
        v.extend_from_slice(&s.to_le_bytes());
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, v).unwrap();
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn rss_kb(pid: u32, key: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn cpu_children() -> Duration {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, ru.as_mut_ptr()) };
    let ru = unsafe { ru.assume_init() };
    Duration::new(ru.ru_utime.tv_sec as u64, ru.ru_utime.tv_usec as u32 * 1000)
        + Duration::new(ru.ru_stime.tv_sec as u64, ru.ru_stime.tv_usec as u32 * 1000)
}

fn cpu_self() -> Duration {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    let ru = unsafe { ru.assume_init() };
    Duration::new(ru.ru_utime.tv_sec as u64, ru.ru_utime.tv_usec as u32 * 1000)
        + Duration::new(ru.ru_stime.tv_sec as u64, ru.ru_stime.tv_usec as u32 * 1000)
}

#[test]
#[ignore = "measurement for the SECFIX_06 decision log; run explicitly"]
fn isolate_costs() {
    let iso = Isolation::new(env!("CARGO_BIN_EXE_plippa-media-helper"), CrashMarks::in_memory());
    let d = tempfile::tempdir().unwrap();
    let wav = d.path().join("long.wav");
    stereo_wav(&wav, 600); // 10 minutes of 44.1 kHz stereo
    let files = [
        ("wav 10 min stereo", wav.clone()),
        ("mp3 3 s", fixture("tone_48k_3s.mp3")),
        ("flac 48k", fixture("tone_48k.flac")),
    ];

    // 1. Open latency.
    for (name, p) in &files {
        let local: Vec<_> = (0..20)
            .map(|_| {
                let t = Instant::now();
                let _ = StreamDecoder::open(p, &caps(), OverspecPolicy::Downsample).unwrap();
                t.elapsed()
            })
            .collect();
        let remote: Vec<_> = (0..20)
            .map(|_| {
                let f = std::fs::File::open(p).unwrap();
                let t = Instant::now();
                let dec = iso.open_decoder(&f, p, &caps(), OverspecPolicy::Downsample).unwrap();
                let e = t.elapsed();
                drop(dec);
                e
            })
            .collect();
        println!("open  {name:<18} in-process {:>8.2?}   helper (spawn+hello+open) {:>8.2?}", median(local), median(remote));
    }

    // 2. Throughput and CPU, chunk 1024 (start) and 4096 (steady state).
    for chunk in [1024usize, 4096] {
        let (name, p) = &files[0];
        let t = Instant::now();
        let c0 = cpu_self();
        let mut local = StreamDecoder::open(p, &caps(), OverspecPolicy::Downsample).unwrap();
        let mut frames = 0u64;
        let mut worst_local = Duration::ZERO;
        loop {
            let t1 = Instant::now();
            match local.next_chunk(chunk).unwrap() {
                Some(c) => frames += c.frame_count() as u64,
                None => break,
            }
            worst_local = worst_local.max(t1.elapsed());
        }
        let local_wall = t.elapsed();
        let local_cpu = cpu_self() - c0;

        let f = std::fs::File::open(p).unwrap();
        let t = Instant::now();
        let (c0, k0) = (cpu_self(), cpu_children());
        let mut remote = iso.open_decoder(&f, p, &caps(), OverspecPolicy::Downsample).unwrap();
        let pid = remote.helper_pid().unwrap();
        let mut rframes = 0u64;
        let mut lat = Vec::new();
        loop {
            let t1 = Instant::now();
            match remote.next_chunk(chunk).unwrap() {
                Some(c) => rframes += c.frame_count() as u64,
                None => break,
            }
            lat.push(t1.elapsed());
        }
        let remote_wall = t.elapsed();
        let (rss, vmpeak) = (rss_kb(pid, "VmRSS:"), rss_kb(pid, "VmPeak:"));
        let parent_cpu = cpu_self() - c0;
        drop(remote);
        let helper_cpu = cpu_children() - k0;
        lat.sort();
        let secs = frames as f64 / 44_100.0;
        println!(
            "stream {name} chunk {chunk}: {secs:.0} s of audio ({frames} = {rframes} frames)\n  \
             in-process wall {:.2?} ({:.0}x real time) cpu {:.2?} worst chunk {:.2?}\n  \
             helper     wall {:.2?} ({:.0}x real time) cpu parent {:.2?} + helper {:.2?}; chunk p50 {:.2?} p99 {:.2?} max {:.2?}; helper RSS {rss} kB VmPeak {vmpeak} kB",
            local_wall,
            secs / local_wall.as_secs_f64(),
            local_cpu,
            worst_local,
            remote_wall,
            secs / remote_wall.as_secs_f64(),
            parent_cpu,
            helper_cpu,
            lat[lat.len() / 2],
            lat[lat.len() * 99 / 100],
            lat[lat.len() - 1],
        );
    }

    // 2b. Compressed formats: the same 3 s file decoded 200 times over (seek to 0, drain).
    for (name, p) in [("mp3 (600 s)", fixture("tone_48k_3s.mp3")), ("flac (200 s)", fixture("tone_48k.flac"))] {
        let reps = if name.starts_with("mp3") { 200 } else { 200 };
        let mut local = StreamDecoder::open(&p, &caps(), OverspecPolicy::Downsample).unwrap();
        let t = Instant::now();
        let mut frames = 0u64;
        for _ in 0..reps {
            local.seek_frame(0).unwrap();
            while let Some(c) = local.next_chunk(4096).unwrap() {
                frames += c.frame_count() as u64;
            }
        }
        let lw = t.elapsed();
        let f = std::fs::File::open(&p).unwrap();
        let mut remote = iso.open_decoder(&f, &p, &caps(), OverspecPolicy::Downsample).unwrap();
        let t = Instant::now();
        for _ in 0..reps {
            remote.seek_frame(0).unwrap();
            while remote.next_chunk(4096).unwrap().is_some() {}
        }
        let rw = t.elapsed();
        println!(
            "compressed {name}: in-process {lw:.2?} ({:.0}x real time)   helper {rw:.2?} ({:.0}x real time)   ratio {:.2}",
            frames as f64 / 48_000.0 / lw.as_secs_f64(),
            frames as f64 / 48_000.0 / rw.as_secs_f64(),
            rw.as_secs_f64() / lw.as_secs_f64()
        );
    }

    // 3. Two helpers at once (a seamless join keeps the playing item and its neighbour open).
    let f1 = std::fs::File::open(&files[0].1).unwrap();
    let f2 = std::fs::File::open(&files[2].1).unwrap();
    let mut a = iso.open_decoder(&f1, &files[0].1, &caps(), OverspecPolicy::Downsample).unwrap();
    let mut b = iso.open_decoder(&f2, &files[2].1, &caps(), OverspecPolicy::Downsample).unwrap();
    for _ in 0..200 {
        a.next_chunk(4096).unwrap();
        b.next_chunk(4096).unwrap();
    }
    let (pa, pb) = (a.helper_pid().unwrap(), b.helper_pid().unwrap());
    println!(
        "two helpers: RSS {} kB + {} kB, VmPeak {} kB + {} kB",
        rss_kb(pa, "VmRSS:"),
        rss_kb(pb, "VmRSS:"),
        rss_kb(pa, "VmPeak:"),
        rss_kb(pb, "VmPeak:")
    );
}
