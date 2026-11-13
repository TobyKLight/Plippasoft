# plippa-board

Loads board profiles from TOML into [`plippa_core::Capabilities`] plus pin/role maps for `plippa-ui`.

```text
boards/prototypes/<id>/config/board.toml   # POC (e.g. plippa-D-poc2)
boards/<id>/config/board.toml              # production
```

```rust,ignore
use plippa_board::BoardProfile;
let profile = BoardProfile::load(repo_root, "plippa-D-poc2")?;
assert!(profile.capabilities.local_display);
```

## Roles

App code uses logical roles only; the profile maps them to hardware and says which exist.

```rust,ignore
use plippa_board::{BoardProfile, ControlRole, EncoderRole, LedRole};
profile.has(ControlRole::PrevTrack);          // optional control present?
profile.button_for(ControlRole::Play);        // Option<&ButtonRef> (expander pin / GPIO)
profile.encoder_for(EncoderRole::Volume);     // Option<&EncoderPins>
profile.led_for(LedRole::NfcRing);            // Option<&LedSpan> { stream, start, count, .. }
profile.optional_roles_summary();             // startup log line
```

`board.toml` sections: `[roles]` (buttons/encoders), `[led_streams.<name>]` (gpio, channel, count) and
`[leds.<led_role>]` (stream, start, count, optional `max_brightness_pct`, `color_order`). All new keys are
optional (`serde(default)`), so older profiles still load. Loading validates every reference and returns
`BoardError::Invalid` with the offending key. Full schema and "how to add a board":
`boards/prototypes/plippa-D-poc2/boardSetup.md` (section "Roles"). A deliberately minimal test profile lives in
`fixtures/minimal.board.toml`.
