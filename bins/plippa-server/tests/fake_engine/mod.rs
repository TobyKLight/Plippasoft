//! A tiny stand-in for the engine for portal tests: reads the commands the server sent to a
//! [`RecordingIpc`] and answers with the status + `nowPlayingList` frames the real engine would
//! publish (NB_04 shapes; navigation rules are simplified: Next = next cue else next track,
//! Previous = restart the entry). Only for tests and the Playwright fixture server.

use plippa_core::{
    NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind, PlayableItemId, SessionState,
    TransportState,
};
use plippa_ipc_proto::{IpcMessage, NowPlayingListChanged, PlaybackCommand, PlaybackStatus};
use plippa_server::{RecordingIpc, ServerService};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Cue {
    start_ms: u64,
    text: String,
}

struct Engine {
    id: u64,
    kind: NowPlayingListKind,
    /// (title, artist, cues of that track)
    tracks: Vec<(String, String, Vec<Cue>)>,
    current: usize,
    pos_ms: u64,
    playing: bool,
    rev: u64,
}

impl Engine {
    fn cue_index(&self) -> Option<u32> {
        let cues = &self.tracks[self.current].2;
        cues.iter()
            .rposition(|c| c.start_ms <= self.pos_ms)
            .map(|i| i as u32)
    }

    fn entries(&self) -> Vec<NowPlayingEntry> {
        let mut out = Vec::new();
        for (i, (title, artist, cues)) in self.tracks.iter().enumerate() {
            out.push(NowPlayingEntry {
                kind: NowPlayingEntryKind::Track,
                title: title.clone(),
                artist: artist.clone(),
                start_ms: 0,
                end_ms: None,
                duration_ms: Some(3_600_000),
                track_index: i as u32,
                playable_id: Some(PlayableItemId::new(self.id)),
                available: true,
                item_position: i as u32,
            });
            if i == self.current {
                for c in cues {
                    out.push(NowPlayingEntry {
                        kind: NowPlayingEntryKind::Cue,
                        title: c.text.clone(),
                        artist: String::new(),
                        start_ms: c.start_ms,
                        end_ms: None,
                        duration_ms: None,
                        track_index: i as u32,
                        playable_id: None,
                        available: true,
                        // Cue rows carry no source position (cues are identified by `start_ms`).
                        item_position: 0,
                    });
                }
            }
        }
        out
    }

    fn current_flat(&self) -> u32 {
        // Only the current track has cue rows after it, so earlier tracks are 1 row each.
        self.current as u32
    }

    fn publish(&mut self, ipc: &RecordingIpc, list_changed: bool) {
        self.rev += 1;
        let s = SessionState {
            transport: if self.playing {
                TransportState::Playing
            } else {
                TransportState::Paused
            },
            current_item: Some(PlayableItemId::new(self.id)),
            position_ms: self.pos_ms,
            duration_ms: Some(3_600_000),
            title: self.tracks[self.current].0.clone(),
            artist: self.tracks[self.current].1.clone(),
            current_entry_index: self.current as u32,
            current_cue_index: self.cue_index(),
            can_prev: true,
            can_next: true,
            ..SessionState::default()
        };
        ipc.inject_playback(PlaybackStatus::from_session_rev(s, self.rev));
        if list_changed {
            ipc.inject_list(NowPlayingListChanged {
                entries: self.entries(),
                current_index: self.current_flat(),
                list_kind: self.kind,
                list_id: 1,
            });
        }
    }
}

fn load(svc: &Arc<Mutex<ServerService>>, id: u64) -> Option<Engine> {
    let mut s = svc.lock().ok()?;
    let d = s.get_collection(id).ok()?;
    let mut tracks = Vec::new();
    for it in &d.items {
        let mut cues = Vec::new();
        if let Ok(ic) = s.get_item_cues(id, Some(it.position)) {
            for l in ic.cue_lists.iter().filter(|l| l.navigable) {
                cues.extend(l.cues.iter().map(|c| Cue {
                    start_ms: c.start_ms,
                    text: c.text.clone(),
                }));
            }
            cues.sort_by_key(|c| c.start_ms);
        }
        tracks.push((
            it.title.clone(),
            it.artist.clone().unwrap_or_default(),
            cues,
        ));
    }
    if tracks.is_empty() {
        return None;
    }
    let kind = match serde_json::to_value(d.summary.kind).ok()?.as_str()? {
        "album" => NowPlayingListKind::Album,
        "playlist" => NowPlayingListKind::Playlist,
        "mix" => NowPlayingListKind::Mix,
        "audiobook" => NowPlayingListKind::Audiobook,
        "podcast" => NowPlayingListKind::Podcast,
        _ => NowPlayingListKind::Single,
    };
    Some(Engine {
        id,
        kind,
        tracks,
        current: 0,
        pos_ms: 0,
        playing: true,
        rev: 0,
    })
}

