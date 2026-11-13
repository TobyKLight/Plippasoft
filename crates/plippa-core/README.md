# plippa-core

Foundational types shared across Plippa processes — no hardware or I/O.

## Owns

- **Playable-item identity** — library index ids and Noubin-facing identity fields (`title`, optional `noubinKeyNormalised`)
- **Transport / session state** — play / pause / stop transitions and volume
- **`Capabilities`** — board-profile flags (Noubin optional features + decode limits) used by engine, audio, and UI

Does **not** own `.noudata` parsing (see `plippa-noubin`), IPC wire messages (see `plippa-ipc-proto`), or audio decode.
