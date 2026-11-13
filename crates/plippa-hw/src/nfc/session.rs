//! Poll orchestration: full-read-before-feedback and same-tag debounce (§3.2).

use super::poll::{PollClock, PollScheduler};
use super::traits::NfcDevice;
use super::types::{NfcError, NfcEvent, TagInfo};
use plippa_noubin::{extract_noubin_key, uid_noubin_key, UidKeyError};

/// Session layer over an [`NfcDevice`].
///
/// - Emits [`NfcEvent::ReadComplete`] only after a successful full NDEF read.
/// - While `loaded` is set, repeats of the same UID are ignored (§3.2 duplicate tap).
pub struct NfcSession<D: NfcDevice, C: PollClock> {
    device: D,
    scheduler: PollScheduler<C>,
    loaded_uid: Option<Vec<u8>>,
    last_uid: Option<Vec<u8>>,
    /// A tag whose UID cannot be used (random / unstable) is reported once, then ignored until
    /// the field is empty again (a random-UID tag shows a new UID every poll).
    uid_refused: bool,
}

/// The `uid:<hex>` key of the tag in the field, confirmed by activating it a second time
/// (§6.1: a UID that differs between two reads is a random UID and MUST be refused).
///
/// `Ok(None)` when the tag left the field before the second read (nothing to report).
pub fn confirmed_uid_key<D: NfcDevice>(
    device: &mut D,
    info: &TagInfo,
) -> Result<Option<Result<String, UidKeyError>>, NfcError> {
    let key = match uid_noubin_key(&info.uid) {
        Ok(key) => key,
        Err(e) => return Ok(Some(Err(e))),
    };
    match device.poll()? {
        None => Ok(None),
        Some(again) if again.uid == info.uid => Ok(Some(Ok(key))),
        Some(_) => Ok(Some(Err(UidKeyError::Unstable))),
    }
}

impl<D: NfcDevice, C: PollClock> NfcSession<D, C> {
    pub fn new(device: D, clock: C) -> Self {
        Self {
            device: device,
            scheduler: PollScheduler::new(clock),
            loaded_uid: None,
            last_uid: None,
            uid_refused: false,
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
                self.uid_refused = false;
                if let Some(uid) = self.last_uid.take() {
                    events.push(NfcEvent::TagRemoved { uid });
                }
            }
            Some(info) => {
                if self.should_ignore(&info) || self.uid_refused {
                    self.last_uid = Some(info.uid);
                    return Ok(events);
                }

                eprintln!(
                    "plippa-hw: NFC field hit uid={:02x?} type={:?} — reading NDEF",
                    info.uid, info.tag_type
                );

                // §3.2: complete full read before clear feedback.
                match self.device.read_full_ndef() {
                    // A usable URI/Text record: the NDEF key (§6.1 phase 1).
                    Ok(ndef) if extract_noubin_key(&ndef).is_ok() => {
                        self.last_uid = Some(info.uid.clone());
                        events.push(NfcEvent::ReadComplete { info, ndef });
                    }
                    // No suitable NDEF record (blank, another record type, or no NDEF at all):
                    // the tag UID is the Noubin Key (§6.1 UID fallback, §1.18.1).
                    Ok(_)
                    | Err(NfcError::Ndef(_))
                    | Err(NfcError::UnsupportedTag) => {
                        eprintln!(
                            "plippa-hw: NFC tag uid={:02x?} has no NDEF Text/URI record — using its UID",
                            info.uid
                        );
                        self.last_uid = Some(info.uid.clone());
                        if let Some(key) = confirmed_uid_key(&mut self.device, &info)? {
                            if key.is_err() {
                                self.uid_refused = true;
                            }
                            events.push(NfcEvent::UidTag { info, key });
                        }
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
