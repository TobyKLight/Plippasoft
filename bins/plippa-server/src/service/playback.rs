//! NB_10: portal transport for the `nowPlayingList` — Next / Previous / jump, "play this
//! collection (from item N / cue)", and the list the engine last published.
//!
//! The engine owns the list; the server only mirrors what it published (`IpcMirror::list`)
//! and forwards NB_04 commands. Playing *from* an item/cue of a collection that is not the
//! loaded one needs the flat entry index, which only exists after the engine has published
//! the new list, so [`play_collection_unlocked`] waits for it (without holding the service
//! lock) and then sends `JumpToEntry` / `SeekMs`.

use super::{serialize_u64_as_string, ServerError, ServerService};
use plippa_core::{NowPlayingEntryKind, PlayableItemId};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long "play from item/cue" waits for the engine to publish the freshly loaded list.
const LIST_WAIT: Duration = Duration::from_secs(15);
const LIST_POLL: Duration = Duration::from_millis(40);

/// Body of `POST /api/collections/:id/play`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlayCollection {
    /// `mediaList` position (from `GET /api/collections/:id`) to start from. Absent = the
    /// whole collection from its start (or its resume point, per NB_04).
    #[serde(default)]
    pub item: Option<u32>,
    /// Start this many ms into the item (a cue's `start_ms`).
    #[serde(default)]
    pub cue_ms: Option<u64>,
}

/// One row of `GET /api/now-playing/list` (a track, or a cue of the current track).
#[derive(Debug, Clone, Serialize)]
pub struct ListEntryOut {
    /// `"track"` or `"cue"`.
    pub kind: &'static str,
    /// Position in the flat list — what `POST /api/transport/jump` takes.
    pub index: u32,
    pub title: String,
    pub artist: String,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub track_index: u32,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NowPlayingListOut {
    /// Bumps whenever the engine published a new list (matches `list_seq` of `/api/now-playing`).
    pub seq: u64,
    pub list_kind: Option<String>,
    /// Flat index of the current *track* row; the playing cue is the row after it plus
    /// `current_cue_index`.
    pub current_index: u32,
    pub current_cue_index: Option<u32>,
    pub entries: Vec<ListEntryOut>,
}

impl ServerService {
    fn send_playback(&mut self, cmd: PlaybackCommand, what: &str) -> Result<(), ServerError> {
        let live = self.with_ipc(|ipc| {
            if !ipc.is_live() {
                return false;
            }
            ipc.poll_inbound();
            ipc.send(IpcMessage::PlaybackCommand(cmd));
            true
        });
        if live {
            Ok(())
        } else {
            Err(ServerError::Message(format!(
                "Engine not connected — start `plippa` / `plippa-engine` so {what} can reach audio"
            )))
        }
    }

    /// Next cue / track (NB_04 rules apply in the engine).
    pub fn next_entry(&mut self) -> Result<(), ServerError> {
        self.send_playback(PlaybackCommand::Next, "Next")
    }

    /// Previous: restart the current entry once it has played for more than 1 s; at the entry
    /// start it steps back, and a second press within the window goes back.
    pub fn previous_entry(&mut self) -> Result<(), ServerError> {
        self.send_playback(PlaybackCommand::Previous, "Previous")
    }

    /// Jump to a row of the published list (a cue row seeks inside the current track).
    pub fn jump_to_entry(&mut self, index: u32) -> Result<(), ServerError> {
        let len = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.mirror().list.map_or(0, |l| l.entries.len())
        });
        if index as usize >= len {
            return Err(ServerError::Invalid(format!(
                "no entry {index} in the list ({len} entries)"
            )));
        }
        self.send_playback(PlaybackCommand::JumpToEntry { index }, "Jump")
    }

    /// `LoadCollection` (honours the persisted resume point, as a tap does).
    pub fn play_collection(&mut self, id: u64) -> Result<(), ServerError> {
        let _ = self.require_record(id)?;
        self.send_playback(
            PlaybackCommand::LoadCollection {
                playable_id: PlayableItemId::new(id),
            },
            "Play",
        )
    }

    /// The rows the engine last published (empty before the first list).
    pub fn now_playing_list(&mut self) -> NowPlayingListOut {
        let mirror = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.mirror()
        });
        let cue = mirror
            .playback
            .as_ref()
            .and_then(|p| p.session.current_cue_index);
        match mirror.list {
            None => NowPlayingListOut {
                seq: mirror.list_seq,
                list_kind: None,
                current_index: 0,
                current_cue_index: None,
                entries: Vec::new(),
            },
            Some(l) => NowPlayingListOut {
                seq: mirror.list_seq,
                list_kind: Some(format!("{:?}", l.list_kind).to_lowercase()),
                current_index: l.current_index,
                current_cue_index: cue,
                entries: l
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| ListEntryOut {
                        kind: match e.kind {
                            NowPlayingEntryKind::Track => "track",
                            NowPlayingEntryKind::Cue => "cue",
                        },
                        index: i as u32,
                        title: e.title.clone(),
                        artist: e.artist.clone(),
                        start_ms: e.start_ms,
                        end_ms: e.end_ms,
                        duration_ms: e.duration_ms,
                        track_index: e.track_index,
                        available: e.available,
                    })
                    .collect(),
            },
        }
    }
}

