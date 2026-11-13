//! END-TO-END GOAL CHECK (NB_VERIFY G4): "play playlists and mixtapes on the device and
//! import/manage them properly in the web UI", on a temp library, no hardware.
//!
//! Real components, wired like production (docs/design/ipc.md): the **engine hub**
//! (`Engine::open_mock_discovered` = the `--sink=mock` binary path, UDS `engine.sock`), the
//! **plippa-server** HTTP portal (real `serve`, real `ServerService`, real resume writer,
//! real `ReconnectingIpc` to the engine), and the **device UI** (`UiApp` + `MockLedCue` with the
//! real plippa-D-poc2 board profile + `MockDisplay`) as a third peer on the same socket.
//! Only the audio sink, display and LED strips are mocks. The journey:
//!
//!  1. import a mixtape through `POST /api/media` with pasted timestamps, and three library
//!     tracks; 2. the mixtape is indexed as kind `mix` with navigable cues; 3. edit its
//!     timestamps via `PUT /api/items/:id/cues`; 4. create a playlist from library tracks and
//!     add one more; 5. load the playlist: Next / Previous (FIX_01C: a first press restarts
//!     only after 1 s of the entry, a paired second press goes back) / jump by HTTP and by
//!     the knob; 6. load the mixtape: cue navigation,
//!     `nowPlayingList` + highlight follow the *media position*; 7. `can_prev`/`can_next`
//!     published by the engine, device UI model, prev/next LEDs; 8. `.noudata` files on disk
//!     are spec-valid and `parse_noudata_with_warnings` yields no warnings (also after the
//!     engine's resume write-back).
//!
//! Only timing in the test: bounded polls (15 s) for real-time mock-sink playback events, one
//! deliberate 3.2 s wait to prove the Previous double-press window expires, and polling for
//! the 1 s restart threshold before pressing Previous (FIX_01C).

