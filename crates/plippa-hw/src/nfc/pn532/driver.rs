//! PN532 [`NfcDevice`] implementation (Type 2 NDEF R/W for plippa-D-poc2).

use super::rpi_i2c::{Pn532I2c, RpiI2cInterface};
use crate::nfc::traits::NfcDevice;
use crate::nfc::types::{NfcError, TagInfo, TagType};
use core::convert::Infallible;
use linux_embedded_hal::nb;
use linux_embedded_hal::{CountDown as HalCountDown, SysTimer};
use pn532::requests::{Command, SAMMode};
use pn532::{CountDown, IntoDuration, Pn532, Request};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const TYPE2_READ_CHUNK: usize = 16;
const TYPE2_USER_START_PAGE: u8 = 4;
const TYPE2_CC_PAGE: u8 = 3;
const TYPE2_MAX_START_PAGE: u8 = 128;

const HLTA: Request<2> = Request::new(Command::InCommunicateThru, [0x50, 0x00]);
const WUPA: Request<1> = Request::new(Command::InCommunicateThru, [0x52]);

struct Pn532Timer(SysTimer);

impl CountDown for Pn532Timer {
    type Time = Duration;

    fn start<T>(&mut self, count: T)
    where
        T: Into<Self::Time>,
    {
        let _ = HalCountDown::start(&mut self.0, count);
    }

    fn wait(&mut self) -> nb::Result<(), Infallible> {
        HalCountDown::wait(&mut self.0)
    }
}

struct ActiveTag {
    uid: Vec<u8>,
    sak: u8,
    capacity: usize,
}

type Pn532Driver = Pn532<RpiI2cInterface, Pn532Timer, 64>;

/// Open options for [`Pn532Nfc`].
#[derive(Debug, Clone)]
pub struct Pn532NfcConfig {
    pub i2c_path: PathBuf,
    /// InListPassiveTarget empty-field timeout (ms).
    pub inlist_timeout_ms: u64,
}

impl Default for Pn532NfcConfig {
    fn default() -> Self {
        Self {
            i2c_path: PathBuf::from("/dev/i2c-1"),
            inlist_timeout_ms: 500,
        }
    }
}

/// Live PN532 over I2C. Prefer Type 2 (NTAG/Ultralight) as used on plippa-D-poc2.
pub struct Pn532Nfc {
    pn532: Pn532Driver,
    active: Option<ActiveTag>,
    inlist_timeout_ms: u64,
    /// After an empty InList (or HLTA), the next [`NfcDevice::poll`] does WUPA+InList
    /// instead of stacking both in one bus-critical section (keeps AW9523 responsive).
    wupa_pending: bool,
}

impl Pn532Nfc {
    pub fn open() -> Result<Self, NfcError> {
        Self::open_with(&Pn532NfcConfig::default())
    }

    pub fn open_with(cfg: &Pn532NfcConfig) -> Result<Self, NfcError> {
        let i2c = Pn532I2c::open(&cfg.i2c_path).map_err(|e| {
            NfcError::Io(format!(
                "open {}: {e:?} — enable I2C and confirm address 0x24",
                cfg.i2c_path.display()
            ))
        })?;
        let mut pn532 = Pn532::new(RpiI2cInterface { i2c }, Pn532Timer(SysTimer::new()));
        thread::sleep(Duration::from_millis(100));

        pn532
            .process(
                &Request::sam_configuration(SAMMode::Normal, false),
                0,
                1000.ms(),
            )
            .map_err(|e| NfcError::Io(format!("SAM configuration failed: {e:?}")))?;

        let _fw = pn532
            .process(&Request::GET_FIRMWARE_VERSION, 4, 500.ms())
            .map_err(|e| NfcError::Io(format!("firmware version read failed: {e:?}")))?;

        Ok(Self {
            pn532,
            active: None,
            inlist_timeout_ms: cfg.inlist_timeout_ms.max(1),
            wupa_pending: false,
        })
    }

    pub fn i2c_path_hint() -> &'static Path {
        Path::new("/dev/i2c-1")
    }

    pub fn inlist_timeout_ms(&self) -> u64 {
        self.inlist_timeout_ms
    }

    /// Next [`NfcDevice::poll`] will WUPA before InList.
    pub fn wake_armed(&self) -> bool {
        self.wupa_pending
    }
}