/// Response of a play-from request.
#[derive(Debug, Clone, Serialize)]
pub struct PlayCollectionOut {
    pub ok: bool,
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
}

fn snapshot(
    service: &Arc<Mutex<ServerService>>,
) -> Result<(crate::ipc::IpcMirror, u64), ServerError> {
    let s = service
        .lock()
        .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
    let m = s.with_ipc(|ipc| {
        ipc.poll_inbound();
        ipc.mirror()
    });
    let seq = m.list_seq;
    Ok((m, seq))
}

/// Play a collection, optionally from `item` (a `mediaList` position) and `cue_ms`.
///
/// Without `item` this is a plain `LoadCollection`. With `item`: if that collection is
/// already loaded its list is reused, else it is loaded from the start (`LoadAndPlay`, not
/// the resume point — the user asked for a specific item); then `JumpToEntry` to the item's
/// track row and, for a cue, `SeekMs`. Waits for the engine between the steps but never
/// holds the service lock while waiting.
pub fn play_collection_unlocked(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
    req: PlayCollection,
) -> Result<PlayCollectionOut, ServerError> {
    let out = PlayCollectionOut { ok: true, id };
    let Some(item) = req.item else {
        service
            .lock()
            .map_err(|_| ServerError::Message("service lock poisoned".into()))?
            .play_collection(id)?;
        return Ok(out);
    };

    // Which track of the list is this item? (ordinal among the collection's items)
    let (ordinal, item_title) = {
        let mut s = service
            .lock()
            .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
        let detail = s.get_collection(id)?;
        let ordinal = detail
            .items
            .iter()
            .position(|i| i.position == item)
            .ok_or_else(|| ServerError::NotFound(format!("no item {item} in this collection")))?;
        (ordinal, detail.items[ordinal].title.clone())
    };

    let (mirror, seq0) = snapshot(service)?;
    let loaded = mirror
        .playback
        .as_ref()
        .and_then(|p| p.session.current_item)
        .map(|i| i.as_u64())
        == Some(id)
        && mirror.list.as_ref().is_some_and(|l| !l.entries.is_empty());
    if !loaded {
        let mut s = service
            .lock()
            .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
        let _ = s.require_record(id)?;
        s.send_playback(
            PlaybackCommand::LoadAndPlay(PlayableItemId::new(id)),
            "Play",
        )?;
    }

    // Wait for the list of this collection (a new publication after LoadAndPlay).
    let deadline = Instant::now() + LIST_WAIT;
    let list = loop {
        let (m, seq) = snapshot(service)?;
        if let Some(l) = m.list {
            if loaded || seq > seq0 {
                break l;
            }
        }
        if Instant::now() >= deadline {
            return Err(ServerError::Unavailable(
                "the engine did not answer in time — is the file playable?".into(),
            ));
        }
        std::thread::sleep(LIST_POLL);
    };

    // Flat index of the wanted track row: by ordinal (list order = item order unless the
    // collection shuffles), confirmed by title, else the first track with that title.
    let tracks: Vec<(usize, &plippa_core::NowPlayingEntry)> = list
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == NowPlayingEntryKind::Track)
        .collect();
    let flat = tracks
        .iter()
        .find(|(_, e)| e.track_index as usize == ordinal && e.title == item_title)
        .or_else(|| tracks.iter().find(|(_, e)| e.title == item_title))
        .or_else(|| {
            tracks
                .iter()
                .find(|(_, e)| e.track_index as usize == ordinal)
        })
        .map(|(i, _)| *i as u32)
        .ok_or_else(|| ServerError::NotFound("that item is not in the playing list".into()))?;

    let cue_ms = req.cue_ms.filter(|ms| *ms > 0);
    let mut s = service
        .lock()
        .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
    let current = list.current_index;
    let seq_now = s.with_ipc(|ipc| ipc.mirror().list_seq);
    if flat != current {
        s.send_playback(PlaybackCommand::JumpToEntry { index: flat }, "Jump")?;
        if cue_ms.is_some() {
            // Let the track change land (the engine publishes a list on it) before seeking.
            drop(s);
            let deadline = Instant::now() + LIST_WAIT;
            loop {
                let (_, seq) = snapshot(service)?;
                if seq > seq_now || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(LIST_POLL);
            }
            s = service
                .lock()
                .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
        }
    } else if loaded {
        // Already on that track: make sure it is playing (a Stopped/Paused list keeps it).
        s.send_playback(PlaybackCommand::Play, "Play")?;
    }
    if let Some(ms) = cue_ms {
        s.send_playback(PlaybackCommand::SeekMs(ms), "Seek")?;
    }
    Ok(out)
}
