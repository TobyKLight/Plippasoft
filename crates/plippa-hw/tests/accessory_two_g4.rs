//! ACC_02 backend tests that need a bus/service. The pure parse and error mapping tests live
//! next to their modules and run with `--features accessory-2g4`.

#![cfg(feature = "accessory-2g4")]

use std::time::Duration;

use plippa_hw::{AccessoryBackend, AccessoryError, TwoG4Backend};

#[test]
#[ignore = "needs the device radio service; run manually in the ACC_02 device pass"]
fn a_machine_without_the_radio_service_reports_unavailable() {
    let mut backend = TwoG4Backend::new();
    std::thread::sleep(Duration::from_millis(250));

    assert!(matches!(backend.known(), Err(AccessoryError::Unavailable)));
    assert!(matches!(
        backend.start_scan(10),
        Err(AccessoryError::Unavailable)
    ));
}

#[test]
#[ignore = "needs the device radio service; run manually in the ACC_02 device pass"]
fn a_machine_without_the_radio_service_emits_one_failure_event() {
    let mut backend = TwoG4Backend::new();
    std::thread::sleep(Duration::from_millis(250));

    let failures = backend
        .poll()
        .into_iter()
        .filter(|event| matches!(event, plippa_hw::AccessoryEvent::Failed(_)))
        .count();
    assert_eq!(failures, 1, "the unavailable notice is emitted exactly once");
}
