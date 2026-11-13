//! Forward-compatible `.noudata` parse / playable / write-through.

use plippa_noubin::{
    is_minimally_playable, normalise_noubin_key, parse_noudata, parse_noudata_str,
};

#[test]
fn minimal_without_key_is_playable() {
    let bytes = include_bytes!("../fixtures/minimal.noudata");
    let doc = parse_noudata(bytes).unwrap();
    assert!(doc.noubin_key.is_none());
    assert!(doc.noubin_key_normalised.is_none());
    assert_eq!(doc.playable_item_title.as_deref(), Some("Minimal Track"));
    assert!(is_minimally_playable(&doc));
}

#[test]
fn messy_unknown_fields_parse_and_stay_playable() {
    let bytes = include_bytes!("../fixtures/messy_unknown.noudata");
    let doc = parse_noudata(bytes).unwrap();
    assert!(is_minimally_playable(&doc));
    assert!(doc.extra.contains_key("futureRootField"));
    assert!(doc.media_list[0].extra.contains_key("unknownItemField"));
    // Unknown enum-like strings live under opaque Value — no panic.
    assert!(doc.release_data.is_some());
}

#[test]
fn empty_media_list_not_playable() {
    let doc = parse_noudata_str(
        r#"{
            "playableItemTitle": "Nope",
            "mediaList": []
        }"#,
    )
    .unwrap();
    assert!(!is_minimally_playable(&doc));
}

#[test]
fn media_item_without_paths_not_playable() {
    let doc = parse_noudata_str(
        r#"{
            "playableItemTitle": "Nope",
            "mediaList": [{"mediaTitle": "x"}]
        }"#,
    )
    .unwrap();
    assert!(!is_minimally_playable(&doc));
}

#[test]
fn set_noubin_keys_write_through_preserves_extras() {
    let bytes = include_bytes!("../fixtures/messy_unknown.noudata");
    let mut doc = parse_noudata(bytes).unwrap();
    let key = "Smooth Jazz.noubin.XYZ123";
    let norm = normalise_noubin_key(key);
    doc.set_noubin_keys(key, &norm);
    let json = doc.to_json_pretty().unwrap();
    let again = parse_noudata_str(&json).unwrap();
    assert_eq!(again.noubin_key.as_deref(), Some(key));
    assert_eq!(again.noubin_key_normalised.as_deref(), Some(norm.as_str()));
    assert!(again.extra.contains_key("futureRootField"));
    assert!(is_minimally_playable(&again));
}
