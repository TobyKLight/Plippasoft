//! Upstream D-Bus identifiers used by the 2.4 GHz accessory backend.
//!
//! Trademark exception: the radio standard and the vendor stack are not named in our crates,
//! modules, symbols, UI text or documents. The identifiers in [`bus_names`] are upstream names
//! we do not own and cannot rename — the service name, object paths, interface names, error
//! names and the pairing capability string. They are confined to this one module and are never
//! shown to the user. Nothing else in our tree may spell the standard's name.
//!
//! The only exception for the written word itself is this comment: the radio standard is
//! Bluetooth, and the vendor stack is BlueZ. Neither is a Plippa name.

/// Module-level explanation of the identifier exception, kept as a constant so the module can
/// point tooling and reviewers at it without putting the name anywhere else.
#[allow(dead_code)]
pub const MODULE_DOC: &str = "Upstream D-Bus identifiers for the 2.4 GHz accessory backend. \
These names belong to the system radio service, not to Plippa; keep every occurrence here.";

/// Every upstream identifier the backend needs.
pub mod bus_names {
    // Service and object tree.
    pub const SERVICE: &str = "org.bluez";
    /// ObjectManager root: `GetManagedObjects` is exported here.
    pub const ROOT_PATH: &str = "/";
    /// Manager interfaces (`AgentManager1`, `ProfileManager1`) live under the service tree,
    /// not at `/`.
    pub const OBJECT_TREE: &str = "/org/bluez";
    pub const AGENT_PATH: &str = "/com/plippa/agent2g4";
    /// Player object we export for the connected accessory's transport buttons.
    pub const PLAYER_PATH: &str = "/com/plippa/player2g4";

    // Interfaces.
    pub const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
    pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    pub const ADAPTER: &str = "org.bluez.Adapter1";
    pub const DEVICE: &str = "org.bluez.Device1";
    pub const BATTERY: &str = "org.bluez.Battery1";
    pub const AGENT_MANAGER: &str = "org.bluez.AgentManager1";
    pub const MEDIA1: &str = "org.bluez.Media1";
    /// The player object BlueZ calls is the standard MPRIS player interface, not a
    /// BlueZ-named one. Also written as a literal in the zbus attribute below; this
    /// constant keeps the upstream spelling for tests.
    #[allow(dead_code)]
    pub const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

    // Object-manager and property members.
    pub const GET_MANAGED_OBJECTS: &str = "GetManagedObjects";
    pub const INTERFACES_ADDED: &str = "InterfacesAdded";
    pub const INTERFACES_REMOVED: &str = "InterfacesRemoved";
    pub const PROPERTIES_CHANGED: &str = "PropertiesChanged";

    // Adapter/device/agent-manager methods.
    pub const START_DISCOVERY: &str = "StartDiscovery";
    pub const STOP_DISCOVERY: &str = "StopDiscovery";
    pub const REMOVE_DEVICE: &str = "RemoveDevice";
    pub const PAIR: &str = "Pair";
    pub const CONNECT: &str = "Connect";
    pub const DISCONNECT: &str = "Disconnect";
    pub const REGISTER_AGENT: &str = "RegisterAgent";
    pub const REQUEST_DEFAULT_AGENT: &str = "RequestDefaultAgent";
    pub const REGISTER_PLAYER: &str = "RegisterPlayer";
    pub const CAPABILITY: &str = "KeyboardDisplay";

    // Properties we read or write (the pairing window writes Pairable/PairableTimeout).
    pub const PAIRABLE: &str = "Pairable";
    pub const PAIRABLE_TIMEOUT: &str = "PairableTimeout";
    pub const ADDRESS: &str = "Address";
    pub const ALIAS: &str = "Alias";
    pub const NAME: &str = "Name";
    pub const PAIRED: &str = "Paired";
    pub const TRUSTED: &str = "Trusted";
    pub const CONNECTED: &str = "Connected";
    pub const POWERED: &str = "Powered";
    pub const UUIDS: &str = "UUIDs";
    pub const CLASS: &str = "Class";
    pub const PERCENTAGE: &str = "Percentage";

    // Local bus service used to check whether the radio service is present.
    pub const DBUS_SERVICE: &str = "org.freedesktop.DBus";
    pub const DBUS_PATH: &str = "/org/freedesktop/DBus";
    pub const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
    pub const NAME_HAS_OWNER: &str = "NameHasOwner";

    // Error names.
    pub const ERR_NOT_READY: &str = "org.bluez.Error.NotReady";
    pub const ERR_NOT_AUTHORIZED: &str = "org.bluez.Error.NotAuthorized";
    pub const ERR_DOES_NOT_EXIST: &str = "org.bluez.Error.DoesNotExist";
    pub const ERR_IN_PROGRESS: &str = "org.bluez.Error.InProgress";
    pub const ERR_ALREADY_EXISTS: &str = "org.bluez.Error.AlreadyExists";
    pub const ERR_ALREADY_CONNECTED: &str = "org.bluez.Error.AlreadyConnected";
    pub const ERR_NOT_CONNECTED: &str = "org.bluez.Error.NotConnected";
    pub const ERR_AUTH_FAILED: &str = "org.bluez.Error.AuthenticationFailed";
    pub const ERR_AUTH_CANCELED: &str = "org.bluez.Error.AuthenticationCanceled";
    pub const ERR_AUTH_REJECTED: &str = "org.bluez.Error.AuthenticationRejected";
    pub const ERR_REJECTED: &str = "org.bluez.Error.Rejected";
    pub const ERR_CANCELED: &str = "org.bluez.Error.Canceled";
    pub const ERR_DBUS_ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
}

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::agent::{Agent, PromptKind, RequestKind};
use super::player::MediaPlayer;
use crate::accessory::{PairPrompt, PromptAnswer};

