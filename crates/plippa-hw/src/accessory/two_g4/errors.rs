//! Translation of radio-service errors into [`AccessoryError`] and pairing outcomes.
//!
//! The D-Bus error *name* decides the mapping. The service message is kept where the backend
//! needs it (`Refused`, `AdapterNotReady`) but is never written into a UI sentence; the UI
//! matches on the enum variants and writes its own wording.

use super::names::bus_names;
use super::AccessoryError;

/// What a pairing call ended up doing. Pairing failures are an outcome, not an API
/// error: the UI wants a `Paired { ok: false }` event, not a thrown request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairOutcome {
    Success,
    Failed(Option<&'static str>),
}

/// Map a fully-qualified D-Bus error name (with its service message) to the backend error.
///
/// `id` is only used for `DoesNotExist`, which is an identity error rather than a service
/// failure.
pub(crate) fn map_named(name: &str, message: &str, id: &str) -> AccessoryError {
    match name {
        bus_names::ERR_NOT_READY => {
            if message.to_ascii_lowercase().contains("block") {
                AccessoryError::Blocked
            } else {
                AccessoryError::AdapterNotReady(message.to_string())
            }
        }
        bus_names::ERR_NOT_AUTHORIZED | bus_names::ERR_DBUS_ACCESS_DENIED => {
            AccessoryError::Refused(message.to_string())
        }
        bus_names::ERR_DOES_NOT_EXIST => AccessoryError::Unknown(id.to_string()),
        bus_names::ERR_IN_PROGRESS => AccessoryError::Refused("busy".to_string()),
        _ => AccessoryError::Refused("the radio service rejected the request".to_string()),
    }
}

/// Map a pairing-call failure. `None` means the error is not a pairing outcome and should go
/// through [`map_named`] as an ordinary API error.
pub(crate) fn pair_outcome(name: &str) -> Option<PairOutcome> {
    match name {
        bus_names::ERR_ALREADY_EXISTS => Some(PairOutcome::Success),
        bus_names::ERR_AUTH_FAILED => Some(PairOutcome::Failed(None)),
        bus_names::ERR_AUTH_CANCELED
        | bus_names::ERR_AUTH_REJECTED
        | bus_names::ERR_CANCELED => Some(PairOutcome::Failed(Some("cancelled"))),
        bus_names::ERR_REJECTED => Some(PairOutcome::Failed(Some("rejected"))),
        _ => None,
    }
}

/// Convert a `zbus` error to the backend error, unwrapping the FDO wrapper.
pub(crate) fn map_error(error: &zbus::Error, id: &str) -> AccessoryError {
    if let Some((name, message)) = method_error(error) {
        return map_named(name, message, id);
    }
    match error {
        zbus::Error::InputOutput(_) | zbus::Error::Connection(_, _) => AccessoryError::Unavailable,
        _ => AccessoryError::Unavailable,
    }
}

/// Extract the upstream error name and message from a method-error reply, if this is one.
pub(crate) fn method_error(error: &zbus::Error) -> Option<(&str, &str)> {
    match error {
        zbus::Error::MethodError(name, message, _) => {
            Some((name.as_str(), message.as_deref().unwrap_or_default()))
        }
        zbus::Error::FDO(inner) => match inner.as_ref() {
            zbus::fdo::Error::ZBus(inner) => method_error(inner),
            _ => None,
        },
        _ => None,
    }
}

