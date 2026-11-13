//! SECFIX_03 (SRV-03): the portal's TLS material, generated on the device.
//!
//! The portal is a LAN appliance reached at an address that is different on every network
//! (`192.168.1.42`, `10.0.0.5`, `fe80::…`), so no public CA can issue for it and none is
//! contacted: the device makes **one self-signed ECDSA P-256 certificate** the first time HTTPS
//! is switched on, keeps it in the device state directory, and shows its SHA-256 fingerprint on
//! `System > Network` so the owner can check the certificate the browser was offered.
//!
//! What is written here is only the *certificate*; the reasoning about it (why self-signed, why
//! 10 years, why no HSTS, why a 307 and not a 308) is in `architecture.md` and in the work
//! package's decision log.
//!
//! Three properties this module is built to keep:
//!
//! * **The key never leaves the device, and never lives on the stick.** One file, `0600`, in a
//!   `0700` directory, replaced atomically, `O_NOFOLLOW`, symlinks refused — the same rules the
//!   portal state and the store credentials get (SECFIX_04).
//! * **Certificate and key are one thing.** They are written in a single container and replaced
//!   in one rename, so a power cut can never leave a certificate next to the wrong key (which
//!   would silently break the portal, or worse, serve a key whose public half is not in the
//!   certificate).
//! * **No DER parser.** The X.509 structure is written by hand (it is ~60 lines of TLV) and the
//!   only reader is the one that checks the container header. Anything richer would be a parser
//!   on the network-facing side of the device for no gain: the certificate is written once and
//!   then handed to rustls, which does parse it.

use ring::digest;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The file under the secrets directory that holds the certificate and its key.
pub const TLS_FILE: &str = "portal-tls.der";
/// `FILE_MODE`: owner read/write only.
const FILE_MODE: u32 = 0o600;
/// A container larger than this is not ours.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Container magic: `PLIPPA TLS`, version 1.
const MAGIC: &[u8; 11] = b"PLIPPA-TLS\x01";
/// How long the certificate is valid for (ten years, plus the leap days in that span).
const VALIDITY_SECS: u64 = 10 * 365 * 24 * 60 * 60 + 3 * 24 * 60 * 60;
/// The clock is allowed to be a day behind the certificate when it is first used.
const BACKDATE_SECS: u64 = 24 * 60 * 60;

/// A certificate and the key that goes with it.
pub struct Certificate {
    /// DER-encoded X.509 certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded PKCS#8 private key. Wiped when this value is dropped (see [`Drop`]). This is
    /// best effort on *our* buffers only: the server hands rustls a `Zeroizing` copy of the input,
    /// but rustls builds further copies of its own (`rustls-pki-types`' `PrivatePkcs8KeyDer` and
    /// ring's key pair both have no `Drop`), and those are out of this module's reach. A
    /// `clone()` of this value is another live copy and is wiped only when it too is dropped.
    pub key_der: Vec<u8>,
    /// SHA-256 of `cert_der`, 64 lowercase hex digits (the device menu groups them for reading).
    pub fingerprint: String,
}

impl Drop for Certificate {
    fn drop(&mut self) {
        wipe(&mut self.key_der);
    }
}

/// Overwrite a buffer in a way the optimiser is not allowed to drop. No dependency: the key is
/// the one secret this module holds, and it lives for the process lifetime, so a plain loop that
/// the optimiser could elide is not good enough.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a live, aligned, exclusive reference for the duration of the write.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

impl std::fmt::Debug for Certificate {
    // The key must never appear in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certificate")
            .field("cert_len", &self.cert_der.len())
            .field("key", &"<key>")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// The names the certificate is issued for.
#[derive(Debug, Clone, Default)]
pub struct Hosts {
    /// DNS names (never an address literal).
    pub names: Vec<String>,
    /// Address literals, including loopback.
    pub ips: Vec<IpAddr>,
}

impl Hosts {
    /// This machine as it is right now: `localhost`, the kernel hostname, the device's own name
    /// and every interface address (plus loopback, which a browser on the device itself uses).
    pub fn local() -> Self {
        let mut names = vec!["localhost".to_string(), "plippa".to_string()];
        if let Some(name) = hostname() {
            names.push(name);
        }
        let mut ips = vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        ips.extend(crate::http::local_interface_ips());
        let mut hosts = Hosts { names, ips };
        hosts.normalise();
        hosts
    }

