/* Plippa media portal — desktop browser UI (MVP_07 / MVP_12 / MVP_17) */

/** @type {string|null} decimal playable id (string — namespaced ids exceed JS Number precision) */
let selectedId = null;
/** @type {Record<string, string>} */
let playablesById = {};

async function api(path, options = {}) {
  // NBSecurityRequirements §5.3: every state-changing call carries a header a cross-site
  // form / no-cors fetch cannot send (the server refuses POST/PUT/PATCH/DELETE without it).
  const method = (options.method || "GET").toUpperCase();
  if (method !== "GET" && method !== "HEAD") {
    options = { ...options, headers: { ...(options.headers || {}), "X-Plippa": "1" } };
  }
  const res = await fetch(path, options);
  const text = await res.text();
  let data = null;
  try {
    data = text ? Ui.scrub(JSON.parse(text)) : null;
  } catch {
    data = { error: Ui.clean(text) };
  }
  if (res.status === 401 && typeof Signin !== "undefined") Signin.expired();
  if (!res.ok) {
    const err = new Error((data && data.error) || res.statusText);
    if (data && data.code) err.code = data.code;
    err.status = res.status;
    throw err;
  }
  return data;
}

function $(id) {
  return document.getElementById(id);
}

/** Coerce API id fields to decimal string. */
function idStr(id) {
  if (id == null) return null;
  return String(id);
}

function showBanner(message) {
  const el = $("banner");
  if (!message) {
    el.classList.add("hidden");
    el.textContent = "";
    return;
  }
  el.textContent = message;
  el.classList.remove("hidden");
}

function showDetailMsg(msg) {
  const el = $("detail-msg");
  if (!msg) {
    el.hidden = true;
    el.textContent = "";
    return;
  }
  el.hidden = false;
  el.textContent = msg;
}

function renderNowPlaying(np, status) {
  const pid = idStr(np.playable_id);
  $("np-transport").textContent = np.transport || "—";
  // Collection · Track n/m · entry title (artist) · cue — all plain text (titles are untrusted).
  const collection =
    np.collection_title || (pid != null ? playablesById[pid] || `id ${pid}` : "");
  $("np-collection").textContent = collection;
  const multi = np.track_count > 1 && np.track_index != null;
  $("np-entry").textContent = [
    multi ? `Track ${np.track_index + 1}/${np.track_count}` : "",
    np.title && np.title !== collection ? np.title : "",
    np.artist || "",
  ]
    .filter(Boolean)
    .join(" · ");
  $("np-cue").textContent = np.cue_title ? `Cue: ${np.cue_title}` : "";
  const pos = np.duration_ms
    ? `${fmtMs(np.position_ms)} / ${fmtMs(np.duration_ms)}`
    : pid != null && np.position_ms
      ? fmtMs(np.position_ms)
      : "";
  $("np-detail").textContent = [pos, np.volume_percent ? `vol ${np.volume_percent}%` : ""]
    .filter(Boolean)
    .join(" · ");
  $("np-ipc").textContent = np.ipc_live
    ? "engine connected"
    : "engine offline — Play/Associate need plippa-engine";
  const btn = $("play-pause-btn");
  const unloadBtn = $("unload-btn");
  const hasItem = pid != null;
  const canToggle =
    np.ipc_live && (np.transport === "Playing" || hasItem || np.transport === "Paused");
  btn.disabled = !canToggle;
  $("prev-btn").disabled = !(np.ipc_live && np.can_prev);
  $("next-btn").disabled = !(np.ipc_live && np.can_next);
  if (unloadBtn) {
    unloadBtn.disabled = !np.ipc_live;
  }
  btn.textContent = np.transport === "Playing" ? "Pause" : "Play";
  if (typeof Library !== "undefined") Library.noteNowPlaying({ ...np, playable_id: pid });
  if (np.last_notice) {
    showBanner(np.last_notice);
  } else if (status && status.warning) {
    showBanner(status.warning);
  }
}

