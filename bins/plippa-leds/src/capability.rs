//! The helper's capability self-check (SYS-02).
//!
//! `rs_ws281x` maps `/dev/mem`; the udev rule admits the *open*, and the kernel's
//! `read`/`write`/`mmap` paths require `CAP_SYS_RAWIO` **regardless of the file's group/mode**.
//! The helper therefore refuses to
//! run without it: a strip that cannot be driven is better than a silent per-frame failure
//! loop. The check reads `CapEff` from `/proc/self/status` (bit 17 is `CAP_SYS_RAWIO`).
//!
//! A build without the `ws281x` feature has no hardware backend, so the check does not apply
//! there (dev machines and CI run the helper with a null sink).

/// `CAP_SYS_RAWIO` is bit 17 of a capability mask (the kernel numbers capabilities from 0).
pub const CAP_SYS_RAWIO: u32 = 17;

/// Exit status when the capability is missing. Distinct from 1 so the journal and the
/// launcher's crash budget tell it apart from a programming error (`EX_CONFIG` in sysexits).
pub const EXIT_NO_CAP: i32 = EXIT_CONFIG;

/// `EX_CONFIG`: a configuration problem (board profile, missing capability), not a crash.
pub const EXIT_CONFIG: i32 = 78;

/// What a missing capability is reported as (one clear line, once).
pub const NO_CAP_MESSAGE: &str = "CAP_SYS_RAWIO is not in CapEff: the strips cannot be driven \
(the file capability was lost — run scripts/deploy.sh, which re-applies and verifies it)";

/// The `CapEff` value from `/proc/self/status`-shaped text. `None` when the line is missing or
/// not hexadecimal.
pub fn parse_cap_eff(status: &str) -> Option<u64> {
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("CapEff:") else {
            continue;
        };
        let hex = rest.split_whitespace().next()?;
        return u64::from_str_radix(hex, 16).ok();
    }
    None
}

/// Whether a capability mask has `bit`.
pub fn mask_has(mask: u64, bit: u32) -> bool {
    bit < 64 && mask & (1u64 << bit) != 0
}

/// Whether `CapEff` text has `CAP_SYS_RAWIO`.
pub fn status_has_cap_sys_rawio(status: &str) -> Option<bool> {
    parse_cap_eff(status).map(|mask| mask_has(mask, CAP_SYS_RAWIO))
}

/// Read `/proc/self/status` and report the bit. `None` if the file or line is unreadable.
pub fn read_cap_sys_rawio() -> Option<bool> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status_has_cap_sys_rawio(&status)
}

/// The start-up gate. `enforced` is the `ws281x` build; `has` comes from [`read_cap_sys_rawio`].
pub fn gate(enforced: bool, has: Option<bool>) -> Result<(), &'static str> {
    if !enforced {
        return Ok(());
    }
    match has {
        Some(true) => Ok(()),
        _ => Err(NO_CAP_MESSAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_CAP: &str = "Name:\tplippa-leds\nUid:\t1000\t1000\t1000\t1000\n\
                            CapInh:\t0000000000000000\nCapPrm:\t0000000000020000\n\
                            CapEff:\t0000000000020000\nCapBnd:\t000001ffffffffff\n";
    const WITHOUT_CAP: &str = "Name:\tplippa-leds\nUid:\t1000\t1000\t1000\t1000\n\
                               CapEff:\t0000000000000000\n";
    const ROOT_MASK: &str = "CapEff:\t000001ffffffffff\n";

    #[test]
    fn bit_seventeen_is_the_raw_io_capability() {
        assert_eq!(
            status_has_cap_sys_rawio(WITH_CAP),
            Some(true),
            "0x20000 is bit 17"
        );
        assert_eq!(status_has_cap_sys_rawio(WITHOUT_CAP), Some(false));
        assert_eq!(status_has_cap_sys_rawio(ROOT_MASK), Some(true));
        assert_eq!(status_has_cap_sys_rawio("Name:\tplippa-leds\n"), None);
        assert_eq!(status_has_cap_sys_rawio("CapEff:\tnothex\n"), None);
        assert!(mask_has(1 << 16, 16));
        assert!(!mask_has(1 << 16, 17));
    }

    #[test]
    fn the_helper_exits_with_a_clear_status_without_cap_sys_rawio() {
        assert_eq!(gate(true, Some(true)), Ok(()));
        let error = gate(true, Some(false)).expect_err("must refuse");
        assert_eq!(error, NO_CAP_MESSAGE);
        assert!(error.contains("CAP_SYS_RAWIO"));
        assert!(gate(true, None).is_err(), "unreadable CapEff is not a pass");
        assert_eq!(EXIT_NO_CAP, 78);
        assert_eq!(EXIT_CONFIG, 78);
        // A build without the hardware backend does not need the capability at all.
        assert_eq!(gate(false, Some(false)), Ok(()));
        assert_eq!(gate(false, None), Ok(()));
    }

    /// The real file must parse on this machine (the sandbox needs no capability to read it).
    #[test]
    fn the_real_status_file_parses() {
        if let Some(has) = read_cap_sys_rawio() {
            let _ = has;
        }
    }
}
