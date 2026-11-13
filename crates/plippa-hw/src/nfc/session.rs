//! Poll orchestration: full-read-before-feedback and same-tag debounce (§3.2).

use super::poll::{PollClock, PollScheduler};
use super::traits::NfcDevice;
use super::types::{NfcError, NfcEvent, TagInfo};

/// Session layer over an [`NfcDevice`].
///
/// - Emits [`NfcEvent::ReadComplete`] only after a successful full NDEF read.
/// - While `loaded` is set, repeats of the same UID are ignored (§3.2 duplicate tap).
pub struct NfcSession<D: NfcDevice, C: PollClock> {
    device: D,
    scheduler: PollScheduler<C>,
    loaded_uid: Option<Vec<u8>>,
    last_uid: Option<Vec<u8>>,
}

impl<D: NfcDevice, C: PollClock> NfcSession<D, C> {
    pub fn new(device: D, clock: C) -> Self {
        Self {
            device: device,
            scheduler: PollScheduler::new(clock),
            loaded_uid: None,
            last_uid: None,
        }
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    pub fn scheduler(&self) -> &PollScheduler<C> {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut PollScheduler<C> {
        &mut self.scheduler
    }

    /// Mark the current tag as loaded/playing so repeat taps of the same UID are ignored.
    pub fn set_loaded(&mut self, uid: Option<Vec<u8>>) {
        self.loaded_uid = uid;
    }

    pub fn loaded_uid(&self) -> Option<&[u8]> {
        self.loaded_uid.as_deref()
    }

    /// Run one poll cycle when due. Returns events (0 or 1 typically).
    ///
    /// Callers should sleep/`time_until_due` between loops to maintain ≥5 Hz without busy-spin.
    pub fn poll_once(&mut self) -> Result<Vec<NfcEvent>, NfcError> {
        if !self.scheduler.should_poll() {
            return Ok(Vec::new());
        }
        self.scheduler.mark_polled();

        let mut events = Vec::new();
        match self.device.poll()? {
            None => {
                if let Some(uid) = self.last_uid.take() {
                    events.push(NfcEvent::TagRemoved { uid });
                }
            }
            Some(info) => {
                if self.should_ignore(&info) {
                    self.last_uid = Some(info.uid);
                    return Ok(events);
                }

                eprintln!(
                    "plippa-hw: NFC field hit uid={:02x?} type={:?} — reading NDEF",
                    info.uid, info.tag_type
                );

                // §3.2: complete full read before clear feedback.
                match self.device.read_full_ndef() {
                    Ok(ndef) => {
                        self.last_uid = Some(info.uid.clone());
                        events.push(NfcEvent::ReadComplete { info, ndef });
                    }
                    Err(NfcError::Ndef(plippa_noubin::NdefError::NoRecord)) => {
                        // Blank / empty: still a completed read of “no record” — surface as error
                        // to the caller by not emitting ReadComplete (associate path uses device
                        // directly). Treat as present but no feedback ding for playback.
                        eprintln!(
                            "plippa-hw: NFC tag uid={:02x?} present but no NDEF record",
                            info.uid
                        );
                        self.last_uid = Some(info.uid);
                    }
                    Err(e) => {
                        // Partial / lost mid-read: do not emit feedback; keep last_uid unset so
                        // a later successful full read can still ding.
                        eprintln!(
                            "plippa-hw: NFC NDEF read failed uid={:02x?}: {e}",
                            info.uid
                        );
                    }
                }
            }
        }
        Ok(events)
    }

    fn should_ignore(&self, info: &TagInfo) -> bool {
        match &self.loaded_uid {
            Some(loaded) if loaded == &info.uid => true,
            _ => false,
        }
    }
}