async function transportStep(which) {
  await api(`/api/transport/${which}`, { method: "POST" });
  setTimeout(() => refreshNowPlaying().catch(() => {}), 250);
}

async function togglePlayPause() {
  await api("/api/transport/play-pause", { method: "POST" });
  await refreshNowPlaying();
}

async function unloadPlayback() {
  await api("/api/transport/unload", { method: "POST" });
  await refreshNowPlaying();
  showBanner("Unloaded — playback stopped, resume bookmarks cleared");
}

async function refreshNowPlaying(status) {
  const np = await api("/api/now-playing");
  renderNowPlaying(np, status);
  return np;
}

async function refreshStatus() {
  const status = await api("/api/status");
  Signin.note(status);
  if (status.warning) {
    showBanner(status.warning);
  } else if (!status.ipc_live) {
    showBanner("Engine not connected — start the plippa launcher (or plippa-engine) for Play / tap");
  } else {
    showBanner(null);
  }
  const rootsEl = $("library-roots");
  if (rootsEl) {
    const roots = status.library_roots || [];
    if (roots.length > 1) {
      rootsEl.textContent = "Libraries: " + roots.map((r) => r.label).join(", ");
      rootsEl.classList.remove("hidden");
    } else if (roots.length === 1) {
      rootsEl.textContent = "Library: " + roots[0].label;
      rootsEl.classList.remove("hidden");
    } else {
      rootsEl.textContent = "";
      rootsEl.classList.add("hidden");
    }
  }
  updateWebOffer(status);
  await refreshNowPlaying(status);
  return status;
}

/** Re-render the current library view (NB_10: library.js owns the views). */
async function refreshList() {
  await Library.refresh();
}

async function openDetail(id) {
  const sid = idStr(id);
  selectedId = sid;
  // WAVE_01: a new selection invalidates any status poll still in flight.
  const waveformToken = ++waveformPollToken;
  showDetailMsg("");
  const detail = await api(`/api/playables/${encodeURIComponent(sid)}`);
  $("detail-empty").classList.add("hidden");
  $("detail-panel").classList.remove("hidden");
  showDetailCovers(sid);
  $("detail-title").value = detail.playable_item_title || detail.summary.title || "";
  $("detail-path").textContent = detail.summary.noudata_relpath;
  // v0.4.0 type; a release or loose track has an editable artist (a link track / playlist
  // takes it from the release / its tracks).
  const ty = detail.noudata_type || "";
  $("detail-type").textContent = ty || "—";
  const editableArtist = ty === "release" || ty === "looseTrack";
  $("detail-artist-row").classList.toggle("hidden", !editableArtist);
  $("detail-artist").value = detail.primary_artist || "";
  $("detail-artist").dataset.orig = detail.primary_artist || "";
  $("detail-artist-hint").hidden = ty !== "looseTrack";
  $("detail-key").textContent =
    detail.noubin_key_normalised || detail.summary.noubin_key_normalised || "—";
  playablesById[sid] = detail.summary.title || playablesById[sid] || "(untitled)";
  detailPlaylistTitle = detail.summary.title || playablesById[sid] || "(untitled)";
  // A track is added to a playlist; a collection's tracks are appended to one (NB_10 UI).
  const isCollection = ["album", "mix", "audiobook", "podcast", "playlist"].includes(
    detail.summary.kind
  );
  $("detail-append-playlist").classList.toggle("hidden", !isCollection);
  $("detail-add-playlist").classList.toggle("hidden", isCollection);
  // ART_06: same rule as the ⋮ menus (no dynamic playlists, no unavailable media, no orphan).
  detailCoversAccess = Library.coversAccess(detail.summary);
  $("detail-covers-btn").classList.toggle("hidden", !detailCoversAccess);
  // WAVE_01: analysis status (a separate read; a failure here must not hide the panel).
  $("detail-waveform").textContent = "—";
  $("detail-waveform").removeAttribute("title");
  await refreshWaveformStatus(sid, waveformToken, 1);
}

