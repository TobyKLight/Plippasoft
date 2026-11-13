//! PN532 NFC session wrapper owned by the I²C service thread (feature `poc2`).

use crate::i2c_service::NfcBusOps;
use plippa_board::NfcConfig;
use plippa_hw::{
    NfcDevice, NfcEvent, NfcSession, Pn532Nfc, Pn532NfcConfig, PollScheduler, SteadyClock,
};
use std::path::PathBuf;
use std::time::Duration;

pub struct Poc2Nfc {
    session: NfcSession<Pn532Nfc, SteadyClock>,
}

impl Poc2Nfc {
    pub fn open(cfg: &NfcConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let path = PathBuf::from(format!("/dev/i2c-{}", cfg.i2c_bus));
        let device = Pn532Nfc::open_with(&Pn532NfcConfig {
            i2c_path: path,
            inlist_timeout_ms: cfg.inlist_timeout_ms,
        })?;
        eprintln!(
            "plippa-ui: PN532 InList timeout={}ms poll_hz={}",
            cfg.inlist_timeout_ms, cfg.poll_hz
        );
        // Coordinator already rate-limits PollTick via poll_hz — do not double-gate
        // inside NfcSession or field checks get skipped while still burning bus turns.
        let mut session = NfcSession::new(device, SteadyClock::new());
        *session.scheduler_mut() =
            PollScheduler::new(SteadyClock::new()).with_max_interval(Duration::ZERO);
        Ok(Self { session })
    }
}

impl NfcBusOps for Poc2Nfc {
    fn poll_events(&mut self) -> Result<Vec<NfcEvent>, String> {
        self.session.poll_once().map_err(|e| e.to_string())
    }

    fn write_first_ndef(&mut self, ndef_message: &[u8]) -> Result<(), String> {
        self.session
            .device_mut()
            .write_first_ndef_record(ndef_message)
            .map_err(|e| e.to_string())
    }

    fn poll_tag_present(&mut self) -> Result<bool, String> {
        Ok(self
            .session
            .device_mut()
            .poll()
            .map_err(|e| e.to_string())?
            .is_some())
    }

    fn set_loaded_uid(&mut self, uid: Option<Vec<u8>>) {
        self.session.set_loaded(uid);
    }

    fn mark_field_tag_as_loaded(&mut self) -> Result<(), String> {
        if let Some(info) = self
            .session
            .device_mut()
            .poll()
            .map_err(|e| e.to_string())?
        {
            self.session.set_loaded(Some(info.uid));
        }
        Ok(())
    }

    fn wake_armed(&self) -> bool {
        self.session.device().wake_armed()
    }
}
