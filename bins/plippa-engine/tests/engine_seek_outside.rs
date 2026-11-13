//! WARP_19: "Seek outside track" at the engine level (state and position bookkeeping).
//!
//! The mock sink is unthrottled, so everything here polls for *state*, never for timing: a
//! paused stream is opened with `SeekMs` (Load + SeekMs, like the NB_04 tests), a scan runs
//! from it, and the assertions are invariants that hold however far the scan got — above all
//! that the list's current entry is the item the audio thread reports, including after a
//! scan crossing (the event names the entered item by its source `item_position`).

use plippa_core::{Capabilities, ScanDir, TransportState};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus, ResumeWriteback};
use plippa_noubin::{
    encode_playable_id, normalise_noubin_key, LibraryReader, LibraryRoot, LibraryWriter,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 8_000;
/// Track lengths in seconds: all different so the audio thread's reported duration names the
/// item it is playing.
const SECS: [u32; 3] = [20, 30, 40];

fn wav(path: &Path, secs: u32) {
    let frames = (RATE * secs) as usize;
    let data_bytes = frames * 2;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = fs::File::create(path).unwrap();
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes as u32).to_le_bytes())
        .unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&RATE.to_le_bytes()).unwrap();
    out.write_all(&(RATE * 2).to_le_bytes()).unwrap();
    out.write_all(&2u16.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for i in 0..frames {
        let s = ((i as f32 / RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn library(root: &Path, profile: &str, repeat: bool) {
    let mut items = Vec::new();
    for (i, secs) in SECS.iter().enumerate() {
        let n = i + 1;
        wav(&root.join(format!("Music/Album/{n:02}.wav")), *secs);
        items.push(format!(
            r#"{{"mediaTitle":"T{n}","localFilePaths":["{n:02}.wav"]}}"#
        ));
    }
    let local = if repeat {
        r#""localPlaybackData":{"repeatList":true},"#
    } else {
        ""
    };
    let p = root.join("Music/Album/_album.noudata");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(
        p,
        format!(
            r#"{{"noubinKey":"album.noubin.a","noubinKeyNormalised":"{}","playableItemTitle":"Album","playableItemProfile":"{profile}",
               "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               {local}
               "mediaList":[{}]}}"#,
            normalise_noubin_key("album.noubin.a"),
            items.join(",")
        ),
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(root))
        .unwrap()
        .full_scan()
        .unwrap();
}

fn gid(root: &Path) -> plippa_core::PlayableItemId {
    let reader = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let rec = reader
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.title.as_deref() == Some("Album"))
        .unwrap();
    plippa_core::PlayableItemId::new(encode_playable_id(root, rec.id))
}

fn cmd(e: &mut Engine, c: PlaybackCommand) {
    e.handle_ipc(IpcMessage::PlaybackCommand(c));
}

fn status(e: &mut Engine) -> PlaybackStatus {
    e.poll_stream_lifecycle();
    e.take_events();
    e.publish_status();
    e.take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status")
}

fn wait_until(e: &mut Engine, what: &str, mut f: impl FnMut(&mut Engine) -> bool) {
    let t = Instant::now();
    loop {
        e.poll_stream_lifecycle();
        if f(e) {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "timed out: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Album loaded and a paused stream open at `ms` of item `item` (0-based) with the setting `on`.
fn setup(
    profile: &str,
    repeat: bool,
    item: usize,
    ms: u64,
    on: bool,
) -> (tempfile::TempDir, PathBuf, Engine) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().to_path_buf();
    library(&root, profile, repeat);
    let mut e = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::Load(gid(&root)));
    for _ in 0..item {
        cmd(&mut e, PlaybackCommand::Next);
    }
    cmd(&mut e, PlaybackCommand::SeekMs(ms));
    // (the duration names the item: a stale stream's value must not satisfy the wait)
    wait_until(&mut e, "stream of the item open", |e| {
        e.current_duration_ms() == Some(dur_ms(item))
    });
    cmd(&mut e, PlaybackCommand::SetSeekOutsideTrack(on));
    (t, root, e)
}

fn dur_ms(item: usize) -> u64 {
    u64::from(SECS[item]) * 1000
}

fn writebacks(e: &mut Engine) -> Vec<ResumeWriteback> {
    e.take_events()
        .into_iter()
        .filter_map(|ev| match ev {
            EngineEvent::ResumeWriteback(w) => Some(w),
            _ => None,
        })
        .collect()
}

#[test]
fn setting_is_stored_published_and_survives_unload() {
    let (_t, _r, mut e) = setup("music", false, 1, 1_000, true);
    let s = status(&mut e).session;
    assert!(s.seek_outside_track);
    // Neighbour durations for the UI's live jog target (both neighbours exist here).
    assert_eq!(
        (s.prev_duration_ms, s.next_duration_ms),
        (Some(dur_ms(0)), Some(dur_ms(2)))
    );
    cmd(&mut e, PlaybackCommand::Unload);
    assert!(
        status(&mut e).session.seek_outside_track,
        "Unload keeps the setting"
    );
    cmd(&mut e, PlaybackCommand::SetSeekOutsideTrack(false));
    let s = status(&mut e).session;
    assert!(!s.seek_outside_track);
    assert_eq!((s.prev_duration_ms, s.next_duration_ms), (None, None));
}

#[test]
fn forward_scan_crosses_into_the_next_item_and_the_state_follows() {
    let (_t, root, mut e) = setup("audiobook", false, 0, 15_000, true);
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    wait_until(&mut e, "crossed into item 2", |e| {
        e.now_playing_entries().len() == 3 && status_index(e) >= 1
    });
    cmd(&mut e, PlaybackCommand::ScanStop);
    wait_until(&mut e, "scan over", |e| {
        status(e).session.scan == plippa_core::ScanState::None
    });
    let s = status(&mut e).session;
    let idx = s.current_entry_index as usize;
    assert!(idx >= 1);
    // The item the engine says is current is the one the audio thread is playing.
    assert_eq!(e.current_duration_ms(), Some(dur_ms(idx)));
    assert_eq!(s.duration_ms, Some(dur_ms(idx)));
    // Neighbour durations follow the new current item.
    assert_eq!(s.prev_duration_ms, Some(dur_ms(idx - 1)));
    assert_eq!(
        s.next_duration_ms,
        SECS.get(idx + 1).map(|_| dur_ms(idx + 1))
    );
    // Resume write-back stays correct: the item that is current now is the one written.
    e.take_events();
    cmd(&mut e, PlaybackCommand::Stop);
    let pos = e.current_position_ms().unwrap_or(0);
    let wb = writebacks(&mut e);
    assert_eq!(wb.len(), 1, "{wb:?}");
    assert_eq!(wb[0].playable_id, gid(&root));
    assert_eq!(wb[0].resume_at_item, Some(idx as u32 + 1));
    assert!(wb[0].resume_from_ms.is_some_and(|ms| ms <= pos + 1_000));
}

fn status_index(e: &mut Engine) -> u32 {
    status(e).session.current_entry_index
}

#[test]
fn off_keeps_the_scan_inside_the_track() {
    let (_t, _r, mut e) = setup("music", false, 0, 15_000, false);
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    wait_until(&mut e, "scan over", |e| {
        status(e).session.scan == plippa_core::ScanState::None
    });
    let s = status(&mut e).session;
    assert_eq!(
        s.current_entry_index, 0,
        "item unchanged with the setting off"
    );
    assert_eq!(e.current_duration_ms(), Some(dur_ms(0)));
    assert_eq!(s.transport, TransportState::Stopped);
}

#[test]
fn first_item_reverse_scan_is_a_clamp_even_with_the_setting_on() {
    let (_t, _r, mut e) = setup("music", false, 0, 2_000, true);
    assert_eq!(status(&mut e).session.prev_duration_ms, None);
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Backward,
        },
    );
    wait_until(&mut e, "scan over", |e| {
        status(e).session.scan == plippa_core::ScanState::None
    });
    let s = status(&mut e).session;
    assert_eq!(s.current_entry_index, 0);
    assert_eq!(e.current_position_ms(), Some(0), "clamped at 0:00");
}

