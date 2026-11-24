//! PN532 [`NfcDevice`] implementation (Type 2 NDEF R/W for plippa-D-poc2).

use super::rpi_i2c::{Pn532I2c, RpiI2cInterface};
use crate::nfc::traits::NfcDevice;
use crate::nfc::types::{NfcError, TagInfo, TagType};
use core::convert::Infallible;
use linux_embedded_hal::nb;
use linux_embedded_hal::{CountDown as HalCountDown, SysTimer};
use pn532::requests::{Command, SAMMode};
use pn532::{CountDown, Error as Pn532Error, IntoDuration, Pn532, Request};
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
    i2c_path: PathBuf,
    /// Firmware version words read at open; every later probe must match.
    firmware: [u8; 4],
}

impl Pn532Nfc {
    pub fn open() -> Result<Self, NfcError> {
        Self::open_with(&Pn532NfcConfig::default())
    }

    pub fn open_with(cfg: &Pn532NfcConfig) -> Result<Self, NfcError> {
        let i2c = open_i2c(&cfg.i2c_path)?;
        let mut pn532 = Pn532::new(RpiI2cInterface::new(i2c), Pn532Timer(SysTimer::new()));
        let firmware = init_chip(&mut pn532)?;

        Ok(Self {
            pn532,
            active: None,
            inlist_timeout_ms: cfg.inlist_timeout_ms.max(1),
            wupa_pending: false,
            i2c_path: cfg.i2c_path.clone(),
            firmware,
        })
    }

    /// Short active probe with a known answer (an idle field and a stuck reader look the same
    /// to polling). Runs GetFirmwareVersion and compares with the version read at open, then a
    /// Diagnose communication-line test echoing a fixed pattern.
    pub fn health_check(&mut self) -> Result<(), NfcError> {
        probe_chip(&mut self.pn532, &self.firmware)
    }

    /// Re-run the open-time init (settle, SAM configuration, firmware read); if that fails,
    /// reopen the I2C device handle and try once more. Touches only the PN532.
    pub fn recover(&mut self) -> Result<(), NfcError> {
        self.active = None;
        self.wupa_pending = false;
        let first = init_chip(&mut self.pn532);
        let first_err = match first {
            Ok(fw) => return self.adopt_firmware(fw),
            Err(e) => e,
        };
        let i2c = open_i2c(&self.i2c_path).map_err(|e| {
            NfcError::Io(format!("re-init failed ({first_err}); reopen failed ({e})"))
        })?;
        self.pn532.interface.i2c = i2c;
        let fw = init_chip(&mut self.pn532)
            .map_err(|e| NfcError::Io(format!("re-init failed ({first_err}); after reopen ({e})")))?;
        self.adopt_firmware(fw)
    }

    fn adopt_firmware(&mut self, fw: [u8; 4]) -> Result<(), NfcError> {
        if fw != self.firmware {
            return Err(NfcError::Io(format!(
                "firmware version changed after re-init: {:02x?} != {:02x?}",
                fw, self.firmware
            )));
        }
        Ok(())
    }

