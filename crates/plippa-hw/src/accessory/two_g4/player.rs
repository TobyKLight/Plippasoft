//! The media-player object exported for a connected accessory's transport buttons.
//!
//! The system radio service calls these methods on the object registered at
//! [`super::names::bus_names::PLAYER_PATH`]; each one becomes an [`AccessoryEvent::Transport`]
//! for the UI worker. The interface implementation itself lives in `names.rs` because its
//! name is one of the upstream identifiers we confine there.

use std::collections::HashMap;
use std::sync::Arc;

use zbus::zvariant::OwnedValue;

use super::Shared;
use crate::accessory::{AccessoryEvent, TransportAction};

/// The exported player object: one per radio connection.
pub(crate) struct MediaPlayer {
    shared: Arc<Shared>,
}

impl MediaPlayer {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    fn emit(&self, action: TransportAction) {
        self.shared.push(AccessoryEvent::Transport(action));
    }

    pub(crate) fn play(&self) {
        self.emit(TransportAction::Play);
    }

    pub(crate) fn pause(&self) {
        self.emit(TransportAction::Pause);
    }

    pub(crate) fn stop(&self) {
        self.emit(TransportAction::Stop);
    }

    pub(crate) fn next(&self) {
        self.emit(TransportAction::Next);
    }

    pub(crate) fn previous(&self) {
        self.emit(TransportAction::Previous);
    }

    pub(crate) fn equalizer(&self, _mode: &str) {}

    pub(crate) fn repeat(&self, _mode: &str) {}

    pub(crate) fn shuffle(&self, _mode: &str) {}

    pub(crate) fn select(&self, _track: &HashMap<String, OwnedValue>) {}

    pub(crate) fn set_position(&self, _track: &HashMap<String, OwnedValue>, _position: u32) {}
}

#[cfg(test)]
mod tests {
    use super::super::lock;
    use super::*;
    use zbus::object_server::Interface;

    fn player() -> (Arc<Shared>, MediaPlayer) {
        let shared = Arc::new(Shared::default());
        let player = MediaPlayer::new(shared.clone());
        (shared, player)
    }

    fn events(shared: &Shared) -> Vec<AccessoryEvent> {
        lock(&shared.events).drain(..).collect()
    }

    #[test]
    fn each_transport_method_queues_its_own_event() {
        let (shared, player) = player();
        player.play();
        player.pause();
        player.stop();
        player.next();
        player.previous();
        assert_eq!(
            events(&shared),
            vec![
                AccessoryEvent::Transport(TransportAction::Play),
                AccessoryEvent::Transport(TransportAction::Pause),
                AccessoryEvent::Transport(TransportAction::Stop),
                AccessoryEvent::Transport(TransportAction::Next),
                AccessoryEvent::Transport(TransportAction::Previous),
            ]
        );
    }

    #[test]
    fn the_completeness_methods_are_no_ops() {
        let (shared, player) = player();
        let track = HashMap::new();
        player.equalizer("Off");
        player.repeat("Off");
        player.shuffle("Off");
        player.select(&track);
        player.set_position(&track, 0);
        assert!(events(&shared).is_empty());
    }

    #[test]
    fn introspection_exports_the_five_transport_methods() {
        let (_shared, player) = player();
        assert_eq!(
            <MediaPlayer as Interface>::name().as_str(),
            "org.mpris.MediaPlayer2.Player",
            "BlueZ 5.82 calls the MPRIS player interface"
        );
        let mut xml = String::new();
        Interface::introspect_to_writer(&player, &mut xml, 0);
        for name in ["Play", "Pause", "Stop", "Next", "Previous"] {
            assert!(
                xml.contains(&format!("<method name=\"{name}\"")),
                "the MPRIS player must export {name}: {xml}"
            );
        }
    }
}