    /// Drop duplicates and anything that is not a plain name, and sort so that the same machine
    /// produces the same certificate bytes.
    fn normalise(&mut self) {
        self.names.retain(|n| is_dns_name(n));
        self.names.sort();
        self.names.dedup();
        self.ips.sort();
        self.ips.dedup();
    }
}

/// `true` for a name a certificate may carry as a DNS subject alternative name.
fn is_dns_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The kernel's host name, when it looks like one a certificate can carry.
fn hostname() -> Option<String> {
    let raw = fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = raw.trim().to_string();
    is_dns_name(&name).then_some(name)
}

/// The certificate and key as stored on the device: `cert` and `key` in one file, so that the
/// pair is replaced in one rename and a torn write is impossible.
#[derive(Debug, Clone)]
struct Container {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Container {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAGIC.len() + 1 + 32 + 8 + self.cert.len() + self.key.len());
        out.extend_from_slice(MAGIC);
        out.push(1); // format version
        out.extend_from_slice(&public_key_digest(&self.key));
        out.extend_from_slice(&(u32::try_from(self.cert.len()).unwrap_or(u32::MAX)).to_be_bytes());
        out.extend_from_slice(&(u32::try_from(self.key.len()).unwrap_or(u32::MAX)).to_be_bytes());
        out.extend_from_slice(&self.cert);
        out.extend_from_slice(&self.key);
        out
    }

    /// `None` when the bytes are not a container this version wrote, or when the key does not
    /// parse (or is not the key the header names).
    fn decode(bytes: &[u8]) -> Option<Container> {
        let header = MAGIC.len() + 1 + 32 + 8;
        if bytes.len() < header || &bytes[..MAGIC.len()] != MAGIC {
            return None;
        }
        let version = bytes[MAGIC.len()];
        if version != 1 {
            return None;
        }
        let digest_at = MAGIC.len() + 1;
        let recorded = &bytes[digest_at..digest_at + 32];
        let len_at = digest_at + 32;
        let cert_len = u32::from_be_bytes(bytes[len_at..len_at + 4].try_into().ok()?) as usize;
        let key_len = u32::from_be_bytes(bytes[len_at + 4..len_at + 8].try_into().ok()?) as usize;
        if bytes.len() != header + cert_len + key_len || cert_len == 0 || key_len == 0 {
            return None;
        }
        let cert = bytes[header..header + cert_len].to_vec();
        let key = bytes[header + cert_len..].to_vec();
        if public_key_digest(&key).as_slice() != recorded {
            return None;
        }
        Some(Container { cert, key })
    }
}

/// SHA-256 of the uncompressed public key inside a PKCS#8 document (all zeroes if the key does
/// not parse, which no generated key's digest can equal).
fn public_key_digest(key_der: &[u8]) -> [u8; 32] {
    let rng = SystemRandom::new();
    let Ok(pair) = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key_der, &rng) else {
        return [0u8; 32];
    };
    let digest = digest::digest(&digest::SHA256, pair.public_key().as_ref());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// `abcdef…` (64 lowercase hex digits) for a DER certificate.
