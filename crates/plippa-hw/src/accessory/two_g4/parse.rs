//! Pure parsing of the radio service's object-manager reply.
//!
//! Everything here is side-effect free and testable without a bus: the backend feeds it the
//! `GetManagedObjects` map and the shapes of `InterfacesAdded`/`PropertiesChanged`.

use std::collections::{BTreeMap, HashMap};

use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue};

use super::names::bus_names;
use crate::accessory::{normalise_address, Accessory, AccessoryKind, AdapterInfo};

/// The object-manager reply shape: object path -> interface -> property -> value.
///
/// The key is `ObjectPath<'static>` rather than `OwnedObjectPath` because only the former
/// implements `Ord` in zvariant 5; the two carry the same static path and convert freely.
pub type ManagedObjects = BTreeMap<ObjectPath<'static>, HashMap<String, HashMap<String, OwnedValue>>>;

/// Short UUIDs that mean the accessory can carry audio.
const AUDIO_UUIDS: [u32; 5] = [
    0x110B, // Audio Sink
    0x110D, // Audio Source (we still treat it as audio-capable)
    0x1112, // Headset
    0x1108, // Headset (older alias used by some headsets)
    0x110E, // Remote Control Target (media buttons on an audio accessory)
];

/// HID service: a human-interface accessory (keyboard, remote, presenter).
const HID_UUID: u32 = 0x1124;

/// Major device class `Audio/Video` (bits 8..=12 of the Class property). A speaker or
/// headset reports this before its audio service UUIDs are resolved.
const CLASS_MAJOR_AUDIO: u32 = 0x04;

/// Major device class `Peripheral` (bits 8..=12 of the Class property).
const CLASS_MAJOR_PERIPHERAL: u32 = 0x05;

/// Every accessory reported by the service, sorted by normalised address.
///
/// The backend keeps paths alongside each accessory through [`devices_from`]; this view is the
/// public/test parsing surface and therefore intentionally not called from the radio loop.
#[allow(dead_code)]
pub fn accessories_from(
    objects: &ManagedObjects,
) -> Vec<Accessory> {
    devices_from(objects)
        .into_iter()
        .map(|(_, accessory)| accessory)
        .collect()
}

/// Every device object, paired with the object path needed to call it later.
pub fn devices_from(
    objects: &ManagedObjects,
) -> Vec<(OwnedObjectPath, Accessory)> {
    let mut devices = Vec::new();
    for (path, interfaces) in objects {
        let Some(props) = interfaces.get(bus_names::DEVICE) else {
            continue;
        };
        let Some(address) = string_prop(props, bus_names::ADDRESS) else {
            continue;
        };
        let id = normalise_address(&address);
        if id.is_empty() {
            continue;
        }
        let name = string_prop(props, bus_names::ALIAS)
            .or_else(|| string_prop(props, bus_names::NAME))
            .unwrap_or_default();
        let uuids = string_array_prop(props, bus_names::UUIDS);
        let class = u32_prop(props, bus_names::CLASS).unwrap_or(0);
        let battery_pct = interfaces
            .get(bus_names::BATTERY)
            .and_then(|battery| u8_prop(battery, bus_names::PERCENTAGE));
        devices.push((
            path.clone().into(),
            Accessory {
                id,
                name,
                kind: kind_from(&uuids, class),
                paired: bool_prop(props, bus_names::PAIRED).unwrap_or(false),
                trusted: bool_prop(props, bus_names::TRUSTED).unwrap_or(false),
                connected: bool_prop(props, bus_names::CONNECTED).unwrap_or(false),
                battery_pct,
            },
        ));
    }
    devices.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    devices
}

/// Every adapter object reported by the service, sorted by adapter id.
pub fn adapters_from(
    objects: &ManagedObjects,
) -> Vec<(OwnedObjectPath, AdapterInfo)> {
    let mut adapters = Vec::new();
    for (path, interfaces) in objects {
        let Some(props) = interfaces.get(bus_names::ADAPTER) else {
            continue;
        };
        let id = last_path_segment(path.as_str()).to_string();
        if id.is_empty() {
            continue;
        }
        let address = string_prop(props, bus_names::ADDRESS)
            .map(|address| normalise_address(&address))
            .unwrap_or_default();
        let name = string_prop(props, bus_names::ALIAS)
            .or_else(|| string_prop(props, bus_names::NAME))
            .unwrap_or_else(|| id.clone());
        adapters.push((
            path.clone().into(),
            AdapterInfo {
                id,
                address,
                name,
                powered: bool_prop(props, bus_names::POWERED).unwrap_or(false),
            },
        ));
    }
    adapters.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    adapters
}

/// Classify an accessory from its advertised services, falling back to the device class.
pub fn kind_from(uuids: &[String], class: u32) -> AccessoryKind {
    let shorts: Vec<u32> = uuids.iter().filter_map(|uuid| short_uuid(uuid)).collect();
    if shorts.iter().any(|uuid| AUDIO_UUIDS.contains(uuid)) {
        return AccessoryKind::Audio;
    }
    let major = (class >> 8) & 0x1F;
    if major == CLASS_MAJOR_AUDIO {
        return AccessoryKind::Audio;
    }
    if shorts.contains(&HID_UUID) {
        return AccessoryKind::Input;
    }
    if major == CLASS_MAJOR_PERIPHERAL {
        return AccessoryKind::Input;
    }
    AccessoryKind::Other
}