/// A stable, user-safe sentence for an event. The enum is the contract; the message must not
/// leak a raw D-Bus string.
pub(crate) fn event_text(error: &AccessoryError) -> &'static str {
    match error {
        AccessoryError::NoAdapter => "no radio adapter",
        AccessoryError::AdapterNotReady(_) => "radio adapter is not ready",
        AccessoryError::Blocked => "radio is blocked",
        AccessoryError::Unknown(_) => "accessory is no longer known",
        AccessoryError::NoPairing => "no pairing is in progress",
        AccessoryError::Refused(_) => "the radio service refused the request",
        AccessoryError::Unavailable => "accessory service unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> &'static str {
        "aa:bb:cc:dd:ee:ff"
    }

    #[test]
    fn not_ready_maps_to_adapter_not_ready_or_blocked() {
        match map_named(bus_names::ERR_NOT_READY, "not ready yet", id()) {
            AccessoryError::AdapterNotReady(message) => assert_eq!(message, "not ready yet"),
            other => panic!("expected AdapterNotReady, got {other:?}"),
        }
        assert!(matches!(
            map_named(bus_names::ERR_NOT_READY, "Blocked through rfkill", id()),
            AccessoryError::Blocked
        ));
    }

    #[test]
    fn authorization_errors_keep_the_service_message_inside_refused() {
        for name in [bus_names::ERR_NOT_AUTHORIZED, bus_names::ERR_DBUS_ACCESS_DENIED] {
            match map_named(name, "policy says no", id()) {
                AccessoryError::Refused(message) => assert_eq!(message, "policy says no"),
                other => panic!("expected Refused for {name}, got {other:?}"),
            }
        }
    }

    #[test]
    fn does_not_exist_is_an_identity_error() {
        match map_named(bus_names::ERR_DOES_NOT_EXIST, "gone", id()) {
            AccessoryError::Unknown(unknown) => assert_eq!(unknown, id()),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn in_progress_is_busy_and_unknown_names_are_generic() {
        match map_named(bus_names::ERR_IN_PROGRESS, "raw detail", id()) {
            AccessoryError::Refused(message) => assert_eq!(message, "busy"),
            other => panic!("expected Refused, got {other:?}"),
        }
        match map_named("org.example.Weird", "raw detail", id()) {
            AccessoryError::Refused(message) => {
                assert!(!message.contains("raw detail"), "raw service text must not leak");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn pairing_outcomes_cover_every_authentication_arm() {
        assert_eq!(
            pair_outcome(bus_names::ERR_ALREADY_EXISTS),
            Some(PairOutcome::Success)
        );
        assert_eq!(
            pair_outcome(bus_names::ERR_AUTH_FAILED),
            Some(PairOutcome::Failed(None))
        );
        assert_eq!(
            pair_outcome(bus_names::ERR_AUTH_CANCELED),
            Some(PairOutcome::Failed(Some("cancelled")))
        );
        assert_eq!(
            pair_outcome(bus_names::ERR_AUTH_REJECTED),
            Some(PairOutcome::Failed(Some("cancelled")))
        );
        assert_eq!(
            pair_outcome(bus_names::ERR_CANCELED),
            Some(PairOutcome::Failed(Some("cancelled")))
        );
        assert_eq!(
            pair_outcome(bus_names::ERR_REJECTED),
            Some(PairOutcome::Failed(Some("rejected")))
        );
        assert_eq!(pair_outcome(bus_names::ERR_NOT_READY), None);
    }

    #[test]
    fn event_text_never_contains_a_raw_service_message() {
        let refused = AccessoryError::Refused("org.example: secret detail".to_string());
        assert_eq!(
            event_text(&refused),
            "the radio service refused the request"
        );
        assert!(!event_text(&refused).contains("secret"));
    }

    #[test]
    fn map_error_unwraps_method_errors_and_falls_back_to_unavailable() {
        let name = zbus::names::OwnedErrorName::try_from(bus_names::ERR_NOT_READY)
            .expect("valid error name");
        let device_path = format!("{}/hci0", bus_names::OBJECT_TREE);
        let reply = zbus::Message::method_call(device_path.as_str(), bus_names::PAIR)
            .expect("message builder")
            .build(&())
            .expect("message");
        let error = zbus::Error::MethodError(
            name,
            Some("Blocked through rfkill".to_string()),
            reply,
        );

        assert_eq!(
            method_error(&error),
            Some((bus_names::ERR_NOT_READY, "Blocked through rfkill"))
        );
        assert!(matches!(map_error(&error, id()), AccessoryError::Blocked));

        let io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "gone",
        )));
        assert_eq!(method_error(&io), None);
        assert!(matches!(
            map_error(&io, id()),
            AccessoryError::Unavailable
        ));
    }
}