impl NfcDevice for Pn532Nfc {
    fn poll(&mut self) -> Result<Option<TagInfo>, NfcError> {
        let found = if self.wupa_pending {
            self.wupa_pending = false;
            // HALT wake path — one InList after WUPA (still one critical section, but
            // only on the tick after an empty field check, not stacked every poll).
            let _ = self.pn532.process_no_response(&WUPA, 20.ms());
            inlist_tag(&mut self.pn532, self.inlist_timeout_ms)
        } else {
            match inlist_tag(&mut self.pn532, self.inlist_timeout_ms) {
                Some(t) => Some(t),
                None => {
                    // Defer WUPA to the next poll so AW9523 can sample between.
                    self.wupa_pending = true;
                    None
                }
            }
        };

        let Some((uid, sak)) = found else {
            self.active = None;
            return Ok(None);
        };

        if is_mifare_classic_1k(sak) {
            self.active = None;
            let _ = hlta(&mut self.pn532);
            self.wupa_pending = true;
            return Err(NfcError::UnsupportedTag);
        }

        // ISO-DEP (Type 4 capable): not yet implemented on this backend.
        if sak & 0x20 != 0 && !is_likely_type2(sak) {
            self.active = None;
            let _ = hlta(&mut self.pn532);
            self.wupa_pending = true;
            return Err(NfcError::UnsupportedTag);
        }

        let capacity = read_type2_capacity(&mut self.pn532).unwrap_or(504);
        self.active = Some(ActiveTag {
            uid: uid.clone(),
            sak,
            capacity,
        });
        Ok(Some(TagInfo {
            uid,
            tag_type: TagType::Type2,
            ndef_capacity: Some(capacity),
        }))
    }

    fn read_full_ndef(&mut self) -> Result<Vec<u8>, NfcError> {
        let active = self.active.as_ref().ok_or(NfcError::NoTag)?;
        if is_mifare_classic_1k(active.sak) {
            return Err(NfcError::UnsupportedTag);
        }
        let mut payload = Vec::new();
        read_type2_user_area(&mut self.pn532, &mut payload)?;
        if payload.is_empty() {
            return Err(NfcError::Ndef(plippa_noubin::NdefError::NoRecord));
        }
        Ok(payload)
    }

    fn write_first_ndef_record(&mut self, ndef_message: &[u8]) -> Result<(), NfcError> {
        let active = self.active.as_ref().ok_or(NfcError::NoTag)?;
        if is_mifare_classic_1k(active.sak) {
            return Err(NfcError::UnsupportedTag);
        }
        // NB_06: a capability container that grants no write access = locked tag. Static
        // lock bits (pages 2-3) are not inspected: those surface as an `ntag_write` error.
        if let Ok(cc) = read_type2_page(&mut self.pn532, TYPE2_CC_PAGE) {
            if type2_cc_is_read_only(&cc) {
                return Err(NfcError::ReadOnly);
            }
        }
        let tlv = plippa_noubin::wrap_type2_ndef_tlv(ndef_message);
        if tlv.len() > active.capacity {
            return Err(NfcError::CapacityExceeded {
                need: tlv.len(),
                have: active.capacity,
            });
        }
        write_type2_user_area(&mut self.pn532, &tlv)?;
        Ok(())
    }
}

fn inlist_tag(pn532: &mut Pn532Driver, timeout_ms: u64) -> Option<(Vec<u8>, u8)> {
    let timeout_ms = timeout_ms.max(1);
    let resp = pn532
        .process(
            &Request::INLIST_ONE_ISO_A_TARGET,
            20,
            timeout_ms.ms(),
        )
        .ok()?;
    if resp.first()? == &0 || resp.len() < 6 {
        return None;
    }
    let sak = resp[4];
    let uid_len = resp[5] as usize;
    let uid = resp.get(6..6 + uid_len)?.to_vec();
    Some((uid, sak))
}

fn hlta(pn532: &mut Pn532Driver) -> Result<(), NfcError> {
    pn532
        .process_no_response(&HLTA, 50.ms())
        .map_err(|e| NfcError::Io(format!("HLTA: {e:?}")))
}

fn is_mifare_classic_1k(sak: u8) -> bool {
    (sak & 0x7F) == 0x08
}

fn is_likely_type2(sak: u8) -> bool {
    matches!(sak & 0x7F, 0x00 | 0x04)
}

fn read_type2_capacity(pn532: &mut Pn532Driver) -> Result<usize, NfcError> {
    let cc = read_type2_page(pn532, TYPE2_CC_PAGE)?;
    Ok(type2_cc_max_ndef_bytes(&cc))
}

fn read_type2_user_area(pn532: &mut Pn532Driver, out: &mut Vec<u8>) -> Result<(), NfcError> {
    let cc_max = read_type2_page(pn532, TYPE2_CC_PAGE)
        .map(|cc| type2_cc_max_ndef_bytes(&cc))
        .unwrap_or(504);
    let first = read_type2_page(pn532, TYPE2_USER_START_PAGE)?;
    let (pages, byte_span) = type2_read_plan(&first, cc_max);
    let mut memory = vec![0u8; byte_span];
    let copy = TYPE2_READ_CHUNK.min(byte_span);
    memory[..copy].copy_from_slice(&first[..copy]);

    for start_page in pages.into_iter().skip(1) {
        let block = read_type2_page(pn532, start_page)?;
        let off = (start_page - TYPE2_USER_START_PAGE) as usize * 4;
        if off < memory.len() {
            let end = (off + TYPE2_READ_CHUNK).min(memory.len());
            memory[off..end].copy_from_slice(&block[..end - off]);
        }
    }
    out.extend_from_slice(&memory);
    Ok(())
}