use plippa_audio::{AudioError, AudioSink};
use plippa_board::{BoardProfile, ControlRole, LedRole};
use plippa_core::{Capabilities, TransportState};
use plippa_engine::{run_hub, Engine, HubShutdown};
use plippa_ipc_proto::{IpcConnection, IpcMessage, IpcPaths, PlaybackCommand, PlaybackStatus};
use plippa_noubin::{parse_noudata_with_warnings, NoubinTimestamp};
use plippa_server::{connect_or_null, spawn_resume_writer, ServerConfig, ServerService};
use plippa_ui::ui::tokens::{ACCENT_2, BG, BG_ALT, BORDER_SOFT, SURFACE, SURFACE_ALT};
use plippa_ui::{
    sync_nav_leds, InputEvent, MockDisplay, MockLedCue, MockWifi, NpMode, NpTarget, UiApp,
    UiConfig, UiOutgoing, UP_NEXT_ROWS, UP_NEXT_ROWS_Y,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- tiny HTTP client

fn http_raw(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http response");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, raw[split + 4..].to_vec())
}

/// JSON request exactly as the portal's `api()` sends it (`Content-Type` + `X-Plippa`).
fn api(port: u16, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let text = body.map(|b| b.to_string()).unwrap_or_default();
    let (st, bytes) = http_raw(
        port,
        method,
        path,
        text.as_bytes(),
        &[("Content-Type", "application/json"), ("X-Plippa", "1")],
    );
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `POST /api/media` (Add media dialog): raw audio body, `cue_text` pasted timestamps.
fn upload(port: u16, kind: &str, filename: &str, wav: &[u8], cue_text: Option<&str>) -> Value {
    let mut path = format!("/api/media?kind={kind}&filename={}", pct(filename));
    if let Some(c) = cue_text {
        path.push_str(&format!("&cue_text={}", pct(c)));
    }
    let (st, bytes) = http_raw(port, "POST", &path, wav, &[("X-Plippa", "1")]);
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(st, 200, "upload {filename}: {v}");
    v
}

// ---------------------------------------------------------------- audio

fn wav(secs: u32) -> Vec<u8> {
    let rate = 8000u32;
    let data = vec![128u8; (rate * secs) as usize];
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// `wav(secs)` carrying ID3v2 artist (+ optional album) tags: the import rule makes that a
/// `release` (no usable tags would make it a loose track).
fn wav_tagged(secs: u32, artist: &str, album: Option<&str>) -> Vec<u8> {
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.wav");
    fs::write(&path, wav(secs)).unwrap();
    let mut tagged = lofty::probe::Probe::open(&path).unwrap().read().unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_artist(artist.into());
    if let Some(a) = album {
        tag.set_album(a.into());
    }
    tagged.insert_tag(tag);
    tagged.save_to_path(&path, WriteOptions::default()).unwrap();
    fs::read(&path).unwrap()
}

// ---------------------------------------------------------------- real-time mock sink

struct PacedSink {
    channels: u16,
    sample_rate_hz: u32,
}

impl AudioSink for PacedSink {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let frames = interleaved.len() / self.channels.max(1) as usize;
        thread::sleep(Duration::from_micros(
            frames as u64 * 1_000_000 / u64::from(self.sample_rate_hz),
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------- the rig

struct Rig {
    root: PathBuf,
    port: u16,
    stop: HubShutdown,
    hub: Option<thread::JoinHandle<()>>,
    /// Device UI peer on the engine socket.
    conn: IpcConnection,
    ui: UiApp,
    wifi: MockWifi,
    display: MockDisplay,
    led: MockLedCue,
    status: Option<PlaybackStatus>,
    _tmp: tempfile::TempDir,
    _ipc: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.hub.take() {
            let _ = h.join();
        }
    }
}

impl Rig {
    fn start() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitkeep"), "").unwrap();
        let ipc_dir = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(ipc_dir.path());

        // Engine hub (the `plippa-engine --sink=mock` code path) on an empty library.
        let stop: HubShutdown = Arc::new(AtomicBool::new(false));
        let hub = {
            let (root, paths, stop) = (root.clone(), paths.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                // Mock audio sink, but *paced at real time*: the stock `SharedMockSink` swallows
                // audio at CPU speed (a 60 s file "plays" in 0.3 s), which makes position-based
                // assertions race. Everything else is the real engine.
                let mut engine = Engine::open_with_sink_factory_discovered(
                    Some(root),
                    Capabilities::desktop_mock(),
                    |channels, sample_rate_hz| {
                        Ok(Box::new(PacedSink {
                            channels,
                            sample_rate_hz,
                        }) as Box<dyn AudioSink>)
                    },
                )
                .unwrap();
                run_hub(
                    &mut engine,
                    &paths,
                    Some(Instant::now() + Duration::from_secs(240)),
                    Some(stop),
                )
                .unwrap();
            })
        };
        let t0 = Instant::now();
        while !paths.engine_sock.exists() {
            assert!(t0.elapsed() < Duration::from_secs(15), "engine socket");
            thread::sleep(Duration::from_millis(20));
        }

        // plippa-server exactly as main.rs wires it (minus the background rescan).
        let ipc = connect_or_null(&paths.engine_sock, Duration::from_secs(5));
        let service = Arc::new(Mutex::new(ServerService::start(
            Some(root.clone()),
            ipc,
            ServerService::default_mock_associate(),
        )));
        let resume_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _ = spawn_resume_writer(
            Arc::clone(&service),
            Duration::from_millis(200),
            Arc::clone(&resume_stop),
        );
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let cfg = ServerConfig {
            bind: format!("127.0.0.1:{port}").parse().unwrap(),
            library_root: Some(root.clone()),
            // No store credentials in this harness: the stores panel reports itself
            // unavailable and the tap flow degrades to its "no store" state.
            secrets_dir: None,
            portal_url: None,
            // The browser side of this journey talks to the portal port; there is no device UI
            // over the control listener here.
            control_bind: None,
            ..ServerConfig::default()
        };
        // The portal switched on with no PIN: the journey is about the library, not the gate.
        let access = Arc::new(plippa_server::PortalAccess::memory(
            Arc::new(plippa_server::ManualClock::new()),
            plippa_server::LockoutPolicy::default(),
        ));
        access.set_enabled(true).unwrap();
        access.set_pin_required(false).unwrap();
        // The browser side of this journey uses `http://127.0.0.1:<port>`; HTTPS is the device
        // default and fails closed with no secrets directory, so plain HTTP is asked for here.
        access.set_https_enabled(false).unwrap();
        {
            let service = Arc::clone(&service);
            thread::spawn(move || {
                let _ = plippa_server::serve_with_access(
                    cfg,
                    service,
                    None,
                    plippa_server::HttpLimits::default(),
                    access,
                );
            });
        }
        let t0 = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(t0.elapsed() < Duration::from_secs(10), "server start");
            thread::sleep(Duration::from_millis(20));
        }

        let conn =
            IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(5)).unwrap();
        let profile =
            BoardProfile::load(plippa_board::workspace_root_from_crate(), "plippa-D-poc2").unwrap();
        Rig {
            root,
            port,
            stop,
            hub: Some(hub),
            conn,
            ui: UiApp::new(UiConfig::default()),
            wifi: MockWifi::default(),
            display: MockDisplay::new(320, 240),
            led: MockLedCue::for_profile(&profile),
            status: None,
            _tmp: tmp,
            _ipc: ipc_dir,
        }
    }

    /// One UI-loop pass: engine messages -> UI model, UI commands -> engine, LEDs from the model.
    fn pump(&mut self) {
        while let Ok(Some(msg)) = self.conn.recv_timeout(Duration::from_millis(15)) {
            if let IpcMessage::PlaybackStatus(s) = &msg {
                self.status = Some(s.clone());
            }
            self.ui.handle_ipc(msg);
        }
        self.flush_ui();
        sync_nav_leds(&self.ui, &mut self.led);
        self.ui.present(&mut self.display);
    }

    fn flush_ui(&mut self) {
        for out in self.ui.take_outgoing() {
            if let UiOutgoing::Ipc(m) = out {
                self.conn.send(&m).unwrap();
            }
        }
    }

    fn until(&mut self, what: &str, f: impl Fn(&Rig) -> bool) {
        let t = Instant::now();
        loop {
            self.pump();
            if std::env::var("GJ_TRACE").is_ok() {
                if let Some(st) = &self.status {
                    eprintln!(
                        "[{what}] t={:?} {:?} pos={} e={} c={:?}",
                        t.elapsed(),
                        st.session.transport,
                        st.session.position_ms,
                        st.session.current_entry_index,
                        st.session.current_cue_index
                    );
                }
            }
            if f(self) {
                return;
            }
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "timed out waiting for: {what} (status: {:?})",
                self.status.as_ref().map(|s| (
                    s.session.transport,
                    s.session.position_ms,
                    s.session.current_entry_index,
                    s.session.current_cue_index
                ))
            );
        }
    }

    fn press(&mut self, role: ControlRole) {
        self.ui
            .handle_input(InputEvent::ControlPressed(role), &mut self.wifi);
        self.flush_ui();
        // Flash requests the press produced (the loop runner does this right after the event).
        plippa_ui::flash_nav_leds(&mut self.ui, &mut self.led);
        self.pump();
    }

    fn knob(&mut self, ev: InputEvent) {
        self.ui.handle_input(ev, &mut self.wifi);
        self.pump();
    }

    fn sess(&self) -> &plippa_core::SessionState {
        &self.status.as_ref().expect("status").session
    }

    fn entry_cue(&self) -> (u32, Option<u32>) {
        (
            self.sess().current_entry_index,
            self.sess().current_cue_index,
        )
    }

    fn row(&self) -> Option<usize> {
        self.ui.model.now_playing_row()
    }

    fn list_titles(&self) -> Vec<String> {
        self.ui
            .model
            .now_playing
            .as_ref()
            .map(|l| l.entries.iter().map(|e| e.title.clone()).collect())
            .unwrap_or_default()
    }

    fn lit(&self, role: LedRole) -> bool {
        self.led.role_colours.contains_key(&role)
    }

    /// (Previous lit, Next lit) as the LEDs show them right now.
    fn leds(&self) -> (bool, bool) {
        (
            self.lit(LedRole::PrevTrackButton),
            self.lit(LedRole::NextTrackButton),
        )
    }

    fn published(&self) -> (bool, bool) {
        (self.sess().can_prev, self.sess().can_next)
    }

    fn collection_id(&self, kind: &str, title: &str) -> String {
        let (st, v) = api(
            self.port,
            "GET",
            &format!("/api/collections?kind={kind}"),
            None,
        );
        assert_eq!(st, 200, "{v}");
        v["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["title"] == title)
            .unwrap_or_else(|| panic!("no {kind} `{title}` in {v}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn play(&self, id: &str) {
        let (st, v) = api(
            self.port,
            "POST",
            &format!("/api/collections/{id}/play"),
            Some(json!({})),
        );
        assert_eq!(st, 200, "play {id}: {v}");
    }
}

// ---------------------------------------------------------------- spec checks on disk

fn all_noudata(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if e.file_name() == ".plippa" {
            continue;
        }
        if p.is_dir() {
            all_noudata(root, &p, out);
        } else if p.extension().is_some_and(|x| x == "noudata") {
            out.push(p);
        }
    }
}

/// Every `.noudata` under the library: parses with **no warnings**, is a JSON object whose
/// `mediaList` items all have a `localFilePaths` entry that exists, cue lists use spec
/// enums and strictly increasing canonical `startTime`s. Returns how many files were checked.
fn assert_library_spec_valid(root: &Path) -> usize {
    let mut files = Vec::new();
    all_noudata(root, root, &mut files);
    assert!(!files.is_empty());
    for f in &files {
        let bytes = fs::read(f).unwrap();
        let (doc, warnings) = parse_noudata_with_warnings(&bytes).unwrap();
        assert!(
            warnings.is_empty(),
            "{}: parse warnings {warnings:?}",
            f.display()
        );
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.is_object());
        assert!(
            v["playableItemTitle"]
                .as_str()
                .is_some_and(|t| !t.trim().is_empty()),
            "{}: title",
            f.display()
        );
        let items = v["mediaList"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: mediaList", f.display()));
        assert!(!items.is_empty(), "{}: empty mediaList", f.display());
        // v0.4.0: every file Plippa writes declares its type and obeys its MUST/MUST NOT rules.
        assert!(v["noudataType"].is_string(), "{}: noudataType", f.display());
        assert!(
            doc.type_violations().is_empty(),
            "{}: {:?}",
            f.display(),
            doc.type_violations()
        );
        assert_eq!(items.len(), doc.media_list.len());
        for item in items {
            let paths = item["localFilePaths"].as_array().expect("localFilePaths");
            assert!(!paths.is_empty());
            for p in paths {
                let rel = p.as_str().unwrap();
                assert!(!rel.contains('\\') && !rel.starts_with('/'), "{rel}");
                let target = f.parent().unwrap().join(rel);
                assert!(target.is_file(), "{}: {rel} does not exist", f.display());
            }
            for list in item["cueLists"].as_array().into_iter().flatten() {
                assert!(
                    ["nav", "credits", "text", "other"]
                        .contains(&list["cueListType"].as_str().unwrap_or("")),
                    "cueListType in {list}"
                );
                let mut last = None;
                for cue in list["cues"].as_array().expect("cues") {
                    let t = cue["startTime"].as_str().expect("startTime");
                    // Spec 5.2 Lazy Timestamp (SS, MM:SS, HH:MM:SS[.mmm] ...).
                    let ms = NoubinTimestamp::parse(t)
                        .unwrap_or_else(|e| panic!("startTime `{t}`: {e}"))
                        .as_millis();
                    assert!(
                        last.is_none_or(|l| l < ms),
                        "cues not strictly increasing at `{t}`"
                    );
                    last = Some(ms);
                    assert!(
                        cue["cueTextSpans"]
                            .as_array()
                            .is_some_and(|s| !s.is_empty()),
                        "cue without text spans: {cue}"
                    );
                }
            }
        }
    }
    files.len()
}

fn doc_for(root: &Path, rel: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(rel)).unwrap()).unwrap()
}

