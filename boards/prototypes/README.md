# Board prototypes

Hardware proof-of-concept board profiles live here, one folder per board.

```
boards/prototypes/
  plippa-D-poc2/     # Model D POC2: config, wiring CSVs, setup notes, systemd unit
```

Each profile has a `config/board.toml` that `plippa-board` reads to produce a `plippa_core::Capabilities` map and the role wiring for `plippa-ui`. Pinouts live as CSVs next to it.

POC2 software setup: [`plippa-D-poc2/boardSetup.md`](plippa-D-poc2/boardSetup.md).