fn write_type2_user_area(pn532: &mut Pn532Driver, data: &[u8]) -> Result<(), NfcError> {
    // Pad to 4-byte pages.
    let mut padded = data.to_vec();
    while padded.len() % 4 != 0 {
        padded.push(0x00);
    }
    let mut page = TYPE2_USER_START_PAGE;
    for chunk in padded.chunks(4) {
        let mut block = [0u8; 4];
        block.copy_from_slice(chunk);
        write_type2_page(pn532, page, &block)?;
        page = page.saturating_add(1);
    }
    Ok(())
}

fn read_type2_page(pn532: &mut Pn532Driver, page: u8) -> Result<[u8; TYPE2_READ_CHUNK], NfcError> {
    match read_type2_page_once(pn532, page) {
        Ok(b) => Ok(b),
        Err(first) => {
            thread::sleep(Duration::from_micros(200));
            read_type2_page_once(pn532, page).or(Err(first))
        }
    }
}

fn read_type2_page_once(
    pn532: &mut Pn532Driver,
    page: u8,
) -> Result<[u8; TYPE2_READ_CHUNK], NfcError> {
    let result = pn532
        .process(&Request::ntag_read(page), 17, 50.ms())
        .map_err(|e| NfcError::Io(format!("ntag_read: {e:?}")))?;
    if result[0] != 0x00 || result.len() < 1 + TYPE2_READ_CHUNK {
        return Err(NfcError::Io(format!(
            "ntag_read status={} len={}",
            result.first().copied().unwrap_or(0xFF),
            result.len()
        )));
    }
    let mut block = [0u8; TYPE2_READ_CHUNK];
    block.copy_from_slice(&result[1..1 + TYPE2_READ_CHUNK]);
    Ok(block)
}

fn write_type2_page(pn532: &mut Pn532Driver, page: u8, bytes: &[u8; 4]) -> Result<(), NfcError> {
    let result = pn532
        .process(&Request::ntag_write(page, bytes), 1, 100.ms())
        .map_err(|e| NfcError::Io(format!("ntag_write: {e:?}")))?;
    if result.first().copied().unwrap_or(0xFF) != 0x00 {
        return Err(NfcError::Io(format!(
            "ntag_write status={}",
            result.first().copied().unwrap_or(0xFF)
        )));
    }
    Ok(())
}

fn type2_cc_max_ndef_bytes(cc: &[u8; TYPE2_READ_CHUNK]) -> usize {
    if cc[0] == 0xE1 && (cc[1] == 0x10 || cc[1] == 0x12) {
        cc[2] as usize * 8
    } else {
        504
    }
}

/// NFC Forum Type 2 CC byte 3: low nibble 0x0 = write access granted, 0xF = none.
fn type2_cc_is_read_only(cc: &[u8; TYPE2_READ_CHUNK]) -> bool {
    cc[0] == 0xE1 && cc[3] & 0x0F != 0
}

fn ndef_tlv_total_bytes(block: &[u8; TYPE2_READ_CHUNK]) -> Option<usize> {
    if block[0] != 0x03 {
        return None;
    }
    if block[1] == 0xFF {
        Some(4 + u16::from_be_bytes([block[2], block[3]]) as usize)
    } else {
        Some(2 + block[1] as usize)
    }
}

fn type2_read_plan(block: &[u8; TYPE2_READ_CHUNK], cc_max: usize) -> (Vec<u8>, usize) {
    let mut tlv = ndef_tlv_total_bytes(block).unwrap_or(TYPE2_READ_CHUNK);
    tlv = tlv.div_ceil(TYPE2_READ_CHUNK) * TYPE2_READ_CHUNK;
    let span = tlv.min(cc_max).max(TYPE2_READ_CHUNK);
    let mut pages = vec![TYPE2_USER_START_PAGE];
    if span <= TYPE2_READ_CHUNK {
        return (pages, span);
    }
    let mut rem = span - TYPE2_READ_CHUNK;
    let mut p = TYPE2_USER_START_PAGE + 4;
    while rem > 0 && p <= TYPE2_MAX_START_PAGE {
        pages.push(p);
        rem = rem.saturating_sub(TYPE2_READ_CHUNK);
        p += 4;
    }
    (pages, span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_write_access_nibble_marks_read_only() {
        let mut cc = [0u8; TYPE2_READ_CHUNK];
        cc[..4].copy_from_slice(&[0xE1, 0x10, 0x12, 0x00]);
        assert!(!type2_cc_is_read_only(&cc));
        cc[3] = 0x0F;
        assert!(type2_cc_is_read_only(&cc));
        // Not a valid CC (blank/unformatted tag): never classified as locked here.
        cc[0] = 0x00;
        assert!(!type2_cc_is_read_only(&cc));
    }
}
