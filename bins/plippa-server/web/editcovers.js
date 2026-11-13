/* Plippa media portal — "Edit cover art" dialog (ART_06).

   One dialog for every collection and loose track: the tiles are the images the item shows now
   (GET /api/collections/:id/covers, in order). The user reorders (drag, or the arrow buttons),
   removes (✕) and adds images (file picker, or files dropped on the dialog). Each added file is
   uploaded at once to POST …/covers/staged (one at a time: the server has one upload slot) and
   becomes a "New" tile. Nothing else is written until Save: PUT …/covers {order:[{keep:n}|
   {staged:id}]} replaces the list in one step. Cancel / Esc / close forget the staged ids (the
   server sweeps them) and write nothing.

   No raster editing: images are shown cropped to a square, the files are never changed.
   Everything from the server (names, errors, alt text) goes in through textContent /
   setAttribute only (NBSecurityRequirements §5.1). Relies on globals: Ui (ui.js), Covers
   (covers.js), api/$ (app.js). */

const EditCovers = (() => {
  const { el, toast } = Ui;
  const MAX_IMAGES = 12; // matches the server
  const MAX_BYTES = 16 * 1024 * 1024;
  const TYPES = new Map([
    ["image/jpeg", true],
    ["image/png", true],
    ["image/webp", true],
  ]);
  const EXT = /\.(jpe?g|png|webp)$/i;
  const BUSY_RETRY_MS = 1200;
  const BUSY_RETRIES = 25;
  const HIDES_ARTIST = "This hides the artist's image; the file stays on the stick.";

  /** @type {{id:string,title:string,opts:object,tiles:object[],removed:object[],orig:string,loaded:boolean,saving:boolean,uploading:object|null,seq:number,dragFrom:object|null}} */
  const st = { id: null, title: "", opts: null, tiles: [], removed: [], orig: "", loaded: false, hidden: 0, saving: false, uploading: null, seq: 0, dragFrom: null };
  let tileSeq = 0;
  let opener = null; // element to give the focus back to when the dialog closes

  const dlg = () => $("covers-dialog");

  // ---- state helpers ----------------------------------------------------------------------

  const isBusy = (t) => t.status === "queued" || t.status === "uploading";
  const live = (t) => t.status !== "failed";
  const signature = () =>
    st.tiles
      .filter((t) => t.status === "ready")
      .map((t) => (t.kind === "keep" ? `k${t.n}` : `s${t.staged}`))
      .join(",");
  const changed = () => st.tiles.some(isBusy) || signature() !== st.orig;

  function setError(msg) {
    $("covers-error").textContent = msg || "";
  }

  function updateButtons() {
    const busy = st.tiles.some(isBusy);
    const save = $("covers-save");
    save.disabled = !st.loaded || st.saving || busy || !changed();
    save.textContent = st.saving ? "Saving…" : "Save";
    $("covers-cancel").disabled = st.saving;
    const full = st.tiles.filter(live).length >= MAX_IMAGES;
    $("covers-pick-btn").disabled = !st.loaded || st.saving || full;
    $("covers-drop").classList.toggle("disabled", !st.loaded || st.saving || full);
    $("covers-limit").classList.toggle("hidden", !full);
    $("covers-empty").classList.toggle("hidden", !st.loaded || st.tiles.length > 0);
    const note = $("covers-note");
    const hides = st.removed.some((t) => t.source === "release");
    note.textContent = hides ? HIDES_ARTIST : "";
    note.classList.toggle("hidden", !hides);
    const unshown = $("covers-unshown");
    unshown.textContent = st.hidden > 0 ? `${st.hidden} image${st.hidden === 1 ? "" : "s"} on the stick can't be shown here and ${st.hidden === 1 ? "is" : "are"} kept.` : "";
    unshown.classList.toggle("hidden", !(st.loaded && st.hidden > 0));
  }

  /** Tell the server to drop an upload that will not be saved (frees its slot at once). Best
   *  effort: the server's TTL cleans up whatever this misses. */
  function discardStaged(t) {
    if (t.kind !== "new" || t.status !== "ready" || !t.staged) return;
    api(`/api/collections/${encodeURIComponent(st.id)}/covers/staged/${encodeURIComponent(t.staged)}`, { method: "DELETE" }).catch(() => {});
  }

  // ---- tiles ------------------------------------------------------------------------------

  /** Build the permanent <li> of a tile; `layout()` keeps its labels and buttons current. */
  function makeTile(init) {
    const t = { id: ++tileSeq, kind: "keep", status: "ready", source: "", ...init };
    const li = el("li", { cls: "cover-tile", attrs: { draggable: "true" } });
    t.li = li;
    t.thumbHost = el("div", { cls: "tile-thumb" });
    t.badges = el("div", { cls: "tile-badges" });
    t.info = el("p", { cls: "tile-info muted" });
    t.msg = el("p", { cls: "tile-msg", attrs: { "aria-live": "polite" } });
    t.btnBefore = el("button", { cls: "icon", text: "←", attrs: { type: "button" }, on: { click: () => move(t, -1) } });
    t.btnAfter = el("button", { cls: "icon", text: "→", attrs: { type: "button" }, on: { click: () => move(t, 1) } });
    t.btnRemove = el("button", { cls: "icon danger", text: "✕", attrs: { type: "button" }, on: { click: () => remove(t) } });
    t.btnRetry = el("button", { cls: "tile-retry hidden", text: "Retry", attrs: { type: "button" }, on: { click: () => retry(t) } });
    const bar = el("div", { cls: "tile-actions" }, [t.btnBefore, t.btnAfter, t.btnRemove]);
    li.append(t.thumbHost, t.badges, t.info, t.msg, t.btnRetry, bar);
    setThumb(t, t.thumbUrl || null);
    wireDrag(t);
    return t;
  }

  function setThumb(t, url) {
    t.thumbUrl = url;
    t.thumbHost.replaceChildren(Covers.thumb(url, { size: 512, alt: "" }));
  }

  function badge(text, cls) {
    return el("span", { cls: `tile-badge ${cls}`, text });
  }

  /** Refresh labels, badges and enabled states of every tile, in order; (re)attach the <li>s. */
  function layout(focus) {
    const grid = $("covers-grid");
    const shown = st.tiles;
    const total = shown.length;
    shown.forEach((t, i) => {
      const pos = i + 1;
      const what = t.status === "failed" ? "Failed upload" : `Cover art ${pos} of ${total}`;
      t.li.setAttribute("aria-label", what);
      t.li.classList.toggle("failed", t.status === "failed");
      t.li.classList.toggle("busy", isBusy(t));
      const badges = [];
      if (i === 0 && live(t)) badges.push(badge("Primary", "primary"));
      if (t.kind === "new" && t.status !== "failed") badges.push(badge("New", "new"));
      t.badges.replaceChildren(...badges);
      const interactive = !isBusy(t);
      t.btnBefore.setAttribute("aria-label", `${what}, move earlier`);
      t.btnAfter.setAttribute("aria-label", `${what}, move later`);
      t.btnRemove.setAttribute("aria-label", t.status === "failed" ? `Remove failed upload ${t.name}` : `Remove cover art ${pos}`);
      t.btnBefore.disabled = i === 0 || !interactive || t.status === "failed" || st.saving;
      t.btnAfter.disabled = i === total - 1 || !interactive || t.status === "failed" || st.saving;
      t.btnRemove.disabled = st.saving;
      t.btnRetry.classList.toggle("hidden", t.status !== "failed" || !t.file);
      t.btnRetry.setAttribute("aria-label", `Retry uploading ${t.name || "image"}`);
      t.li.draggable = interactive && t.status !== "failed" && !st.saving;
      if (t.status === "ready") {
        const bits = [];
        if (t.w && t.h) bits.push(`${t.w}×${t.h}`);
        if (t.bytes != null) bits.push(Ui.fmtBytes(t.bytes));
        t.info.textContent = bits.join(" · ");
        t.msg.textContent = "";
      } else if (t.status === "queued") {
        t.info.textContent = t.name || "";
        t.msg.textContent = "Waiting to upload…";
      } else if (t.status === "failed") {
        t.info.textContent = t.name || "";
        t.msg.textContent = t.error || "Upload failed";
      }
    });
    const want = shown.map((t) => t.li);
    const have = [...grid.children];
    if (want.length !== have.length || want.some((n, i) => n !== have[i])) {
      grid.replaceChildren(...want);
    }
    if (focus) focus();
    updateButtons();
  }

  function move(t, delta) {
    const i = st.tiles.indexOf(t);
    const j = i + delta;
    if (i < 0 || j < 0 || j >= st.tiles.length) return;
    st.tiles.splice(i, 1);
    st.tiles.splice(j, 0, t);
    setError("");
    layout(() => {
      const mine = delta < 0 ? [t.btnBefore, t.btnAfter] : [t.btnAfter, t.btnBefore];
      (mine.find((b) => !b.disabled) || t.btnRemove).focus();
    });
  }

  function remove(t) {
    const i = st.tiles.indexOf(t);
    if (i < 0) return;
    if (t.upload) t.upload.abort();
    discardStaged(t);
    st.tiles.splice(i, 1);
    if (t.kind === "keep") st.removed.push(t);
    setError("");
    layout(() => {
      const next = st.tiles[Math.min(i, st.tiles.length - 1)];
      (next ? next.btnRemove : $("covers-pick-btn")).focus();
    });
  }

  // ---- drag to reorder ----------------------------------------------------------------------

  function wireDrag(t) {
    const li = t.li;
    li.addEventListener("dragstart", (ev) => {
      st.dragFrom = t;
      li.classList.add("dragging");
      if (ev.dataTransfer) {
        ev.dataTransfer.effectAllowed = "move";
        ev.dataTransfer.setData("text/plain", String(st.tiles.indexOf(t)));
      }
    });
    li.addEventListener("dragend", () => {
      st.dragFrom = null;
      li.classList.remove("dragging");
      dlg().querySelectorAll("li.drop-target").forEach((n) => n.classList.remove("drop-target"));
    });
    li.addEventListener("dragover", (ev) => {
      if (!st.dragFrom || st.dragFrom === t) return;
      ev.preventDefault();
      if (ev.dataTransfer) ev.dataTransfer.dropEffect = "move";
      li.classList.add("drop-target");
    });
    li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
    li.addEventListener("drop", (ev) => {
      if (!st.dragFrom) return; // a file drop: the dialog handles it
      ev.preventDefault();
      ev.stopPropagation();
      li.classList.remove("drop-target");
      const from = st.dragFrom;
      st.dragFrom = null;
      const a = st.tiles.indexOf(from);
      const b = st.tiles.indexOf(t);
      if (a < 0 || b < 0 || a === b) return;
      st.tiles.splice(a, 1);
      st.tiles.splice(b, 0, from);
      setError("");
      layout();
    });
  }

  // ---- adding images ------------------------------------------------------------------------

  function refusal(file) {
    const name = file.name || "file";
    const typeOk = TYPES.has(file.type) || (!file.type && EXT.test(name));
    if (!typeOk) return `“${name}” is not a JPEG, PNG or WebP image.`;
    if (file.size > MAX_BYTES) return `“${name}” is larger than 16 MB.`;
    if (file.size === 0) return `“${name}” is empty.`;
    return null;
  }

  function addFiles(files) {
    if (!st.loaded || st.saving || !dlg().open) return;
    const problems = [];
    for (const f of files) {
      const why = refusal(f);
      if (why) {
        problems.push(why);
        continue;
      }
      if (st.tiles.filter(live).length >= MAX_IMAGES) {
        problems.push(`At most ${MAX_IMAGES} images: “${f.name}” was not added.`);
        continue;
      }
      const t = makeTile({ kind: "new", status: "queued", name: f.name, file: f, progress: 0 });
      st.tiles.push(t);
    }
    setError(problems.join(" "));
    layout();
    pump();
  }

  /** Upload queued tiles one at a time (the server has a single upload slot). */
  function pump() {
    if (st.uploading || !dlg().open) return;
    const t = st.tiles.find((x) => x.status === "queued");
    if (!t) return;
    st.uploading = t;
    t.status = "uploading";
    t.msg.textContent = "Uploading… 0%";
    layout();
    const mine = st.seq;
    const url = `/api/collections/${encodeURIComponent(st.id)}/covers/staged?name=${encodeURIComponent(t.name || "image")}`;
    t.upload = Ui.upload(url, t.file, (loaded, total) => {
      if (mine === st.seq && t.status === "uploading") t.msg.textContent = `Uploading… ${Math.round((loaded / total) * 100)}%`;
    });
    t.upload.promise.then(
      (res) => {
        t.upload = null;
        if (mine !== st.seq) return;
        st.uploading = null;
        if (!st.tiles.includes(t)) return pump();
        t.staged = res.staged_id;
        t.w = res.w;
        t.h = res.h;
        t.bytes = res.bytes;
        t.status = "ready";
        t.file = null;
        setThumb(t, res.preview_url);
        layout();
        pump();
      },
      (err) => {
        t.upload = null;
        if (mine !== st.seq) return;
        st.uploading = null;
        if (err.aborted || !st.tiles.includes(t)) return pump();
        if (err.status === 429 && (t.busyTries = (t.busyTries || 0) + 1) <= BUSY_RETRIES) {
          // Another upload holds the slot: wait and try this one again.
          t.status = "queued";
          t.msg.textContent = "Waiting for the server…";
          layout();
          setTimeout(() => mine === st.seq && pump(), BUSY_RETRY_MS);
          return;
        }
        t.status = "failed";
        t.error = err.message;
        layout();
        pump();
      }
    );
  }

  function retry(t) {
    if (t.status !== "failed" || !t.file) return;
    t.status = "queued";
    t.error = "";
    t.busyTries = 0;
    setError("");
    layout();
    pump();
  }

  // ---- save / close -------------------------------------------------------------------------

  /** The cover URLs are cached by the browser for an hour; the files behind them just changed
   *  order, so fetch the new list's images fresh into the cache before the views redraw. */
  async function refreshCache(covers) {
    const urls = [];
    for (const c of covers || []) {
      for (const size of [128, 512, 1024]) urls.push(Covers.sized(c.url, size));
    }
    await Promise.allSettled(urls.map((u) => fetch(u, { cache: "reload" })));
  }

  async function save() {
    if ($("covers-save").disabled) return;
    const order = st.tiles.filter((t) => t.status === "ready").map((t) => (t.kind === "keep" ? { keep: t.n } : { staged: t.staged }));
    st.saving = true;
    setError("");
    layout();
    let result;
    try {
      result = await api(`/api/collections/${encodeURIComponent(st.id)}/covers`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ order }),
      });
    } catch (e) {
      st.saving = false;
      setError(e.message);
      layout();
      return;
    }
    const opts = st.opts;
    await refreshCache(result && result.covers);
    st.tiles = st.tiles.filter((t) => t.kind !== "new" || t.status === "failed"); // consumed by the save
    finish();
    toast("Cover art updated");
    if (opts && opts.onSaved) opts.onSaved(result);
  }

  function requestClose() {
    if (st.saving) return;
    if (changed() && !window.confirm("Discard your changes to the cover art?")) return;
    finish();
  }

  /** Close the dialog and forget everything: in-flight uploads are cancelled, staged ids dropped. */
  function finish() {
    st.seq++;
    for (const t of st.tiles) {
      if (t.upload) t.upload.abort();
      discardStaged(t);
    }
    st.uploading = null;
    st.tiles = [];
    st.removed = [];
    st.dragFrom = null;
    if (dlg().open) dlg().close();
    $("covers-grid").replaceChildren();
  }

  // ---- open ---------------------------------------------------------------------------------

  async function open(opts) {
    const d = dlg();
    if (d.open) return;
    st.seq++;
    st.opts = opts;
    st.id = String(opts.id);
    st.title = opts.title || "";
    st.tiles = [];
    st.removed = [];
    st.orig = "";
    st.hidden = 0;
    st.loaded = false;
    st.saving = false;
    st.uploading = null;
    $("covers-title").textContent = st.title ? `Cover art — ${st.title}` : "Cover art";
    $("covers-grid").replaceChildren();
    setError("");
    $("covers-loading").classList.remove("hidden");
    opener = opts.opener || document.activeElement;
    updateButtons();
    d.showModal();
    $("covers-pick-btn").focus();
    const mine = st.seq;
    let list;
    try {
      list = await api(`/api/collections/${encodeURIComponent(st.id)}/covers`);
    } catch (e) {
      if (mine !== st.seq) return;
      $("covers-loading").classList.add("hidden");
      setError(e.message);
      updateButtons();
      return;
    }
    if (mine !== st.seq) return;
    $("covers-loading").classList.add("hidden");
    st.tiles = (list.covers || []).map((c) =>
      makeTile({ kind: "keep", n: c.n, source: c.source, thumbUrl: c.url, w: c.w, h: c.h, bytes: c.bytes, name: "" })
    );
    st.hidden = Number(list.hidden) || 0;
    st.orig = signature();
    st.loaded = true;
    layout();
    // Files dropped before the list arrived are not accepted (addFiles checks `loaded`).
  }

  // ---- wiring (once) ------------------------------------------------------------------------

  function wire() {
    const d = dlg();
    Ui.modal(d, () => opener);
    $("covers-cancel").addEventListener("click", requestClose);
    $("covers-save").addEventListener("click", save);
    $("covers-pick-btn").addEventListener("click", () => $("covers-file").click());
    $("covers-file").addEventListener("change", () => {
      const files = Array.from($("covers-file").files || []);
      $("covers-file").value = "";
      if (files.length) addFiles(files);
    });
    d.addEventListener("cancel", (ev) => {
      // Esc: ask first when there is something to lose.
      ev.preventDefault();
      requestClose();
    });
    d.addEventListener("close", () => {
      // Closed by any route: nothing may keep uploading into a dialog nobody sees.
      if (st.tiles.length || st.uploading) finish();
    });
    const hasFiles = (ev) => ev.dataTransfer && Array.from(ev.dataTransfer.types || []).includes("Files");
    d.addEventListener("dragover", (ev) => {
      if (!hasFiles(ev)) return;
      ev.preventDefault();
      ev.dataTransfer.dropEffect = "copy";
      $("covers-drop").classList.add("hover");
    });
    d.addEventListener("dragleave", (ev) => {
      if (!d.contains(ev.relatedTarget)) $("covers-drop").classList.remove("hover");
    });
    d.addEventListener("drop", (ev) => {
      $("covers-drop").classList.remove("hover");
      if (!hasFiles(ev)) return;
      ev.preventDefault();
      const files = Array.from(ev.dataTransfer.files || []);
      if (files.length) addFiles(files);
    });
  }
  wire();

  return { open };
})();
