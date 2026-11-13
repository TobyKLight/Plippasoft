/* Plippa media portal — "Delete" dialog (BCS_10).

   Preview first, then confirm: opening the dialog calls
   GET /api/playables/:id/delete/preview (or …/files/:n/delete/preview for one media file) and
   shows exactly what would go (files, covers, the .noudata, the folder), what would break
   (playlists that lose entries, keys that stop resolving) and every blocker. Nothing is deleted
   until the confirm button is pressed, which repeats the request as
   DELETE …/files/:n {force:true}; the server rebuilds the plan, so a change made in between is
   never acted on blindly. A refused delete (409: playing / blocked / confirm_required) keeps the
   dialog open and re-reads the preview.

   Everything from the server (titles, paths, playlist names, warnings) goes in through
   textContent / createElement only (NBSecurityRequirements §5.1). Relies on globals: Ui (ui.js),
   api/$ (app.js). */

const DeleteItem = (() => {
  const { el, toast } = Ui;

  const st = {
    id: null,
    item: null, // mediaList position for a single-file delete, else null
    title: "",
    preview: null,
    busy: false,
    partial: false, // the last delete stopped partway: the confirm button retries
    seq: 0, // drops answers of a superseded open()
  };
  let opener = null; // element to give the focus back to when the dialog closes

  const dlg = () => $("delete-dialog");

  function setError(msg) {
    $("delete-error").textContent = msg || "";
  }

  function previewUrl(id, item) {
    const base = `/api/playables/${encodeURIComponent(id)}`;
    return item == null
      ? `${base}/delete/preview`
      : `${base}/files/${encodeURIComponent(String(item))}/delete/preview`;
  }

  function deleteUrl(id, item) {
    const base = `/api/playables/${encodeURIComponent(id)}`;
    return item == null ? base : `${base}/files/${encodeURIComponent(String(item))}`;
  }

  function plural(n, one, many) {
    return `${n} ${n === 1 ? one : many}`;
  }

  /** What a ⋮ menu / the details panel may offer. `null` = no delete action for this record. */
  function access(rec, item) {
    if (!rec || !rec.id) return null;
    // Playlists are removed with DELETE /api/playlists/:id (their media is never deleted).
    if (rec.kind === "playlist" || rec.is_dynamic) return null;
    const id = idStr(rec.id);
    if (!id) return null;
    if (item == null) return { id, item: null, title: rec.title || "" };
    // One media file of a release: the last one cannot go on its own (the server says so too).
    if (rec.item_count != null && Number(rec.item_count) <= 1) return null;
    return { id, item: Number(item), title: rec.title || "" };
  }

  // ---- rendering the preview ---------------------------------------------------------------

  function summary(preview) {
    const files = (preview.files || []).length + (preview.covers || []).length;
    const parts = [plural(files, "file", "files")];
    const bytes = Number(preview.total_bytes) || 0;
    if (bytes > 0) parts.push(Ui.fmtBytes(bytes));
    const missing = (preview.files || []).filter((f) => f.missing).length;
    if (missing) parts.push(`${missing} already missing`);
    return parts.join(" · ");
  }

  function pathItem(f) {
    return el("li", { cls: f.missing ? "muted" : null }, [
      el("code", { text: f.path }),
      f.missing ? el("span", { cls: "flag", text: "missing" }) : null,
      !f.missing && f.bytes ? el("span", { cls: "muted", text: ` · ${Ui.fmtBytes(f.bytes)}` }) : null,
    ]);
  }

  function render(preview) {
    st.preview = preview;
    const t = preview.playable || {};
    const named = st.title || t.title || "";
    $("delete-title").textContent = st.item == null ? "Delete this item?" : "Delete this file?";
    $("delete-subject").textContent = named;
    $("delete-loading").classList.add("hidden");

    const body = $("delete-body");
    body.classList.remove("hidden");
    $("delete-summary").textContent = summary(preview);

    // Blockers: why nothing can be deleted, in plain words.
    const blockers = preview.blockers || [];
    $("delete-blockers").replaceChildren(
      ...blockers.map((b) => el("li", { attrs: { "data-kind": b.kind }, text: b.detail }))
    );
    $("delete-blockers").classList.toggle("hidden", blockers.length === 0);

    // Files, in a <details> so a long release does not fill the dialog.
    const files = preview.files || [];
    const covers = preview.covers || [];
    const all = files.length + covers.length;
    $("delete-files-sum").textContent =
      all === 0
        ? "No files"
        : `Files (${files.length})${covers.length ? ` and artwork (${covers.length})` : ""}`;
    $("delete-files").replaceChildren(...files.concat(covers).map(pathItem));
    $("delete-files-wrap").open = all <= 12;

    // What else changes.
    const extra = [];
    if (preview.noudata) {
      extra.push(el("p", { cls: "muted" }, [el("span", { text: "Metadata: " }), el("code", { text: preview.noudata })]));
    }
    if (preview.folder) {
      extra.push(el("p", { cls: "muted" }, [el("span", { text: "Folder: " }), el("code", { text: preview.folder }), el("span", { text: " (removed if it becomes empty)" })]));
    }
    if ((preview.playlists || []).length) {
      const names = [...new Set(preview.playlists.map((p) => p.title || p.id))];
      extra.push(
        el("p", { cls: "note" }, [
          el("span", {
            text: `${plural(names.length, "playlist", "playlists")} will lose the deleted ${
              (preview.files || []).length === 1 ? "track" : "tracks"
            }: `,
          }),
          el("span", { text: names.join(", ") }),
          el("span", { text: ". The playlist itself is kept." }),
        ])
      );
    }
    if ((preview.keys || []).length) {
      extra.push(
        el("p", { cls: "muted", text: `Noubin ${plural(preview.keys.length, "key", "keys")} released: ${preview.keys.join(", ")}` })
      );
    }
    $("delete-lists").replaceChildren(...extra);

    const warnings = preview.warnings || [];
    $("delete-warnings").replaceChildren(...warnings.map((w) => el("li", { text: w })));
    $("delete-warnings-box").classList.toggle("hidden", warnings.length === 0);

    updateButtons();
  }

  function updateButtons() {
    const p = st.preview;
    const ok = $("delete-ok");
    const blocked = !!(p && ((p.blockers || []).length || p.playing));
    ok.disabled = !p || st.busy || blocked;
    ok.textContent = st.busy ? "Deleting…" : "Delete";
    $("delete-cancel").disabled = st.busy;
    $("delete-cancel").textContent = p ? "Cancel" : "Close";
  }

  // ---- flow ---------------------------------------------------------------------------------

  /** Open the dialog for `opts` = {id, item?, title?, opener?, onDeleted?(id, outcome)}. */
  async function open(opts) {
    const d = dlg();
    if (d.open) return;
    const mine = ++st.seq;
    st.id = String(opts.id);
    st.item = opts.item == null ? null : Number(opts.item);
    st.title = opts.title || "";
    st.preview = null;
    st.busy = false;
    st.partial = false;
    st.opts = opts;

    $("delete-title").textContent = "Delete";
    $("delete-subject").textContent = st.title;
    $("delete-summary").textContent = "";
    $("delete-body").classList.add("hidden");
    $("delete-files").replaceChildren();
    $("delete-lists").replaceChildren();
    $("delete-blockers").replaceChildren();
    $("delete-warnings").replaceChildren();
    $("delete-warnings-box").classList.add("hidden");
    $("delete-blockers").classList.add("hidden");
    $("delete-loading").textContent = "Checking what would be deleted…";
    $("delete-loading").classList.remove("hidden");
    $("delete-ok").textContent = "Delete";
    setError("");
    opener = opts.opener || document.activeElement;
    updateButtons();
    d.showModal();
    // Cancel is the default focus: the destructive button is never the one Enter/Space hits.
    $("delete-cancel").focus();

    let preview;
    try {
      preview = await api(previewUrl(st.id, st.item));
    } catch (e) {
      if (mine !== st.seq) return;
      $("delete-loading").classList.add("hidden");
      setError(e.message);
      updateButtons();
      return;
    }
    if (mine !== st.seq) return;
    render(preview);
  }

  async function confirm() {
    if (!st.preview || st.busy || $("delete-ok").disabled) return;
    const mine = st.seq;
    st.busy = true;
    setError("");
    updateButtons();
    let outcome;
    try {
      outcome = await api(deleteUrl(st.id, st.item), {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ force: true, reason: "portal" }),
      });
    } catch (e) {
      if (mine !== st.seq) return;
      st.busy = false;
      setError(e.message);
      updateButtons();
      if (e.code === "blocked" || e.code === "playing" || e.code === "confirm_required") {
        // The plan changed (or the server refused): show the fresh reasons.
        try {
          const fresh = await api(previewUrl(st.id, st.item));
          if (mine === st.seq) render(fresh);
        } catch {
          /* the item is gone: the message above says enough */
        }
      }
      return;
    }
    if (mine !== st.seq) return;
    st.busy = false;
    finish(outcome);
  }

  /** The delete happened: say what it freed, or how much was left behind. */
  function finish(outcome) {
    const done = Number(outcome.freed_bytes) || 0;
    const n = (outcome.deleted || []).length;
    if (outcome.partial) {
      const left = outcome.remaining || [];
      $("delete-body").classList.add("hidden");
      $("delete-loading").classList.add("hidden");
      $("delete-title").textContent = "Partly deleted";
      $("delete-summary").textContent = "";
      const box = el("div", { attrs: { id: "delete-result" } }, [
        el("p", { cls: "note" }, [
          el("span", { text: `Deleted ${plural(n, "file", "files")} (${Ui.fmtBytes(done)}). ` }),
          el("span", { text: `${plural(left.length, "file", "files")} could not be removed:` }),
        ]),
        el("ul", { cls: "preview-warn" }, left.map((f) => el("li", { text: f.reason ? `${f.path} — ${f.reason}` : f.path }))),
      ]);
      $("delete-lists").replaceChildren(box);
      $("delete-lists").classList.remove("hidden");
      $("delete-ok").textContent = "Retry";
      $("delete-ok").classList.remove("hidden");
      $("delete-ok").disabled = false;
      st.partial = true;
      return;
    }
    const opts = st.opts || {};
    toast(`Deleted ${plural(n, "file", "files")}, ${Ui.fmtBytes(done)}`);
    close();
    if (opts.onDeleted) opts.onDeleted(st.id, outcome);
  }

  /** Retry after a partial delete: re-preview the same target (the item may be gone already). */
  async function retry() {
    st.partial = false;
    $("delete-ok").textContent = "Delete";
    const mine = ++st.seq;
    $("delete-title").textContent = "Delete";
    $("delete-lists").replaceChildren();
    $("delete-loading").textContent = "Checking what is left…";
    $("delete-loading").classList.remove("hidden");
    setError("");
    let preview;
    try {
      preview = await api(previewUrl(st.id, st.item));
    } catch (e) {
      if (mine !== st.seq) return;
      $("delete-loading").classList.add("hidden");
      setError(
        `${e.message} — what is left on the stick is listed as an orphan item in the library; delete that entry to finish.`
      );
      $("delete-ok").disabled = true;
      $("delete-cancel").textContent = "Close";
      return;
    }
    if (mine !== st.seq) return;
    render(preview);
  }

  function close(refocus) {
    st.seq++;
    if (dlg().open) dlg().close();
    const o = opener;
    if (refocus !== false && o && document.contains(o)) o.focus();
  }

  function requestClose() {
    if (st.busy) return;
    close();
  }

  // ---- wiring (once) ------------------------------------------------------------------------

  function wire() {
    const d = dlg();
    if (!d) return;
    Ui.modal(d, () => opener);
    $("delete-cancel").addEventListener("click", requestClose);
    $("delete-ok").addEventListener("click", () => {
      // After a partial delete the same button retries instead of confirming again.
      if (st.partial) retry();
      else confirm();
    });
    d.addEventListener("cancel", (ev) => {
      ev.preventDefault(); // Esc closes, but never while the request is in flight
      requestClose();
    });
  }
  wire();

  return { open, access };
})();
