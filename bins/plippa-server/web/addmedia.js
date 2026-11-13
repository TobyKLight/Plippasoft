/* Plippa media portal — Add media / Add multiple media dialogs (NB_11, ART_07).

   Both dialogs work on an import session of the /api/imports protocol and share one "Cover art"
   section. Files are uploaded AS THEY ARE DROPPED (one at a time: the server takes one upload
   at a time); the server extracts and de-duplicates the art embedded in the audio and returns
   the session-wide list after every file, so the preview is always the server's own. OK / Import
   is only the confirmation (POST …/finish with the audio order and the art ids) and is enabled
   while at least one audio file is staged. Cancel, Esc, closing the dialog or leaving the page
   discards the session (DELETE /api/imports/:id), so nothing stays on the stick.

   The single "Add media" dialog uses a one-file session ({single:true}); POST /api/media is no
   longer used by the UI.

   Everything from files/tags/servers is rendered with textContent only
   (NBSecurityRequirements §5.1). Relies on globals: Ui (ui.js), Covers (covers.js),
   api/$/idStr/refreshStatus/goTo/openDetail (app.js), Library (library.js). */

const AddMedia = (() => {
  const { el, fmtBytes, toast } = Ui;
  const LONG_FORM = new Set(["mix", "audiobook", "podcast"]);
  /** `finish` takes a JSON body of at most 256 KB; keep the pasted tracklist well under it. */
  const MAX_CUE_BYTES = 200000;
  const KIND_TAB = { music: "tracks", mix: "mixes", audiobook: "audiobooks", podcast: "podcasts", other: "tracks" };
  const JSON_HEADERS = { "Content-Type": "application/json" };
  const ART_HINT = "Add at least one audio file. To add cover art to an existing release use ⋮ › Edit cover art.";
  /** Image types people drop on the dialogs; the server decides which of them it can use. */
  const IMAGE_EXT = /\.(jpe?g|png|webp|gif|bmp|tiff?|heic|heif|avif|svg)$/i;
  const looksImage = (f) => (f.type && f.type.startsWith("image/")) || IMAGE_EXT.test(f.name || "");

  // What this player decodes, from `/api/status` (MEDIA_01). The server is authoritative (it
  // sniffs the bytes and refuses what the board cannot play); this is only so the person is told
  // before the upload. `formats` stays null when the status cannot be read: nothing is blocked
  // here then and the server refuses per file.
  const device = { formats: null, board: "" };
  /** File extension -> `audio_formats` key. An extension not listed is left to the server. */
  const FORMAT_KEY = { mp3: "mp3", wav: "wav", flac: "flac", m4a: "aac", m4b: "aac", aac: "aac", opus: "opus" };
  const FORMAT_LABEL = { m4a: "M4A/AAC", m4b: "M4A/AAC", aac: "M4A/AAC", ogg: "Ogg Vorbis", oga: "Ogg Vorbis", opus: "Opus" };

  async function loadDeviceFormats() {
    device.formats = null;
    device.board = "";
    try {
      const st = await api("/api/status");
      if (st && st.audio_formats && typeof st.audio_formats === "object") {
        device.formats = st.audio_formats;
        device.board = typeof st.board_id === "string" ? st.board_id : "";
      }
    } catch (e) {
      // Unreadable status: do not block anything here; the server decides per file.
    }
  }

  /** Why this player cannot decode `name`, or "" when it can (or when that is not known here). */
  function unplayableReason(name) {
    if (!device.formats) return "";
    const m = /\.([a-z0-9]+)$/i.exec(name || "");
    if (!m) return "";
    const ext = m[1].toLowerCase();
    const key = FORMAT_KEY[ext];
    // Ogg is in no board's format list, so it is refused everywhere.
    const playable = key ? device.formats[key] !== false : !(ext === "ogg" || ext === "oga");
    if (playable) return "";
    const label = FORMAT_LABEL[ext] || ext.toUpperCase();
    return `${label} is not supported by this player${device.board ? ` (${device.board})` : ""}`;
  }

  function clear(node) {
    node.replaceChildren();
  }

  /** Tab that lists an item of this library kind (the server reports what it really made). */
  const TAB_OF_KIND = { track: "tracks", album: "albums", playlist: "playlists", mix: "mixes", audiobook: "audiobooks", podcast: "podcasts" };

  function goTo(hash) {
    if (location.hash === hash) Library.refresh();
    else location.hash = hash;
  }

  function fmtHms(ms) {
    const s = Math.round(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    const two = (n) => String(n).padStart(2, "0");
    return h ? `${h}:${two(m)}:${two(sec)}` : `${m}:${two(sec)}`;
  }

  const sessionUrl = (sid, rest) => `/api/imports/${encodeURIComponent(sid)}${rest || ""}`;

  /** Best-effort DELETE of a session; a failure is swallowed (the server's TTL sweeps it). */
  async function discardById(sid) {
    if (!sid) return;
    try {
      await api(sessionUrl(sid), { method: "DELETE" });
    } catch {
      /* the server drops idle sessions on its own */
    }
  }

  // =====================================================================================
  // Cover art section (both dialogs)
  // =====================================================================================

  /**
   * The "Cover art" section of a dialog (elements `<prefix>-art`, `-art-notes`, `-art-list`).
   * `host`: sid() current session id, locked() true while an upload / finish runs, error(msg),
   * expired() when the server forgot the session.
   */
  function artSection(prefix, host) {
    const box = $(`${prefix}-art`);
    const notes = $(`${prefix}-art-notes`);
    const list = $(`${prefix}-art-list`);
    const state = { art: [], merged: [], skipped: 0, problems: [] };
    let busy = false;
    let focus = null; // {id, action} to restore after a re-render

    const ids = () => state.art.map((a) => a.art_id);

    function noteLines() {
      const lines = [];
      const m = state.merged.length;
      if (m) {
        const exact = state.merged.every((x) => x.exact);
        lines.push(`${m} ${exact ? "identical" : "duplicate"} ${m === 1 ? "image" : "images"} merged`);
      }
      if (state.skipped) {
        lines.push(`${state.skipped} unsupported ${state.skipped === 1 ? "image" : "images"} ignored`);
      }
      return lines.concat(state.problems);
    }

    function render() {
      clear(list);
      clear(notes);
      for (const line of noteLines()) notes.appendChild(el("p", { cls: "muted hint", text: line }));
      const locked = busy || host.locked();
      const n = state.art.length;
      state.art.forEach((a, i) => {
        const btn = (label, aria, action, disabled, handler) =>
          el("button", {
            text: label,
            cls: "icon",
            attrs: {
              type: "button",
              "aria-label": aria,
              "data-id": a.art_id,
              "data-action": action,
              ...(disabled ? { disabled: "" } : {}),
            },
            on: { click: handler },
          });
        const li = el(
          "li",
          { cls: "art-tile", attrs: { draggable: locked ? "false" : "true", "data-id": a.art_id } },
          [
            Covers.thumb(a.url, { size: 128, alt: `Cover art ${i + 1}`, cls: "art-thumb" }),
            el("span", { cls: i === 0 ? "art-pos primary" : "art-pos muted", text: i === 0 ? "Primary" : String(i + 1) }),
            el("span", { cls: "row-actions" }, [
              btn("↑", `Move cover art ${i + 1} earlier`, "up", locked || i === 0, () => move(i, i - 1, a.art_id, "up")),
              btn("↓", `Move cover art ${i + 1} later`, "down", locked || i === n - 1, () => move(i, i + 1, a.art_id, "down")),
              btn("✕", `Remove cover art ${i + 1}`, "remove", locked, () => remove(a.art_id)),
            ]),
          ]
        );
        li.addEventListener("dragstart", (ev) => {
          if (busy || host.locked()) return ev.preventDefault();
          ev.dataTransfer.setData("text/x-plippa-art", String(i));
          ev.dataTransfer.effectAllowed = "move";
          li.classList.add("dragging");
        });
        li.addEventListener("dragend", () => {
          li.classList.remove("dragging");
          for (const x of list.querySelectorAll(".drop-target")) x.classList.remove("drop-target");
        });
        li.addEventListener("dragover", (ev) => {
          if (!Array.from(ev.dataTransfer.types || []).includes("text/x-plippa-art")) return;
          ev.preventDefault();
          li.classList.add("drop-target");
        });
        li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
        li.addEventListener("drop", (ev) => {
          const from = Number(ev.dataTransfer.getData("text/x-plippa-art"));
          if (Number.isNaN(from)) return;
          ev.preventDefault();
          move(from, i, state.art[from] && state.art[from].art_id, null);
        });
        list.appendChild(li);
      });
      box.classList.toggle("hidden", !n && !notes.childElementCount);
      if (focus && !locked) {
        const f = focus;
        focus = null;
        // The moved tile may now be first / last: fall back to its other arrow.
        const pick = (act) => list.querySelector(`button[data-id="${f.id}"][data-action="${act}"]:not([disabled])`);
        const target = pick(f.action) || pick(f.action === "up" ? "down" : "up") || pick("remove");
        if (target) target.focus();
      }
    }

    /** Take the server's answer (`art` / `merged` / `art_skipped`) as the truth. */
    function apply(res) {
      if (!res) return;
      if (Array.isArray(res.art)) state.art = res.art;
      if (Array.isArray(res.merged)) state.merged = res.merged;
      if (res.art_skipped) state.skipped += res.art_skipped;
      render();
    }

    function reset() {
      state.art = [];
      state.merged = [];
      state.skipped = 0;
      state.problems = [];
      busy = false;
      focus = null;
      render();
    }

    async function run(call) {
      const sid = host.sid();
      if (!sid || busy) return;
      busy = true;
      render();
      try {
        apply(await call(sid));
      } catch (e) {
        if (e.status === 404 && e.code === "unknown_import") host.expired();
        else host.error(e.message);
      } finally {
        busy = false;
        render();
      }
    }

    function remove(id) {
      focus = null;
      return run((sid) => api(sessionUrl(sid, `/art/${encodeURIComponent(id)}`), { method: "DELETE" }));
    }

    function move(from, to, id, action) {
      if (to < 0 || to >= state.art.length || from === to) return;
      focus = action && id ? { id, action } : null;
      const order = ids();
      const [x] = order.splice(from, 1);
      order.splice(to, 0, x);
      return run((sid) =>
        api(sessionUrl(sid, "/art/order"), { method: "PUT", headers: JSON_HEADERS, body: JSON.stringify({ order }) })
      );
    }

    return {
      apply,
      reset,
      ids,
      render,
      count: () => state.art.length,
      setProblems: (p) => {
        state.problems = p;
        render();
      },
    };
  }

  // =====================================================================================
  // Modal 1: Add media (one audio file, optional cover images)
  // =====================================================================================
  const one = {
    file: null,
    duration: null,
    sid: null,
    fileId: null,
    uploading: false, // an upload is in flight
    finishing: false,
    upload: null,
    gen: 0, // bumped by every reset: a stale async continuation must not touch a newer dialog
    opener: null,
  };

  // Shared paste box + live preview (ui.js); the same component drives "Edit timestamps".
  const onePaste = Ui.cuePaste({
    textarea: $("add-cue-text"),
    box: $("add-preview"),
    getDuration: () => one.duration,
    active: () => oneCuesVisible(),
  });

  const oneArt = artSection("add", {
    sid: () => one.sid,
    locked: () => one.uploading || one.finishing,
    error: (m) => oneSetError(m),
    expired: () => oneExpired(),
  });

  const oneKind = () => document.querySelector('input[name="add-kind"]:checked').value;

  function oneSetError(msg) {
    $("add-error").textContent = msg || "";
  }

  function oneUpdateOk() {
    const staged = one.fileId != null;
    $("add-ok").disabled = !staged || one.uploading || one.finishing;
    $("add-hint").classList.toggle("hidden", staged || one.uploading || !oneArt.count());
    $("add-pick-btn").disabled = one.uploading || one.finishing;
    $("add-drop").classList.toggle("disabled", one.uploading || one.finishing);
    for (const r of document.querySelectorAll('input[name="add-kind"]')) r.disabled = one.finishing;
    $("add-cue-text").disabled = one.finishing;
  }

  function oneReset() {
    one.gen++;
    one.file = null;
    one.duration = null;
    one.sid = null;
    one.fileId = null;
    one.uploading = false;
    one.finishing = false;
    one.upload = null;
    onePaste.reset();
    $("add-file-line").classList.add("hidden");
    clear($("add-file-line"));
    document.querySelector('input[name="add-kind"][value="music"]').checked = true;
    $("add-cue-text").value = "";
    clear($("add-preview"));
    $("add-cues").classList.add("hidden");
    $("add-progress-box").classList.add("hidden");
    $("add-progress").value = 0;
    $("add-progress-text").textContent = "";
    oneArt.reset();
    oneSetError("");
    oneUpdateOk();
  }

  /** Throw away the session (the staged file and its art go with it). */
  function oneDiscard() {
    const sid = one.sid;
    one.sid = null;
    one.fileId = null;
    return discardById(sid);
  }

  function openOne(opener) {
    const dlg = $("add-dialog");
    if (dlg.open) return;
    one.opener = opener || null;
    oneReset();
    loadDeviceFormats();
    dlg.showModal();
    $("add-pick-btn").focus();
  }

  /** The server forgot the session (expired, restarted): what was staged is gone. */
  function oneExpired() {
    one.sid = null;
    one.fileId = null;
    one.file = null;
    $("add-file-line").classList.add("hidden");
    oneArt.reset();
    oneSetError("The import expired on the device. Add the file again.");
    oneUpdateOk();
  }

  function oneTake(files) {
    if (one.uploading || one.finishing) return;
    oneSetError("");
    let audio = files.filter((f) => Ui.looksAudio(f));
    const images = files.filter((f) => !Ui.looksAudio(f) && looksImage(f));
    const other = files.filter((f) => !Ui.looksAudio(f) && !looksImage(f));
    if (audio.length > 1) {
      oneSetError("Only one audio file here. Use “Add multiple media” to import several files at once.");
      return;
    }
    if (other.length) {
      oneSetError(`“${other[0].name}” does not look like an audio file.`);
      if (!audio.length && !images.length) return;
    }
    if (audio.length === 1) {
      const why = unplayableReason(audio[0].name);
      if (why) {
        oneSetError(`“${audio[0].name}” was not added: ${why}.`);
        audio = [];
        if (!images.length) return;
      }
    }
    const keepError = $("add-error").textContent;
    oneRun(audio[0] || null, images).then(() => {
      if (keepError && !$("add-error").textContent) oneSetError(keepError);
    });
  }

  async function oneStartSession() {
    if (one.sid) return;
    const s = await api("/api/imports", { method: "POST", headers: JSON_HEADERS, body: JSON.stringify({ single: true }) });
    one.sid = s.id;
  }

  /** Upload the audio file (replacing a staged one), then each image, as the user dropped them. */
  async function oneRun(audio, images) {
    const gen = one.gen;
    const fresh = () => gen === one.gen;
    one.uploading = true;
    oneUpdateOk();
    oneArt.render();
    $("add-progress-box").classList.remove("hidden");
    const problems = [];
    try {
      await oneStartSession();
      if (audio) {
        if (one.fileId != null) {
          // The one-file session takes one audio file: take the staged one back out first.
          const res = await api(sessionUrl(one.sid, `/files/${one.fileId}`), { method: "DELETE" });
          one.fileId = null;
          if (fresh()) oneArt.apply(res);
        }
        one.file = audio;
        one.duration = null;
        const line = $("add-file-line");
        line.classList.remove("hidden");
        line.textContent = `${audio.name} · ${fmtBytes(audio.size)}`;
        // The browser can often read the length cheaply; it lets the preview flag times past the end.
        Ui.audioDuration(audio).then((ms) => {
          if (!fresh() || one.file !== audio) return;
          one.duration = ms;
          if (ms) line.textContent = `${audio.name} · ${fmtBytes(audio.size)} · ${fmtHms(ms)}`;
          onePaste.soon(0);
        });
        try {
          const r = await oneSend(audio);
          if (!fresh()) return;
          one.fileId = r.file_id;
          oneArt.apply(r);
        } catch (e) {
          if (e.aborted || !fresh()) return;
          if (e.status === 404) throw e;
          one.file = null;
          line.classList.add("hidden");
          problems.push(e.message);
        }
      }
      for (const img of images) {
        try {
          const r = await oneSend(img);
          if (!fresh()) return;
          oneArt.apply(r);
        } catch (e) {
          if (e.aborted || !fresh()) return;
          if (e.status === 404) throw e;
          problems.push(`“${img.name}” was not added: ${e.message}`);
        }
      }
    } catch (e) {
      if (e.aborted || !fresh()) return;
      if (e.status === 404) oneExpired();
      else problems.push(e.message);
    } finally {
      if (fresh()) {
        one.uploading = false;
        one.upload = null;
        $("add-progress-box").classList.add("hidden");
        if (problems.length) oneSetError(problems.join(" "));
        oneUpdateOk();
        oneArt.render();
      }
    }
  }

  function oneSend(file) {
    oneSetProgress(0, file.size || 1);
    one.upload = Ui.upload(sessionUrl(one.sid, `/files?name=${encodeURIComponent(file.name)}`), file, oneSetProgress);
    return one.upload.promise;
  }

  function oneCuesVisible() {
    return LONG_FORM.has(oneKind());
  }

  function oneSetProgress(loaded, total) {
    const pct = total ? Math.round((loaded / total) * 100) : 0;
    $("add-progress").value = pct;
    $("add-progress-text").textContent =
      loaded >= total ? "Processing on the device…" : `${fmtBytes(loaded)} of ${fmtBytes(total)} (${pct}%)`;
  }

  async function oneSubmit() {
    if (one.fileId == null || one.uploading || one.finishing) return;
    const kind = oneKind();
    const cue = LONG_FORM.has(kind) ? $("add-cue-text").value : "";
    if (new Blob([cue]).size > MAX_CUE_BYTES) {
      oneSetError("That tracklist is too long to import together with the file. Trim it.");
      return;
    }
    const body = { kind, art: oneArt.ids() };
    if (cue.trim()) body.cue_text = cue;
    oneSetError("");
    const gen = one.gen;
    one.finishing = true;
    oneUpdateOk();
    oneArt.render();
    $("add-progress-box").classList.remove("hidden");
    $("add-progress").removeAttribute("value");
    $("add-progress-text").textContent = "Adding to your library…";
    try {
      const created = await api(sessionUrl(one.sid, "/finish"), {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify(body),
      });
      one.sid = null; // the server closed it
      one.finishing = false;
      $("add-dialog").close();
      const cues = created && created.cues ? created.cues.cue_count : 0;
      const warn = created && created.cues && created.cues.warnings ? created.cues.warnings.length : 0;
      toast(
        `Added “${created.title || (one.file && one.file.name) || "file"}”` +
          (cues ? ` with ${cues} ${cues === 1 ? "cue" : "cues"}` : "") +
          (warn ? ` (${warn} ${warn === 1 ? "note" : "notes"})` : "")
      );
      refreshStatus().catch(() => {});
      goTo(`#/${TAB_OF_KIND[created && (created.group || created.kind)] || KIND_TAB[kind] || "tracks"}`);
      // Open its details panel so the upload can be checked (title, association, playlists).
      openDetail(idStr(created.id)).catch((e) => showBanner(e.message));
    } catch (e) {
      if (gen !== one.gen) return;
      one.finishing = false;
      if (e.status === 404 && e.code === "unknown_import") oneExpired();
      else oneSetError(e.message);
    } finally {
      if (gen === one.gen) {
        one.finishing = false;
        $("add-progress-box").classList.add("hidden");
        $("add-progress").value = 0;
        oneUpdateOk();
        oneArt.render();
      }
    }
  }

  function oneCancel() {
    if (one.upload && one.uploading) one.upload.abort();
    if ($("add-dialog").open) $("add-dialog").close();
  }

  function wireOne() {
    const dlg = $("add-dialog");
    Ui.modal(dlg, () => one.opener);
    const dz = Ui.dropZone($("add-drop"), $("add-file"), oneTake);
    $("add-pick-btn").addEventListener("click", dz.open);
    $("add-drop").addEventListener("keydown", (ev) => {
      if ((ev.key === "Enter" || ev.key === " ") && ev.target === $("add-drop")) {
        ev.preventDefault();
        dz.open();
      }
    });
    for (const r of document.querySelectorAll('input[name="add-kind"]')) {
      r.addEventListener("change", () => {
        $("add-cues").classList.toggle("hidden", !oneCuesVisible());
        onePaste.soon(0);
      });
    }
    $("add-cue-text").addEventListener("input", () => onePaste.soon(300));
    $("add-ok").addEventListener("click", () => oneSubmit());
    $("add-cancel").addEventListener("click", oneCancel);
    // Esc / ✕ / Cancel: stop an upload in flight; closing discards the staged session.
    dlg.addEventListener("cancel", () => {
      if (one.upload && one.uploading) one.upload.abort();
    });
    dlg.addEventListener("close", () => {
      if (one.upload && one.uploading) one.upload.abort();
      one.gen++;
      one.uploading = false;
      one.finishing = false;
      oneDiscard();
    });
    $("add-open-btn").addEventListener("click", (ev) => openOne(ev.currentTarget));
  }

  // =====================================================================================
  // Modal 2: Add multiple media
  // =====================================================================================
  const multi = {
    items: [], // audio rows {key, file, status: pending|uploading|done|failed, fileId, error, loaded}
    images: [], // dropped image files {key, file, status: pending|uploading|done|failed, error}
    session: null,
    running: false, // the upload pump is going
    finishing: false,
    upload: null,
    gen: 0,
    cancelled: false,
    phase: "idle", // idle | failed (some uploads failed) | finish (all uploaded, finish failed)
    nameTouched: false,
    nextKey: 1,
    opener: null,
    focus: null, // {key, action} to restore after a re-render
  };

  const multiMode = () => document.querySelector('input[name="addmulti-mode"]:checked').value;

  const multiArt = artSection("addmulti", {
    sid: () => multi.session,
    locked: () => multi.running || multi.finishing,
    error: (m) => multiSetError(m),
    expired: () => multiExpired(),
  });

  const MODE_HINT = {
    album:
      "The files are copied into Music/<artist>/<name>/ together with one _<name>.noudata that lists them in the order below.",
    playlist:
      "Each file becomes its own track in your library; Playlists/_<name>.noudata lists them in the order below.",
  };

  const multiBusy = () => multi.running || multi.finishing;
  /** Rows that are sent to the device. An `unsupported` row is listed but never uploaded. */
  const allUploads = () => multi.items.filter((it) => it.status !== "unsupported").concat(multi.images);

  function multiSetError(msg) {
    $("addmulti-error").textContent = msg || "";
  }

  function multiTotals() {
    const all = allUploads();
    const total = all.reduce((a, it) => a + (it.file.size || 0), 0);
    const done = all.reduce(
      (a, it) => a + (it.status === "done" ? it.file.size || 0 : it.status === "uploading" ? it.loaded || 0 : 0),
      0
    );
    const filesDone = all.filter((it) => it.status === "done").length;
    return { total, done, filesDone, count: all.length };
  }

  function multiProgress() {
    const { total, done, filesDone, count } = multiTotals();
    const pct = total ? Math.round((done / total) * 100) : 0;
    $("addmulti-progress").value = pct;
    $("addmulti-progress-text").textContent =
      `${filesDone} of ${count} files · ${fmtBytes(done)} of ${fmtBytes(total)} (${pct}%)`;
  }

  function multiUpdateControls() {
    const all = allUploads();
    const failed = all.filter((it) => it.status === "failed").length;
    const pending = all.filter((it) => it.status === "pending").length;
    const audioDone = multi.items.filter((it) => it.status === "done").length;
    const named = !!$("addmulti-name").value.trim();
    const busy = multiBusy();
    const ok = $("addmulti-ok");
    ok.textContent = failed ? "Retry failed files" : pending ? "Upload again" : "Import";
    // The confirmation needs a name and at least one audio file that is really staged.
    ok.disabled = busy || (failed || pending ? false : !(named && audioDone));
    const skip = $("addmulti-skipfailed");
    skip.classList.toggle("hidden", !(failed && !busy));
    skip.disabled = !(named && audioDone);
    $("addmulti-hint").classList.toggle("hidden", !(multiArt.count() && !audioDone && !busy));
    for (const id of ["addmulti-pick-btn", "addmulti-name"]) $(id).disabled = busy;
    for (const r of document.querySelectorAll('input[name="addmulti-mode"]')) r.disabled = busy;
    $("addmulti-sort-btn").disabled = busy || multi.items.length < 2;
    $("addmulti-drop").classList.toggle("disabled", busy);
  }

  function multiReset() {
    multi.gen++;
    multi.items = [];
    multi.images = [];
    multi.session = null;
    multi.running = false;
    multi.finishing = false;
    multi.cancelled = false;
    multi.upload = null;
    multi.phase = "idle";
    multi.nameTouched = false;
    document.querySelector('input[name="addmulti-mode"][value="album"]').checked = true;
    $("addmulti-name").value = "";
    $("addmulti-skipped").textContent = "";
    $("addmulti-unplayable").textContent = "";
    $("addmulti-progress-box").classList.add("hidden");
    multiArt.reset();
    multiSetError("");
    $("addmulti-mode-hint").textContent = MODE_HINT.album;
    renderMultiList();
    multiUpdateControls();
  }

  function openMulti(opener) {
    const dlg = $("addmulti-dialog");
    if (dlg.open) return;
    multi.opener = opener || null;
    multiReset();
    loadDeviceFormats();
    dlg.showModal();
    $("addmulti-pick-btn").focus();
  }

  function multiTake(files) {
    if (multiBusy()) return;
    const skipped = [];
    const batch = [];
    const imgs = [];
    const same = (it, f) => it.file.name === f.name && it.file.size === f.size && it.file.lastModified === f.lastModified;
    for (const f of files) {
      if (Ui.looksAudio(f)) {
        if (multi.items.some((it) => same(it, f)) || batch.some((it) => same(it, f))) continue;
        const why = unplayableReason(f.name);
        batch.push({
          key: multi.nextKey++,
          file: f,
          status: why ? "unsupported" : "pending",
          fileId: null,
          error: why,
          loaded: 0,
        });
      } else if (looksImage(f)) {
        // Dropping the same image file again is a deliberate "bring it back": always sent.
        imgs.push({ key: multi.nextKey++, file: f, status: "pending", error: "", loaded: 0 });
      } else {
        skipped.push(f.name);
      }
    }
    // FIX_01A: only the incoming batch is sorted, so rows already in the list keep the user's
    // order. The name prefill below still uses the batch's first file as handed over (one folder).
    const first = batch[0];
    batch.sort((a, b) => Ui.naturalCompare(a.file.name, b.file.name));
    multi.items.push(...batch);
    multi.images.push(...imgs);
    if (multi.items.length > 500) {
      multi.items.length = 500;
      $("addmulti-skipped").textContent = "At most 500 files per import; the rest were left out.";
    } else {
      $("addmulti-skipped").textContent = skipped.length
        ? `Skipped (not audio or image): ${skipped.slice(0, 5).join(", ")}${skipped.length > 5 ? ` and ${skipped.length - 5} more` : ""}`
        : "";
    }
    // Prefill the name from the folder of the first file when the browser tells us one.
    const nameFrom = first || multi.items[0];
    if (!multi.nameTouched && !$("addmulti-name").value.trim() && nameFrom) {
      const rel = nameFrom.file.webkitRelativePath || "";
      const parts = rel.split("/");
      if (parts.length > 1) $("addmulti-name").value = parts[parts.length - 2];
    }
    multi.phase = "idle";
    renderMultiList();
    multiUpdateControls();
    if (batch.some((it) => it.status === "pending") || imgs.length) multiPump();
  }

  /** The files this player cannot decode, by name (not just a count). */
  function multiUnplayableNote() {
    const names = multi.items.filter((it) => it.status === "unsupported").map((it) => it.file.name);
    if (!names.length) return "";
    const shown = names.slice(0, 5).join(", ");
    const more = names.length > 5 ? ` and ${names.length - 5} more` : "";
    return `${names.length} ${names.length === 1 ? "file" : "files"} this Plippa cannot play: ${shown}${more}`;
  }

  /** FIX_01A: re-sort the whole list (e.g. after several drops) with the same comparator. */
  function multiSort() {
    if (multiBusy() || multi.items.length < 2) return;
    multi.items.sort((a, b) => Ui.naturalCompare(a.file.name, b.file.name));
    renderMultiList();
    $("addmulti-sort-btn").focus();
  }

  function multiMove(from, to) {
    if (multiBusy() || from === to || to < 0 || to >= multi.items.length) return;
    const [it] = multi.items.splice(from, 1);
    multi.items.splice(to, 0, it);
    renderMultiList();
  }

  /** Remove a row. An uploaded file is also taken out of the session on the device (its staged
   *  copy is deleted and the art only it carried disappears; the server returns the new list). */
  async function multiRemove(i) {
    if (multiBusy()) return;
    const it = multi.items[i];
    if (!it) return;
    if (it.status === "done" && multi.session) {
      multi.finishing = true; // locks the list while the request runs
      renderMultiList();
      const gen = multi.gen;
      try {
        multiArt.apply(await api(sessionUrl(multi.session, `/files/${it.fileId}`), { method: "DELETE" }));
      } catch (e) {
        if (gen !== multi.gen) return;
        multi.finishing = false;
        if (e.status === 404 && e.code === "unknown_import") multiExpired();
        else if (e.status !== 404) {
          multiSetError(e.message);
          renderMultiList();
          return;
        }
      }
      if (gen !== multi.gen) return;
      multi.finishing = false;
    }
    const at = multi.items.indexOf(it);
    if (at >= 0) multi.items.splice(at, 1);
    if (!multi.items.length) multi.phase = "idle";
    renderMultiList();
    multiProgress();
  }

  function statusText(it) {
    switch (it.status) {
      case "uploading": {
        const pct = it.file.size ? Math.round(((it.loaded || 0) / it.file.size) * 100) : 0;
        return `Uploading ${pct}%`;
      }
      case "done":
        return "Uploaded";
      case "failed":
        return `Failed: ${it.error}`;
      case "unsupported":
        return `Not added: ${it.error}`;
      default:
        return multi.running ? "Waiting" : "";
    }
  }

  function renderMultiList() {
    const list = $("addmulti-list");
    clear(list);
    const n = multi.items.length;
    let track = 0; // numbering counts only the files that will be imported
    multi.items.forEach((it, i) => {
      const unsupported = it.status === "unsupported";
      if (!unsupported) track++;
      const locked = multiBusy();
      const btn = (label, aria, action, disabled, handler) =>
        el("button", {
          text: label,
          cls: "icon",
          attrs: {
            type: "button",
            "aria-label": aria,
            "data-key": String(it.key),
            "data-action": action,
            ...(disabled ? { disabled: "" } : {}),
          },
          on: { click: handler },
        });
      const li = el(
        "li",
        {
          cls: `multi-item ${it.status}`,
          attrs: { draggable: locked ? "false" : "true", "data-key": String(it.key) },
        },
        [
          el("span", { cls: "grip", text: "⠿", attrs: { "aria-hidden": "true", title: "Drag to reorder" } }),
          el("span", { cls: "item-no", text: unsupported ? "–" : String(track) }),
          el("span", { cls: "multi-name" }, [
            el("span", { cls: "multi-fname", text: it.file.name }),
            el("span", { cls: "muted multi-size", text: fmtBytes(it.file.size) }),
          ]),
          el("span", { cls: `multi-status ${it.status}`, text: statusText(it) }),
          el("span", { cls: "row-actions" }, [
            btn("↑", `Move ${it.file.name} up`, "up", locked || unsupported || i === 0, () => {
              multi.focus = { key: it.key, action: "up" };
              multiMove(i, i - 1);
            }),
            btn("↓", `Move ${it.file.name} down`, "down", locked || unsupported || i === n - 1, () => {
              multi.focus = { key: it.key, action: "down" };
              multiMove(i, i + 1);
            }),
            btn("✕", `Remove ${it.file.name}`, "remove", locked, () => {
              multi.focus = null;
              multiRemove(i);
            }),
          ]),
        ]
      );
      // Pointer/mouse reorder (HTML5 drag and drop); the arrows above are the keyboard/touch path.
      li.addEventListener("dragstart", (ev) => {
        if (multiBusy()) return ev.preventDefault();
        ev.dataTransfer.setData("text/x-plippa-index", String(i));
        ev.dataTransfer.effectAllowed = "move";
        li.classList.add("dragging");
      });
      li.addEventListener("dragend", () => {
        li.classList.remove("dragging");
        for (const x of list.querySelectorAll(".drop-target")) x.classList.remove("drop-target");
      });
      li.addEventListener("dragover", (ev) => {
        if (!Array.from(ev.dataTransfer.types || []).includes("text/x-plippa-index")) return;
        ev.preventDefault();
        li.classList.add("drop-target");
      });
      li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
      li.addEventListener("drop", (ev) => {
        const from = Number(ev.dataTransfer.getData("text/x-plippa-index"));
        if (Number.isNaN(from)) return;
        ev.preventDefault();
        multiMove(from, i);
      });
      list.appendChild(li);
    });
    if (multi.focus) {
      const f = multi.focus;
      multi.focus = null;
      const target = list.querySelector(`button[data-key="${f.key}"][data-action="${f.action}"]:not([disabled])`);
      if (target) target.focus();
    }
    multiArt.setProblems(
      multi.images.filter((x) => x.status === "failed").map((x) => `“${x.file.name}” was not added: ${x.error}`)
    );
    $("addmulti-unplayable").textContent = multiUnplayableNote();
    multiUpdateControls();
  }

  /** The server forgot the session: everything uploaded so far is gone and must be sent again. */
  function multiExpired() {
    multi.session = null;
    for (const x of allUploads()) if (x.status === "done") x.status = "pending";
    multiArt.reset();
    multi.phase = "idle";
    multiSetError("The import expired on the device. Press “Upload again” to send the files again.");
    renderMultiList();
  }

  async function discardSession() {
    const sid = multi.session;
    multi.session = null;
    await discardById(sid);
  }

  /** Send every waiting file (audio rows in list order, then dropped images) to the session. */
  async function multiPump() {
    if (multiBusy()) return;
    const gen = multi.gen;
    const live = () => gen === multi.gen && !multi.cancelled;
    multi.running = true;
    multiSetError("");
    $("addmulti-progress-box").classList.remove("hidden");
    renderMultiList();
    multiProgress();
    try {
      if (!multi.session) {
        try {
          const s = await api("/api/imports", { method: "POST" });
          if (!live()) return discardById(s.id);
          multi.session = s.id;
          multiArt.reset();
        } catch (e) {
          if (!live()) return;
          for (const it of allUploads()) {
            if (it.status === "pending") {
              it.status = "failed";
              it.error = e.message;
            }
          }
          multi.phase = "failed";
          return;
        }
      }
      for (const it of allUploads()) {
        if (it.status !== "pending") continue;
        if (!live()) return;
        const isAudio = multi.items.includes(it);
        it.status = "uploading";
        it.loaded = 0;
        renderMultiList();
        const url = sessionUrl(multi.session, `/files?name=${encodeURIComponent(it.file.name)}`);
        multi.upload = Ui.upload(url, it.file, (loaded) => {
          it.loaded = loaded;
          multiProgress();
          // update only this row's status text (a full re-render per progress event is wasteful)
          const row = $("addmulti-list").querySelector(`li[data-key="${it.key}"] .multi-status`);
          if (row) row.textContent = statusText(it);
        });
        try {
          const r = await multi.upload.promise;
          if (!live()) return;
          it.status = "done";
          it.error = "";
          if (isAudio) it.fileId = r.file_id;
          multiArt.apply(r);
          if (isAudio && !multi.nameTouched && !$("addmulti-name").value.trim() && r.album) {
            $("addmulti-name").value = r.album;
          }
        } catch (e) {
          if (e.aborted || !live()) return;
          if (e.status === 404) {
            // The session vanished (expired / server restarted): what was uploaded is gone.
            multi.session = null;
            for (const x of allUploads()) if (x.status === "done") x.status = "pending";
            multiArt.reset();
            it.status = "failed";
            it.error = "the import expired on the device, press Retry to upload again";
            multi.phase = "failed";
            break;
          }
          it.status = "failed";
          it.error = e.message;
        } finally {
          multi.upload = null;
        }
        renderMultiList();
        multiProgress();
      }
      const failed = allUploads().filter((it) => it.status === "failed");
      if (failed.length) {
        multi.phase = "failed";
        multiSetError(
          `${failed.length} ${failed.length === 1 ? "file" : "files"} could not be uploaded. The others are waiting on the device: retry the failed files, or continue without them.`
        );
      }
    } finally {
      if (gen === multi.gen) {
        multi.running = false;
        multi.upload = null;
        renderMultiList();
        multiProgress();
      }
    }
  }

  /** OK: retry failed / expired uploads first; otherwise confirm the import. */
  function multiOk() {
    const all = allUploads();
    if (all.some((it) => it.status === "failed" || it.status === "pending")) {
      for (const it of all) {
        if (it.status === "failed") {
          it.status = "pending";
          it.error = "";
        }
      }
      return multiPump();
    }
    return multiFinish();
  }

  /** "Continue without failed files": forget the failed rows, then confirm. */
  function multiSkipFailed() {
    multi.items = multi.items.filter((it) => it.status !== "failed");
    multi.images = multi.images.filter((it) => it.status !== "failed");
    multi.phase = "idle";
    multiSetError("");
    renderMultiList();
    return multiFinish();
  }

  async function multiFinish() {
    if (multiBusy()) return;
    const name = $("addmulti-name").value.trim();
    const chosen = multi.items.filter((it) => it.status === "done");
    if (!name || !chosen.length || !multi.session) return;
    const mode = multiMode();
    const gen = multi.gen;
    multi.finishing = true;
    multiSetError("");
    multi.phase = "finish";
    renderMultiList();
    try {
      const res = await api(sessionUrl(multi.session, "/finish"), {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({ mode, name, order: chosen.map((it) => it.fileId), art: multiArt.ids() }),
      });
      multi.session = null; // the server closed it
      multi.phase = "idle";
      multi.finishing = false;
      $("addmulti-dialog").close();
      toast(
        `Created ${mode === "album" ? "album" : "playlist"} “${res.title || name}” with ${res.tracks} ${res.tracks === 1 ? "track" : "tracks"}`
      );
      refreshStatus().catch(() => {});
      goTo(`#/${mode === "album" ? "albums" : "playlists"}/${encodeURIComponent(res.id)}`);
      // Open its details panel so the import can be checked (title, association, playlists).
      openDetail(idStr(res.id)).catch((e) => showBanner(e.message));
    } catch (e) {
      if (gen !== multi.gen) return;
      if (e.status === 404 && e.code === "unknown_import") {
        multi.finishing = false;
        multiExpired();
      } else {
        // A refused finish undoes every move (the files go back to staging), so say plainly
        // that nothing was created.
        multiSetError(
          e.status && e.status >= 400
            ? `${e.message} — nothing was created and no file was left in the library; fix that and press "Import" again.`
            : e.message
        );
      }
    } finally {
      if (gen === multi.gen) {
        multi.finishing = false;
        renderMultiList();
        multiProgress();
      }
    }
  }

  /** Cancel / Esc / ✕: stop uploads, discard the session. `false` when the user keeps the dialog. */
  async function multiCancel(fromEsc) {
    if (multi.running) {
      // eslint-disable-next-line no-alert
      if (!confirm("Cancel the import? Files uploaded so far are discarded.")) return false;
      multi.cancelled = true;
      if (multi.upload) multi.upload.abort();
    }
    multi.gen++;
    multi.running = false;
    multi.finishing = false;
    const gone = discardSession(); // (closing first: the dialog must not wait for the network)
    if (!fromEsc && $("addmulti-dialog").open) $("addmulti-dialog").close();
    await gone;
    return true;
  }

  function wireMulti() {
    const dlg = $("addmulti-dialog");
    Ui.modal(dlg, () => multi.opener);
    const dz = Ui.dropZone($("addmulti-drop"), $("addmulti-file"), multiTake);
    $("addmulti-pick-btn").addEventListener("click", dz.open);
    $("addmulti-drop").addEventListener("keydown", (ev) => {
      if ((ev.key === "Enter" || ev.key === " ") && ev.target === $("addmulti-drop")) {
        ev.preventDefault();
        dz.open();
      }
    });
    for (const r of document.querySelectorAll('input[name="addmulti-mode"]')) {
      r.addEventListener("change", () => {
        $("addmulti-mode-hint").textContent = MODE_HINT[multiMode()];
      });
    }
    $("addmulti-name").addEventListener("input", () => {
      multi.nameTouched = true;
      multiUpdateControls();
    });
    $("addmulti-sort-btn").addEventListener("click", multiSort);
    $("addmulti-ok").addEventListener("click", () => multiOk());
    $("addmulti-skipfailed").addEventListener("click", () => multiSkipFailed());
    $("addmulti-cancel").addEventListener("click", () => multiCancel(false));
    // Esc: while uploading ask first; an uploaded-but-unfinished session is discarded on close.
    dlg.addEventListener("cancel", (ev) => {
      if (multi.running) {
        ev.preventDefault();
        multiCancel(false);
      }
    });
    dlg.addEventListener("close", () => {
      multi.gen++;
      multi.running = false;
      multi.finishing = false;
      if (multi.session) discardSession();
    });
    $("addmulti-open-btn").addEventListener("click", (ev) => openMulti(ev.currentTarget));
  }

  // =====================================================================================
  // Leaving the page
  // =====================================================================================

  /** Uploads in flight are lost on a reload: let the browser ask first. */
  window.addEventListener("beforeunload", (ev) => {
    if (one.uploading || one.finishing || multi.running || multi.finishing) {
      ev.preventDefault();
      ev.returnValue = "";
    }
  });

  /** Navigating away or closing the tab: free the staging on the stick (the TTL is the fallback). */
  window.addEventListener("pagehide", () => {
    for (const sid of [one.sid, multi.session]) {
      if (!sid) continue;
      try {
        fetch(sessionUrl(sid), { method: "DELETE", keepalive: true, headers: { "X-Plippa": "1" } }).catch(() => {});
      } catch {
        /* best effort */
      }
    }
  });

  function init() {
    wireOne();
    wireMulti();
  }

  return { init, openOne, openMulti };
})();

document.addEventListener("DOMContentLoaded", () => AddMedia.init());