#[test]
fn a_retired_repeat_list_key_does_not_wrap_the_neighbours() {
    let (_t, _r, mut e) = setup("music", true, 0, 2_000, true);
    // Noubin v0.4.1: the file's `repeatList` is no longer honoured, so the first track has no
    // backwards neighbour (and there is no last-track-to-first wrap).
    let s = status(&mut e).session;
    assert_eq!(
        (s.prev_duration_ms, s.next_duration_ms),
        (None, Some(dur_ms(1)))
    );
}

#[test]
fn a_reorder_while_scanning_resends_the_neighbours() {
    // PLAY_02: an order change is list-only, and a scan crossing names the item it entered by
    // its source position. The audio thread keeps the item it was scanning; the neighbours it
    // is given (and the durations published for the jog target) are the *new* order's, and
    // when the queued crossing is drained the list must still name the item that was heard.
    let (_t, root, mut e) = setup("mix", false, 1, 29_000, true);
    e.set_shuffle_seed(0x5EED); // the new order below is [T2, T1, T3]
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    assert_eq!(e.session().scan, plippa_core::ScanState::Forward);

    // The audio thread crosses into the *old* next item (T3, 40 s) before the control plane
    // drains the event: the duration it reports names the item it is playing. This is state,
    // not timing — and the engine is deliberately not polled here, so the `CrossEvent` stays
    // queued across the reorder.
    let t = Instant::now();
    while e.current_duration_ms() != Some(dur_ms(2)) {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "the scan did not cross into T3"
        );
        thread::sleep(Duration::from_millis(1));
    }

    let prepares = e.stream_prepare_count();
    cmd(
        &mut e,
        PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle),
    );
    // No stream command, no prepare, no playhead move: purely a list operation.
    assert_eq!(
        e.stream_prepare_count(),
        prepares,
        "a reorder never re-prepares"
    );
    assert_eq!(
        e.session().scan,
        plippa_core::ScanState::Forward,
        "the scan keeps running"
    );
    // `shuffle_keep_current_first` moved the scanned item (T2) to the front; the old
    // neighbour T3 is no longer the list's next item.
    assert_eq!(e.session().current_entry_index, 0);
    let entries = e.now_playing_entries();
    let order: Vec<&str> = entries.iter().map(|x| x.title.as_str()).collect();
    assert_eq!(order, ["T2", "T1", "T3"], "the new order");
    let secs_of = |title: &str| -> u64 {
        let n: usize = title.trim_start_matches('T').parse().unwrap();
        u64::from(SECS[n - 1]) * 1000
    };
    let s = e.session().clone();
    assert_eq!(s.prev_duration_ms, None, "first item of the new order");
    assert_eq!(
        s.next_duration_ms,
        Some(secs_of("T1")),
        "the neighbouring item is the new order's"
    );

    // Drain the queued crossing. The list must move to the item the audio actually entered
    // (by source position), not to "whatever is next after the reorder" at drain time.
    wait_until(&mut e, "the queued crossing is applied", |e| {
        e.poll_stream_lifecycle();
        e.session().current_entry_index != 0
    });
    let entries = e.now_playing_entries();
    let idx = e.session().current_entry_index as usize;
    // This file's invariant: the list's current entry is the item the audio thread reports.
    assert_eq!(
        e.current_duration_ms(),
        Some(dur_ms(entries[idx].item_position as usize)),
        "the list's current entry is the item the audio is playing"
    );
    assert_eq!(
        status(&mut e).session.duration_ms,
        Some(dur_ms(entries[idx].item_position as usize)),
        "the published session follows the audio too"
    );

    cmd(&mut e, PlaybackCommand::ScanStop);
    wait_until(&mut e, "scan over", |e| {
        status(e).session.scan == plippa_core::ScanState::None
    });
    // Drain the settle too (event-driven: poll until whatever item is current is the one the
    // audio reports — no fixed sleep, and every queued crossing is applied first).
    wait_until(&mut e, "the settled item is coherent", |e| {
        e.poll_stream_lifecycle();
        let idx = e.session().current_entry_index as usize;
        e.now_playing_entries().get(idx).is_some_and(|entry| {
            e.current_duration_ms() == Some(dur_ms(entry.item_position as usize))
        })
    });

    // Resume write-back names the item that was actually heard.
    let entries = e.now_playing_entries();
    let idx = e.session().current_entry_index as usize;
    let heard = entries[idx].item_position;
    wait_until(&mut e, "the heard playhead is past the boundary", |e| {
        e.current_position_ms().unwrap_or(0) > 0
    });
    e.take_events();
    cmd(&mut e, PlaybackCommand::Stop);
    let wb = writebacks(&mut e);
    assert_eq!(wb.len(), 1, "{wb:?}");
    assert_eq!(wb[0].playable_id, gid(&root));
    assert_eq!(
        wb[0].resume_at_item,
        Some(heard + 1),
        "the bookmark names the item that was heard"
    );
    assert!(wb[0].resume_from_ms.is_some(), "{wb:?}");
}

