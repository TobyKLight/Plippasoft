// see nfc_test_PN532/pinout.md for hardware setup

mod rpi_i2c;

use core::convert::Infallible;
use linux_embedded_hal::nb;
use linux_embedded_hal::{CountDown as HalCountDown, SysTimer};
use pn532::requests::{Command, SAMMode};
use pn532::{CountDown, IntoDuration, Pn532, Request};
use rpi_i2c::{Pn532I2c, RpiI2cInterface};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

/// Wraps linux-embedded-hal's timer for the pn532 driver's CountDown trait.
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
    atqa: [u8; 2],
    sak: u8,
    uid: Vec<u8>,
}

type Pn532Driver = Pn532<RpiI2cInterface, Pn532Timer, 64>;

const HLTA: Request<2> = Request::new(Command::InCommunicateThru, [0x50, 0x00]);
const WUPA: Request<1> = Request::new(Command::InCommunicateThru, [0x52]);

fn interrupted() -> bool {
    !RUNNING.load(Ordering::SeqCst)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::SeqCst);
    })?;

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

    // Let the PN532 finish boot after power-up.
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
                 hints: power-cycle PN532 if the bus misbehaves; `i2ctransfer -y 1 r1@0x24` \
                 may need 2–3 tries after power-up (EIO is normal); see pinout.md"
            )
        })?;

    let fw = pn532
        .process(&Request::GET_FIRMWARE_VERSION, 4, 500.ms())
        .map_err(|e| format!("firmware version read failed: {e:?}"))?;
    println!(
        "PN532 connected, firmware IC=0x{:02X} ver={}.{} support=0x{:02X}",
        fw[0], fw[1], fw[2], fw[3]
    );

    println!("Polling for NFC tags every 100ms (Ctrl-C to exit)…");

    let mut last_uid: Option<Vec<u8>> = None;

    while RUNNING.load(Ordering::SeqCst) {
        match poll_tag(&mut pn532) {
            Some(tag) => {
                if last_uid.as_ref() == Some(&tag.uid) {
                    println!("Tag UID {} read again", format_uid(&tag.uid));
                } else if interrupted() {
                    break;
                } else {
                    print_tag_details(&tag);
                    if interrupted() {
                        break;
                    }
                    read_and_print_tag_memory(&mut pn532, tag.sak);
                    if interrupted() {
                        break;
                    }
                    println!("Attempting HLTA…");
                    match hlta(&mut pn532) {
                        Ok(()) => println!("HLTA: ok (PICC should be in HALT)"),
                        Err(e) => println!("HLTA: failed ({e:?})"),
                    }
                    last_uid = Some(tag.uid);
                }
            }
            None => last_uid = None,
        }

        thread::sleep(Duration::from_millis(100));
    }

    println!("Shutting down…");
    if let Some(tag) = poll_tag(&mut pn532) {
        println!(
            "Tag still in field at exit, sending HLTA for UID {}…",
            format_uid(&tag.uid)
        );
        let _ = hlta(&mut pn532);
    }

    Ok(())
}

/// InListPassiveTarget for idle tags; WUPA + retry for tags already in HALT (e.g. after HLTA).
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
    if resp.first()? == &0 {
        return None;
    }
    if resp.len() < 6 {
        return None;
    }

    let atqa = [resp[2], resp[3]];
    let sak = resp[4];
    let uid_len = resp[5] as usize;
    let uid_end = 6 + uid_len;
    let uid = resp.get(6..uid_end)?.to_vec();

    Some(TagInfo { atqa, sak, uid })
}

fn hlta(pn532: &mut Pn532Driver) -> Result<(), pn532::Error<linux_embedded_hal::I2CError>> {
    pn532.process_no_response(&HLTA, 50.ms())
}

fn format_uid(uid: &[u8]) -> String {
    uid.iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

fn uid_cascade_label(uid_len: usize) -> &'static str {
    match uid_len {
        4 => "single (4-byte)",
        7 => "double (7-byte)",
        10 => "triple (10-byte)",
        _ => "unknown cascade",
    }
}

fn sak_type_name(sak: u8) -> &'static str {
    match sak & 0x7F {
        0x04 => "NotComplete (UID incomplete)",
        0x09 => "MIFARE Mini",
        0x08 => "MIFARE Classic 1K",
        0x18 => "MIFARE Classic 4K",
        0x00 => "MIFARE Ultralight",
        0x10 | 0x11 => "MIFARE Plus",
        0x01 => "TNP3XXX",
        0x20 => "ISO/IEC 14443-4",
        0x40 => "ISO/IEC 18092 (NFC)",
        _ => "Unknown",
    }
}

