# Linkhost cross-check fixtures

Generated test fixtures: 11 representative releases as the Noubin linkhost publishes them under standard v0.4.0. They are committed unmodified and must not be hand-edited. The tests read them through `CARGO_MANIFEST_DIR`; nothing is fetched.

One directory per case:

- `web.noudata`: byte-for-byte what the linkhost publishes for the release.
- `case.json`: the Noubin URLs the linkhost shows the artist (`longUrl`, `shortUrl`, `publicUrl`), the URL shape the case was published under (`publishedUrlShape`), the metadata URL and the files it publishes (`publishedFiles`). `longUrl` is `null` when the release has no path-artist slug. A release is served under one URL shape only; the other listed form is informational and is not served.

`manifest.json` lists the cases and the standard version.

## What the files guarantee

Only standard v0.4.0 properties: no `mediaDownloadPolicy`, `showDownloadFreeButton` or `imageWidth` / `imageHeight`, and no `noubinKey*` (players set and compute those; they must not appear in `web.noudata`). `_whatAmILookingAt` is the standard's fixed text and the first key. `coverImage` is omitted, never empty, when a release has no cover. Paid cases carry `expectedFilename` and/or `expectedLength` on every item, free cases carry `freeDownloadURL` on every item, and multi-item lists carry a `mediaTitle` on every item.
