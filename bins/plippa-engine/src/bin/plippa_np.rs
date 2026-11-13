//! `plippa-np` — dev/bring-up tool for the `nowPlayingList` (NB_04).
//!
//! Sends one playback command to the running engine and prints what comes back for a
//! moment: the list (flat, cues nested under the current track), and status lines with
//! the current track / cue / media position. There is no on-device UI for Next/Previous
//! etc. yet (NB_12/NB_14), so this is how they are exercised on the CM4.
//!
//! ```text
//! export PLIPPA_IPC_DIR=<dir of the running engine's socket>
//! plippa-np show | next | prev | jump <flat-index> | load <playable-id>
//!           | cuelist <n> on|off | play | pause | stop | seek <ms>
//!           | rate <permille> | rate reset | speedmode shift|correct | seekmode shift|correct
//!           | scan fwd|back|stop | seekto <ms> | seekoutside on|off | seekitem -1|0|1 <ms>
//!           | order normal|shuffle | repeat off|queue|item | pauseafter on|off
//!           | reshuffle on|off
//! ```
//!
//! The last line (WARP_01) drives the tempo/warp/scan/jog commands: `rate` is the signed
//! set speed in permille (1000 = 1.0x; 100..=2000 or -2000..=-100), `seekto` the absolute
//! jog target in media ms. `seekoutside` is the play-mode setting "Seek outside track" and
//! `seekitem` the jog commit into the previous (-1) / next (1) item (WARP_19). The `status` line
//! shows the warp fields; the `mode` line (PLAY_02) shows the play-mode settings and whether the
//! loaded list has a fixed order to go back to.

use plippa_core::{
    NowPlayingEntryKind, OrderMode, PlayableItemId, RepeatMode, ScanDir, SourceOrder, WarpMode,
};
use plippa_ipc_proto::{resolve_paths, IpcConnection, IpcMessage, PlaybackCommand};
use std::time::{Duration, Instant};

fn usage() -> ! {
    eprintln!(
        "usage: plippa-np show | next | prev | jump <flat-index> | load <playable-id>\n\
         \x20                 | cuelist <n> on|off | play | pause | stop | seek <ms>\n\
         \x20                 | rate <permille>|reset | speedmode shift|correct\n\
         \x20                 | seekmode shift|correct | scan fwd|back|stop | seekto <ms>\n\
         \x20                 | seekoutside on|off | seekitem -1|0|1 <ms>\n\
         \x20                 | order normal|shuffle | repeat off|queue|item\n\
         \x20                 | pauseafter on|off | reshuffle on|off\n\
         (set PLIPPA_IPC_DIR to the running engine's IPC directory)"
    );
    std::process::exit(2);
}

fn parse<T: std::str::FromStr>(s: Option<&String>) -> T {
    s.and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())
}

fn on_off(s: Option<&String>) -> bool {
    match s.map(String::as_str) {
        Some("on") => true,
        Some("off") => false,
        _ => usage(),
    }
}

fn order_mode(s: Option<&String>) -> OrderMode {
    match s.map(String::as_str) {
        Some("normal") => OrderMode::Normal,
        Some("shuffle") => OrderMode::Shuffle,
        _ => usage(),
    }
}

fn repeat_mode(s: Option<&String>) -> RepeatMode {
    match s.map(String::as_str) {
        Some("off") => RepeatMode::Off,
        Some("queue") => RepeatMode::Queue,
        Some("item") => RepeatMode::Item,
        _ => usage(),
    }
}

