# plippa-noubin

Noubin standard helpers for Plippa — stick exactly to https://github.com/TobyKLight/Noubin.

## Owns

- **`.noudata` types** for playback and management (root keys, `mediaList`, `localFilePaths`)
- Forward-compatible JSON parse (unknown fields / enum values ignored; write-through preserves extras)
- **§6.1** NDEF → `noubinKey` extraction (URI + Text; `noubin.com` `?redirect=` unwrap)
- **§6.2** Noubin Key Normalisation
- Library path helpers (§4.4 skip `.*` dirs; scan depth ≥ 8)
- Local key generation (§1.16) with capacity shortening + collision checks
- **Playlist file helpers** (`playlist.rs`): `Playlists/_<title>.noudata` naming, new-playlist document, relative media paths, atomic guarded write (`write_noudata_atomic`, `FileRevision`)
- **Media library SQLite index** under `.plippa/` on the media volume (§4.6): full scan, lazy rescan, WAL, key lookup

Scan + index stay in this crate (not a separate `plippa-library`) so path/`noudata`/normalise helpers share one package. Long-running scan ownership still belongs to `plippa-server`; this crate exposes the shared library API.

## Library API roles

| Type | Process | Role |
|------|---------|------|
| `LibraryWriter` | `plippa-server` | create/open DB, full/lazy scan, ingest |
| `LibraryReader` | `plippa-engine` | read-only `noubinKeyNormalised` lookup (prefer stale over blocking) |

Both share one DB file in WAL mode — never atomically swap/replace the file between processes.

Does **not** own NFC hardware I/O, §6.3 web URL normalisation, or unite (§4.7).

## Index schema v2 (`.plippa/library.sqlite`, `meta.schema_version = 2`)

The index is **derived**: every column is computed from `.noudata` files / the directory tree.
Deleting `.plippa/` and rescanning reproduces it (same ids for the same tree). Unknown or newer
schema versions are dropped and rebuilt; a v1 file is upgraded in place (ids kept, every row
re-read by the next scan, so playback of stale rows keeps working meanwhile).

| Table | Purpose |
|-------|---------|
| `playables` | One row per `.noudata` (or per orphan media file: `noudata_relpath` NULL). `kind` (`track/album/playlist/mix/audiobook/podcast`), `release_profile` (the root `playableItemProfile`; a link track carries its release's), `noudata_type` (effective `noudataType`), `type_declared`, `is_dynamic`, `is_orphan`, `primary_artist`, `cover_relpath`, `refers_to_relpath`, `duration_ms` (sum, only when every item is known), `has_navigable_cues`, `media_relpaths_json` (flat v1-compatible list), fingerprint `mtime_ns` + `file_size`. `kind_ingest`, `artist_own`, `cover_own` keep the per-file values that cross-file resolution starts from. |
| `playable_items` | One row per `mediaList` entry: chosen `media_relpath` + all `media_relpaths_json` alternates, `media_title` (own), `title`/`artist` (effective: authoritative release for playlist entries), `resolved_from`, `expected_ms` (declared), `duration_ms` + `duration_src` (`unite`/`probed`/`failed`), `available`, `has_navigable_cues`, `cue_count`. |
| `media_claims` | (playable, item, media path) for every `localFilePaths` alternate: drives orphan detection, playlist resolution and the "claimed twice" warning. |
| `media_files` | Media files seen in scanned folders (mp3 flac wav ogg opus m4a m4b aac) with size/mtime. |
| `dirs` | Directory mtimes (ns). An unchanged directory is not listed again on a lazy scan. |
| `noudata_skips` | Fingerprints of `.noudata` that are unparsable / not playable, so they are not re-read every scan. |

### Roles

- **Kind (§4.9):** `releaseData` ⇒ release (`music`/unknown/`other` ⇒ `album`, `mix`, `audiobook`,
  `podcast`); no `releaseData` + `referToReleaseNoudata` resolving to a release ⇒ `track`;
  otherwise `playlist` (a `searchString` with an empty `mediaList` is a *dynamic* playlist and
  is indexed, §3.7). An orphan media file (no `.noudata` references it) is a `track` titled by
  its file name. A media file referenced only by a playlist is *not* an orphan.
- **Playlist entries** take title/artist/length from the authoritative release item that claims
  the same media (release wins over the playlist's own `mediaTitle`); unresolved entries use their
  own `mediaTitle`, else the file name.
- **Cues:** *not* stored in SQLite. `items_of()` carries `has_navigable_cues` + `cue_count`;
  `cues_for(playable_id, item_position)` reads the `.noudata` on demand and returns
  [`CueEntry`] values (all lists; filter `navigable`), trimmed to the item's known length.
- **Durations:** declared `uniteData.expectedLength` is a hint only. `LibraryWriter::probe_durations`
  takes a `FnMut(&Path) -> Option<u64>` (the server passes a lofty header read), measures a
  bounded batch of available, not-yet-probed items and writes the value back to the `.noudata`
  (`uniteData.expectedLength`, only when it differs by >500 ms; orphans are index-only), then updates
  the index and the file's fingerprint so the next scan does not see it as an external edit.
  A media file replaced in place is measured again; an unreadable one is marked `failed` and not retried.

### Lazy scan

`full_scan()` re-lists every folder and re-reads every `.noudata`. `lazy_rescan()` lists only
folders whose mtime changed (folders modified <3 s ago are always relisted: coarse FS timestamps),
`stat`s each known `.noudata` and re-reads only those whose mtime (ns) or size changed. A scan that
finds nothing to do performs **no write transaction**. Reads happen without holding the SQLite
write lock; the apply step is one short `BEGIN IMMEDIATE` transaction. `.`-folders are skipped,
depth ≥ 8, symlinked folders are not followed, `._*` AppleDouble files ignored. `ScanReport`
carries `noudata_reads` / `dirs_listed` counters and `changed()` (what the server uses to bump
the generation and notify). Not covered: inotify (polling only).

### Reader API (both `LibraryReader` and `LibraryWriter`)

`list_playables`, `list_by_kind(kind)`, `get_playable(id)` (= `lookup_by_id`), `items_of(id)`,
`find_by_key(key)` (**all** matches; `lookup_by_normalised_key` wraps it), `cues_for(id, pos)`,
`library_warnings()` (duplicate `noubinKeyNormalised`; media claimed by two release `.noudata`).
Also `item_counts()`, `kind_counts()` and `indexed_fingerprint(id)` (list counts, `/api/kinds`, stale-index check).
A titled `.noudata` with `createdByUser` and no playable entry (an emptied user playlist) is indexed as a playlist with zero items.
A `LibraryReader` opened on a still-v1 file serves the flat columns and returns empty for the new APIs.