/// Pull the 16-bit form out of a UUID string. Accepts the canonical 128-bit spelling, an
/// 8-digit short form and a 4-digit short form; anything else is ignored.
fn short_uuid(uuid: &str) -> Option<u32> {
    let trimmed = uuid.trim();
    let without_prefix = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    match without_prefix.len() {
        4 => u32::from_str_radix(without_prefix, 16).ok(),
        8 => u32::from_str_radix(without_prefix, 16)
            .ok()
            .map(|value| value & 0xFFFF),
        36 => without_prefix
            .get(..8)
            .and_then(|prefix| u32::from_str_radix(prefix, 16).ok())
            .map(|value| value & 0xFFFF),
        _ => None,
    }
}

fn last_path_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn property<'a>(
    props: &'a HashMap<String, OwnedValue>,
    name: &str,
) -> Option<&'a OwnedValue> {
    props.get(name)
}

fn string_prop(props: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    string_value(property(props, name)?)
}

/// Read a D-Bus string as Rust text. Values are kept verbatim; the UI sanitises display.
pub(crate) fn string_value(value: &OwnedValue) -> Option<String> {
    value.downcast_ref::<&str>().ok().map(str::to_owned)
}

/// Read a D-Bus byte (or a wider integer some stacks send) as a battery percentage.
pub(crate) fn percentage_value(value: &OwnedValue) -> Option<u8> {
    if let Ok(byte) = u8::try_from(value) {
        return Some(byte);
    }
    if let Ok(word) = u16::try_from(value) {
        return Some(word.min(u16::from(u8::MAX)) as u8);
    }
    u64::try_from(value)
        .ok()
        .map(|wide| wide.min(u64::from(u8::MAX)) as u8)
}

fn bool_prop(props: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    bool::try_from(property(props, name)?).ok()
}

fn u32_prop(props: &HashMap<String, OwnedValue>, name: &str) -> Option<u32> {
    u32::try_from(property(props, name)?).ok()
}

fn u8_prop(props: &HashMap<String, OwnedValue>, name: &str) -> Option<u8> {
    if let Some(value) = property(props, name) {
        if let Ok(byte) = u8::try_from(value) {
            return Some(byte);
        }
        if let Ok(word) = u16::try_from(value) {
            return Some(word.min(u16::from(u8::MAX)) as u8);
        }
        if let Ok(value) = u64::try_from(value) {
            return Some(value.min(u64::from(u8::MAX)) as u8);
        }
    }
    None
}

