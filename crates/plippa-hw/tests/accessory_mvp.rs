//! ACC_01 accessory model, mock and policy acceptance tests (no hardware).

use plippa_hw::{
    auto_connect_target, normalise_address, props_match_address, Accessory, AccessoryBackend,
    AccessoryError, AccessoryEvent, AccessoryKind, AdapterInfo, AutoConnectBudget,
    MockAccessoryBackend, PairPrompt, PromptAnswer, ScanWindow, MAX_SCAN_SECONDS, MAX_TRIES,
    RETRY_SPACING,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

fn accessory(id: &str, kind: AccessoryKind) -> Accessory {
    Accessory {
        id: normalise_address(id),
        name: "Device".to_string(),
        kind,
        paired: true,
        trusted: true,
        connected: false,
        battery_pct: None,
    }
}

fn adapter() -> AdapterInfo {
    AdapterInfo {
        id: "hci0".to_string(),
        address: normalise_address("00:11:22:33:44:55"),
        name: "Built-in".to_string(),
        powered: false,
    }
}

#[test]
fn normalise_address_formats_match() {
    let expected = "aa:bb:cc:dd:ee:ff";
    assert_eq!(normalise_address("AA:BB:CC:DD:EE:FF"), expected);
    assert_eq!(normalise_address("aa-bb-cc-dd-ee-ff"), expected);
    assert_eq!(normalise_address("aabbccddeeff"), expected);
    assert_eq!(normalise_address("AABBCCDDEEFF"), expected);
    assert_eq!(normalise_address("garbage-AABB"), "garbage-aabb");
    assert_eq!(normalise_address("aabbccddeef"), "aabbccddeef");
    assert_eq!(normalise_address(""), "");
}

#[test]
fn auto_connect_needs_marked_paired_audio_and_nothing_connected() {
    let low = accessory("aa:bb:cc:dd:ee:01", AccessoryKind::Audio);
    let high = accessory("aa:bb:cc:dd:ee:02", AccessoryKind::Audio);
    let input = accessory("aa:bb:cc:dd:ee:00", AccessoryKind::Input);

    assert!(auto_connect_target(&[low.clone(), high.clone()], &BTreeSet::new()).is_none());

    let mut marked = BTreeSet::new();
    marked.insert(high.id.clone());
    assert_eq!(
        auto_connect_target(&[low.clone(), high.clone()], &marked).map(|a| a.id.clone()),
        Some(high.id.clone())
    );

    let mut input_ids = BTreeSet::new();
    input_ids.insert(input.id.clone());
    assert!(auto_connect_target(std::slice::from_ref(&input), &input_ids).is_none());

    let mut unpaired = low.clone();
    unpaired.paired = false;
    let mut unpaired_ids = BTreeSet::new();
    unpaired_ids.insert(unpaired.id.clone());
    assert!(auto_connect_target(&[unpaired], &unpaired_ids).is_none());

    let mut connected = low.clone();
    connected.connected = true;
    assert!(auto_connect_target(&[connected, high.clone()], &marked).is_none());

    let mut both = marked.clone();
    both.insert(low.id.clone());
    let candidates = [high.clone(), low.clone()];
    let target = auto_connect_target(&candidates, &both).expect("target");
    assert_eq!(target.id, low.id);

    let mut forgotten = BTreeSet::new();
    forgotten.insert(normalise_address("ff:ff:ff:ff:ff:ff"));
    assert!(auto_connect_target(&[low, high], &forgotten).is_none());
}

#[test]
fn auto_connect_budget_has_three_tries_per_id_and_spaces_retries() {
    assert_eq!(MAX_TRIES, 3);
    assert_eq!(RETRY_SPACING, Duration::from_secs(5));

    let mut budget = AutoConnectBudget::new();
    let id = normalise_address("aa:bb:cc:dd:ee:01");

    assert!(budget.try_take(Duration::ZERO, &id));
    assert!(!budget.try_take(Duration::from_secs(4), &id));
    assert!(budget.try_take(Duration::from_secs(5), &id));
    assert!(budget.try_take(Duration::from_secs(10), &id));
    assert!(!budget.try_take(Duration::from_secs(100), &id));
    assert_eq!(budget.tries(&id), MAX_TRIES);

    let other = normalise_address("aa:bb:cc:dd:ee:02");
    assert!(budget.try_take(Duration::from_secs(100), &other));
    assert_eq!(budget.tries(&other), 1);
}

#[test]
fn scan_window_clamps_expires_at_deadline_and_floors_remaining() {
    assert_eq!(MAX_SCAN_SECONDS, 60);

    let window = ScanWindow::start(Duration::ZERO, 120);
    assert_eq!(window.seconds(), MAX_SCAN_SECONDS);
    assert_eq!(window.remaining(Duration::ZERO), Duration::from_secs(60));
    assert!(window.is_open(Duration::from_secs(59)));
    assert!(!window.is_open(Duration::from_secs(60)));
    assert_eq!(window.remaining(Duration::from_secs(60)), Duration::ZERO);
    assert_eq!(window.remaining(Duration::from_secs(600)), Duration::ZERO);

    let mut closed = ScanWindow::start(Duration::from_secs(10), 30);
    assert!(closed.is_open(Duration::from_secs(10)));
    closed.close();
    assert!(!closed.is_open(Duration::from_secs(10)));
    assert_eq!(closed.remaining(Duration::from_secs(10)), Duration::ZERO);
}

#[test]
fn props_match_address_uses_values_not_keys_and_ignores_formatting() {
    let address = "aa:bb:cc:dd:ee:ff";
    let mut props = BTreeMap::new();
    props.insert(
        "node.name".to_string(),
        "aa_bb_cc_dd_ee_ff".to_string(),
    );
    assert!(!props_match_address(&props, address));

    props.insert("device.address".to_string(), "AA-BB-CC-DD-EE-FF".to_string());
    assert!(props_match_address(&props, address));
    assert!(props_match_address(&props, "aabbccddeeff"));

    let mut key_only = BTreeMap::new();
    key_only.insert(address.to_string(), "not an address".to_string());
    assert!(!props_match_address(&key_only, address));
}

#[test]
fn mock_scripted_pair_flow_pairs_after_matching_passkey() {
    let id = normalise_address("AA:BB:CC:DD:EE:FF");
    let mut mock = MockAccessoryBackend::new();
    mock.script_scan(vec![(
        id.clone(),
        "Speaker".to_string(),
        AccessoryKind::Audio,
    )]);
    mock.script_pair(&id, [PromptAnswer::Passkey(1234)]);

    mock.start_scan(30).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Discovered {
            id: id.clone(),
            name: "Speaker".to_string(),
            kind: AccessoryKind::Audio,
        }]
    );
    mock.set_now(Duration::from_secs(31));
    assert_eq!(mock.poll(), vec![AccessoryEvent::ScanFinished]);

    mock.pair(&id).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Prompt(PairPrompt::Confirm {
            peer: plippa_hw::Peer::new(Some("Speaker".into()), &id),
            passkey: 1234,
        })]
    );
    mock.answer(PromptAnswer::Passkey(1234)).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Paired {
            id: id.clone(),
            ok: true,
            error: None,
        }]
    );

    let known = mock.known().unwrap();
    let paired = known.iter().find(|a| a.id == id).expect("paired accessory");
    assert!(paired.paired);
    assert_eq!(paired.kind, AccessoryKind::Audio);
}

