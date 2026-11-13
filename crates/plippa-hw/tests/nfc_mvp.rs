//! MVP_06 NFC acceptance tests (mock backend; no hardware).

use plippa_hw::{
    associate_playable_with_tag, AssociateParams, FakeClock, MockNfc, MockTag, NfcDevice,
    NfcError, NfcEvent, NfcSession, PollScheduler, TagType, MIN_POLL_INTERVAL,
};
use plippa_noubin::{
    encode_text_record, encode_uri_record, extract_noubin_key, normalise_noubin_key,
    parse_noudata_str, DEFAULT_TEXT_LANG,
};
use std::collections::HashSet;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn mock_read_type2_text_ndef_extract_normalise() {
    let msg = encode_text_record("Smooth Jazz.noubin.XYZ123", DEFAULT_TEXT_LANG).unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(
        MockTag::blank_type2(vec![0x01, 0x02, 0x03, 0x04], 144).with_ndef_message(msg),
    );
    let info = nfc.poll().unwrap().unwrap();
    assert_eq!(info.tag_type, TagType::Type2);
    let ndef = nfc.read_full_ndef().unwrap();
    let key = extract_noubin_key(&ndef).unwrap();
    assert_eq!(key, "Smooth Jazz.noubin.XYZ123");
    assert_eq!(
        normalise_noubin_key(&key),
        "smooth jazz.noubin.xyz123"
    );
}

#[test]
fn mock_read_type4_uri_ndef_extract_normalise() {
    let msg = encode_uri_record("https://artist.com/noubin/album/").unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(
        MockTag::blank_type4(vec![0xAA, 0xBB], 256).with_ndef_message(msg),
    );
    let info = nfc.poll().unwrap().unwrap();
    assert_eq!(info.tag_type, TagType::Type4);
    let ndef = nfc.read_full_ndef().unwrap();
    let key = extract_noubin_key(&ndef).unwrap();
    assert_eq!(key, "https://artist.com/noubin/album/");
    assert_eq!(
        normalise_noubin_key(&key),
        "artist.com/noubin/album/"
    );
}

#[test]
fn write_overwrite_first_record() {
    let old = encode_text_record("old.noubin.AAA", DEFAULT_TEXT_LANG).unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![1], 144).with_ndef_message(old));
    nfc.poll().unwrap();
    let new_msg = encode_text_record("new.noubin.BBB", DEFAULT_TEXT_LANG).unwrap();
    nfc.write_first_ndef_record(&new_msg).unwrap();
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), "new.noubin.BBB");
}

#[test]
fn blank_tag_create_ndef() {
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![9], 144));
    nfc.poll().unwrap();
    let msg = encode_text_record("fresh.noubin.CCC", DEFAULT_TEXT_LANG).unwrap();
    nfc.write_first_ndef_record(&msg).unwrap();
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), "fresh.noubin.CCC");
}

#[test]
fn full_read_before_feedback_sequencing() {
    let msg = encode_text_record("ding.noubin.1", DEFAULT_TEXT_LANG).unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![0x10], 144).with_ndef_message(msg));
    nfc.set_pending_partial_reads(2);

    let mut session = NfcSession::new(nfc, FakeClock { now: Duration::ZERO });

    // First poll: partial → no ReadComplete
    let ev = session.poll_once().unwrap();
    assert!(ev.is_empty(), "must not ding before full read");

    // Advance clock so next poll is due; still partial
    session.scheduler_mut().clock_mut().now = Duration::from_millis(200);
    let ev = session.poll_once().unwrap();
    assert!(ev.is_empty());

    // Third attempt: full read → feedback
    session.scheduler_mut().clock_mut().now = Duration::from_millis(400);
    let ev = session.poll_once().unwrap();
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        NfcEvent::ReadComplete { ndef, .. } => {
            assert_eq!(extract_noubin_key(ndef).unwrap(), "ding.noubin.1");
        }
        other => panic!("expected ReadComplete, got {other:?}"),
    }
}

#[test]
fn repeat_tap_ignored_while_loaded() {
    let msg = encode_text_record("same.noubin.1", DEFAULT_TEXT_LANG).unwrap();
    let uid = vec![0xDE, 0xAD];
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(uid.clone(), 144).with_ndef_message(msg));

    let mut session = NfcSession::new(nfc, FakeClock { now: Duration::ZERO });
    let ev = session.poll_once().unwrap();
    assert!(matches!(ev[0], NfcEvent::ReadComplete { .. }));
    session.set_loaded(Some(uid));

    session.scheduler_mut().clock_mut().now = Duration::from_millis(200);
    let ev = session.poll_once().unwrap();
    assert!(ev.is_empty(), "repeat same UID while loaded must be ignored");
}