/** ART_06: what "Edit cover art…" in the details panel opens, or null. */
let detailCoversAccess = null;

/** ART_05: cover slot of the details panel (placeholder first, slider once the list arrives). */
let detailCovers = null;
function showDetailCovers(sid) {
  if (detailCovers) detailCovers.destroy();
  const slot = $("detail-cover");
  slot.classList.remove("has-slider");
  slot.replaceChildren(Covers.thumb(null, { cls: "cover-stage" }));
  detailCovers = Covers.mount(slot, sid, { size: 512 });
}

// ---------------------------------------------------------------------------
// WAVE_01: waveform analysis status (analysed / analysing / queued / failed).
// ---------------------------------------------------------------------------

/** Invalidates polls from a previous selection (bumped by openDetail). */
let waveformPollToken = 0;

/** Title of the playable in the details panel (for the "add to playlist" menu label). */
let detailPlaylistTitle = "";

/** Open the playlist picker for the details panel selection, anchored to `btn`. */
function addSelectedToPlaylist(btn) {
  if (selectedId == null) return;
  Library.pickPlaylist(btn, selectedId, detailPlaylistTitle);
}

/** Render one `GET /api/waveforms/:id` answer as plain text + a title tooltip. */
function renderWaveformStatus(status) {
  const el = $("detail-waveform");
  const items = status.items || [];
  const total = items.length;
  const analysed = items.filter((i) => i.state === "analysed").length;
  const counts = `${analysed} of ${total} items analysed`;
  el.removeAttribute("title");
  if (status.state === "all") {
    el.textContent = "analysed";
    return false;
  }
  if (status.state === "none") {
    el.textContent = "—";
    return false;
  }
  const analysing = items.some((i) => i.state === "analysing");
  const failed = items.filter((i) => i.state === "failed").map((i) => Ui.clean(i.title || ""));
  const unavailable = items
    .filter((i) => i.state === "unavailable")
    .map((i) => Ui.clean(i.title || ""));
  if (analysing) {
    el.textContent = total > 1 ? `analysing… (${analysed}/${total})` : "analysing…";
    el.title = counts;
    return true;
  }
  if (failed.length) {
    el.textContent = "failed";
    el.title = failed.join(", ");
    return false;
  }
  // Nothing left that could still be produced: the files themselves are missing from the stick.
  if (unavailable.length && !items.some((i) => i.state === "queued")) {
    el.textContent = "unavailable";
    el.title = unavailable.join(", ");
    return false;
  }
  el.textContent = total > 1 ? `queued (${analysed}/${total})` : "queued";
  el.title = counts;
  return true;
}

/**
 * Fetch the analysis status, then poll (3 s, at most 20 polls) while the worker is still
 * pending. Stops when the selection changes or the panel is hidden.
 */
async function refreshWaveformStatus(sid, token, attempt) {
  if (token !== waveformPollToken || selectedId !== sid) return;
  const panel = $("detail-panel");
  if (!panel || panel.classList.contains("hidden")) return;
  let status;
  try {
    status = await api(`/api/waveforms/${encodeURIComponent(sid)}`);
  } catch (e) {
    if (token !== waveformPollToken || selectedId !== sid) return;
    const el = $("detail-waveform");
    el.textContent = "unknown";
    el.title = e.message || String(e);
    return;
  }
  if (token !== waveformPollToken || selectedId !== sid) return;
  const pending = renderWaveformStatus(status);
  if (pending && attempt < 20) {
    setTimeout(() => refreshWaveformStatus(sid, token, attempt + 1), 3000);
  }
}