#[test]
fn mock_scan_stops_and_drops_queued_discoveries_at_the_deadline() {
    let first = normalise_address("aa:bb:cc:dd:ee:01");
    let second = normalise_address("aa:bb:cc:dd:ee:02");
    let third = normalise_address("aa:bb:cc:dd:ee:03");
    let mut mock = MockAccessoryBackend::new();
    mock.script_scan(vec![
        (first.clone(), "First".to_string(), AccessoryKind::Audio),
        (second.clone(), "Second".to_string(), AccessoryKind::Audio),
        (third.clone(), "Third".to_string(), AccessoryKind::Audio),
    ]);

    mock.start_scan(10).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Discovered {
            id: first.clone(),
            name: "First".to_string(),
            kind: AccessoryKind::Audio,
        }]
    );

    mock.set_now(Duration::from_secs(11));
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::ScanFinished],
        "the deadline ends the scan instead of draining the rest of the queue"
    );
    assert!(mock.poll().is_empty(), "the remaining discoveries are dropped");
}

#[test]
fn mock_rejected_prompt_leaves_accessory_unpaired() {
    let id = normalise_address("11:22:33:44:55:66");
    let mut unpaired = accessory(&id, AccessoryKind::Audio);
    unpaired.paired = false;
    let mut mock = MockAccessoryBackend::new().with_accessory(unpaired);
    mock.script_pair(&id, [PromptAnswer::Accept]);

    mock.pair(&id).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Prompt(PairPrompt::Authorise {
            peer: plippa_hw::Peer::new(Some("Device".into()), &id),
        })]
    );
    mock.answer(PromptAnswer::Reject).unwrap();

    let events = mock.poll();
    assert_eq!(events.len(), 1);
    match &events[0] {
        AccessoryEvent::Paired { ok, error, .. } => {
            assert!(!ok);
            assert_eq!(error.as_deref(), Some("rejected"));
        }
        other => panic!("expected Paired, got {other:?}"),
    }
    let known = mock.known().unwrap();
    let paired = known.iter().find(|a| a.id == id).expect("accessory");
    assert!(!paired.paired);
}