/// Spawn the fake engine. `delay` postpones every answer (tests the wait logic).
pub fn spawn(ipc: RecordingIpc, svc: Arc<Mutex<ServerService>>, delay: Duration) {
    std::thread::spawn(move || {
        let mut seen = 0usize;
        let mut eng: Option<Engine> = None;
        loop {
            std::thread::sleep(Duration::from_millis(15));
            let msgs = ipc.messages();
            for m in msgs.iter().skip(seen) {
                let IpcMessage::PlaybackCommand(cmd) = m else {
                    continue;
                };
                std::thread::sleep(delay);
                match cmd {
                    PlaybackCommand::LoadAndPlay(id)
                    | PlaybackCommand::LoadCollection { playable_id: id } => {
                        eng = load(&svc, id.as_u64());
                        if let Some(e) = eng.as_mut() {
                            e.publish(&ipc, true);
                        }
                    }
                    PlaybackCommand::JumpToEntry { index } => {
                        if let Some(e) = eng.as_mut() {
                            let entries = e.entries();
                            if let Some(en) = entries.get(*index as usize) {
                                if en.kind == NowPlayingEntryKind::Track {
                                    e.current = en.track_index as usize;
                                    e.pos_ms = 0;
                                    e.playing = true;
                                    e.publish(&ipc, true);
                                } else {
                                    e.pos_ms = en.start_ms;
                                    e.publish(&ipc, false);
                                }
                            }
                        }
                    }
                    PlaybackCommand::SeekMs(ms) | PlaybackCommand::SeekTo { target_ms: ms } => {
                        if let Some(e) = eng.as_mut() {
                            e.pos_ms = *ms;
                            e.publish(&ipc, false);
                        }
                    }
                    PlaybackCommand::Next | PlaybackCommand::NextTrack => {
                        if let Some(e) = eng.as_mut() {
                            let next_cue = e.tracks[e.current]
                                .2
                                .iter()
                                .find(|c| c.start_ms > e.pos_ms)
                                .map(|c| c.start_ms);
                            match next_cue {
                                Some(ms) => {
                                    e.pos_ms = ms;
                                    e.publish(&ipc, false);
                                }
                                None if e.current + 1 < e.tracks.len() => {
                                    e.current += 1;
                                    e.pos_ms = 0;
                                    e.publish(&ipc, true);
                                }
                                None => {}
                            }
                        }
                    }
                    PlaybackCommand::Previous | PlaybackCommand::PreviousTrack => {
                        if let Some(e) = eng.as_mut() {
                            e.pos_ms = 0;
                            e.publish(&ipc, false);
                        }
                    }
                    PlaybackCommand::Play | PlaybackCommand::Pause => {
                        if let Some(e) = eng.as_mut() {
                            e.playing = matches!(cmd, PlaybackCommand::Play);
                            e.publish(&ipc, false);
                        }
                    }
                    PlaybackCommand::Stop | PlaybackCommand::Unload => eng = None,
                    // WARP_01: tempo / warp-mode / scan commands are accepted and ignored (the
                    // fake keeps no warp state; the real engine is covered by its own tests).
                    PlaybackCommand::SetRate { .. }
                    | PlaybackCommand::ResetRate
                    | PlaybackCommand::ResetRateInstant
                    | PlaybackCommand::SetSpeedWarpMode(_)
                    | PlaybackCommand::SetSeekWarpMode(_)
                    | PlaybackCommand::ScanStart { .. }
                    | PlaybackCommand::ScanStop => {}
                    _ => {}
                }
            }
            seen = msgs.len();
        }
    });
}
