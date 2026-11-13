//! Tiny client used by `scripts/mvp09_ipc_smoke.sh` to prove a live UDS round-trip.

use plippa_core::Volume;
use plippa_ipc_proto::{
    resolve_paths, IpcConnection, IpcMessage, PlaybackCommand, TapKeyPayload,
};
use std::env;
use std::time::Duration;

fn main() {
    let paths = resolve_paths();
    let mut client = IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(5))
        .unwrap_or_else(|e| {
            eprintln!("smoke-client: connect failed: {e}");
            std::process::exit(1);
        });

    client
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
            Volume::new(33),
        )))
        .expect("send volume");

    // Hub may push an initial status on connect; wait specifically for volume=33.
    let mut saw = false;
    for _ in 0..20 {
        match client.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(IpcMessage::PlaybackStatus(s))) if s.session.volume.percent() == 33 => {
                println!("smoke-client: got PlaybackStatus volume=33");
                saw = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(e) => {
                eprintln!("smoke-client: recv error: {e}");
                std::process::exit(1);
            }
        }
    }
    if !saw {
        eprintln!("smoke-client: expected PlaybackStatus volume=33");
        std::process::exit(1);
    }

    if let Ok(key) = env::var("PLIPPA_SMOKE_TAP_KEY") {
        client
            .send(&IpcMessage::TapKey(TapKeyPayload {
                noubin_key: key,
                tap_id: 1,
                already_normalised: false,
            }))
            .expect("send tap");
        let mut ok = false;
        for _ in 0..20 {
            if let Ok(Some(IpcMessage::PlaybackStatus(s))) =
                client.recv_timeout(Duration::from_millis(250))
            {
                println!("smoke-client: transport={:?}", s.transport);
                ok = true;
                break;
            }
        }
        if !ok {
            eprintln!("smoke-client: no status after TapKey (library may be empty)");
        }
    }

    println!("smoke-client: OK");
}