// ---------------------------------------------------------------- the journey

#[test]
fn goal_import_manage_play_playlists_and_mixtapes() {
    let mut r = Rig::start();
    let port = r.port;

    // ===== Step 0: nothing loaded -> engine publishes can_* = false, both LEDs dark, presses inert.
    r.until("first status", |r| r.status.is_some());
    assert_eq!(r.published(), (false, false), "nothing loaded");
    assert_eq!(r.leds(), (false, false), "nothing loaded: prev/next dark");
    r.press(ControlRole::NextTrack);
    r.press(ControlRole::PrevTrack);
    assert!(r.led.role_flashes.is_empty(), "unavailable press: no flash");

    // ===== Step 1: import. Mixtape with pasted timestamps + three library tracks.
    let pasted = "Tracklist (copied from the release page)\n0:00 Intro\n0:10 - Second Tune\n0:20 Third Tune\n0:30 Closer\nthanks!\n";
    let mix = upload(
        port,
        "mix",
        "Friday Night Mix.wav",
        &wav_tagged(60, "DJ Test", None),
        Some(pasted),
    );
    assert_eq!(mix["cues"]["cue_count"], 4, "{mix}");
    let mix_id = mix["id"].as_str().unwrap().to_string();
    let mix_rel = mix["noudata_relpath"].as_str().unwrap().to_string();
    for (n, name) in ["01 Alpha", "02 Bravo", "03 Charlie"].iter().enumerate() {
        let v = upload(
            port,
            "music",
            &format!("{name}.wav"),
            &wav_tagged(40 + n as u32, "The Band", None),
            None,
        );
        assert!(v["id"].is_string(), "{v}");
    }
    // A tagless file is a looseTrack (no releaseData, no placeholder artist), kind `track`.
    let loose = upload(port, "music", "untagged demo.wav", &wav(30), None);
    let loose_doc = doc_for(&r.root, loose["noudata_relpath"].as_str().unwrap());
    assert_eq!(loose_doc["noudataType"], "looseTrack", "{loose_doc}");
    assert!(loose_doc.get("releaseData").is_none());
    assert_eq!(loose_doc["playableItemProfile"], "music");
    let (_, tr) = api(port, "GET", "/api/collections?kind=track", None);
    assert!(
        tr["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["title"] == "untagged demo"),
        "loose track listed under Tracks: {tr}"
    );

    // ===== Step 2: indexed as kind mix, with navigable cues; library tracks listed.
    let (st, v) = api(port, "GET", "/api/collections?kind=mix", None);
    assert_eq!(st, 200);
    let mixes = v["collections"].as_array().unwrap();
    assert_eq!(mixes.len(), 1, "{v}");
    assert_eq!(mixes[0]["kind"], "mix");
    assert_eq!(mixes[0]["title"], "Friday Night Mix");
    assert_eq!(mixes[0]["has_navigable_cues"], true);
    assert_eq!(mixes[0]["id"], mix_id.as_str());
    let (_, tracks) = api(port, "GET", "/api/collections?kind=album", None);
    assert_eq!(
        tracks["collections"].as_array().unwrap().len(),
        3,
        "{tracks}"
    );
    let (_, cues) = api(port, "GET", &format!("/api/items/{mix_id}/cues"), None);
    assert_eq!(cues["cue_lists"][0]["cues"].as_array().unwrap().len(), 4);
    assert_eq!(
        cues["cue_lists"][0]["cues"][1]["text"], "Second Tune",
        "leading ` - ` stripped: {cues}"
    );
    let doc = doc_for(&r.root, &mix_rel);
    assert_eq!(doc["playableItemProfile"], "mix");
    assert_eq!(doc["mediaList"][0]["cueLists"][0]["cueListType"], "nav");
    assert_library_spec_valid(&r.root);

    // ===== Step 3: edit the timestamps through the API (revision guarded).
    let rev = cues["revision"].as_str().unwrap().to_string();
    let edited = "0:00 Opening\n0:15 Second half\n0:40 Finale\n";
    let (st, saved) = api(
        port,
        "PUT",
        &format!("/api/items/{mix_id}/cues"),
        Some(json!({"text": edited, "revision": rev})),
    );
    assert_eq!(st, 200, "{saved}");
    let (_, again) = api(port, "GET", &format!("/api/items/{mix_id}/cues"), None);
    let names: Vec<&str> = again["cue_lists"][0]["cues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Opening", "Second half", "Finale"]);
    // A stale revision is refused, nothing changes.
    let (st, _) = api(
        port,
        "PUT",
        &format!("/api/items/{mix_id}/cues"),
        Some(json!({"text": "0:00 X\n", "revision": rev})),
    );
    assert_eq!(st, 409, "stale revision");
    let doc = doc_for(&r.root, &mix_rel);
    assert_eq!(
        doc["mediaList"][0]["cueLists"][0]["cues"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_library_spec_valid(&r.root);

    // ===== Step 4: playlist from library tracks (create with two, then add a third).
    let id_of = |title: &str| r.collection_id("album", title);
    let (alpha, bravo, charlie) = (id_of("01 Alpha"), id_of("02 Bravo"), id_of("03 Charlie"));
    let (st, pl) = api(
        port,
        "POST",
        "/api/playlists",
        Some(json!({"title": "Road Trip", "tracks": [{"track_id": charlie}, {"track_id": alpha}]})),
    );
    assert_eq!(st, 200, "{pl}");
    let pl_id = pl["id"].as_str().unwrap().to_string();
    let (st, pl) = api(
        port,
        "POST",
        &format!("/api/playlists/{pl_id}/items"),
        Some(json!({"track_id": bravo, "revision": pl["revision"]})),
    );
    assert_eq!(st, 200, "{pl}");
    let order: Vec<&str> = pl["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["03 Charlie", "01 Alpha", "02 Bravo"]);
    assert_eq!(r.collection_id("playlist", "Road Trip"), pl_id);
    assert_library_spec_valid(&r.root);

    // ===== Step 5: load the playlist into the engine; device UI shows it; Next/Prev/jump.
    r.play(&pl_id);
    r.until("playlist list on the device", |r| {
        r.list_titles() == ["03 Charlie", "01 Alpha", "02 Bravo"]
    });
    r.until("playing", |r| r.sess().transport == TransportState::Playing);
    assert!(r.ui.model.list_panel_visible(), "3 entries: list panel");
    assert_eq!(r.row(), Some(0));
    assert_eq!(r.entry_cue(), (0, None));
    // NB_04b: Previous is lit on track 1 (something is loaded); Next lit.
    assert_eq!(r.published(), (true, true));
    assert_eq!(r.leds(), (true, true));
    // WAVE_02: the list is an overlay reached by rotating to the list target; the waveform
    // owns the screen otherwise. Reveal it, check the painting, then return to the waveform.
    assert!(!r.ui.model.up_next_visible());
    r.knob(InputEvent::MenuRotate(1));
    assert_eq!(r.ui.model.np_focus.target, NpTarget::List);
    assert!(r.ui.model.up_next_visible());
    // Rows paint their own grounds, so count only foreground ink from the first row, and
    // require the playing marker specifically.
    let fb = r.display.last.as_ref().unwrap();
    let start = UP_NEXT_ROWS_Y as usize * fb.width as usize;
    let end = (UP_NEXT_ROWS_Y + UP_NEXT_ROWS * plippa_ui::ui::tokens::ROW_H) as usize
        * fb.width as usize;
    let rows = &fb.pixels[start..end];
    let lit_px = rows
        .iter()
        .filter(|p| {
            ![BG, BG_ALT, SURFACE, SURFACE_ALT, BORDER_SOFT].contains(p)
        })
        .count();
    assert!(lit_px > 200, "Up Next overlay painted ({lit_px})");
    assert!(
        rows.iter().any(|p| *p == ACCENT_2),
        "the playing row carries its ACCENT_2 marker"
    );
    // UI_13: rotation opens the overlay and moves the cursor; Back returns to the waveform.
    r.knob(InputEvent::Back);
    assert_eq!(
        r.ui.model.np_focus.target,
        NpTarget::CurrentTime,
        "mode {:?}, visible {}",
        r.ui.model.np_focus.mode,
        r.ui.model.up_next_visible()
    );

    r.press(ControlRole::NextTrack);
    r.until("entry 1", |r| r.entry_cue().0 == 1);
    assert_eq!(r.row(), Some(1));
    assert_eq!(
        r.led.role_flashes.last(),
        Some(&(LedRole::NextTrackButton, plippa_ui_flash()))
    );
    // FIX_01C double press: with the playhead more than 1 s into entry 1 the first Previous
    // restarts it (NB_04b's 3 s window still pairs the presses) ...
    r.until("playhead past the 1 s restart threshold", |r| {
        r.sess().position_ms > 1_200
    });
    r.press(ControlRole::PrevTrack);
    r.until("restart settled", |r| r.sess().position_ms < 800);
    assert_eq!(
        r.entry_cue().0,
        1,
        "first Previous restarts an entry that is past 1 s"
    );
    // ... a second press within 3 s goes back.
    r.press(ControlRole::PrevTrack);
    r.until("entry 0", |r| r.entry_cue().0 == 0);
    assert_eq!(r.row(), Some(0));
    // Jump by HTTP (portal) to the last entry: Next goes dark, Previous stays lit.
    let (st, v) = api(
        port,
        "POST",
        "/api/transport/jump",
        Some(json!({"index": 2})),
    );
    assert_eq!(st, 200, "{v}");
    r.until("entry 2", |r| r.entry_cue().0 == 2);
    r.until("next unavailable", |r| r.published() == (true, false));
    assert_eq!(
        r.leds(),
        (true, false),
        "last entry, repeat off: Next dark, Previous lit"
    );
    let flashes = r.led.role_flashes.len();
    r.press(ControlRole::NextTrack);
    assert_eq!(r.led.role_flashes.len(), flashes, "unlit Next: no flash");
    assert_eq!(r.entry_cue().0, 2, "unlit Next: nothing happens");
    // Previous after the 3 s window is a fresh press again (the window is measured between
    // presses). Get the playhead past the 1 s restart threshold first so the outcome is
    // observable: a fresh press restarts entry 2, whereas a leaked pairing would step back.
    r.until("playhead past the 1 s threshold at entry 2", |r| {
        r.sess().position_ms > 1_200
    });
    r.press(ControlRole::PrevTrack); // restart entry 2; arms the pair timer
    r.until("restart settled at entry 2", |r| r.sess().position_ms < 800);
    thread::sleep(Duration::from_millis(3_200));
    r.press(ControlRole::PrevTrack); // >3 s later: fresh press, restart again
    r.pump();
    assert_eq!(
        r.entry_cue().0,
        2,
        "press after the window restarts instead of going back"
    );
    // Immediately pairing it up: second of two quick presses goes back.
    r.press(ControlRole::PrevTrack);
    r.until("entry 1 again", |r| r.entry_cue().0 == 1);
    // Let the engine finish the transition before the knob and the next load. UI_13's
    // one-turn overlay reaches the jump sooner than the old two-step list did.
    thread::sleep(Duration::from_millis(300));
    // UI_13 knob: one turn either way opens the overlay on the playing row and moves the
    // cursor; a push jumps to that row and closes the overlay.
    r.knob(InputEvent::MenuRotate(1));
    assert_eq!(r.ui.model.np_focus.target, NpTarget::List);
    assert_eq!(r.ui.model.np_focus.mode, NpMode::List);
    assert_eq!(
        r.ui.model.np_focus.cursor, 2,
        "the opening turn seeds on the playing row (1) then moves one row"
    );
    r.knob(InputEvent::MenuRotate(-1));
    r.knob(InputEvent::MenuRotate(-1));
    assert_eq!(r.ui.model.np_focus.cursor, 0);
    r.knob(InputEvent::MenuEnter);
    r.until("knob jump to entry 0", |r| r.entry_cue().0 == 0);
    assert_eq!(
        r.ui.model.np_focus.mode,
        NpMode::Select,
        "push jumps and returns to the waveform"
    );
    assert!(!r.ui.model.up_next_visible());
    // Let the engine finish opening the restarted stream before the next load: a load racing
    // the jump's stream open can be rolled back by the engine.
    thread::sleep(Duration::from_millis(300));

    // ===== Step 6: load the mixtape (edited timestamps are what the engine sees).
    r.play(&mix_id);
    r.until("mixtape list with 3 cue rows", |r| {
        r.list_titles() == ["Friday Night Mix", "Opening", "Second half", "Finale"]
    });
    assert!(r.ui.model.list_panel_visible());
    r.until("mixtape playing", |r| {
        r.sess().transport == TransportState::Playing
    });
    assert_eq!(r.entry_cue(), (0, Some(0)));
    assert_eq!(r.row(), Some(1), "highlight on the first cue row");
    assert_eq!(r.published(), (true, true));
    assert_eq!(r.leds(), (true, true));
    // Cue navigation seeks inside the one file: Next -> "Second half" at 0:15.
    r.press(ControlRole::NextTrack);
    r.until("cue 1", |r| r.entry_cue().1 == Some(1));
    assert_eq!(r.row(), Some(2));
    let pos = r.sess().position_ms;
    assert!(
        (15_000..17_000).contains(&pos),
        "seek to the edited cue time, got {pos}"
    );
    // FIX_01C: once cue 1 has played for >1 s the first Previous restarts it; a second press
    // within 3 s goes back to the previous cue. The wait makes the restart (rather than a
    // step back) the observable outcome.
    r.until("cue 1 past the 1 s restart threshold", |r| {
        r.sess().position_ms > 16_100
    });
    r.press(ControlRole::PrevTrack);
    r.until("cue restart", |r| {
        r.sess().position_ms < 15_800 && r.entry_cue().1 == Some(1)
    });
    r.press(ControlRole::PrevTrack);
    r.until("cue 0", |r| r.entry_cue().1 == Some(0));
    assert_eq!(r.row(), Some(1));
    // The highlight follows *media position*: seek 0.3 s before 0:15 and let playback cross it.
    r.conn
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(
            14_700,
        )))
        .unwrap();
    r.until("position near the boundary", |r| {
        (14_600..15_000).contains(&r.sess().position_ms)
    });
    assert_eq!(
        r.entry_cue().1,
        Some(0),
        "still in cue 0 before the boundary"
    );
    r.until("playback itself crosses into cue 1", |r| {
        r.entry_cue().1 == Some(1)
    });
    assert_eq!(
        r.row(),
        Some(2),
        "device highlight moved with no user input"
    );
    // Jump by HTTP to the last cue row (flat index 3): Next goes dark, Previous lit.
    let (st, v) = api(
        port,
        "POST",
        "/api/transport/jump",
        Some(json!({"index": 3})),
    );
    assert_eq!(st, 200, "{v}");
    r.until("last cue", |r| r.entry_cue().1 == Some(2));
    r.until("next unavailable on the last cue", |r| {
        r.published() == (true, false)
    });
    assert_eq!(r.leds(), (true, false));
    assert_eq!(r.row(), Some(3));
    let (_, np) = api(port, "GET", "/api/now-playing", None);
    assert_eq!(np["can_prev"], true);
    assert_eq!(
        np["can_next"], false,
        "portal now-playing reports the engine's flags: {np}"
    );
    // Seeking back before the last cue makes Next available again (flag is position based).
    r.conn
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(
            20_000,
        )))
        .unwrap();
    r.until("next available again", |r| r.published() == (true, true));
    assert_eq!(r.leds(), (true, true));

    // ===== Step 7: pause -> the engine's resume write-back lands in the .noudata without
    // breaking it (cues intact, still no parse warnings).
    r.conn
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::Pause))
        .unwrap();
    let t = Instant::now();
    loop {
        r.pump();
        let doc = doc_for(&r.root, &mix_rel);
        if doc["localPlaybackData"].get("resumeAtItem").is_some()
            || doc["mediaList"][0]["itemLocalPlaybackData"]
                .get("resumeFromTime")
                .is_some()
        {
            assert_eq!(
                doc["mediaList"][0]["cueLists"][0]["cues"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "resume write-back never reached the .noudata"
        );
    }

    // ===== Step 8: everything on disk is spec-valid, no parse warnings; nothing stray.
    let n = assert_library_spec_valid(&r.root);
    assert_eq!(n, 6, "mixtape + 3 track files + 1 loose track + 1 playlist");
    let (st, status) = api(port, "GET", "/api/status", None);
    assert_eq!(st, 200);
    assert_eq!(status["ipc_live"], true, "{status}");
}

/// `NAV_LED_FLASH` (white) as the loop runner passes it; kept local so the test does not need
/// the constant exported.
fn plippa_ui_flash() -> plippa_ui::Rgb {
    plippa_ui::Rgb::new(255, 255, 255)
}