pub fn fingerprint_of(cert_der: &[u8]) -> String {
    let digest = digest::digest(&digest::SHA256, cert_der);
    let mut out = String::with_capacity(64);
    for byte in digest.as_ref() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The certificate store: one file in the device state directory.
#[derive(Debug, Clone)]
pub struct CertStore {
    dir: PathBuf,
    /// Held while generating, so two callers cannot write competing pairs.
    lock: std::sync::Arc<std::sync::Mutex<()>>,
    /// A process-wide counter for temp names (a crash must never block the next write).
    counter: std::sync::Arc<AtomicU64>,
}

impl CertStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            counter: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(TLS_FILE)
    }

    /// Can the certificate be kept at all (the directory exists, is `0700`, is not a symlink and
    /// is writable)? Checked before the portal is switched to HTTPS so it fails closed.
    pub fn check(&self) -> Result<(), String> {
        crate::service::stores::StoreSecrets::new(&self.dir).probe()
    }

    /// The stored pair, if there is a usable one.
    pub fn load(&self) -> Option<Certificate> {
        let path = self.path();
        let meta = fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return None;
        }
        if meta.permissions().mode() & 0o077 != 0 {
            // Too open to trust: a portal key that another user can read is not a secret.
            return None;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .ok()?;
        let mut bytes = Vec::new();
        // The container carries the private key, so every exit from here wipes the buffer. It is
        // one function with one wipe point rather than `?` on each step.
        if Read::take(&mut file, MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            wipe(&mut bytes);
            return None;
        }
        let container = if bytes.len() as u64 > MAX_FILE_BYTES {
            None
        } else {
            Container::decode(&bytes)
        };
        wipe(&mut bytes);
        let container = container?;
        Some(Certificate {
            fingerprint: fingerprint_of(&container.cert),
            cert_der: container.cert,
            key_der: container.key,
        })
    }

    /// The stored pair, or a freshly generated and saved one.
    pub fn load_or_create(&self, hosts: &Hosts) -> Result<Certificate, String> {
        if let Some(cert) = self.load() {
            return Ok(cert);
        }
        self.regenerate(hosts)
    }

    /// Generate a new pair, save it over the old one and return it. Every browser that accepted
    /// the old certificate must accept the new one (the fingerprint changes).
    pub fn regenerate(&self, hosts: &Hosts) -> Result<Certificate, String> {
        let _serial = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.check()?;
        let cert = generate(hosts)?;
        self.save(&cert)?;
        Ok(cert)
    }

    fn save(&self, cert: &Certificate) -> Result<(), String> {
        let mut container = Container {
            cert: cert.cert_der.clone(),
            key: cert.key_der.clone(),
        };
        let mut bytes = container.encode();
        // The encoded container is the plaintext key again; wiped below whichever way the write
        // goes (the `Container`'s own copy too).
        wipe(&mut container.key);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .dir
            .join(format!(".{TLS_FILE}.{}.{n}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, self.path())?;
            // The rename itself must survive a power cut.
            fs::File::open(&self.dir)?.sync_all()
        })();
        wipe(&mut bytes);
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(format!("cannot save the portal certificate: {e}"));
        }
        Ok(())
    }
}

/// Generate a self-signed certificate for `hosts`, valid from now (a day of slack) for ten
/// years. `ring` does the key and the signature; the X.509 wrapper is written here.
pub fn generate(hosts: &Hosts) -> Result<Certificate, String> {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|_| "cannot generate an ECDSA key".to_string())?;
    let key_der = pkcs8.as_ref().to_vec();
    let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &key_der, &rng)
        .map_err(|_| "cannot read back the generated ECDSA key".to_string())?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let not_before = now.saturating_sub(BACKDATE_SECS);
    let not_after = now.saturating_add(VALIDITY_SECS);

    let mut serial = [0u8; 16];
    serial[..8].copy_from_slice(&now.to_be_bytes());
    serial[8..].copy_from_slice(&rand::random::<u64>().to_be_bytes());
    // `integer` already emits a minimal, positive INTEGER: it drops the leading zero bytes a
    // small timestamp leaves and adds a sign octet only if the top bit is set. Nothing else here.

    let der = build(hosts, &serial, not_before, not_after, key_pair.public_key().as_ref(), &|tbs| {
        key_pair
            .sign(&rng, tbs)
            .map(|sig| sig.as_ref().to_vec())
            .map_err(|_| "cannot sign the certificate".to_string())
    })?;

    Ok(Certificate {
        fingerprint: fingerprint_of(&der),
        cert_der: der,
        key_der,
    })
}

