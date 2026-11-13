# plippa-hw

Hardware drivers for Plippa (NFC first).

## NFC

- Traits and `MockNfc` are always available (desktop / CI).
- `NfcSession` enforces the Noubin §3.2 full-read-before-feedback rule and same-tag debounce.
- `PollScheduler` targets at least 5 Hz (`MIN_POLL_INTERVAL` = 200 ms).
- `associate_playable_with_tag` writes NDEF and updates `.noudata`.

### PN532 (plippa-D-poc2)

Enable with `--features pn532`. Opens `/dev/i2c-1` at 0x24 (see
[`eval/poc2/nfc_test_PN532/pinout.md`](../../eval/poc2/nfc_test_PN532/pinout.md)). Type 2 NTAG read/write is
supported; Type 4 is covered by the mock backend for unit tests.

Optional smoke test on a device, after building with the feature:

```bash
cargo test -p plippa-hw --features pn532 -- --ignored
```