    /// A tag is currently selected (seen by the last poll).
    pub fn has_active_tag(&self) -> bool {
        self.active.is_some()
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

fn open_i2c(path: &Path) -> Result<Pn532I2c, NfcError> {
    Pn532I2c::open(path).map_err(|e| {
        NfcError::Io(format!(
            "open {}: {e:?} — enable I2C and confirm address 0x24",
            path.display()
        ))
    })
}

/// Settle, SAM configuration, firmware read. Returns the firmware version words.
fn init_chip(pn532: &mut Pn532Driver) -> Result<[u8; 4], NfcError> {
    thread::sleep(Duration::from_millis(100));
    pn532
        .process(
            &Request::sam_configuration(SAMMode::Normal, false),
            0,
            1000.ms(),
        )
        .map_err(|e| NfcError::Io(format!("SAM configuration failed: {e:?}")))?;
    read_firmware(pn532)
}

fn read_firmware(pn532: &mut Pn532Driver) -> Result<[u8; 4], NfcError> {
    let fw = pn532
        .process(&Request::GET_FIRMWARE_VERSION, 4, 500.ms())
        .map_err(|e| NfcError::Io(format!("firmware version read failed: {e:?}")))?;
    <[u8; 4]>::try_from(fw)
        .map_err(|_| NfcError::Io(format!("firmware version reply has {} bytes", fw.len())))
}

/// Diagnose test 0x00 (communication line): the chip echoes the pattern back.
const COMMLINE_TEST: Request<5> = Request::new(Command::Diagnose, [0x00, 0xA5, 0x5A, 0x3C, 0xC3]);

fn probe_chip(pn532: &mut Pn532Driver, expected_fw: &[u8; 4]) -> Result<(), NfcError> {
    let fw = read_firmware(pn532)?;
    if &fw != expected_fw {
        return Err(NfcError::Io(format!(
            "firmware version {fw:02x?} != {expected_fw:02x?}"
        )));
    }
    let echo = pn532
        .process(&COMMLINE_TEST, COMMLINE_TEST.data.len(), 100.ms())
        .map_err(|e| NfcError::Io(format!("comm-line test failed: {e:?}")))?;
    if echo != COMMLINE_TEST.data {
        return Err(NfcError::Io(format!("comm-line test echo {echo:02x?}")));
    }
    Ok(())
}

impl NfcDevice for Pn532Nfc {
    fn poll(&mut self) -> Result<Option<TagInfo>, NfcError> {
        let outcome = if self.wupa_pending {
            self.wupa_pending = false;
            // HALT wake path — one InList after WUPA (still one critical section, but
            // only on the tick after an empty field check, not stacked every poll).
            let _ = self.pn532.process_no_response(&WUPA, 20.ms());
            inlist_tag(&mut self.pn532, self.inlist_timeout_ms)
        } else {
            let r = inlist_tag(&mut self.pn532, self.inlist_timeout_ms);
            if matches!(r, Ok(InList::Idle)) {
                // Defer WUPA to the next poll so AW9523 can sample between.
                self.wupa_pending = true;
            }
            r
        };

        let found = match outcome {
            Ok(InList::Tag(uid, sak)) => Some((uid, sak)),
            Ok(InList::Idle) => None,
            Err(e) => {
                self.active = None;
                return Err(NfcError::Io(e));
            }
        };

        let Some((uid, sak)) = found else {
            self.active = None;
            return Ok(None);
        };

        // MIFARE Classic, and ISO-DEP (Type 4 capable, not yet implemented on this backend):
        // no NDEF we can read or write, but the tag is present and its UID is a usable Noubin
        // Key (§1.18.1). Keep it active so reads and writes report `UnsupportedTag`.
        if is_non_ndef(sak) {
            self.active = Some(ActiveTag {
                uid: uid.clone(),
                sak,
                capacity: 0,
            });
            return Ok(Some(TagInfo {
                uid,
                tag_type: TagType::NonNdef,
                ndef_capacity: None,
            }));
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
        if is_non_ndef(active.sak) {
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
        if is_non_ndef(active.sak) {
            return Err(NfcError::UnsupportedTag);
        }
        // NB_06: a capability container that grants no write access = locked tag. Static
        // lock bits (pages 2-3) are not inspected: those surface as an `ntag_write` error.
        if let Ok(cc) = read_type2_page(&mut self.pn532, TYPE2_CC_PAGE) {
            if type2_cc_is_read_only(&cc) {
                return Err(NfcError::ReadOnly);
            }
        }
        // A tag that already carries a link is never overwritten (read on the same tag, right
        // before the write, so nothing can change in between).
        if let Ok(existing) = self.read_full_ndef() {
            if plippa_noubin::first_record_is_uri_key(&existing) {
                return Err(NfcError::HasLink);
            }
        }
        let active = self.active.as_ref().ok_or(NfcError::NoTag)?;
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

/// Result of one InListPassiveTarget that did not fail.
#[derive(Debug, PartialEq, Eq)]
enum InList {
    Tag(Vec<u8>, u8),
    /// Empty field (zero targets, or the normal response timeout).
    Idle,
}

fn inlist_tag(pn532: &mut Pn532Driver, timeout_ms: u64) -> Result<InList, String> {
    let timeout_ms = timeout_ms.max(1);
    let errors_before = pn532.interface.wait_errors;
    let r = pn532
        .process(&Request::INLIST_ONE_ISO_A_TARGET, 20, timeout_ms.ms())
        .map(<[u8]>::to_vec);
    let bus_errors = pn532.interface.wait_errors != errors_before;
    classify_inlist(r, bus_errors)
}

/// Tag / idle / hard failure. A response timeout is the normal empty-field outcome, unless the
/// bus errored while waiting (the interface hides those as "not ready"). Everything else
/// (bad frames, ACK timeout, interface errors, malformed replies) is a hard failure.
fn classify_inlist<E: core::fmt::Debug>(
    r: Result<impl AsRef<[u8]>, Pn532Error<E>>,
    bus_errors: bool,
) -> Result<InList, String> {
    match r {
        Ok(resp) => parse_inlist(resp.as_ref()),
        Err(Pn532Error::TimeoutResponse) if !bus_errors => Ok(InList::Idle),
        Err(Pn532Error::TimeoutResponse) => Err("InList: I2C errors while waiting".into()),
        Err(e) => Err(format!("InList: {e:?}")),
    }
}

fn parse_inlist(resp: &[u8]) -> Result<InList, String> {
    match resp.first() {
        None => return Err("InList: empty response".into()),
        Some(0) => return Ok(InList::Idle),
        Some(_) => {}
    }
    if resp.len() < 6 {
        return Err(format!("InList: short response {resp:02x?}"));
    }
    let sak = resp[4];
    let uid_len = resp[5] as usize;
    let uid = resp
        .get(6..6 + uid_len)
        .ok_or_else(|| format!("InList: truncated UID in {resp:02x?}"))?
        .to_vec();
    Ok(InList::Tag(uid, sak))
}

fn hlta(pn532: &mut Pn532Driver) -> Result<(), NfcError> {
    pn532
        .process_no_response(&HLTA, 50.ms())
        .map_err(|e| NfcError::Io(format!("HLTA: {e:?}")))
}

fn is_mifare_classic_1k(sak: u8) -> bool {
    (sak & 0x7F) == 0x08
}

/// Tags this backend cannot read or write as NDEF: MIFARE Classic and ISO-DEP (Type 4).
fn is_non_ndef(sak: u8) -> bool {
    is_mifare_classic_1k(sak) || (sak & 0x20 != 0 && !is_likely_type2(sak))
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

    type E = Pn532Error<()>;

    fn fail(e: E, bus: bool) -> Result<InList, String> {
        classify_inlist::<()>(Err::<Vec<u8>, E>(e), bus)
    }

    #[test]
    fn inlist_classification_separates_tag_idle_and_failure() {
        let tag = [1, 1, 0x00, 0x04, 0x00, 4, 0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(
            classify_inlist::<()>(Ok(&tag), false),
            Ok(InList::Tag(vec![0xDE, 0xAD, 0xBE, 0xEF], 0x00))
        );
        assert_eq!(classify_inlist::<()>(Ok(&[0]), false), Ok(InList::Idle));
        assert_eq!(
            fail(E::TimeoutResponse, false),
            Ok(InList::Idle)
        );
        // Timeout while the bus was erroring is not an empty field.
        assert!(fail(E::TimeoutResponse, true).is_err());
        assert!(fail(E::TimeoutAck, false).is_err());
        assert!(fail(E::BadAck, false).is_err());
        assert!(fail(E::InterfaceError(()), false).is_err());
        assert!(classify_inlist::<()>(Ok(&[]), false).is_err());
        assert!(classify_inlist::<()>(Ok(&[1, 1, 0]), false).is_err());
        // UID length runs past the end of the reply.
        assert!(classify_inlist::<()>(Ok(&[1, 1, 0, 4, 0, 7, 1, 2]), false).is_err());
    }
}