async function saveMetadata() {
  if (selectedId == null) return;
  const playableItemTitle = $("detail-title").value.trim();
  const body = { playableItemTitle };
  const artist = $("detail-artist").value.trim();
  if (artist && artist !== $("detail-artist").dataset.orig) body.primaryArtist = artist;
  await api(`/api/playables/${encodeURIComponent(selectedId)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  showDetailMsg("Saved to .noudata");
  await refreshList();
  await openDetail(selectedId);
}

async function playSelected() {
  if (selectedId == null) return;
  showDetailMsg("Sent Play — decoding on engine (long MP3s can take a while)…");
  await api(`/api/playables/${encodeURIComponent(selectedId)}/play`, { method: "POST" });
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 500));
    const np = await refreshNowPlaying();
    if (np.last_notice && /missing|not found|fail/i.test(np.last_notice)) {
      showDetailMsg(np.last_notice);
      return;
    }
    const pid = idStr(np.playable_id);
    if (np.transport === "Playing" && (pid == null || pid === selectedId)) {
      showDetailMsg(`Playing${pid != null ? ` id ${pid}` : ""}`);
      return;
    }
  }
  const np = await refreshNowPlaying();
  showDetailMsg(
    np.last_notice ||
      `Still ${np.transport} after waiting — check engine CLI for decode progress`
  );
}

async function associateSelected(keepExistingKey) {
  if (selectedId == null) return;
  showDetailMsg(
    keepExistingKey
      ? "Tap a Noubin on the player now… (reading existing key, no overwrite)"
      : "Tap a Noubin on the player now… (waiting for NFC write)"
  );
  showDetailMsg(await associatePlayable(selectedId, keepExistingKey));
  await openDetail(selectedId);
}

/**
 * Write `sid`'s key to a Noubin the user taps on the player. Returns the message to show.
 * `keepExistingKey` reads the tag's key instead of overwriting it (NB_06).
 */
async function associatePlayable(sid, keepExistingKey) {
  const keep = !!keepExistingKey;
  const qs = keep ? "?keep_existing_key=1" : "";
  const result = await api(
    `/api/playables/${encodeURIComponent(idStr(sid))}/associate${qs}`,
    { method: "POST" }
  );
  if (!result.ok) {
    return (
      result.error ||
      (keep
        ? "Associate failed — tag key not kept, .noudata unchanged"
        : "Associate failed — tag not written, .noudata unchanged")
    );
  }
  // NB_06: say what changed (textContent only — titles come from files).
  const names = (l) => (l || []).map((i) => i.title).join(", ");
  let msg = `Associated OK — key ${result.noubin_key_normalised || result.noubin_key}.`;
  if (result.bound && result.bound.created_track_file) msg += " Created a file for this track.";
  if (result.previous_titles && result.previous_titles.length)
    msg += ` The tag was on: ${result.previous_titles.join(", ")} (they keep their key).`;
  if (result.cleared && result.cleared.length)
    msg += ` Locked tag: key moved from ${names(result.cleared)}.`;
  for (const w of result.warnings || []) msg += ` Warning: ${w.message} (${names(w.items)}).`;
  return msg;
}

// (NB_11: adding media moved to addmedia.js — the Add media / Add multiple media dialogs.)

// ---------------------------------------------------------------------------
// NB_08 web lookup. Two steps, both explicit: "prepare" shows the exact URL and sends
// nothing; only the second button ("Contact server") sends the request. Everything from the
// network is rendered with textContent / setAttribute, never innerHTML.
// ---------------------------------------------------------------------------

let webKey = null;
/** Token of the release fetched with the user's consent (handed to the unite dialog). */
let webToken = null;
let webDismissedSeq = 0;
let webSeq = 0;

function clearNode(el) {
  el.replaceChildren();
}

function text(tag, str, cls) {
  const el = document.createElement(tag);
  el.textContent = Ui.clean(String(str));
  if (cls) el.className = cls;
  return el;
}

/** An https link shown as text; anything else is shown as plain text without an href. */
function safeLink(label, url) {
  if (typeof url === "string" && /^https:\/\//i.test(url)) {
    const a = document.createElement("a");
    a.textContent = label;
    a.setAttribute("href", url);
    a.setAttribute("rel", "noopener noreferrer nofollow");
    a.setAttribute("target", "_blank");
    return a;
  }
  return text("span", label);
}

function fmtMs(ms) {
  if (ms == null) return "";
  const s = Math.round(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

function updateWebOffer(status) {
  const box = $("web-offer");
  if (!box) return;
  const u = status.unmatched_noubin;
  if (!u || u.seq <= webDismissedSeq) {
    box.classList.add("hidden");
    return;
  }
  webKey = u.noubin_key;
  webSeq = u.seq;
  $("web-offer-key").textContent = u.noubin_key;
  const can = !!status.web_capable;
  $("web-prepare-btn").disabled = !can;
  $("web-offer-note").textContent = can
    ? "Nothing has been sent. Looking it up contacts a server on the internet, and only after you confirm."
    : "Online lookup is not available: " + (status.web_reason || "unknown reason");
  box.classList.remove("hidden");
}

async function webPrepare() {
  if (!webKey) return;
  const p = await api("/api/web/prepare", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ noubinKey: webKey }),
  });
  $("web-consent-url").textContent = p.fetch_url;
  $("web-consent-text").textContent = p.consent_message;
  $("web-consent").classList.remove("hidden");
  $("web-overview").classList.add("hidden");
}

async function webFetch() {
  if (!webKey) return;
  $("web-fetch-btn").disabled = true;
  try {
    const r = await api("/api/web/fetch", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ noubinKey: webKey, consent: true }),
    });
    $("web-consent").classList.add("hidden");
    webToken = r.token;
    renderOverview(r.overview);
  } finally {
    $("web-fetch-btn").disabled = false;
  }
}

function renderOverview(o) {
  const box = $("web-overview-body");
  clearNode(box);
  box.appendChild(text("h3", o.title || "(untitled release)"));
  if (o.artist) box.appendChild(text("p", o.artist, "muted"));
  const meta = [o.profile];
  if (o.total_expected_ms != null) meta.push(fmtMs(o.total_expected_ms));
  box.appendChild(text("p", meta.join(" · "), "muted"));
  if (o.cover_url) {
    const p = document.createElement("p");
    p.appendChild(text("span", "Cover (not downloaded): ", "muted"));
    p.appendChild(safeLink(o.cover_url, o.cover_url));
    box.appendChild(p);
  }
  if (o.free_download && o.free_download.available) {
    box.appendChild(
      text(
        "p",
        `Free download available for ${o.free_download.item_count} of ${o.tracks.length} tracks.`,
        "note"
      )
    );
  }
  if (o.tracks.length) {
    box.appendChild(text("h4", "Tracks"));
    const ol = document.createElement("ol");
    o.tracks.forEach((t) => {
      const li = document.createElement("li");
      li.textContent = (t.title || "(untitled)") + (t.expected_length_ms != null ? "  " + fmtMs(t.expected_length_ms) : "");
      ol.appendChild(li);
    });
    box.appendChild(ol);
  }
  if (o.platforms.length) {
    box.appendChild(text("h4", "Where to get it"));
    const ul = document.createElement("ul");
    o.platforms.forEach((p) => {
      const li = document.createElement("li");
      li.appendChild(safeLink(p.label, p.url));
      li.appendChild(text("span", " — " + p.url, "muted"));
      ul.appendChild(li);
    });
    box.appendChild(ul);
  }
  if (o.further_links.length) {
    box.appendChild(text("h4", "More"));
    const ul = document.createElement("ul");
    o.further_links.forEach((l) => {
      const li = document.createElement("li");
      li.appendChild(safeLink(l.title || l.url, l.url));
      ul.appendChild(li);
    });
    box.appendChild(ul);
  }
  if (o.warnings.length) {
    box.appendChild(text("p", `${o.warnings.length} problem(s) in the metadata were repaired or ignored.`, "muted"));
  }
  $("web-overview").classList.remove("hidden");
}

function webClose() {
  $("web-consent").classList.add("hidden");
  $("web-overview").classList.add("hidden");
}

function webDismiss() {
  webDismissedSeq = webSeq;
  $("web-offer").classList.add("hidden");
  webClose();
}

function wireWeb() {
  if (!$("web-offer")) return;
  $("web-prepare-btn").addEventListener("click", () =>
    webPrepare().catch((e) => showBanner(e.message))
  );
  $("web-fetch-btn").addEventListener("click", () =>
    webFetch().catch((e) => {
      $("web-consent").classList.add("hidden");
      showBanner(e.message);
    })
  );
  $("web-cancel-btn").addEventListener("click", webClose);
  $("web-dismiss-btn").addEventListener("click", webDismiss);
  $("web-overview-close").addEventListener("click", webClose);
}

function wireUi() {
  wireWeb();
  $("save-btn").addEventListener("click", () => saveMetadata().catch((e) => showDetailMsg(e.message)));
  $("play-btn").addEventListener("click", () => playSelected().catch((e) => showDetailMsg(e.message)));
  $("play-pause-btn").addEventListener("click", () =>
    togglePlayPause().catch((e) => showBanner(e.message))
  );
  $("prev-btn").addEventListener("click", () => transportStep("previous").catch((e) => showBanner(e.message)));
  $("next-btn").addEventListener("click", () => transportStep("next").catch((e) => showBanner(e.message)));
  $("unload-btn").addEventListener("click", () =>
    unloadPlayback().catch((e) => showBanner(e.message))
  );
  $("associate-btn").addEventListener("click", () =>
    associateSelected(false).catch((e) => showDetailMsg(e.message))
  );
  $("associate-keep-btn").addEventListener("click", () =>
    associateSelected(true).catch((e) => showDetailMsg(e.message))
  );
  $("detail-covers-btn").addEventListener("click", (ev) => {
    if (detailCoversAccess) Library.editCovers(detailCoversAccess, ev.currentTarget);
  });
  // BCS_10: delete the selected item (preview first). A playlist is refused by the server; the
  // dialog shows its explanation and the dialog's own Delete button stays disabled.
  $("detail-delete-btn").addEventListener("click", (ev) => {
    if (selectedId == null) return;
    DeleteItem.open({
      id: selectedId,
      item: null,
      title: detailPlaylistTitle,
      opener: ev.currentTarget,
      onDeleted: () => {
        selectedId = null;
        detailCoversAccess = null;
        if (detailCovers) detailCovers.destroy();
        detailCovers = null;
        $("detail-panel").classList.add("hidden");
        $("detail-empty").classList.remove("hidden");
        showDetailMsg("");
        refreshList().catch(() => {});
      },
    });
  });
  $("detail-add-playlist").addEventListener("click", (ev) => {
    try {
      addSelectedToPlaylist(ev.currentTarget);
    } catch (e) {
      showDetailMsg(e.message);
    }
  });
  $("detail-append-playlist").addEventListener("click", (ev) => {
    try {
      addSelectedToPlaylist(ev.currentTarget);
    } catch (e) {
      showDetailMsg(e.message);
    }
  });
  $("rescan-btn").addEventListener("click", () =>
    api("/api/rescan", { method: "POST" })
      .then(() => refreshStatus())
      .then(() => refreshList())
      .catch((e) => showBanner(e.message))
  );
  setInterval(() => {
    refreshNowPlaying().catch(() => {});
  }, 1000);
  // NB_08: notice an unmatched URL tap. Polls this server only, never the internet.
  setInterval(() => {
    api("/api/status").then(updateWebOffer).catch(() => {});
  }, 3000);
}

// Classic scripts run in order before DOMContentLoaded, so library.js / unite.js are defined.
document.addEventListener("DOMContentLoaded", async () => {
  // SECFIX_02: without the PIN the page shows its sign-in form and loads nothing else.
  if (!(await Signin.gate())) return;
  wireUi();
  refreshStatus().catch((e) => showBanner(e.message));
  Library.init().catch((e) => showBanner(e.message));
});