/// The exported pairing-agent object. The interface name and error prefix are two of the
/// upstream identifiers this module exists to confine; zbus attribute grammar needs them as
/// literals, so the interface implementation lives here rather than in `agent.rs`.
#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {}

    async fn cancel(&self) {
        self.cancel_pending();
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let Some(peer) = self.gate(&device, RequestKind::Confirm, None)? else {
            return Ok(());
        };
        match self
            .ask(PromptKind::Confirm, PairPrompt::Confirm { peer, passkey })
            .await
        {
            Some(PromptAnswer::Accept) => Ok(()),
            Some(_) => Err(AgentError::Rejected),
            None => Err(AgentError::Canceled),
        }
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        let Some(peer) = self.gate(&device, RequestKind::Passkey, None)? else {
            return Err(AgentError::Rejected);
        };
        match self
            .ask(PromptKind::EnterPasskey, PairPrompt::EnterPasskey { peer })
            .await
        {
            Some(PromptAnswer::Passkey(passkey)) => Ok(passkey),
            Some(_) => Err(AgentError::Rejected),
            None => Err(AgentError::Canceled),
        }
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        let Some(peer) = self.gate(&device, RequestKind::Pin, None)? else {
            return Err(AgentError::Rejected);
        };
        match self
            .ask(PromptKind::EnterPin, PairPrompt::EnterPin { peer })
            .await
        {
            Some(PromptAnswer::Pin(pin)) => Ok(pin),
            Some(_) => Err(AgentError::Rejected),
            None => Err(AgentError::Canceled),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        let Some(peer) = self.gate(&device, RequestKind::Authorize, None)? else {
            return Ok(());
        };
        match self
            .ask(PromptKind::Authorise, PairPrompt::Authorise { peer })
            .await
        {
            Some(PromptAnswer::Accept) => Ok(()),
            Some(_) => Err(AgentError::Rejected),
            None => Err(AgentError::Canceled),
        }
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        service_uuid: String,
    ) -> Result<(), AgentError> {
        let Some(peer) = self.gate(&device, RequestKind::AuthorizeService, Some(&service_uuid))?
        else {
            return Ok(());
        };
        match self
            .ask(PromptKind::Authorise, PairPrompt::Authorise { peer })
            .await
        {
            Some(PromptAnswer::Accept) => Ok(()),
            Some(_) => Err(AgentError::Rejected),
            None => Err(AgentError::Canceled),
        }
    }
}

/// The exported media-player object. The service calls these methods when the paired
/// accessory's transport buttons are pressed. The interface name is one of the upstream
/// identifiers this module confines, so the implementation lives here with the agent's.
#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MediaPlayer {
    #[zbus(name = "Play")]
    async fn dbus_play(&self) {
        self.play();
    }

    #[zbus(name = "Pause")]
    async fn dbus_pause(&self) {
        self.pause();
    }

    #[zbus(name = "Stop")]
    async fn dbus_stop(&self) {
        self.stop();
    }

    #[zbus(name = "Next")]
    async fn dbus_next(&self) {
        self.next();
    }

    #[zbus(name = "Previous")]
    async fn dbus_previous(&self) {
        self.previous();
    }

    #[zbus(name = "Equalizer")]
    async fn dbus_equalizer(&self, equalizer: String) {
        self.equalizer(&equalizer);
    }

    #[zbus(name = "Repeat")]
    async fn dbus_repeat(&self, repeat: String) {
        self.repeat(&repeat);
    }

    #[zbus(name = "Shuffle")]
    async fn dbus_shuffle(&self, shuffle: String) {
        self.shuffle(&shuffle);
    }

    #[zbus(name = "Select")]
    async fn dbus_select(&self, track: HashMap<String, OwnedValue>) {
        self.select(&track);
    }

    #[zbus(name = "SetPosition")]
    async fn dbus_set_position(&self, track: HashMap<String, OwnedValue>, position: u32) {
        self.set_position(&track, position);
    }
}

/// Errors the pairing agent returns to the service.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub(crate) enum AgentError {
    Rejected,
    Canceled,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zbus::object_server::Interface;

    use super::super::Shared;
    use super::*;

    fn introspection() -> String {
        let agent = Agent::new(Arc::new(Shared::default()));
        let mut xml = String::new();
        Interface::introspect_to_writer(&agent, &mut xml, 0);
        xml
    }

    #[test]
    fn request_passkey_declares_a_uint32_output() {
        let xml = introspection();
        let method = xml
            .split("<method name=\"RequestPasskey\"")
            .nth(1)
            .expect("RequestPasskey must be exported");
        let body = method.split("</method>").next().expect("method body");
        assert!(
            body.contains("type=\"u\""),
            "RequestPasskey must return uint32: {body}"
        );
        assert!(
            !body.contains("type=\"i\""),
            "RequestPasskey must not return int32: {body}"
        );
    }

    #[test]
    fn cancel_is_exported_by_the_agent() {
        let xml = introspection();
        assert!(
            xml.contains("<method name=\"Cancel\""),
            "the agent must answer the service's cancel call"
        );
    }

    #[test]
    fn manager_interfaces_live_under_the_service_tree_not_the_bus_root() {
        // The service exports `GetManagedObjects` at `/` but `AgentManager1` at `/org/bluez`.
        // Asking for RegisterAgent on `/` returns UnknownMethod and leaves pairing unavailable.
        assert_eq!(bus_names::ROOT_PATH, "/");
        assert_eq!(bus_names::OBJECT_TREE, "/org/bluez");
    }
}