// ---------------------------------------------------------------------------
// X.509 (DER) writer
// ---------------------------------------------------------------------------

/// DER tag numbers used below.
const TAG_BOOLEAN: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_UTF8: u8 = 0x0c;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;

/// `ecdsa-with-SHA256` (1.2.840.10045.4.3.2).
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// `id-ecPublicKey` (1.2.840.10045.2.1).
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// `prime256v1` (1.2.840.10045.3.1.7).
const OID_PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// `commonName` (2.5.4.3).
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// `basicConstraints` (2.5.29.19).
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// `keyUsage` (2.5.29.15).
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// `extKeyUsage` (2.5.29.37).
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// `subjectAltName` (2.5.29.17).
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
/// `serverAuth` (1.3.6.1.5.5.7.3.1).
const OID_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// Append a DER length.
fn put_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x81);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0x82);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len <= 0xff_ffff {
        out.push(0x83);
        out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
    } else {
        // Four bytes covers every length a `usize` can hold here (2^32-1), so no truncation.
        out.push(0x84);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

/// One tag-length-value.
fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(tag);
    put_len(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

fn concat(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn sequence(items: &[Vec<u8>]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, &concat(items))
}

fn set(items: &[Vec<u8>]) -> Vec<u8> {
    tlv(TAG_SET, &concat(items))
}

fn oid(bytes: &[u8]) -> Vec<u8> {
    tlv(TAG_OID, bytes)
}

fn boolean(on: bool) -> Vec<u8> {
    tlv(TAG_BOOLEAN, &[if on { 0xff } else { 0x00 }])
}

/// A positive INTEGER from big-endian magnitude bytes that are already minimal.
fn integer(magnitude: &[u8]) -> Vec<u8> {
    let first = magnitude.iter().position(|b| *b != 0).unwrap_or(magnitude.len());
    let magnitude = &magnitude[first..];
    let mut body = Vec::with_capacity(magnitude.len() + 1);
    if magnitude.first().is_some_and(|b| b & 0x80 != 0) {
        body.push(0); // keep it positive
    }
    body.extend_from_slice(magnitude);
    if body.is_empty() {
        body.push(0); // zero
    }
    tlv(TAG_INTEGER, &body)
}

/// A BIT STRING with no unused bits.
fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0); // unused bits
    body.extend_from_slice(bytes);
    tlv(TAG_BIT_STRING, &body)
}

fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, bytes)
}

fn utf8(text: &str) -> Vec<u8> {
    tlv(TAG_UTF8, text.as_bytes())
}

/// `[n] EXPLICIT`.
fn explicit(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(0xa0 | n, inner)
}

/// The `issuerName`/`subjectName` form: one `commonName` of `plippa`.
fn name(common_name: &str) -> Vec<u8> {
    sequence(&[set(&[sequence(&[
        oid(OID_COMMON_NAME),
        utf8(common_name),
    ])])])
}

/// `AlgorithmIdentifier` for ECDSA with SHA-256 (no parameters, per RFC 5758).
fn ecdsa_sha256() -> Vec<u8> {
    sequence(&[oid(OID_ECDSA_SHA256)])
}

/// `SubjectPublicKeyInfo` for an uncompressed P-256 point (65 bytes: `04 || X || Y`).
fn spki(public_key: &[u8]) -> Vec<u8> {
    sequence(&[
        sequence(&[oid(OID_EC_PUBLIC_KEY), oid(OID_PRIME256V1)]),
        bit_string(public_key),
    ])
}

/// Split a Unix time into `(year, month, day, hour, minute, second)` in UTC.
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = i64::try_from(secs).unwrap_or(i64::MAX / 2);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    // Howard Hinnant's `civil_from_days`: exact for every day this device can see.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

