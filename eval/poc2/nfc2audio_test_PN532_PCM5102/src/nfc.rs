use crate::ndef_parse::media_key_from_ndef;
use crate::rpi_i2c::{Pn532I2c, RpiI2cInterface};
use core::convert::Infallible;
use linux_embedded_hal::nb;
use linux_embedded_hal::{CountDown as HalCountDown, SysTimer};
use pn532::requests::{Command, SAMMode};
use pn532::{CountDown, IntoDuration, Pn532, Request};
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

struct TagInfo {
    sak: u8,
    uid: Vec<u8>,
}

type Pn532Driver = Pn532<RpiI2cInterface, Pn532Timer, 64>;

pub struct TagUid(Vec<u8>);

impl TagUid {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub struct NfcReader {
    pn532: Pn532Driver,
    last_tag: Option<TagInfo>,
}

impl NfcReader {
    pub fn open() -> Result<Self, String> {
        let i2c = Pn532I2c::open("/dev/i2c-1").map_err(|e| {
            format!(
                "could not open /dev/i2c-1 ({e:?}) — enable I2C in raspi-config and confirm \
                 `i2cdetect -y 1` shows 24"
            )
        })?;
        let mut pn532 = Pn532::new(
            RpiI2cInterface { i2c },
            Pn532Timer(SysTimer::new()),
        );

        thread::sleep(Duration::from_millis(100));

        pn532
            .process(
                &Request::sam_configuration(SAMMode::Normal, false),
                0,
                1000.ms(),
            )
            .map_err(|e| {
                format!(
                    "SAM configuration failed: {e:?}\n\
                     hints: power-cycle PN532 if the bus misbehaves; see pinout.md"
                )
            })?;

        let fw = pn532
            .process(&Request::GET_FIRMWARE_VERSION, 4, 500.ms())
            .map_err(|e| format!("firmware version read failed: {e:?}"))?;
        println!(
            "PN532 connected, firmware IC=0x{:02X} ver={}.{} support=0x{:02X}",
            fw[0], fw[1], fw[2], fw[3]
        );

        Ok(Self {
            pn532,
            last_tag: None,
        })
    }

    pub fn poll_uid(&mut self) -> Option<TagUid> {
        match poll_tag(&mut self.pn532) {
            Some(tag) => {
                let uid = TagUid(tag.uid.clone());
                self.last_tag = Some(tag);
                Some(uid)
            }
            None => {
                self.last_tag = None;
                None
            }
        }
    }