fn warp_mode(s: Option<&String>) -> WarpMode {
    match s.map(String::as_str) {
        Some("shift") => WarpMode::PitchShift,
        Some("correct") => WarpMode::PitchCorrect,
        _ => usage(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match args.first().map(String::as_str) {
        Some("show") => None,
        Some("next") => Some(PlaybackCommand::Next),
        Some("prev") => Some(PlaybackCommand::Previous),
        Some("jump") => Some(PlaybackCommand::JumpToEntry {
            index: parse(args.get(1)),
        }),
        Some("load") => Some(PlaybackCommand::LoadCollection {
            playable_id: PlayableItemId::new(parse(args.get(1))),
        }),
        Some("cuelist") => Some(PlaybackCommand::SetCueListEnabled {
            list_index: parse(args.get(1)),
            enabled: on_off(args.get(2)),
        }),
        Some("play") => Some(PlaybackCommand::Play),
        Some("pause") => Some(PlaybackCommand::Pause),
        Some("stop") => Some(PlaybackCommand::Stop),
        Some("seek") => Some(PlaybackCommand::SeekMs(parse(args.get(1)))),
        Some("rate") => Some(match args.get(1).map(String::as_str) {
            Some("reset") => PlaybackCommand::ResetRate,
            _ => PlaybackCommand::SetRate {
                permille: parse(args.get(1)),
            },
        }),
        Some("speedmode") => Some(PlaybackCommand::SetSpeedWarpMode(warp_mode(args.get(1)))),
        Some("seekmode") => Some(PlaybackCommand::SetSeekWarpMode(warp_mode(args.get(1)))),
        Some("scan") => Some(match args.get(1).map(String::as_str) {
            Some("fwd") => PlaybackCommand::ScanStart {
                dir: ScanDir::Forward,
            },
            Some("back") => PlaybackCommand::ScanStart {
                dir: ScanDir::Backward,
            },
            Some("stop") => PlaybackCommand::ScanStop,
            _ => usage(),
        }),
        Some("seekto") => Some(PlaybackCommand::SeekTo {
            target_ms: parse(args.get(1)),
        }),
        Some("seekoutside") => Some(PlaybackCommand::SetSeekOutsideTrack(on_off(
            args.get(1),
        ))),
        Some("seekitem") => Some(PlaybackCommand::SeekToItem {
            item_delta: parse(args.get(1)),
            target_ms: parse(args.get(2)),
        }),
        // PLAY_02: the play-mode settings (engine memory; no `.noudata` involvement).
        Some("order") => Some(PlaybackCommand::SetOrderMode(order_mode(args.get(1)))),
        Some("repeat") => Some(PlaybackCommand::SetRepeatMode(repeat_mode(args.get(1)))),
        Some("pauseafter") => Some(PlaybackCommand::SetPauseAfterEach(on_off(args.get(1)))),
        Some("reshuffle") => Some(PlaybackCommand::SetReshuffleOnRepeat(on_off(
            args.get(1),
        ))),
        _ => usage(),
    };

    let paths = resolve_paths();
    let mut client = IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3))
        .unwrap_or_else(|e| {
            eprintln!(
                "plippa-np: cannot connect to {} ({e}); is PLIPPA_IPC_DIR right?",
                paths.engine_sock.display()
            );
            std::process::exit(1);
        });
    if let Some(c) = cmd {
        client
            .send(&IpcMessage::PlaybackCommand(c))
            .expect("send command");
    }

    // The hub pushes list + status on connect and status every 500 ms.
    let until = Instant::now() + Duration::from_millis(1800);
    let mut list = None;
    let mut status = None;
    while Instant::now() < until {
        match client.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(IpcMessage::NowPlayingListChanged(l))) => list = Some(l),
            Ok(Some(IpcMessage::PlaybackStatus(s))) => status = Some(s),
            Ok(Some(IpcMessage::UserNotice(n))) => {
                println!("notice [{:?}] {}: {}", n.severity, n.code, n.message);
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                eprintln!("plippa-np: recv error: {e}");
                break;
            }
        }
    }

    let (cur_track, cur_cue) = status
        .as_ref()
        .map(|s| (s.session.current_entry_index, s.session.current_cue_index))
        .unwrap_or((0, None));
    match &list {
        Some(l) => {
            println!("list kind={:?} entries={}", l.list_kind, l.entries.len());
            for (i, e) in l.entries.iter().enumerate() {
                let is_track = e.kind == NowPlayingEntryKind::Track;
                let playing = if is_track {
                    i as u32 == l.current_index
                } else {
                    cur_cue.is_some_and(|c| l.current_index + 1 + c == i as u32)
                };
                println!(
                    "{}{:>4}  {}{}  [{} ms{}]{}",
                    if playing { ">" } else { " " },
                    i,
                    if is_track { "" } else { "    cue: " },
                    e.title,
                    e.start_ms,
                    e.end_ms.map(|x| format!("..{x}")).unwrap_or_default(),
                    if e.available { "" } else { "  (MISSING)" },
                );
            }
        }
        None => println!("(no nowPlayingList — nothing loaded)"),
    }
    if let Some(s) = status {
        println!(
            "status transport={:?} track_entry={} cue={:?} position={} ms duration={:?}",
            s.transport, cur_track, cur_cue, s.session.position_ms, s.session.duration_ms
        );
        let x = &s.session;
        println!(
            "warp rate={} target_rate={} speed_mode={:?} seek_mode={:?} scan={:?} seek_target={:?} \
             voice={:?} active={} can_ff={} can_rw={} seek_outside={} prev_dur={:?} next_dur={:?}",
            x.rate_permille,
            x.target_rate_permille,
            x.speed_warp_mode,
            x.seek_warp_mode,
            x.scan,
            x.seek_target_ms,
            x.speed_voice,
            x.warp_active,
            x.can_ff,
            x.can_rw,
            x.seek_outside_track,
            x.prev_duration_ms,
            x.next_duration_ms
        );
        // PLAY_02: the player's play-mode settings and whether this list has a source order.
        println!(
            "mode order={} repeat={} pauseafter={} reshuffle={} source={}",
            match x.order_mode {
                OrderMode::Normal => "normal",
                OrderMode::Shuffle => "shuffle",
            },
            match x.repeat_mode {
                RepeatMode::Off => "off",
                RepeatMode::Queue => "queue",
                RepeatMode::Item => "item",
            },
            if x.pause_after_each { "on" } else { "off" },
            if x.reshuffle_on_repeat { "on" } else { "off" },
            match x.source_order {
                SourceOrder::Fixed => "fixed",
                SourceOrder::Dynamic => "dynamic",
            },
        );
    }
}
