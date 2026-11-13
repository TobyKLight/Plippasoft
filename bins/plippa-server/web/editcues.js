/* Plippa media portal — "Edit timestamps" for an EXISTING item, and "Create metadata" for a media
   file that has no .noudata yet (NB_16).

   Edit timestamps: GET /api/items/:id/cues pre-fills the paste box (`text` = paste text of the
   owned nav list) and gives the `revision` and the item's real length; the paste box + live
   preview is the very same component as in the Add media dialog (Ui.cuePaste); Save is
   PUT /api/items/:id/cues with that revision (409 = changed meanwhile -> message + Reload). An
   empty box removes the timestamps. Everything from the server/files is rendered with
   textContent only (NBSecurityRequirements §5.1). Relies on globals: Ui (ui.js), api/$ (app.js). */

const EditCues = (() => {
  const { toast } = Ui;
  const LONG_FORM = new Set(["mix", "audiobook", "podcast"]);
  const KIND_TAB = { music: "tracks", mix: "mixes", audiobook: "audiobooks", podcast: "podcasts", other: "tracks" };
  const TAB_OF_KIND = { track: "tracks", album: "albums", playlist: "playlists", mix: "mixes", audiobook: "audiobooks", podcast: "podcasts" };

  function fmtHms(ms) {
    const s = Math.round(ms / 1000);
    const two = (n) => String(n).padStart(2, "0");
    const h = Math.floor(s / 3600);
    return h ? `${h}:${two(Math.floor((s % 3600) / 60))}:${two(s % 60)}` : `${Math.floor(s / 60)}:${two(s % 60)}`;
  }

  // =====================================================================================
  // Edit timestamps
  // =====================================================================================
  const st = { id: null, item: 0, revision: null, hadCues: false, duration: null, opts: null, saving: false, loading: false, seq: 0 };

  const paste = Ui.cuePaste({
    textarea: $("cues-text"),
    box: $("cues-preview"),
    getDuration: () => st.duration,
    showEmpty: true,
    emptyText: "The box is empty: saving will remove the timestamps from this item.",
  });

  function setError(msg, showReload) {
    $("cues-error").textContent = msg || "";
    $("cues-reload").classList.toggle("hidden", !showReload);
  }

  function updateSave() {
    const b = $("cues-save");
    b.disabled = st.loading || st.saving || st.revision == null;
    const blank = !$("cues-text").value.trim();
    b.textContent = st.saving ? "Saving…" : blank && st.hadCues ? "Remove timestamps" : "Save";
  }

  async function load() {
    const mine = ++st.seq;
    st.loading = true;
    st.revision = null;
    $("cues-text").disabled = true;
    setError("", false);
    updateSave();
    let r;
    try {
      r = await api(`/api/items/${encodeURIComponent(st.id)}/cues?item=${st.item}`);
    } catch (e) {
      if (mine !== st.seq) return;
      st.loading = false;
      setError(e.message, false);
      updateSave();
      return;
    }
    if (mine !== st.seq) return;
    st.loading = false;
    st.revision = r.revision;
    st.baseText = r.text || "";
    st.duration = r.duration_ms;
    st.hadCues = !!r.text;
    const parts = [r.title || "(untitled)"];
    if (r.item_count > 1) parts.push(`item ${r.item + 1} of ${r.item_count}`);
    parts.push(r.duration_ms ? `length ${fmtHms(r.duration_ms)}` : "length unknown (checked on save)");
    $("cues-subject").textContent = parts.join(" · ");
    const others = (r.cue_lists || []).filter((l) => !l.owned).length;
    $("cues-other").textContent = others
      ? `${others} other cue ${others === 1 ? "list" : "lists"} (lyrics, credits …) on this item will be kept as they are.`
      : "";
    const box = $("cues-text");
    box.value = r.text || "";
    box.disabled = false;
    updateSave();
    paste.flush();
    box.focus();
  }

  function open(opts) {
    const dlg = $("cues-dialog");
    if (dlg.open) return;
    st.opts = opts;
    st.id = String(opts.id);
    st.item = opts.item || 0;
    st.duration = null;
    st.hadCues = false;
    st.saving = false;
    $("cues-subject").textContent = opts.title || "";
    $("cues-text").value = "";
    $("cues-other").textContent = "";
    paste.reset();
    const loaded = opts.isLoaded && opts.isLoaded();
    const note = $("cues-loaded-note");
    note.textContent = loaded
      ? "This item is loaded on the player right now. New timestamps apply the next time it is loaded (press Play on it again); the running list does not change."
      : "";
    note.classList.toggle("hidden", !loaded);
    dlg.showModal();
    load();
  }

  async function save(attempt) {
    if (st.saving || st.loading || st.revision == null) return;
    const text = $("cues-text").value;
    st.saving = true;
    setError("", false);
    updateSave();
    try {
      const r = await api(`/api/items/${encodeURIComponent(st.id)}/cues?item=${st.item}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text, item: st.item, revision: st.revision }),
      });
      st.saving = false;
      $("cues-dialog").close();
      const n = r.cue_count;
      const notes = (r.warnings || []).length;
      toast(
        r.cleared
          ? "Timestamps removed"
          : `Saved ${n} ${n === 1 ? "timestamp" : "timestamps"}` + (notes ? ` (${notes} ${notes === 1 ? "note" : "notes"})` : "")
      );
      if (st.opts && st.opts.onSaved) st.opts.onSaved(r);
    } catch (e) {
      st.saving = false;
      if (e.status === 409 && (attempt || 0) < 4 && (await cuesUnchangedSince())) {
        // Only something else in the file changed (typically the player writing its resume point
        // when you paused or stopped): the timestamps are what they were when this dialog opened,
        // so nobody's edit can be lost. Take the new revision and save the user's text once more.
        return save((attempt || 0) + 1);
      }
      if (e.status === 409) {
        setError(
          "This item changed since you opened this dialog (another browser tab, or the device). Your text is still in the box; “Reload current version” replaces it with what is saved now.",
          true
        );
      } else {
        setError(e.message, false);
      }
      updateSave();
    }
  }

  /** After a 409: is the saved timestamp text still the one this dialog was opened with? If so the
   *  revision is refreshed (st.revision) and the caller may retry. */
  async function cuesUnchangedSince() {
    const mine = st.seq;
    try {
      const r = await api(`/api/items/${encodeURIComponent(st.id)}/cues?item=${st.item}`);
      if (mine !== st.seq || (r.text || "") !== st.baseText) return false;
      st.revision = r.revision;
      return true;
    } catch {
      return false;
    }
  }

  function wireEdit() {
    const dlg = $("cues-dialog");
    Ui.modal(dlg, () => st.opts && st.opts.opener);
    $("cues-text").addEventListener("input", () => {
      updateSave();
      paste.soon(300);
    });
    $("cues-save").addEventListener("click", () => save(0));
    $("cues-cancel").addEventListener("click", () => dlg.close());
    $("cues-reload").addEventListener("click", () => load());
    dlg.addEventListener("close", () => {
      st.seq++; // drop a late GET
      paste.reset();
    });
  }

  // =====================================================================================
  // Create metadata for an orphan media file
  // =====================================================================================
  const mk = { id: null, opts: null, busy: false };

  function openCreate(opts) {
    const dlg = $("meta-dialog");
    if (dlg.open) return;
    mk.id = String(opts.id);
    mk.opts = opts;
    mk.busy = false;
    $("meta-subject").textContent = opts.title || "";
    $("meta-error").textContent = "";
    $("meta-title-input").value = "";
    document.querySelector('input[name="meta-kind"][value="music"]').checked = true;
    $("meta-ok").disabled = false;
    dlg.showModal();
    $("meta-ok").focus();
  }

  async function create() {
    if (mk.busy) return;
    mk.busy = true;
    $("meta-ok").disabled = true;
    $("meta-error").textContent = "";
    const kind = document.querySelector('input[name="meta-kind"]:checked').value;
    const title = $("meta-title-input").value.trim();
    try {
      const made = await api(`/api/items/${encodeURIComponent(mk.id)}/noudata`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ kind, ...(title ? { title } : {}) }),
      });
      $("meta-dialog").close();
      toast(`Created metadata for “${made.title || "the file"}”`);
      const newId = String(made.id);
      if (mk.opts && mk.opts.onCreated) mk.opts.onCreated({ id: newId, kind, tab: TAB_OF_KIND[made.group || made.kind] || KIND_TAB[kind] || "tracks", longForm: LONG_FORM.has(kind), title: made.title });
    } catch (e) {
      mk.busy = false;
      $("meta-ok").disabled = false;
      $("meta-error").textContent = e.message;
    }
  }

  function wireCreate() {
    const dlg = $("meta-dialog");
    Ui.modal(dlg, () => mk.opts && mk.opts.opener);
    $("meta-ok").addEventListener("click", create);
    $("meta-cancel").addEventListener("click", () => dlg.close());
  }

  wireEdit();
  wireCreate();
  return { open, openCreate, LONG_FORM, KIND_TAB };
})();