    pub fn read_media_key(&mut self, uid: &TagUid) -> Option<String> {
        let tag = self.last_tag.as_ref()?;
        if tag.uid != uid.as_bytes() {
            return None;
        }

        if is_mifare_classic_1k(tag.sak) {
            println!("MIFARE Classic 1K tag — not supported");
            let _ = hlta(&mut self.pn532);
            return None;
        }

        let mut payload = Vec::new();
        read_type2_memory(&mut self.pn532, &mut payload);
        let _ = hlta(&mut self.pn532);

        let raw = extract_ndef_content(&payload)?;
        let key = media_key_from_ndef(&raw);
        if key.is_empty() {
            None
        } else {
            println!("Tag payload → key '{key}'");
            Some(key)
        }
    }
}

pub fn format_uid(uid: &[u8]) -> String {
    uid.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn poll_tag(pn532: &mut Pn532Driver) -> Option<TagInfo> {
    match inlist_tag(pn532) {
        Some(tag) => Some(tag),
        None => {
            let _ = pn532.process_no_response(&WUPA, 50.ms());
            inlist_tag(pn532)
        }
    }
}

fn inlist_tag(pn532: &mut Pn532Driver) -> Option<TagInfo> {
    let resp = pn532
        .process(&Request::INLIST_ONE_ISO_A_TARGET, 20, 500.ms())
        .ok()?;
    parse_inlist_response(resp)
}

fn parse_inlist_response(resp: &[u8]) -> Option<TagInfo> {
    if resp.first()? == &0 || resp.len() < 6 {
        return None;
    }
    let sak = resp[4];
    let uid_len = resp[5] as usize;
    let uid = resp.get(6..6 + uid_len)?.to_vec();
    Some(TagInfo { sak, uid })
}

fn hlta(pn532: &mut Pn532Driver) -> Result<(), pn532::Error<linux_embedded_hal::I2CError>> {
    pn532.process_no_response(&HLTA, 50.ms())
}

fn is_mifare_classic_1k(sak: u8) -> bool {
    (sak & 0x7F) == 0x08
}

fn read_type2_memory(pn532: &mut Pn532Driver, out: &mut Vec<u8>) {
    let cc_max = read_type2_page(pn532, TYPE2_CC_PAGE)
        .map(|cc| type2_cc_max_ndef_bytes(&cc))
        .unwrap_or(504);

    let first = match read_type2_page(pn532, TYPE2_USER_START_PAGE) {
        Ok(b) => b,
        Err(_) => return,
    };

    let (read_pages, byte_span) = type2_read_plan(&first, cc_max);
    let base = TYPE2_USER_START_PAGE as usize * 4;
    let mut memory = vec![0u8; base + byte_span];
    memory[base..base + TYPE2_READ_CHUNK].copy_from_slice(&first);

    for start_page in read_pages.into_iter().skip(1) {
        if let Ok(block) = read_type2_page(pn532, start_page) {
            let off = start_page as usize * 4;
            if off + TYPE2_READ_CHUNK <= memory.len() {
                memory[off..off + TYPE2_READ_CHUNK].copy_from_slice(&block);
            }
        }
    }

    out.extend_from_slice(&memory[base..base + byte_span.min(memory.len() - base)]);
}

enum PageReadError {
    Failed,
}

fn read_type2_page(
    pn532: &mut Pn532Driver,
    page: u8,
) -> Result<[u8; TYPE2_READ_CHUNK], PageReadError> {
    match read_type2_page_once(pn532, page) {
        Ok(b) => Ok(b),
        Err(first_err) => {
            thread::sleep(Duration::from_micros(200));
            read_type2_page_once(pn532, page).or(Err(first_err))
        }
    }
}

fn read_type2_page_once(
    pn532: &mut Pn532Driver,
    page: u8,
) -> Result<[u8; TYPE2_READ_CHUNK], PageReadError> {
    let result = pn532
        .process(&Request::ntag_read(page), 17, 50.ms())
        .map_err(|_| PageReadError::Failed)?;

    if result[0] != 0x00 || result.len() < 1 + TYPE2_READ_CHUNK {
        return Err(PageReadError::Failed);
    }

    let mut block = [0u8; TYPE2_READ_CHUNK];
    block.copy_from_slice(&result[1..1 + TYPE2_READ_CHUNK]);
    Ok(block)
}

fn type2_cc_max_ndef_bytes(cc: &[u8; TYPE2_READ_CHUNK]) -> usize {
    if cc[0] == 0xE1 && (cc[1] == 0x10 || cc[1] == 0x12) {
        cc[2] as usize * 8
    } else {
        504
    }
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

fn extract_ndef_content(data: &[u8]) -> Option<String> {
    extract_ndef_tlv_records(data)
        .or_else(|| scan_ndef_uri_record(data))
        .or_else(|| scan_ndef_text_payload(data))
}

fn extract_ndef_tlv_records(data: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < data.len() {
        let tlv = *data.get(i)?;
        if tlv == 0x00 {
            i += 1;
            continue;
        }
        if tlv == 0xFE {
            break;
        }
        let len = if tlv == 0x03 {
            let lb = *data.get(i + 1)?;
            if lb == 0xFF {
                let len = u16::from_be_bytes([*data.get(i + 2)?, *data.get(i + 3)?]) as usize;
                let msg = data.get(i + 4..i + 4 + len)?;
                if let Some(t) = parse_ndef_message(msg) {
                    return Some(t);
                }
                i += 4 + len;
                continue;
            } else {
                lb as usize
            }
        } else {
            let len = *data.get(i + 1)? as usize;
            i += 2 + len;
            continue;
        };
        let msg = data.get(i + 2..i + 2 + len)?;
        if let Some(t) = parse_ndef_message(msg) {
            return Some(t);
        }
        i += 2 + len;
    }
    None
}

fn parse_ndef_message(message: &[u8]) -> Option<String> {
    let mut off = 0;
    while off < message.len() {
        let header = *message.get(off)?;
        let sr = header & 0x10 != 0;
        let il = header & 0x08 != 0;
        let tnf = header & 0x07;
        off += 1;
        let type_len = *message.get(off)? as usize;
        off += 1;
        let payload_len = if sr {
            let l = *message.get(off)? as usize;
            off += 1;
            l
        } else {
            let l = u32::from_be_bytes([
                0,
                *message.get(off)?,
                *message.get(off + 1)?,
                *message.get(off + 2)?,
            ]) as usize;
            off += 3;
            l
        };
        let id_len = if il {
            let l = *message.get(off)? as usize;
            off += 1;
            l
        } else {
            0
        };
        let rt = message.get(off..off + type_len)?;
        off += type_len;
        off += id_len;
        let payload = message.get(off..off + payload_len)?;
        off += payload_len;
        if tnf == 0x01 && rt == b"U" {
            if let Some(u) = parse_ndef_uri_payload(payload) {
                return Some(u);
            }
        }
        if tnf == 0x01 && rt == b"T" {
            if let Some(t) = parse_ndef_text_payload(payload) {
                return Some(t);
            }
        }
    }
    None
}

fn scan_ndef_uri_record(data: &[u8]) -> Option<String> {
    for i in 0..data.len().saturating_sub(6) {
        if data[i] == 0xD1 && data.get(i + 1) == Some(&0x01) && data.get(i + 3) == Some(&b'U') {
            let n = data[i + 2] as usize;
            let p = data.get(i + 4..i + 4 + n)?;
            if let Some(u) = parse_ndef_uri_payload(p) {
                return Some(u);
            }
        }
    }
    None
}

fn scan_ndef_text_payload(data: &[u8]) -> Option<String> {
    for i in 0..data.len().saturating_sub(6) {
        if data[i] == 0xD1 && data.get(i + 1) == Some(&0x01) && data.get(i + 3) == Some(&b'T') {
            let n = data[i + 2] as usize;
            let p = data.get(i + 4..i + 4 + n)?;
            if let Some(t) = parse_ndef_text_payload(p) {
                return Some(t);
            }
        }
    }
    None
}

fn parse_ndef_text_payload(payload: &[u8]) -> Option<String> {
    let status = *payload.first()?;
    let lang = (status & 0x3F) as usize;
    if status & 0x80 != 0 {
        return None;
    }
    String::from_utf8(payload.get(1 + lang..)?.to_vec()).ok()
}

fn parse_ndef_uri_payload(payload: &[u8]) -> Option<String> {
    let code = *payload.first()?;
    let rest = payload.get(1..)?;
    let prefix = match code {
        0x00 => "",
        0x01 => "http://www.",
        0x02 => "https://www.",
        0x03 => "http://",
        0x04 => "https://",
        _ => "",
    };
    String::from_utf8(rest.to_vec())
        .ok()
        .map(|r| format!("{prefix}{r}"))
}