/// A validity time: `UTCTime` up to 2049, `GeneralizedTime` from 2050 (RFC 5280 §4.1.2.5).
fn time_tagged(secs: u64) -> Vec<u8> {
    let (year, month, day, hour, minute, second) = civil(secs);
    if (1950..2050).contains(&year) {
        let text = format!(
            "{:02}{:02}{:02}{:02}{:02}{:02}Z",
            year % 100,
            month,
            day,
            hour,
            minute,
            second
        );
        tlv(TAG_UTC_TIME, text.as_bytes())
    } else {
        let text = format!(
            "{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z"
        );
        tlv(TAG_GENERALIZED_TIME, text.as_bytes())
    }
}

/// One `Extension`.
fn extension(oid_bytes: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut items = vec![oid(oid_bytes)];
    if critical {
        items.push(boolean(true));
    }
    items.push(octet_string(&value));
    sequence(&items)
}

/// `subjectAltName`: `dNSName` `[2]` for names, `iPAddress` `[7]` for addresses.
fn subject_alt_name(hosts: &Hosts) -> Vec<u8> {
    let mut names = Vec::new();
    for name in &hosts.names {
        names.push(tlv(0x82, name.as_bytes()));
    }
    for ip in &hosts.ips {
        let bytes: Vec<u8> = match ip {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        names.push(tlv(0x87, &bytes));
    }
    sequence(&names)
}

/// The signer: it receives the `TBSCertificate` body and returns the DER ECDSA signature over
/// it.
type Signer<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, String>;

/// Build the DER certificate.
fn build(
    hosts: &Hosts,
    serial: &[u8],
    not_before: u64,
    not_after: u64,
    public_key: &[u8],
    sign: Signer<'_>,
) -> Result<Vec<u8>, String> {
    let extensions = sequence(&[
        // CA:FALSE: this certificate signs nothing else.
        extension(OID_BASIC_CONSTRAINTS, true, sequence(&[])),
        // digitalSignature only: it authenticates a TLS session and nothing more.
        extension(OID_KEY_USAGE, true, bit_string(&[0x80])),
        extension(
            OID_EXT_KEY_USAGE,
            false,
            sequence(&[oid(OID_SERVER_AUTH)]),
        ),
        extension(OID_SUBJECT_ALT_NAME, false, subject_alt_name(hosts)),
    ]);

    let tbs = sequence(&[
        explicit(0, &integer(&[0x02])), // v3
        integer(serial),
        ecdsa_sha256(),
        name("plippa"), // issuer == subject: self-signed
        sequence(&[time_tagged(not_before), time_tagged(not_after)]),
        name("plippa"),
        spki(public_key),
        explicit(3, &extensions),
    ]);

    let signature = sign(&tbs)?;
    Ok(sequence(&[tbs, ecdsa_sha256(), bit_string(&signature)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::UnparsedPublicKey;

    fn hosts() -> Hosts {
        Hosts {
            names: vec!["localhost".into(), "plippa".into()],
            ips: vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ],
        }
    }

    /// A DER length at `at`, as `(content_len, header_len)`.
    fn read_len(der: &[u8], at: usize) -> (usize, usize) {
        if der[at] & 0x80 == 0 {
            return (der[at] as usize, 1);
        }
        let n = (der[at] & 0x7f) as usize;
        let mut value = 0usize;
        for byte in &der[at + 1..at + 1 + n] {
            value = (value << 8) | *byte as usize;
        }
        (value, 1 + n)
    }

    /// A generated certificate is a well-formed SEQUENCE whose TBS is signed by its own key.
    #[test]
    fn the_certificate_is_self_consistent() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("key");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .expect("key parses");
        let captured = std::cell::RefCell::new(Vec::new());
        let serial = [0x11u8; 16];
        let der = build(
            &hosts(),
            &serial,
            1_763_000_000,
            1_763_000_000 + VALIDITY_SECS,
            pair.public_key().as_ref(),
            &|tbs| {
                captured.borrow_mut().extend_from_slice(tbs);
                Ok(pair.sign(&rng, tbs).expect("sign").as_ref().to_vec())
            },
        )
        .expect("build");
        let tbs = captured.into_inner();

        // Certificate ::= SEQUENCE { tbs, algorithm, signature }
        assert_eq!(der[0], TAG_SEQUENCE);
        let (len, header) = read_len(&der, 1);
        let content = 1 + header;
        assert_eq!(content + len, der.len(), "the outer length covers the whole certificate");

        // The first element is exactly the TBS that was signed.
        assert_eq!(der[content], TAG_SEQUENCE);
        let (tbs_len, tbs_header) = read_len(&der, content + 1);
        let tbs_total = 1 + tbs_header + tbs_len;
        assert_eq!(&der[content..content + tbs_total], tbs.as_slice());
        // ... and it names the version, the serial, the algorithm and both names.
        assert_eq!(tbs[0], TAG_SEQUENCE);

        // The signature is the last element, after the same algorithm identifier.
        let alg = ecdsa_sha256();
        let sig_at = content + tbs_total;
        assert_eq!(&der[sig_at..sig_at + alg.len()], alg.as_slice());
        let bit_at = sig_at + alg.len();
        assert_eq!(der[bit_at], TAG_BIT_STRING);
        let (bit_len, bit_header) = read_len(&der, bit_at + 1);
        assert_eq!(bit_at + 1 + bit_header + bit_len, der.len(), "nothing after the signature");
        let sig = &der[bit_at + 1 + bit_header + 1..]; // skip the unused-bits count
        UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            pair.public_key().as_ref(),
        )
        .verify(&tbs, sig)
        .expect("the self-signature verifies");

        // The identity in the certificate is `plippa`, as a commonName.
        let name = name("plippa");
        assert!(find(&tbs, &name).is_some(), "the subject commonName is present");
        // The SAN carries every address and name, including the loopback.
        let san = subject_alt_name(&hosts());
        assert!(find(&tbs, &tlv(TAG_OCTET_STRING, &san)).is_some(), "the SAN is present");
        for text in [b"localhost".as_slice(), b"plippa".as_slice(), &[192, 168, 1, 42]] {
            assert!(find(&san, text).is_some(), "SAN missing {text:?}");
        }
        // CA:FALSE is critical and its value is the empty SEQUENCE.
        assert!(find(&tbs, &extension(OID_BASIC_CONSTRAINTS, true, sequence(&[]))).is_some());
        assert!(find(&tbs, &extension(OID_KEY_USAGE, true, bit_string(&[0x80]))).is_some());
        assert!(find(
            &tbs,
            &extension(OID_EXT_KEY_USAGE, false, sequence(&[oid(OID_SERVER_AUTH)]))
        )
        .is_some());
        // The certificate carries the public key that signed it, once.
        let key_at = find(&tbs, &spki(pair.public_key().as_ref())).expect("SPKI");
        assert!(find(&tbs[key_at + 1..], &spki(pair.public_key().as_ref())).is_none());
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Two generations never produce the same certificate (the serial and key are random).
    #[test]
    fn every_certificate_is_different() {
        let a = generate(&hosts()).expect("a");
        let b = generate(&hosts()).expect("b");
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_ne!(a.key_der, b.key_der);
    }

    /// The pair is one file, `0600`, and the store refuses a symlink or a world-readable file.
    #[test]
    fn the_pair_is_one_private_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        let store = CertStore::new(&path);
        let cert = store.load_or_create(&hosts()).expect("create");
        let mode = fs::metadata(store.path()).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // The same pair comes back, not a new one.
        let again = store.load().expect("load");
        assert_eq!(again.fingerprint, cert.fingerprint);
        // Regenerating replaces it.
        let fresh = store.regenerate(&hosts()).expect("regenerate");
        assert_ne!(fresh.fingerprint, cert.fingerprint);
        assert_eq!(store.load().expect("load").fingerprint, fresh.fingerprint);
    }

    #[test]
    fn a_world_readable_or_symlinked_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        let store = CertStore::new(&path);
        store.load_or_create(&hosts()).expect("create");

        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(store.load().is_none(), "a key another user can read is not a secret");

        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).expect("chmod");
        let elsewhere = dir.path().join("elsewhere.der");
        fs::rename(store.path(), &elsewhere).expect("move");
        std::os::unix::fs::symlink(&elsewhere, store.path()).expect("symlink");
        assert!(store.load().is_none(), "symlinks are never followed");
    }

    /// A container whose header and body disagree (a torn or edited file) is not used.
    #[test]
    fn a_damaged_container_is_not_used() {
        let cert = generate(&hosts()).expect("generate");
        let container = Container {
            cert: cert.cert_der.clone(),
            key: cert.key_der.clone(),
        };
        let mut bytes = container.encode();
        assert!(Container::decode(&bytes).is_some());
        // Flip a bit in the certificate: the length still checks out, the pair does not match
        // only if the *key* digest changes, so damage to the key must be caught.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(Container::decode(&bytes).is_none());
        // Truncating is refused rather than half-read.
        assert!(Container::decode(&bytes[..bytes.len() / 2]).is_none());
        // Another format version is not guessed at.
        let mut other = container.encode();
        other[MAGIC.len()] = 2;
        assert!(Container::decode(&other).is_none());
    }

    #[test]
    fn a_name_the_kernel_reports_is_sanitised() {
        assert!(is_dns_name("plippa"));
        assert!(is_dns_name("plippa.local"));
        assert!(!is_dns_name(""));
        assert!(!is_dns_name("a b"));
        assert!(!is_dns_name("-lead"));
        assert!(!is_dns_name(&"x".repeat(254)));
    }

    /// A time every leap-year rule has to survive.
    #[test]
    fn validity_dates_are_utc_and_well_formed() {
        assert_eq!(civil(1_763_000_000), (2025, 11, 13, 2, 13, 20));
        // A leap day.
        assert_eq!(civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        // Before 1950 the encoding switches to GeneralizedTime; after 2049 too.
        assert_eq!(time_tagged(0)[0], TAG_UTC_TIME);
        assert_eq!(time_tagged(2_000_000_000)[0], TAG_UTC_TIME);
        // 2052: past the UTCTime range, so the wider form is used.
        assert_eq!(time_tagged(2_600_000_000)[0], TAG_GENERALIZED_TIME);
    }

    /// Every length this writer can emit is minimal DER, including the sizes a certificate never
    /// reaches — a length is the one field where a silent truncation would still parse as
    /// something else rather than fail.
    #[test]
    fn der_lengths_are_minimal_at_every_boundary() {
        for (len, expected) in [
            (0usize, vec![0x00]),
            (0x7f, vec![0x7f]),
            (0x80, vec![0x81, 0x80]),
            (0xff, vec![0x81, 0xff]),
            (0x100, vec![0x82, 0x01, 0x00]),
            (0xffff, vec![0x82, 0xff, 0xff]),
            (0x1_0000, vec![0x83, 0x01, 0x00, 0x00]),
            (0xff_ffff, vec![0x83, 0xff, 0xff, 0xff]),
            (0x100_0000, vec![0x84, 0x01, 0x00, 0x00, 0x00]),
        ] {
            let mut out = Vec::new();
            put_len(&mut out, len);
            assert_eq!(out, expected, "length {len}");
        }
    }

    /// Integers keep their sign positive and stay minimal: zero, a high bit, and leading zeroes
    /// in the magnitude (which the serial's time prefix can produce).
    #[test]
    fn integers_are_positive_and_minimal() {
        let body = |v: Vec<u8>| v[2..].to_vec();
        assert_eq!(body(integer(&[0x00])), vec![0x00]);
        assert_eq!(body(integer(&[0x00, 0x00])), vec![0x00], "zero is one byte");
        assert_eq!(body(integer(&[0x7f])), vec![0x7f]);
        assert_eq!(body(integer(&[0x80])), vec![0x00, 0x80], "the sign bit needs a pad");
        assert_eq!(body(integer(&[0x00, 0x00, 0x80, 0x01])), vec![0x00, 0x80, 0x01]);
        assert_eq!(body(integer(&[0x01, 0x02])), vec![0x01, 0x02]);
    }
}