fn print_tag_details(tag: &TagInfo) {
    println!("--- tag detected ---");
    println!("  ATQA: {:02X} {:02X}", tag.atqa[0], tag.atqa[1]);
    println!(
        "    bit frame anticollision: {}",
        if tag.atqa[0] & 0x1F != 0 {
            "proprietary / non-standard"
        } else {
            "standard"
        }
    );
    println!(
        "    UID size hint (bits 7-6 of ATQA byte 0): {}",
        match (tag.atqa[0] >> 6) & 0x03 {
            0b00 => "single size",
            0b01 => "double size",
            0b10 => "triple size",
            _ => "RFU",
        }
    );
    println!("  cascade: {}", uid_cascade_label(tag.uid.len()));
    println!(
        "  UID ({} bytes): {}",
        tag.uid.len(),
        format_uid(&tag.uid)
    );
    println!(
        "  UID XOR (BCC check): 0x{:02X}",
        tag.uid.iter().fold(0u8, |a, b| a ^ b)
    );
    println!("  SAK: 0x{:02X}", tag.sak);
    println!("    type: {}", sak_type_name(tag.sak));
    println!(
        "    UID complete: {}",
        if tag.sak & (1 << 2) == 0 {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "    ISO/IEC 14443-4 compliant: {}",
        if tag.sak & (1 << 5) != 0 {
            "yes"
        } else {
            "no"
        }
    );
    println!("--------------------");
}

fn is_mifare_classic_1k(sak: u8) -> bool {
    (sak & 0x7F) == 0x08
}

fn read_and_print_tag_memory(pn532: &mut Pn532Driver, sak: u8) {
    println!("  tag memory (SAK 0x{sak:02X}):");

    if is_mifare_classic_1k(sak) {
        println!("    no data read, note mifare classic 1K not supported");
        return;
    }

    let mut payload = Vec::new();
    println!("    Type 2 read (NTAG/Ultralight)…");
    read_type2_memory(pn532, &mut payload);

    if payload.is_empty() {
        println!("    no data read, note mifare classic 1K not supported");
        return;
    }

    if let Some(content) = extract_ndef_content(&payload) {
        println!("    data (NDEF): {content}");
    } else if let Some(text) = extract_printable_utf8(&payload) {
        println!("    data (text): {text}");
    }

    let hex_limit = if payload.len() > 64 { 192 } else { 128 };
    println!(
        "    combined hex ({} bytes): {}",
        payload.len(),
        hex_preview(&payload, hex_limit)
    );
}

const TYPE2_READ_CHUNK: usize = 16;
const TYPE2_USER_START_PAGE: u8 = 4;
const TYPE2_CC_PAGE: u8 = 3;
/// Highest page we READ (NTAG215/216 have more; CC caps actual NDEF length).
const TYPE2_MAX_START_PAGE: u8 = 128;

#[derive(Debug)]
#[allow(dead_code)]
enum PageReadError {
    Pn532(pn532::Error<linux_embedded_hal::I2CError>),
    Status(u8),
    Short(usize),
}

/// Type 2 READ at page `p` returns 16 bytes (pages p..=p+3). NDEF TLV starts at page 4.
fn read_type2_memory(pn532: &mut Pn532Driver, out: &mut Vec<u8>) {
    if interrupted() {
        return;
    }

    let cc_max = match read_type2_page(pn532, TYPE2_CC_PAGE) {
        Ok(cc) => {
            let max = type2_cc_max_ndef_bytes(&cc);
            println!("    CC page {TYPE2_CC_PAGE}: ok (max NDEF ~{max} bytes)");
            max
        }
        Err(e) => {
            println!("    CC page {TYPE2_CC_PAGE}: read failed ({e:?}), assuming 504 byte limit");
            504
        }
    };

    let first = match read_type2_page(pn532, TYPE2_USER_START_PAGE) {
        Ok(block) => block,
        Err(e) => {
            println!("    page {TYPE2_USER_START_PAGE}: read failed ({e:?})");
            return;
        }
    };
    println!(
        "    page {TYPE2_USER_START_PAGE}–{}: ok",
        TYPE2_USER_START_PAGE + 3
    );

    let (read_pages, byte_span) = type2_read_plan(&first, cc_max);
    let base = TYPE2_USER_START_PAGE as usize * 4;
    let mut memory = vec![0u8; base + byte_span];
    memory[base..base + TYPE2_READ_CHUNK].copy_from_slice(&first);

    let mut chunks_ok = 1u32;
    for start_page in read_pages.into_iter().skip(1) {
        if interrupted() {
            println!("    (Type 2 read interrupted)");
            break;
        }
        match read_type2_page(pn532, start_page) {
            Ok(block) => {
                println!("    page {start_page}–{}: ok", start_page + 3);
                let off = start_page as usize * 4;
                if off + TYPE2_READ_CHUNK <= memory.len() {
                    memory[off..off + TYPE2_READ_CHUNK].copy_from_slice(&block);
                }
                chunks_ok += 1;
            }
            Err(e) => println!("    page {start_page}: read failed ({e:?})"),
        }
    }

    let bytes_read = byte_span.min(memory.len().saturating_sub(base));
    println!("    read {bytes_read} bytes in {chunks_ok} chunk(s) (TLV span {byte_span})");
    out.extend_from_slice(&memory[base..base + byte_span.min(memory.len() - base)]);
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
        .map_err(PageReadError::Pn532)?;

    if result[0] != 0x00 {
        return Err(PageReadError::Status(result[0]));
    }
    if result.len() < 1 + TYPE2_READ_CHUNK {
        return Err(PageReadError::Short(result.len()));
    }

    let mut block = [0u8; TYPE2_READ_CHUNK];
    block.copy_from_slice(&result[1..1 + TYPE2_READ_CHUNK]);
    Ok(block)
}

/// NFC Forum T2T capability container (page 3): max NDEF size in bytes.
fn type2_cc_max_ndef_bytes(cc_page: &[u8; TYPE2_READ_CHUNK]) -> usize {
    if cc_page[0] == 0xE1 && (cc_page[1] == 0x10 || cc_page[1] == 0x12) {
        cc_page[2] as usize * 8
    } else {
        504
    }
}

/// Total byte size of the NDEF TLV at the start of the user area (type + length + message).
fn ndef_tlv_total_bytes(block4_7: &[u8; TYPE2_READ_CHUNK]) -> Option<usize> {
    if block4_7[0] != 0x03 {
        return None;
    }
    if block4_7[1] == 0xFF {
        let msg_len = u16::from_be_bytes([block4_7[2], block4_7[3]]) as usize;
        Some(4 + msg_len)
    } else {
        Some(2 + block4_7[1] as usize)
    }
}

/// Which start pages to READ and how many user bytes to keep (from TLV + CC cap).
fn type2_read_plan(block4_7: &[u8; TYPE2_READ_CHUNK], cc_max_ndef: usize) -> (Vec<u8>, usize) {
    let mut tlv_bytes = ndef_tlv_total_bytes(block4_7).unwrap_or(TYPE2_READ_CHUNK);
    tlv_bytes = tlv_bytes.div_ceil(TYPE2_READ_CHUNK) * TYPE2_READ_CHUNK;
    let byte_span = tlv_bytes.min(cc_max_ndef).max(TYPE2_READ_CHUNK);

    let mut pages = vec![TYPE2_USER_START_PAGE];
    if byte_span <= TYPE2_READ_CHUNK {
        return (pages, byte_span);
    }

    let mut remaining = byte_span - TYPE2_READ_CHUNK;
    let mut start_page = TYPE2_USER_START_PAGE + 4;
    while remaining > 0 && start_page <= TYPE2_MAX_START_PAGE {
        pages.push(start_page);
        remaining = remaining.saturating_sub(TYPE2_READ_CHUNK);
        start_page += 4;
    }
    (pages, byte_span)
}

fn extract_ndef_content(data: &[u8]) -> Option<String> {
    if let Some(s) = extract_ndef_tlv_records(data) {
        return Some(s);
    }
    if let Some(s) = scan_ndef_uri_record(data) {
        return Some(s);
    }
    scan_ndef_text_payload(data)
}

fn scan_ndef_text_payload(data: &[u8]) -> Option<String> {
    for i in 0..data.len().saturating_sub(5) {
        let status = data[i];
        let lang_len = (status & 0x3F) as usize;
        if lang_len > 8 || status & 0x80 != 0 {
            continue;
        }
        if let Some(payload) = data.get(i..) {
            if let Some(text) = parse_ndef_text_payload(payload) {
                if text.chars().any(|c| c.is_ascii_alphabetic()) && text.len() >= 3 {
                    return Some(text);
                }
            }
        }
    }

    for i in 0..data.len().saturating_sub(6) {
        if data[i] == 0xD1 && data.get(i + 1) == Some(&0x01) && data.get(i + 3) == Some(&b'T') {
            let payload_len = data[i + 2] as usize;
            let payload = data.get(i + 4..i + 4 + payload_len)?;
            if let Some(text) = parse_ndef_text_payload(payload) {
                return Some(text);
            }
        }
    }
    None
}

fn extract_ndef_tlv_records(data: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < data.len() {
        let tlv_type = *data.get(i)?;
        if tlv_type == 0x00 {
            i += 1;
            continue;
        }
        if tlv_type == 0xFE {
            break;
        }
        let len = if tlv_type == 0x03 {
            let len_byte = *data.get(i + 1)?;
            if len_byte == 0xFF {
                let len = u16::from_be_bytes([*data.get(i + 2)?, *data.get(i + 3)?]) as usize;
                let msg = data.get(i + 4..i + 4 + len)?;
                if let Some(text) = parse_ndef_message(msg) {
                    return Some(text);
                }
                i += 4 + len;
                continue;
            } else {
                len_byte as usize
            }
        } else {
            let len = *data.get(i + 1)? as usize;
            i += 2 + len;
            continue;
        };

        let msg = data.get(i + 2..i + 2 + len)?;
        if let Some(text) = parse_ndef_message(msg) {
            return Some(text);
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
            let len = *message.get(off)? as usize;
            off += 1;
            len
        } else {
            let len = u32::from_be_bytes([
                0,
                *message.get(off)?,
                *message.get(off + 1)?,
                *message.get(off + 2)?,
            ]) as usize;
            off += 3;
            len
        };

        let id_len = if il { *message.get(off)? as usize } else { 0 };
        if il {
            off += 1;
        }

        let record_type = message.get(off..off + type_len)?;
        off += type_len;
        let id = message.get(off..off + id_len)?;
        off += id_len;
        let payload = message.get(off..off + payload_len)?;
        off += payload_len;

        if tnf == 0x01 && record_type == b"U" {
            if let Some(url) = parse_ndef_uri_payload(payload) {
                return Some(url);
            }
        }
        if tnf == 0x01 && record_type == b"T" {
            if let Some(text) = parse_ndef_text_payload(payload) {
                return Some(text);
            }
        }

        let _ = id;
    }
    None
}

fn scan_ndef_uri_record(data: &[u8]) -> Option<String> {
    for i in 0..data.len().saturating_sub(6) {
        if data[i] == 0xD1 && data.get(i + 1) == Some(&0x01) && data.get(i + 3) == Some(&b'U') {
            let payload_len = data[i + 2] as usize;
            let payload = data.get(i + 4..i + 4 + payload_len)?;
            if let Some(url) = parse_ndef_uri_payload(payload) {
                return Some(url);
            }
        }
    }
    None
}

fn parse_ndef_text_payload(payload: &[u8]) -> Option<String> {
    let status = *payload.first()?;
    let lang_len = (status & 0x3F) as usize;
    let text_start = 1 + lang_len;
    let text_bytes = payload.get(text_start..)?;
    if status & 0x80 != 0 {
        return None;
    }
    String::from_utf8(text_bytes.to_vec()).ok()
}

fn parse_ndef_uri_payload(payload: &[u8]) -> Option<String> {
    let prefix_code = *payload.first()?;
    let uri_start = payload.get(1..)?;
    let prefix = match prefix_code {
        0x00 => "",
        0x01 => "http://www.",
        0x02 => "https://www.",
        0x03 => "http://",
        0x04 => "https://",
        0x05 => "tel:",
        0x06 => "mailto:",
        _ => "",
    };
    String::from_utf8(uri_start.to_vec())
        .ok()
        .map(|rest| format!("{prefix}{rest}"))
}

fn extract_printable_utf8(data: &[u8]) -> Option<String> {
    let mut best = String::new();
    let mut current = String::new();

    for &b in data {
        let c = b as char;
        if b >= 0x20 && b < 0x7F || b == b'\n' || b == b'\r' || b == b'\t' {
            current.push(c);
        } else if !current.is_empty() {
            if current.len() > best.len() {
                best = current.clone();
            }
            current.clear();
        }
    }
    if current.len() > best.len() {
        best = current;
    }

    let mut trimmed = best.trim().to_string();
    if trimmed.len() > 4 {
        let rest = trimmed.get(2..)?;
        if trimmed.as_bytes()[0].is_ascii_lowercase()
            && trimmed.as_bytes()[1].is_ascii_lowercase()
            && matches!(
                rest.chars().next(),
                Some(c) if c.is_ascii_uppercase() || c.is_ascii_digit()
            )
        {
            trimmed = rest.to_string();
        }
    }
    if trimmed.len() >= 3 {
        Some(trimmed)
    } else {
        None
    }
}

fn hex_preview(data: &[u8], max: usize) -> String {
    let take = data.len().min(max);
    let hex: String = data[..take]
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ");
    if data.len() > max {
        format!("{hex} … (+{} bytes)", data.len() - max)
    } else {
        hex
    }
}