fn string_array_prop(props: &HashMap<String, OwnedValue>, name: &str) -> Vec<String> {
    let Some(value) = property(props, name) else {
        return Vec::new();
    };
    let Ok(array) = value.downcast_ref::<Array>() else {
        return Vec::new();
    };
    array
        .inner()
        .iter()
        .filter_map(|item| item.downcast_ref::<&str>().ok().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn text(value: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(value)).expect("string value")
    }

    fn number(value: u32) -> OwnedValue {
        OwnedValue::from(value)
    }

    fn byte(value: u8) -> OwnedValue {
        OwnedValue::from(value)
    }

    fn flag(value: bool) -> OwnedValue {
        OwnedValue::from(value)
    }

    fn texts(values: &[&str]) -> OwnedValue {
        let values: Vec<String> = values.iter().map(|value| (*value).to_string()).collect();
        OwnedValue::try_from(Value::from(values)).expect("string array")
    }

    fn props(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn device(
        address: &str,
        alias: Option<&str>,
        name: Option<&str>,
        uuids: Option<&[&str]>,
        class: u32,
        paired: bool,
        trusted: bool,
        connected: bool,
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut device = props(&[
            (bus_names::ADDRESS, text(address)),
            (bus_names::CLASS, number(class)),
            (bus_names::PAIRED, flag(paired)),
            (bus_names::TRUSTED, flag(trusted)),
            (bus_names::CONNECTED, flag(connected)),
        ]);
        if let Some(alias) = alias {
            device.insert(bus_names::ALIAS.to_string(), text(alias));
        }
        if let Some(name) = name {
            device.insert(bus_names::NAME.to_string(), text(name));
        }
        if let Some(uuids) = uuids {
            device.insert(bus_names::UUIDS.to_string(), texts(uuids));
        }
        HashMap::from([(bus_names::DEVICE.to_string(), device)])
    }

    fn adapter(address: &str, alias: Option<&str>, name: Option<&str>, powered: bool) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut adapter = props(&[
            (bus_names::ADDRESS, text(address)),
            (bus_names::POWERED, flag(powered)),
        ]);
        if let Some(alias) = alias {
            adapter.insert(bus_names::ALIAS.to_string(), text(alias));
        }
        if let Some(name) = name {
            adapter.insert(bus_names::NAME.to_string(), text(name));
        }
        HashMap::from([(bus_names::ADAPTER.to_string(), adapter)])
    }

    fn path(value: &str) -> ObjectPath<'static> {
        ObjectPath::try_from(value.to_string()).expect("object path")
    }

    fn fixture() -> ManagedObjects {
        let speaker_path = path(&format!(
            "{}/hci0/dev_AA_BB_CC_DD_EE_01",
            bus_names::OBJECT_TREE
        ));
        let mut speaker = device(
            "AA:BB:CC:DD:EE:01",
            Some("Kitchen speaker"),
            Some("Speaker 1000"),
            Some(&["0000110b-0000-1000-8000-00805f9b34fb"]),
            0x240404,
            true,
            true,
            true,
        );
        speaker.insert(
            bus_names::BATTERY.to_string(),
            props(&[(bus_names::PERCENTAGE, byte(73))]),
        );

        let keyboard_path = path(&format!(
            "{}/hci0/dev_AA_BB_CC_DD_EE_02",
            bus_names::OBJECT_TREE
        ));
        let keyboard = device(
            "AA:BB:CC:DD:EE:02",
            None,
            Some("Keeb"),
            Some(&["00001124-0000-1000-8000-00805f9b34fb"]),
            0,
            false,
            false,
            false,
        );

        let other_path = path(&format!(
            "{}/hci0/dev_AA_BB_CC_DD_EE_03",
            bus_names::OBJECT_TREE
        ));
        let other = device(
            "AA:BB:CC:DD:EE:03",
            None,
            Some("Mystery"),
            Some(&["0000180f-0000-1000-8000-00805f9b34fb"]),
            0,
            false,
            false,
            false,
        );

        BTreeMap::from([
            (path(&format!("{}/hci0", bus_names::OBJECT_TREE)), adapter("11:22:33:44:55:66", None, Some("Adapter zero"), true)),
            (path(&format!("{}/hci1", bus_names::OBJECT_TREE)), adapter("11:22:33:44:55:77", Some("Second"), None, false)),
            (speaker_path, speaker),
            (keyboard_path, keyboard),
            (other_path, other),
        ])
    }

    #[test]
    fn parses_adapters_devices_kinds_flags_battery_and_sort_order() {
        let objects = fixture();
        let adapters = adapters_from(&objects);
        assert_eq!(adapters.len(), 2);
        assert_eq!(adapters[0].1.id, "hci0");
        assert_eq!(adapters[0].1.address, "11:22:33:44:55:66");
        assert_eq!(adapters[0].1.name, "Adapter zero");
        assert!(adapters[0].1.powered);
        assert_eq!(adapters[1].1.id, "hci1");
        assert_eq!(adapters[1].1.name, "Second");
        assert!(!adapters[1].1.powered);

        let accessories = accessories_from(&objects);
        assert_eq!(accessories.len(), 3);
        let ids: Vec<&str> = accessories.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02", "aa:bb:cc:dd:ee:03"]
        );

        let speaker = &accessories[0];
        assert_eq!(speaker.name, "Kitchen speaker");
        assert_eq!(speaker.kind, AccessoryKind::Audio);
        assert!(speaker.paired && speaker.trusted && speaker.connected);
        assert_eq!(speaker.battery_pct, Some(73));

        let keyboard = &accessories[1];
        assert_eq!(keyboard.name, "Keeb", "Alias falls back to Name");
        assert_eq!(keyboard.kind, AccessoryKind::Input);
        assert!(!keyboard.paired && !keyboard.trusted && !keyboard.connected);
        assert_eq!(keyboard.battery_pct, None);

        assert_eq!(accessories[2].name, "Mystery");
        assert_eq!(accessories[2].kind, AccessoryKind::Other);
        assert_eq!(accessories[2].battery_pct, None);
    }

    #[test]
    fn kind_from_audio_hid_class_and_empty() {
        assert_eq!(
            kind_from(&["0000110b-0000-1000-8000-00805f9b34fb".to_string()], 0),
            AccessoryKind::Audio
        );
        assert_eq!(
            kind_from(&["0x110D".to_string()], 0),
            AccessoryKind::Audio
        );
        assert_eq!(
            kind_from(&["00001124-0000-1000-8000-00805f9b34fb".to_string()], 0),
            AccessoryKind::Input
        );
        assert_eq!(kind_from(&[], 0x0500), AccessoryKind::Input);
        assert_eq!(kind_from(&[], 0), AccessoryKind::Other);
        // A speaker reports Audio/Video in its class before its service UUIDs resolve.
        assert_eq!(kind_from(&[], 0x240404), AccessoryKind::Audio);
        assert_eq!(kind_from(&["not-a-uuid".to_string()], 0), AccessoryKind::Other);
    }

    #[test]
    fn a_36_byte_uuid_with_multibyte_text_does_not_panic() {
        // 12 copies of a three-byte character: 36 bytes, with byte offset 8 inside a
        // character, so the old `[..8]` slice would have panicked.
        let text = "€".repeat(12);
        assert_eq!(text.len(), 36);
        assert_eq!(kind_from(&[text], 0), AccessoryKind::Other);
    }
}