#[test]
fn seek_to_item_lands_in_the_neighbour_and_off_is_a_clamped_seek() {
    // Previous item.
    let (_t, _r, mut e) = setup("music", false, 1, 3_000, true);
    cmd(
        &mut e,
        PlaybackCommand::SeekToItem {
            item_delta: -1,
            target_ms: 17_000,
        },
    );
    wait_until(&mut e, "landed", |e| {
        e.current_duration_ms() == Some(dur_ms(0))
    });
    let s = status(&mut e).session;
    assert_eq!(s.current_entry_index, 0);
    assert_eq!(e.current_position_ms(), Some(17_000));
    // Next item (clamped inside it).
    cmd(
        &mut e,
        PlaybackCommand::SeekToItem {
            item_delta: 1,
            target_ms: 999_999,
        },
    );
    wait_until(&mut e, "landed", |e| {
        e.current_duration_ms() == Some(dur_ms(1))
    });
    assert_eq!(status(&mut e).session.current_entry_index, 1);
    assert_eq!(e.current_position_ms(), Some(dur_ms(1) - 250));
    // delta 0 = SeekTo.
    cmd(
        &mut e,
        PlaybackCommand::SeekToItem {
            item_delta: 0,
            target_ms: 4_000,
        },
    );
    wait_until(&mut e, "seek", |e| {
        status(e).session.seek_target_ms.is_none()
    });
    assert_eq!(status(&mut e).session.current_entry_index, 1);
    // Last item, forward: a clamp inside the item.
    let (_t2, _r2, mut e2) = setup("music", false, 2, 3_000, true);
    cmd(
        &mut e2,
        PlaybackCommand::SeekToItem {
            item_delta: 1,
            target_ms: 5_000,
        },
    );
    wait_until(&mut e2, "clamped seek", |e| {
        status(e).session.seek_target_ms.is_none()
    });
    assert_eq!(status(&mut e2).session.current_entry_index, 2);
    // Setting off: the same command is a clamped seek in the current item.
    let (_t3, _r3, mut e3) = setup("music", false, 1, 3_000, false);
    cmd(
        &mut e3,
        PlaybackCommand::SeekToItem {
            item_delta: -1,
            target_ms: 17_000,
        },
    );
    wait_until(&mut e3, "clamped seek", |e| {
        status(e).session.seek_target_ms.is_none()
    });
    let s = status(&mut e3).session;
    assert_eq!(s.current_entry_index, 1);
    assert!(e3.current_position_ms().unwrap() < 1_000);
}

#[test]
fn an_unavailable_neighbour_is_skipped_with_a_notice() {
    let (_t, root, mut e) = setup("music", false, 0, 15_000, true);
    fs::remove_file(root.join("Music/Album/02.wav")).unwrap();
    e.take_events();
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    let events = e.take_events();
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, EngineEvent::Notice(n) if n.message.contains("T2"))),
        "a skipped-item notice"
    );
    cmd(&mut e, PlaybackCommand::ScanStop);
    wait_until(&mut e, "scan over", |e| {
        status(e).session.scan == plippa_core::ScanState::None
    });
    // The neighbour is now the third item.
    let s = status(&mut e).session;
    let idx = s.current_entry_index as usize;
    assert!(idx == 0 || idx == 2, "item 2 is unplayable: {idx}");
}
