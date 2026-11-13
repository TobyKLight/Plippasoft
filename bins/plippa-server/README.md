# plippa-server

Media management process and LAN desktop browser portal.

## Run

```bash
export CARGO_BUILD_JOBS=1
cargo run -p plippa-server -- --library-root /media/<user>/<volume>
```

The portal is **off by default**. The owner switches it on from the device menu, and it asks for a six-digit PIN. It serves HTTPS with a self-signed certificate generated on the device (fingerprint shown under System > Network). Default bind is `0.0.0.0:8787`. State (portal on/off, PIN, certificate, store credentials) lives in the secrets directory, never on the media stick. On a dev machine point it somewhere writable with `--secrets-dir`.

The on-device UI talks to the server over a separate loopback-only control listener (`127.0.0.1:8786`) using a per-boot token.

### Options

| Flag / env | Meaning |
|------------|---------|
| `--library-root PATH` / `PLIPPA_LIBRARY_ROOT` | Noubin media library root |
| `--bind ADDR` | Portal listen address (default `0.0.0.0:8787`) |
| `--control-bind ADDR` / `PLIPPA_CONTROL_BIND` | Loopback control listener (default `127.0.0.1:8786`; must stay loopback) |
| `--secrets-dir PATH` / `PLIPPA_SECRETS_DIR` | Where portal state and store credentials are kept (default `/var/lib/plippa/secrets`) |
| `--portal-url URL` / `PLIPPA_PORTAL_URL` | Address the device advertises for its portal (default: worked out from the network interfaces) |
| `--no-web` / `PLIPPA_WEB=off` | Turn the online Noubin lookup off completely (`web_capable=false`) |
| `PLIPPA_PORTAL_HOSTS` | Extra DNS names the portal answers to (comma separated); by default only IP literals, `localhost`, single-label names and `*.local`/`*.lan` are accepted in `Host` (DNS-rebinding guard) |

## Web lookups: privacy and consent

The player is local-first: tapping a Noubin that is in your library never contacts the internet.
Only when a **URL** Noubin matches nothing locally does the portal show "Tag not in your library"
with a **Look up online** button. Nothing is sent until you confirm:

1. `POST /api/web/prepare {"noubinKey": ...}` returns the exact address that would be contacted
   (spec §6.3, e.g. `https://artist.com/noubin/album/web.noudata`). Sends nothing.
2. `POST /api/web/fetch {"noubinKey": ..., "consent": true}` performs the request. Without
   `"consent": true` the server answers 403 and opens no socket (enforced in the server, not the page).

What the request is: one HTTPS GET, no cookies, no `Referer`, no credentials, a fixed
`User-Agent: Plippa-Noubin`, no compression, no proxy. It only accepts `https` on port 443, refuses
private / loopback / link-local / CGNAT / `.local` targets (checked on the resolved address when
connecting, and again on every redirect, at most 3), gives up after 10 s to connect / 30 s in total,
reads at most 1 MiB, and only accepts a JSON **object** (`application/json`, `text/json`,
`application/*+json`, or `text/plain` holding an object). Anything else is dropped. The answer is
parsed with the Noubin parser and then sanitised field by field; it is kept in memory for 10 minutes
(at most 4 releases) and **nothing is written to the library** until the download step.
Only one lookup can run at a time (a second gets 429). There are no background or automatic lookups.
Cover art and media are not downloaded by this step.

To disable all of it: start the server with `--no-web` or set `PLIPPA_WEB=off`. `/api/status`
then reports `web_capable: false` with the reason (also when no network interface is up).

Portal hardening (all responses): `Content-Security-Policy` (same-origin, no inline script),
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store`.
State-changing requests (POST/PUT/PATCH/DELETE) must carry `Content-Type: application/json` or
`X-Plippa: 1`, and an `Origin` header must match `Host`.

## Adding a release: downloads, uploads, refresh

The portal button **Add release from a Noubin URL…** (or **Add this release to my library…** in the
lookup overview) drives `/api/unite/*`. Everything staged lives in `<library>/.plippa/tmp/unite-<session>/`
(same volume as the library, so the last step is an atomic rename); it is deleted with the session
(30 min idle, at most 4 sessions) and swept on the next start. Nothing enters the library before
`apply`, which creates the folder (`Music/<artist>/<release>/`, ...), moves the files in, and writes
`_<release>.noudata` **last**; any failure moves everything back.

* **Case A (free release)**: `start` (metadata lookup, consent as above) -> `download/prepare` lists
  **every URL** (media, cover, thumbnail tiers; sizes are unknown before the first response, a size
  request would itself be a network action) -> `download {consent: true, consentId}` (403 without both) ->
  poll `GET /api/unite/<session>` -> `proposal` -> `confirm {accept: "all"}` -> `apply`.
* **Case B (files bought elsewhere)**: `start`, upload with `POST /api/unite/<session>/files?name=...`
  (raw body, header `X-Plippa: 1`), `proposal`, `confirm` (manual `assignments` for anything
  low-confidence), `apply`. Cover art can be downloaded too (`download/prepare {"media": false}`);
  if purchased media also had art, the higher-resolution image is primary and the other is kept as `<name>-alt.<ext>`.
* **Case C (a `_<release>.noudata` came with the files)**: `start {associateOnly: true}` needs no network;
  the document is sanitised as imported data, its `localFilePaths` are pointed at the files that
  really arrived, the key is bound; no matching.
* **Refresh**: `POST /api/playables/<id>/refresh/prepare` shows the address, `.../refresh {consent: true}`
  merges the latest metadata. The URL is the item's `noubinKey` when that is a URL, else the
  recorded `plippaSourceURL` property (written by unite), else you supply one (then recorded).
  Notes, ratings, tags, resume points, key, file assignments and local image files are never touched.
  Cover images are a second, separate consent (`refresh/images`).

Download rules: https only, port 443, private/loopback/link-local/CGNAT/`.local` targets refused on
the resolved address and on every redirect (max 3); no cookies/`Referer`/credentials; **file type from
the first bytes** (MP3/FLAC/WAV/Ogg/Opus/M4A/AAC audio; JPEG/PNG/WebP images; URL extension and
`Content-Type` are ignored); a per-file cap (default **2 GB**, `PLIPPA_DOWNLOAD_MAX_MB`), a per-session
cap (default 4 GB, `PLIPPA_DOWNLOAD_BATCH_MAX_MB`), images <= 16 MB and <= 5000 x 5000 px (read from
the header, never decoded); caps and free space (64 MB stays free) are enforced while streaming, not
only from `Content-Length`; a stalled connection is dropped after 30 s; failed / cancelled / cut-off
files are deleted. One download at a time (429). Resuming a partial file is not supported.
Uploads use the same streaming and sniffing; the client's file name only supplies a stem
(the stored name is sanitised and gets the sniffed extension).
`--no-web` / `PLIPPA_WEB=off` disables downloads and lookups; uploads and Case C still work offline.

## Tests

```bash
export CARGO_BUILD_JOBS=1
cd <repo>
cargo test -p plippa-server
```

Prefer one-crate builds with `CARGO_BUILD_JOBS=1` on the 1 GB board.