#[test]
fn associate_short_key_for_48_byte_capacity() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("track.noudata");
    fs::write(
        &path,
        r#"{
          "playableItemTitle": "Smooth Jazz Playlist With A Very Long Name",
          "mediaList": [{"localFilePaths": ["a.mp3"]}]
        }"#,
    )
    .unwrap();

    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![0x48], 48));
    let existing = HashSet::new();
    let result = associate_playable_with_tag(
        &mut nfc,
        &AssociateParams {
            noudata_path: &path,
            playable_item_title: "Smooth Jazz Playlist With A Very Long Name",
            existing_normalised: &existing,
            noubin_url: None,
            random_len: 12,
        },
    )
    .unwrap();

    assert!(result.noubin_key.len() <= 48);
    assert!(
        !result.noubin_key.contains(".noubin."),
        "48-byte budget should drop title form"
    );
    let doc = parse_noudata_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc.noubin_key.as_deref(), Some(result.noubin_key.as_str()));
    assert_eq!(
        doc.noubin_key_normalised.as_deref(),
        Some(result.noubin_key_normalised.as_str())
    );
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), result.noubin_key);
}

#[test]
fn associate_collision_fails_clearly() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("track.noudata");
    fs::write(
        &path,
        r#"{"playableItemTitle":"Mix","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();

    // Capacity 11 → text key budget of 1 byte (Type2 TLV + NDEF + "en" overhead).
    // Seed every possible 1-char alnum normalised key so generation must exhaust.
    let mut existing = HashSet::new();
    for c in b'A'..=b'Z' {
        existing.insert((c as char).to_ascii_lowercase().to_string());
    }
    for c in b'0'..=b'9' {
        existing.insert((c as char).to_string());
    }

    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![0x02], 11));
    let err = associate_playable_with_tag(
        &mut nfc,
        &AssociateParams {
            noudata_path: &path,
            playable_item_title: "Mix",
            existing_normalised: &existing,
            noubin_url: None,
            random_len: 1,
        },
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("collision") || msg.contains("unique"),
        "expected clear collision error, got {msg}"
    );
}

#[test]
fn write_then_read_round_trip_matches_key() {
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![0x11], 144));
    nfc.poll().unwrap();
    let key = "RoundTrip.noubin.RT01";
    let msg = encode_text_record(key, DEFAULT_TEXT_LANG).unwrap();
    nfc.write_first_ndef_record(&msg).unwrap();
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), key);
}

#[test]
fn poll_interval_at_least_five_hz() {
    let mut sched = PollScheduler::new(FakeClock {
        now: Duration::ZERO,
    });
    assert_eq!(sched.max_interval(), MIN_POLL_INTERVAL);
    assert!(sched.should_poll());
    sched.mark_polled();
    sched.clock_mut().now += Duration::from_millis(199);
    assert!(!sched.should_poll());
    sched.clock_mut().now += Duration::from_millis(1);
    assert!(sched.should_poll());
}

#[test]
fn associate_updates_noudata_and_mock_memory() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("item.noudata");
    fs::write(
        &path,
        r#"{"playableItemTitle":"Mix","mediaList":[{"localFilePaths":["a.mp3"]}],"extraKeep":true}"#,
    )
    .unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![0x33], 256));
    let existing = HashSet::new();
    let result = associate_playable_with_tag(
        &mut nfc,
        &AssociateParams {
            noudata_path: &path,
            playable_item_title: "Mix",
            existing_normalised: &existing,
            noubin_url: None,
            random_len: 8,
        },
    )
    .unwrap();
    assert!(result.noubin_key.contains(".noubin."));
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("extraKeep"));
    assert!(raw.contains(&result.noubin_key));
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), result.noubin_key);
}

// --- NB_06: locked / read-only tags ---------------------------------------------------

#[test]
fn locked_tag_refuses_write_but_stays_readable() {
    let old = encode_text_record("fixed.noubin.LOCK", DEFAULT_TEXT_LANG).unwrap();
    let mut nfc = MockNfc::new();
    nfc.inject(
        MockTag::blank_type4(vec![7], 128)
            .with_ndef_message(old)
            .locked(),
    );
    nfc.poll().unwrap();
    let new_msg = encode_text_record("new.noubin.XYZ", DEFAULT_TEXT_LANG).unwrap();
    let err = nfc.write_first_ndef_record(&new_msg).unwrap_err();
    assert!(matches!(err, NfcError::ReadOnly), "{err:?}");
    assert_eq!(err.to_string(), plippa_hw::TAG_READ_ONLY_MSG);
    let ndef = nfc.read_full_ndef().unwrap();
    assert_eq!(extract_noubin_key(&ndef).unwrap(), "fixed.noubin.LOCK");
}

#[test]
fn blank_locked_tag_is_unwritable_and_unreadable() {
    let mut nfc = MockNfc::new();
    nfc.inject(MockTag::blank_type2(vec![8], 144).locked());
    nfc.poll().unwrap();
    let msg = encode_text_record("x.noubin.1", DEFAULT_TEXT_LANG).unwrap();
    assert!(matches!(
        nfc.write_first_ndef_record(&msg),
        Err(NfcError::ReadOnly)
    ));
    assert!(nfc.read_full_ndef().is_err());
}