#[test]
fn mock_scripted_pair_failure_reports_error() {
    let id = normalise_address("11:22:33:44:55:77");
    let mut mock = MockAccessoryBackend::new();
    mock.script_scan(vec![(
        id.clone(),
        "Headset".to_string(),
        AccessoryKind::Audio,
    )]);
    mock.script_pair_failure(&id, "device refused");
    mock.start_scan(10).unwrap();
    mock.poll();

    mock.pair(&id).unwrap();
    match mock.poll().as_slice() {
        [AccessoryEvent::Paired {
            ok: false,
            error: Some(error),
            ..
        }] => assert_eq!(error, "device refused"),
        other => panic!("expected failed Paired, got {other:?}"),
    }
    assert!(mock.known().unwrap().iter().all(|a| a.id != id));
}

#[test]
fn mock_unavailable_mode_returns_unavailable_and_empty_poll() {
    let mut mock = MockAccessoryBackend::unavailable();
    let id = normalise_address("aa:bb:cc:dd:ee:ff");

    assert!(matches!(mock.adapters(), Err(AccessoryError::Unavailable)));
    assert!(matches!(mock.known(), Err(AccessoryError::Unavailable)));
    assert!(matches!(
        mock.power_on("hci0"),
        Err(AccessoryError::Unavailable)
    ));
    assert!(matches!(
        mock.start_scan(10),
        Err(AccessoryError::Unavailable)
    ));
    assert!(matches!(mock.stop_scan(), Err(AccessoryError::Unavailable)));
    assert!(matches!(mock.pair(&id), Err(AccessoryError::Unavailable)));
    assert!(matches!(
        mock.answer(PromptAnswer::Accept),
        Err(AccessoryError::Unavailable)
    ));
    assert!(matches!(mock.connect(&id), Err(AccessoryError::Unavailable)));
    assert!(matches!(
        mock.disconnect(&id),
        Err(AccessoryError::Unavailable)
    ));
    assert!(matches!(
        mock.set_trusted(&id, true),
        Err(AccessoryError::Unavailable)
    ));
    assert!(matches!(mock.forget(&id), Err(AccessoryError::Unavailable)));
    assert!(mock.poll().is_empty());
}

#[test]
fn mock_records_calls_in_order() {
    let id = normalise_address("aa:bb:cc:dd:ee:ff");
    let mut mock =
        MockAccessoryBackend::new().with_accessory(accessory(&id, AccessoryKind::Audio));

    mock.start_scan(10).unwrap();
    mock.stop_scan().unwrap();
    mock.connect(&id).unwrap();

    assert_eq!(
        mock.calls(),
        &[
            "start_scan 10".to_string(),
            "stop_scan".to_string(),
            format!("connect {id}"),
        ]
    );
    assert_eq!(
        mock.calls()
            .iter()
            .filter(|call| call.starts_with("connect "))
            .count(),
        1
    );
    assert_eq!(
        mock.poll(),
        vec![
            AccessoryEvent::ScanFinished,
            AccessoryEvent::Connected(id),
        ]
    );
}

#[test]
fn mock_scripts_seed_accessories_and_operations_are_idempotent() {
    let id = normalise_address("aa:bb:cc:dd:ee:99");
    let mut mock = MockAccessoryBackend::new().with_adapter(adapter());

    mock.power_on("hci0").unwrap();
    mock.power_on("hci0").unwrap();
    assert!(mock.adapters().unwrap()[0].powered);

    mock.script_connect(&id);
    mock.connect(&id).unwrap();
    mock.connect(&id).unwrap();
    assert_eq!(mock.poll(), vec![AccessoryEvent::Connected(id.clone())]);

    mock.script_disconnect(&id);
    mock.disconnect(&id).unwrap();
    assert_eq!(
        mock.poll(),
        vec![AccessoryEvent::Disconnected(id.clone())]
    );

    mock.script_forget(&id);
    mock.forget(&id).unwrap();
    assert!(mock.known().unwrap().is_empty());

    mock.stop_scan().unwrap();
    assert!(mock.poll().is_empty());

    assert!(matches!(
        mock.forget(&normalise_address("ff:ff:ff:ff:ff:ff")),
        Err(AccessoryError::Unknown(_))
    ));
}
