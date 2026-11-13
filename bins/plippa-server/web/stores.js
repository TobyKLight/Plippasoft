/* Plippa media portal — "Stores" panel (BCS_03).

   One store today (Bandcamp) with the whole connection lifecycle:
     Connect…  POST /api/stores/bandcamp/connect  {user, password, consent}
     Test      POST /api/stores/bandcamp/validate
     Sync now  POST /api/stores/bandcamp/sync      → poll GET …/sync
     Cancel    POST /api/stores/bandcamp/sync/cancel
     Disconnect DELETE /api/stores/bandcamp

   Security rules this file follows (NBSecurityRequirements §5):
   * everything from the server goes in through textContent / Ui.el — never innerHTML;
   * state-changing calls send `Content-Type: application/json` (and app.js's api() adds
     the X-Plippa header the server wants);
   * the password only ever lives in the input element and the request body. It is not
     stored in a variable, not logged, not put in the DOM anywhere else, and it is
     cleared when the dialog closes.
   CSP is unchanged: no inline handlers, no eval, no remote scripts. */

const Stores = (() => {
  const { el, toast } = Ui;

  // Server codes, said in words a person can act on.
  const CODES = {
    invalid_credentials: "Enter both values copied from Bandcamp settings.",
    bad_credentials: "Bandcamp rejected those values. Generate a new pair and try again.",
    blocked: "Bandcamp answered with a bot check instead of data. Try again later, or from another network.",
    unreachable: "Bandcamp could not be reached. Check the connection and try again.",
    consent_required: "Tick the consent box first.",
    unavailable: "Bandcamp cannot be used on this Plippa: the secrets directory is not usable.",
    busy: "Another store request is already running. Wait a moment.",
    not_connected: "No Bandcamp connection is stored yet.",
    rate_limited: "Bandcamp asked Plippa to slow down. Try again later.",
    cache_too_large: "The collection is too large for the cache. Ask the operator to look at this.",
  };

  let status = null; // last GET /api/stores payload
  let sync = null; // last sync status
  let pollTimer = null;
  let pollDeadline = 0;
  let busy = false;
  let opener = null;

  const dlg = () => $("store-dialog");

  function codeText(e) {
    if (!e) return "Something went wrong.";
    return CODES[e.code] || e.message || "Something went wrong.";
  }

  function setError(id, message, quiet) {
    const box = $(id);
    if (!box) {
      // WEB_01: the header may be off screen (another library tab). Never swallow the reason a
      // user action produced — but a background poll failing every 30 s must not raise a toast
      // on a tab the owner is not looking at, so pollers pass `quiet` and only log it.
      if (message && !quiet) toast(message, true);
      return;
    }
    box.textContent = message || "";
    box.classList.toggle("hidden", !message);
  }

  /**
   * WEB_01: Bandcamp's state in the shape the Distros list and the tab strip need, without a
   * second `/api/stores` request. `ready` is false before the first answer.
   */
  function summary() {
    const store = (status && status.stores && status.stores[0]) || null;
    const unavailable = !!(status && !status.secretsAvailable);
    const cache = (store && store.cache) || {};
    return {
      ready: !!status,
      unavailable,
      reason: (status && status.secretsReason) || "no secrets directory",
      connected: !!(store && store.connected) && !unavailable,
      rejected: !!(store && store.rejected),
      // `null` is "not known yet" (no cache), which the Distros row omits; `0` is a real empty
      // collection and is shown.
      releases: typeof cache.releases === "number" ? cache.releases : null,
    };
  }

  /** "just now" / "3 h ago" / "2 days ago" for a Unix-seconds timestamp. */
  function ago(seconds) {
    if (!seconds) return "never";
    const diff = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
    if (diff < 60) return "just now";
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
    return `${Math.floor(diff / 86400)} ${Math.floor(diff / 86400) === 1 ? "day" : "days"} ago`;
  }

  function plural(n, one, many) {
    return `${n} ${n === 1 ? one : many}`;
  }

  // ---- rendering ---------------------------------------------------------------------------

  function stateText(store) {
    if (!store) return "not available";
    if (!store.connected) {
      return store.rejected ? "not connected (credentials were rejected)" : "not connected";
    }
    if (store.rejected) return "credentials rejected — connect again";
    return store.lastValidated
      ? `connected, last checked ${ago(store.lastValidated)}`
      : "connected";
  }

  function cacheText(store) {
    const cache = (store && store.cache) || {};
    if (!cache.releases) return "no collection cached yet";
    const parts = [plural(cache.releases, "release", "releases")];
    parts.push(cache.syncedAt ? `synced ${ago(cache.syncedAt)}` : "never fully synced");
    if (cache.stale) parts.push("out of date");
    if (!cache.complete) parts.push("incomplete");
    if (cache.lastError) parts.push(cache.lastError);
    return parts.join(" · ");
  }

  function render(data) {
    status = data || null;
    updateHeader();
    const s = summary();
    // WEB_01: Distros is always visible; this keeps its row and the tab-strip pointer current.
    // `setStoreConnected` ignores an unchanged value, so the 30 s poll cannot steal focus from
    // a keyboard user on those rows.
    if (typeof Library !== "undefined" && Library.setStoreConnected) {
      Library.setStoreConnected(s.connected);
    }
  }

  /**
   * WEB_01: the Bandcamp connection row. This was the page's Stores bar; it now sits at the top
   * of the Bandcamp view, so it is built here (the element ids are unchanged).
   */
  function header() {
    const box = el("div", { cls: "store-head" }, [
      el("div", { cls: "store-row" }, [
        el("div", { cls: "store-main" }, [
          el("strong", { text: "Bandcamp" }),
          el("span", {
            cls: "muted",
            text: "not connected",
            attrs: { id: "store-state", role: "status" },
          }),
          el("span", { cls: "muted", attrs: { id: "store-cache" } }),
        ]),
        el("div", { cls: "actions" }, [
          el("button", {
            text: "Connect…",
            cls: "primary",
            attrs: { type: "button", id: "store-connect-btn" },
            on: { click: open },
          }),
          el("button", {
            text: "Test",
            // Disabled until `updateHeader()` runs: it is only called once the row is attached,
            // so the pre-attach default must not be a live button (WEB_01 review).
            attrs: { type: "button", id: "store-test-btn", disabled: "" },
            on: { click: test },
          }),
          el("button", {
            text: "Sync now",
            attrs: { type: "button", id: "store-sync-btn", disabled: "" },
            on: { click: syncNow },
          }),
          el("button", {
            text: "Disconnect",
            cls: "danger",
            attrs: { type: "button", id: "store-disconnect-btn", disabled: "" },
            on: { click: disconnect },
          }),
        ]),
      ]),
      el("div", { cls: "hidden", attrs: { id: "store-sync-box", "aria-live": "polite" } }, [
        el("progress", {
          attrs: {
            id: "store-sync-progress",
            max: "100",
            value: "0",
            "aria-label": "Collection sync progress",
          },
        }),
        el("span", { cls: "muted", attrs: { id: "store-sync-text" } }),
        el("button", {
          text: "Cancel sync",
          attrs: { type: "button", id: "store-sync-cancel" },
          on: { click: cancelSync },
        }),
      ]),
      el("p", { cls: "msg", attrs: { id: "store-error", role: "alert" } }),
    ]);
    // The row paints itself only once it is in the document: `$` is `getElementById`, so calling
    // `updateHeader()` here would find nothing. `Bandcamp.render` calls it straight after
    // attaching, which is what makes the first paint correct (WEB_01 review).
    return box;
  }

  /**
   * Repaint the header from the last status. A no-op when the Bandcamp view is not on screen —
   * in which case the next `Bandcamp.render` paints it.
   */
  function updateHeader() {
    const stateEl = $("store-state");
    if (!stateEl) return;
    const store = (status && status.stores && status.stores[0]) || null;
    const s = summary();
    stateEl.textContent = stateText(store);
    stateEl.classList.toggle("warn", !!(store && store.rejected));
    const cacheEl = $("store-cache");
    if (cacheEl) cacheEl.textContent = cacheText(store);
    $("store-connect-btn").textContent = s.connected ? "Reconnect…" : "Connect…";
    $("store-connect-btn").disabled = s.unavailable || busy;
    $("store-test-btn").disabled = !s.connected || busy;
    $("store-sync-btn").disabled = !s.connected || busy;
    $("store-disconnect-btn").disabled = !s.connected || busy;
    // The progress box belongs to the same row, so one call repaints both (no recursion:
    // `renderSync` never calls `updateHeader`).
    renderSync(sync);
  }

  function renderSync(s) {
    sync = s || null;
    const box = $("store-sync-box");
    if (!box) return; // the Bandcamp view is not on screen; `sync` is still current
    const running = !!(s && s.state === "running");
    box.classList.toggle("hidden", !s || (!running && s.state === "idle" && !s.error));
    $("store-sync-cancel").classList.toggle("hidden", !running);
    const progress = $("store-sync-progress");
    if (running) {
      // No known total: an indeterminate bar while the pages arrive.
      progress.removeAttribute("value");
      $("store-sync-text").textContent = `Syncing… ${plural(s.pages || 0, "page", "pages")}, ${plural(s.releases || 0, "release", "releases")}`;
    } else {
      progress.setAttribute("value", "100");
      if (s && s.state === "done") {
        $("store-sync-text").textContent = `Synced ${plural(s.releases || 0, "release", "releases")} in ${plural(s.pages || 0, "page", "pages")}.`;
      } else if (s && s.cancelled) {
        $("store-sync-text").textContent = `Sync stopped after ${plural(s.pages || 0, "page", "pages")}; the previous collection is kept.`;
      } else if (s && s.state === "failed") {
        $("store-sync-text").textContent = `Sync failed: ${CODES[s.errorCode] || s.error || "unknown error"}`;
      } else {
        $("store-sync-text").textContent = "";
      }
    }
    $("store-sync-btn").textContent = running ? "Syncing…" : "Sync now";
    $("store-sync-btn").disabled = running || busy || !(status && status.stores && status.stores[0] && status.stores[0].connected);
  }

  // ---- data --------------------------------------------------------------------------------

  async function refresh(quiet) {
    let data;
    try {
      data = await api("/api/stores");
    } catch (e) {
      // The 30 s poll passes `quiet`: no toast on another tab for a background failure.
      setError("store-error", codeText(e), quiet);
      return;
    }
    render(data);
    try {
      renderSync(await api("/api/stores/bandcamp/sync"));
    } catch {
      /* the sync endpoint is only information; the row above is still valid */
    }
  }

  function stopPolling() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  /** Poll the sync endpoint until it stops, then reload the store row. */
  function pollSync() {
    stopPolling();
    pollDeadline = Date.now() + 15 * 60 * 1000;
    pollTimer = setInterval(async () => {
      if (Date.now() > pollDeadline) {
        stopPolling();
        return;
      }
      let s;
      try {
        s = await api("/api/stores/bandcamp/sync");
      } catch (e) {
        stopPolling();
        setError("store-error", codeText(e), true);
        return;
      }
      renderSync(s);
      if (!s || s.state !== "running") {
        stopPolling();
        await refresh();
        // WEB_01: the collection list is a platform view now; a sync that finishes while it is
        // open must show the new releases without a reload.
        if (typeof Bandcamp !== "undefined" && Bandcamp.refresh) Bandcamp.refresh();
        if (s && s.state === "done") toast("Collection synced");
        if (s && s.state === "failed") setError("store-error", codeText({ code: s.errorCode, message: s.error }));
      }
    }, 1500);
  }

  // ---- actions -----------------------------------------------------------------------------

  async function test() {
    busy = true;
    render(status);
    setError("store-error", "");
    try {
      await api("/api/stores/bandcamp/validate", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      toast("Bandcamp credentials accepted");
    } catch (e) {
      setError("store-error", codeText(e));
    }
    busy = false;
    await refresh();
  }

  async function syncNow() {
    busy = true;
    setError("store-error", "");
    try {
      const answer = await api("/api/stores/bandcamp/sync", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      busy = false;
      renderSync(answer && answer.sync);
      pollSync();
      render(status);
    } catch (e) {
      busy = false;
      setError("store-error", codeText(e));
      render(status);
    }
  }

  async function cancelSync() {
    try {
      const answer = await api("/api/stores/bandcamp/sync/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      renderSync(answer && answer.sync);
      toast("Sync cancelled");
    } catch (e) {
      setError("store-error", codeText(e));
    }
  }

  async function disconnect() {
    if (!window.confirm("Disconnect Bandcamp? The stored credentials and the cached collection are removed from this Plippa.")) {
      return;
    }
    busy = true;
    setError("store-error", "");
    try {
      await api("/api/stores/bandcamp", { method: "DELETE", headers: { "Content-Type": "application/json" } });
      toast("Bandcamp disconnected");
    } catch (e) {
      setError("store-error", codeText(e));
    }
    busy = false;
    await refresh();
    // WEB_01: the album list under this row was showing the collection that just went away; it
    // must drop to the "not connected" state instead of staying on screen (review).
    if (typeof Bandcamp !== "undefined" && Bandcamp.refresh) Bandcamp.refresh();
  }

  // ---- connect dialog ----------------------------------------------------------------------

  function updateOk() {
    const user = $("store-user").value.trim();
    const password = $("store-password").value;
    $("store-ok").disabled = busy || !user || !password;
    $("store-ok").textContent = busy ? "Checking…" : "Connect";
  }

  function clearFields() {
    $("store-user").value = "";
    $("store-password").value = "";
    $("store-consent").checked = false;
  }

  function open() {
    if (dlg().open) return;
    if (status && !status.secretsAvailable) {
      setError("store-error", codeText({ code: "unavailable", message: "unavailable" }));
      return;
    }
    clearFields();
    setError("store-dialog-error", "");
    $("store-ok").textContent = "Connect";
    busy = false;
    updateOk();
    opener = document.activeElement;
    dlg().showModal();
    $("store-user").focus();
  }

  function close() {
    if (!dlg().open) return;
    // The password never outlives the dialog.
    $("store-password").value = "";
    $("store-user").value = "";
    $("store-consent").checked = false;
    dlg().close();
    if (opener && document.contains(opener)) opener.focus();
  }

  async function submit() {
    if ($("store-ok").disabled) return;
    busy = true;
    setError("store-dialog-error", "");
    updateOk();
    let answer;
    try {
      answer = await api("/api/stores/bandcamp/connect", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          user: $("store-user").value,
          password: $("store-password").value,
          consent: $("store-consent").checked,
        }),
      });
    } catch (e) {
      busy = false;
      updateOk();
      setError("store-dialog-error", codeText(e));
      return;
    }
    busy = false;
    close();
    toast("Bandcamp connected");
    if (answer && answer.store) render({ stores: [answer.store], secretsAvailable: true });
    await refresh();
    // WEB_01: the album list was showing "not connected" under this row a moment ago.
    if (typeof Bandcamp !== "undefined" && Bandcamp.refresh) Bandcamp.refresh();
  }

  // ---- wiring ------------------------------------------------------------------------------

  function init() {
    const d = dlg();
    if (!d) return;
    Ui.modal(d, () => opener);
    // WEB_01: the connection buttons are built by `header()` and wired there, because the row
    // only exists while the Bandcamp view is open.
    $("store-cancel").addEventListener("click", () => {
      if (!busy) close();
    });
    $("store-ok").addEventListener("click", submit);
    $("store-user").addEventListener("input", updateOk);
    $("store-password").addEventListener("input", updateOk);
    d.addEventListener("cancel", (ev) => {
      ev.preventDefault(); // Esc closes, but never while the request is in flight
      if (!busy) close();
    });
    d.addEventListener("close", clearFields);

    refresh();
    // Keep the cache age honest without a request storm; paused while the tab is hidden.
    setInterval(() => {
      if (!document.hidden) refresh(true);
    }, 30000);
  }

  init();

  return { refresh, openConnect: open, header, updateHeader, summary };
})();

/* BCS_04: the Bandcamp library view.

   Collection (cache-only, searchable, paged), one release with its tracks, a consent sheet
   that says exactly what will be fetched, then the sequential download queue with per-track
   progress, a running byte total, cancel and an honest result.

   Every byte on screen goes through Ui.el / textContent (NBSecurityRequirements §5.1); there
   is no innerHTML and no client-supplied URL is ever requested. The audio URL the store hands
   the server is a bearer capability: it never reaches this file or any API payload. */

const Bandcamp = (() => {
  const { el, toast } = Ui;
  const SIZE = 50;

  const CODES = {
    not_connected: "Connect Bandcamp first.",
    unavailable: "Bandcamp cannot be used on this Plippa: the secrets directory is not usable.",
    busy: "Another store request is already running. Wait a moment.",
    consent_required: "Tick the consent box before downloading.",
    stale_consent: "The selection changed. Review the download list again.",
    unknown_session: "That download is no longer running.",
    no_space: "There is not enough free space for this selection.",
    too_large: "That selection is larger than the batch limit.",
    cache_too_large: "The store says this is too large to download.",
    no_cover: "This release has no cover art.",
    not_cached: "That release is not in the cached collection. Look it up again.",
    not_ready: "There is nothing to download in this selection.",
    wrong_type: "The store sent something that is not audio.",
    blocked: "The store sent a bot check instead of the file.",
    rate_limited: "Bandcamp asked Plippa to slow down. Try again later.",
    not_found: "That release is not in the collection.",
  };

  const state = {
    query: "",
    page: null,
    seq: 0,
    loading: false,
    release: null,
    releaseId: null,
    selected: new Set(),
    // A Noubin URL the person pasted for this release (Path A); empty = store metadata.
    noubinKey: "",
    consent: null,
    consentId: null,
    phase: "idle", // idle | consent | running | result
    status: null,
    session: null,
    busy: false,
    pollTimer: null,
    opener: null,
    listView: null,
    searchTimer: null,
  };

  const dlg = () => $("store-dl-dialog");

  function codeText(e) {
    if (!e) return "Something went wrong.";
    return CODES[e.code] || e.message || "Something went wrong.";
  }

  function setError(message) {
    const box = $("store-dl-error");
    if (!box) return;
    box.textContent = message || "";
  }

  function jsonBody(obj) {
    return {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(obj),
    };
  }

  function ago(seconds) {
    if (!seconds) return "never";
    const diff = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
    if (diff < 60) return "just now";
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
    return `${Math.floor(diff / 86400)} ${Math.floor(diff / 86400) === 1 ? "day" : "days"} ago`;
  }

  function plural(n, one, many) {
    return `${n} ${n === 1 ? one : many}`;
  }

  function duration(seconds) {
    return seconds == null ? "" : fmtMs(seconds * 1000);
  }

  function coverUrl(id) {
    return `/api/stores/bandcamp/release/${encodeURIComponent(id)}/cover`;
  }

  function coverBox(release, cls) {
    const box = el("div", { cls: cls || "card-cover" });
    if (release && release.hasCover) {
      box.appendChild(
        el("img", { attrs: { src: coverUrl(release.id), alt: "", loading: "lazy" } })
      );
    } else {
      box.classList.add("nocover");
      box.setAttribute("aria-hidden", "true");
    }
    return box;
  }

  // ---- collection -------------------------------------------------------------------------

  function head() {
    const search = el("input", {
      cls: "filter",
      attrs: {
        type: "search",
        placeholder: "Search title or artist…",
        "aria-label": "Search the Bandcamp collection",
        value: state.query,
        id: "bc-search",
      },
      on: {
        input: (ev) => {
          state.query = ev.target.value;
          clearTimeout(state.searchTimer);
          state.searchTimer = setTimeout(() => loadList(0), 250);
        },
      },
    });
    // WEB_01: no Sync here any more. The connection row at the top of the view owns Sync now,
    // Test and Disconnect, so there is exactly one of each on screen.
    return el("div", { cls: "view-head" }, [
      el("h2", { text: "Bandcamp" }),
      el("div", { cls: "bc-tools" }, [search]),
    ]);
  }

  function renderList(view) {
    state.listView = view;
    state.release = null;
    state.releaseId = null;
    // WEB_01: the row above explains what to do; do not fire a request that can only fail.
    const s = Stores.summary();
    if (s.ready && (s.unavailable || !s.connected)) {
      const why = s.unavailable
        ? `Bandcamp is unavailable on this Plippa: ${s.reason}.`
        : "Not connected — use Connect… above.";
      view.replaceChildren(head(), el("p", { cls: "empty", text: why }));
      return;
    }
    view.replaceChildren(head(), el("div", { attrs: { id: "bc-list", "aria-live": "polite" } }));
    if (!state.page || state.page.query !== state.query) loadList(0);
    else paintList();
  }

  async function loadList(offset) {
    const my = ++state.seq;
    state.loading = true;
    const box = $("bc-list");
    if (box && offset === 0) {
      box.replaceChildren(el("p", { cls: "muted", text: "Loading your Bandcamp collection…" }));
    }
    const params = new URLSearchParams({
      query: state.query,
      offset: String(offset),
      size: String(SIZE),
    });
    let data;
    try {
      data = await api(`/api/stores/bandcamp/collection?${params.toString()}`);
    } catch (e) {
      if (my !== state.seq) return;
      state.loading = false;
      if (box) box.replaceChildren(el("p", { cls: "msg", text: codeText(e) }));
      return;
    }
    if (my !== state.seq) return;
    state.loading = false;
    data.query = state.query;
    if (offset > 0 && state.page) {
      state.page = {
        ...data,
        releases: state.page.releases.concat(data.releases || []),
      };
    } else {
      state.page = data;
    }
    paintList();
  }

  function paintList() {
    const box = $("bc-list");
    if (!box || !state.page) return;
    const page = state.page;
    const releases = page.releases || [];
    const parts = [];

    const meta = [
      `${plural(releases.length, "release", "releases")} of ${page.total || 0}`,
      page.syncedAt ? `synced ${ago(page.syncedAt)}` : "never synced",
    ];
    if (page.stale) meta.push("out of date");
    parts.push(el("p", { cls: "muted bc-meta", text: meta.join(" · ") }));
    if (page.stale && !page.syncedAt) {
      parts.push(
        el("p", {
          cls: "note",
          text: "No collection is cached yet. Use Sync now to fetch it from Bandcamp.",
        })
      );
    }

    if (!releases.length) {
      parts.push(
        el("p", {
          cls: "muted",
          text: state.query
            ? "Nothing in the cached collection matches that search."
            : "The cached collection is empty.",
        })
      );
    } else {
      const grid = el("div", { cls: "cards" });
      for (const r of releases) {
        const meta = [];
        if (r.trackCount != null) meta.push(plural(r.trackCount, "track", "tracks"));
        if (r.durationS != null) meta.push(duration(r.durationS));
        if (r.year) meta.push(String(r.year));
        grid.appendChild(
          el(
            "a",
            {
              cls: "card",
              attrs: {
                href: `#/bandcamp/${encodeURIComponent(r.id)}`,
                "data-id": r.id,
              },
            },
            [
              coverBox(r, "card-cover"),
              el("div", { cls: "card-text" }, [
                el("strong", { text: r.title || "(untitled)" }),
                r.artist ? el("span", { cls: "muted", text: r.artist }) : null,
                el("span", { cls: "muted", text: meta.join(" · ") }),
              ]),
            ]
          )
        );
      }
      parts.push(grid);
    }

    if (releases.length < (page.total || 0)) {
      parts.push(
        el("button", {
          text: `Show more (${(page.total || 0) - releases.length} more)`,
          cls: "more",
          attrs: { type: "button" },
          on: { click: () => loadList(releases.length) },
        })
      );
    }
    box.replaceChildren(...parts);
  }

  // ---- release detail ---------------------------------------------------------------------

  async function loadRelease(id) {
    // SRV-06: GET is cache-only. An id that is not in the cached collection answers
    // `not_cached`; one JSON POST is the consented lookup, which fills the cache. Ids
    // normally come from the cached list, so this is the reload-after-a-change path.
    try {
      return await api(`/api/stores/bandcamp/release/${encodeURIComponent(id)}`);
    } catch (e) {
      if (e.code !== "not_cached") throw e;
      return api("/api/stores/bandcamp/release", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id }),
      });
    }
  }

  async function renderDetail(view, id) {
    const my = ++state.seq;
    if (state.releaseId !== id) {
      state.releaseId = id;
      state.selected = new Set();
      state.noubinKey = "";
    }
    view.replaceChildren(el("p", { cls: "muted", text: "Loading release…" }));
    let d;
    try {
      d = await loadRelease(id);
    } catch (e) {
      if (my !== state.seq) return;
      view.replaceChildren(
        el("div", { cls: "empty" }, [
          el("p", { text: codeText(e) }),
          el("a", { cls: "back", text: "← Bandcamp", attrs: { href: "#/bandcamp" } }),
        ])
      );
      return;
    }
    if (my !== state.seq) return;
    state.release = d;
    paintDetail(view, d);
  }

  function paintDetail(view, d) {
    const r = d.release;
    const tracks = d.tracks || [];
    const root = el("div", { cls: "collection" });
    root.appendChild(el("a", { cls: "back", text: "← Bandcamp", attrs: { href: "#/bandcamp" } }));

    const headBox = el("div", { cls: "coll-head" }, [
      r.hasCover ? coverBox(r, "coll-cover") : el("div", { cls: "coll-cover nocover" }),
      el("div", { cls: "coll-title" }, [
        el("h2", { text: r.title || "(untitled)" }),
        el("p", { cls: "muted", text: [r.artist, r.year ? String(r.year) : ""].filter(Boolean).join(" · ") }),
        el("p", {
          cls: "muted",
          text: [
            plural(tracks.length, "track", "tracks"),
            r.durationS != null ? duration(r.durationS) : "",
            d.cached ? "in the cached collection" : "fetched now",
          ]
            .filter(Boolean)
            .join(" · "),
        }),
      ]),
    ]);
    root.appendChild(headBox);

    // The honesty note cannot be dropped: a stream is not the purchased file.
    root.appendChild(
      el("p", { cls: "note bc-quality", role: "note" }, [
        el("strong", { text: "Lossy stream, not the purchased FLAC. " }),
        el("span", { text: d.quality.note }),
      ])
    );
    root.appendChild(
      el("p", { cls: "muted", text: "The server downloads each track in turn, with a short pause between tracks." })
    );

    if (!tracks.length) {
      root.appendChild(el("p", { cls: "muted", text: "This release lists no tracks." }));
      view.replaceChildren(root);
      return;
    }

    const selectAll = el("button", {
      text: "Select all",
      attrs: { type: "button" },
      on: {
        click: () => {
          const all = tracks.every((t) => state.selected.has(t.id));
          state.selected = new Set(all ? [] : tracks.map((t) => t.id));
          paintDetail(view, d);
        },
      },
    });
    const download = el("button", {
      text: "Download…",
      cls: "primary",
      attrs: { type: "button", id: "bc-download" },
      on: { click: () => prepareDownload() },
    });
    download.disabled = state.selected.size === 0;

    // Linking is optional, and the sheet then says the release will be fetched. A key is
    // only ever written from here; the server validates it before anything is downloaded.
    const noubin = el("input", {
      cls: "bc-noubin",
      attrs: {
        type: "url",
        id: "bc-noubin",
        placeholder: "https://… (optional Noubin release URL)",
        "aria-label": "Noubin release URL for this download (optional)",
        autocomplete: "off",
        spellcheck: "false",
      },
      on: {
        input: (ev) => {
          state.noubinKey = ev.target.value.trim();
        },
      },
    });
    noubin.value = state.noubinKey;
    const linkRow = el("div", { cls: "bc-link" }, [
      el("label", { attrs: { for: "bc-noubin" }, text: "Noubin URL (optional)" }),
      noubin,
      el("p", {
        cls: "muted",
        text: "With a URL the download is united against that release's metadata and its cover art. Without one it uses the fields Bandcamp provides.",
      }),
    ]);

    const actions = el("div", { cls: "coll-actions" }, [
      selectAll,
      el("span", { cls: "muted", text: plural(state.selected.size, "track selected", "tracks selected") }),
      download,
    ]);
    root.appendChild(linkRow);
    root.appendChild(actions);

    const table = el("table", { cls: "tracks" }, [
      el("thead", null, [
        el("tr", null, [
          el("th", { attrs: { scope: "col" }, text: "Select" }),
          el("th", { attrs: { scope: "col" }, text: "#" }),
          el("th", { attrs: { scope: "col" }, text: "Title" }),
          el("th", { attrs: { scope: "col" }, text: "Length" }),
          el("th", { attrs: { scope: "col" }, text: "Declared" }),
        ]),
      ]),
      el(
        "tbody",
        null,
        tracks.map((t) => {
          const box = el("input", {
            attrs: {
              type: "checkbox",
              "aria-label": `Select ${t.title || "track"}`,
            },
            on: {
              change: (ev) => {
                if (ev.target.checked) state.selected.add(t.id);
                else state.selected.delete(t.id);
                const btn = $("bc-download");
                if (btn) btn.disabled = state.selected.size === 0;
              },
            },
          });
          box.checked = state.selected.has(t.id);
          const declared = [];
          if (t.suffix) declared.push(t.suffix.toUpperCase());
          if (t.declaredBytes != null) declared.push(Ui.fmtBytes(t.declaredBytes));
          if (t.declaredBitrate != null) declared.push(`${t.declaredBitrate} kbps`);
          return el("tr", null, [
            el("td", null, [box]),
            el("td", { text: t.track != null ? String(t.track) : "" }),
            el("td", { text: t.title || "(untitled)" }),
            el("td", { text: t.durationS != null ? duration(t.durationS) : "" }),
            el("td", { cls: "muted", text: declared.join(" · ") }),
          ]);
        })
      ),
    ]);
    root.appendChild(el("div", { cls: "table-scroll" }, [table]));
    root.appendChild(
      el("p", {
        cls: "muted",
        text: "“Declared” is the file the store advertises; the stream that arrives is a different, lossy encode.",
      })
    );
    view.replaceChildren(root);
  }

  // ---- consent and queue ------------------------------------------------------------------

  function selectedTracks() {
    const d = state.release;
    if (!d) return [];
    return (d.tracks || [])
      .filter((t) => state.selected.has(t.id))
      .map((t) => ({ release: d.release.id, track: t.id }));
  }

  async function prepareDownload() {
    const tracks = selectedTracks();
    if (!tracks.length) return;
    state.busy = true;
    setError("");
    try {
      const c = await api(
        "/api/stores/bandcamp/download/prepare",
        jsonBody(state.noubinKey ? { tracks, noubinKey: state.noubinKey } : { tracks })
      );
      state.consent = c;
      state.consentId = c.consentId;
      state.phase = "consent";
      state.status = null;
      state.session = null;
      openDialog();
    } catch (e) {
      toast(codeText(e), true);
    }
    state.busy = false;
  }

  function openDialog() {
    const d = dlg();
    if (!d || d.open) return;
    state.opener = document.activeElement;
    Ui.modal(d, () => state.opener);
    setError("");
    if (state.phase === "consent") paintConsent();
    else if (state.phase === "running" || state.phase === "result") paintProgress();
    d.showModal();
    // Confirm is deliberately not the default: reading the sheet comes first.
    const cancel = $("store-dl-cancel");
    if (cancel) cancel.focus();
  }

  function closeDialog() {
    stopPoll();
    const d = dlg();
    if (d && d.open) d.close();
  }

  function paintConsent() {
    const c = state.consent;
    const body = $("store-dl-body");
    if (!c || !body) return;
    const parts = [];
    parts.push(el("p", { text: c.consentMessage }));
    parts.push(
      el("p", { cls: "note bc-quality", role: "note" }, [
        el("strong", { text: "Quality: lossy stream. " }),
        el("span", { text: c.quality.warning }),
      ])
    );
    if (c.noubin) {
      parts.push(
        el("p", { cls: "note", role: "note" }, [
          el("strong", { text: "Metadata: linked Noubin release. " }),
          el("span", { text: `This fetches ${c.noubin.fetchUrl} from ${c.noubin.host} and unites the download against it. Each track's measured length is checked against the release; a mismatch is reported, not hidden.` }),
        ])
      );
    }
    const size = c.estimatedBytes != null ? Ui.fmtBytes(c.estimatedBytes) : "unknown";
    parts.push(
      el("p", {
        cls: "muted",
        text: [
          `${plural(c.tracks, "track", "tracks")} · about ${size}`,
          c.estimated ? "estimated from the store's declared file sizes" : "the store declared no sizes",
          c.freeBytes != null ? `${Ui.fmtBytes(c.freeBytes)} free` : "",
          `limits: ${Ui.fmtBytes(c.limits.maxFileBytes)} per file, ${Ui.fmtBytes(c.limits.maxBatchBytes)} per batch`,
        ]
          .filter(Boolean)
          .join(" · "),
      })
    );

    const list = el("ul", { cls: "bc-consent-list" });
    for (const r of c.releases || []) {
      list.appendChild(
        el("li", null, [
          el("strong", { text: `${r.title} — ${r.artist}` }),
          el("span", { cls: "muted", text: ` · ${plural(r.trackCount, "track", "tracks")}${r.estimatedBytes != null ? ` · ${Ui.fmtBytes(r.estimatedBytes)}` : ""}` }),
          el(
            "ul",
            null,
            (r.tracks || []).map((t) =>
              el("li", {
                text: `${t.disc && t.disc > 1 ? `Disc ${t.disc}, ` : ""}${t.track != null ? `Track ${t.track}: ` : ""}${t.title}${t.estimatedBytes != null ? ` (${Ui.fmtBytes(t.estimatedBytes)})` : ""}`,
              })
            )
          ),
        ])
      );
    }
    parts.push(list);

    if ((c.refused || []).length) {
      parts.push(
        el("div", { cls: "note", role: "alert" }, [
          el("strong", { text: "Not included: " }),
          el("ul", null, c.refused.map((r) => el("li", { text: `${r.release}: ${r.reason}` }))),
        ])
      );
    }

    body.replaceChildren(...parts);
    const ok = $("store-dl-ok");
    ok.textContent = `Download ${plural(c.tracks, "track", "tracks")}`;
    ok.disabled = c.tracks === 0;
    $("store-dl-cancel").textContent = "Cancel";
  }

  async function startDownload() {
    if (state.phase !== "consent" || !state.consentId) return;
    state.busy = true;
    setError("");
    try {
      const answer = await api(
        "/api/stores/bandcamp/download",
        jsonBody({ consent: true, consentId: state.consentId })
      );
      state.session = answer.session;
      state.phase = "running";
      state.status = { state: "running", items: [], doneBytes: 0, totalBytes: 0, warnings: [], errors: [] };
      paintProgress();
      pollStatus();
    } catch (e) {
      setError(codeText(e));
    }
    state.busy = false;
    updateButtons();
  }

  function stopPoll() {
    if (state.pollTimer) clearTimeout(state.pollTimer);
    state.pollTimer = null;
  }

  function pollStatus() {
    stopPoll();
    state.pollTimer = setTimeout(async () => {
      if (!state.session) return;
      let s;
      try {
        s = await api(`/api/stores/bandcamp/download/${encodeURIComponent(state.session)}`);
      } catch (e) {
        if (e.code === "unknown_session") {
          state.phase = "result";
          state.status = { state: "failed", items: [], warnings: [], errors: [codeText(e)] };
          paintProgress();
          return;
        }
        stopPoll();
        setError(codeText(e));
        return;
      }
      state.status = s;
      const running = s.state === "running";
      if (!running) state.phase = "result";
      try {
        paintProgress();
      } finally {
        // Even if a paint throws, keep the queue moving: a stuck dialog must not outlive a
        // download that is still running server-side.
        if (running) {
          pollStatus();
        } else {
          toast(
            s.state === "done"
              ? "Download complete"
              : s.state === "partial"
                ? "Some tracks were not downloaded"
                : s.state === "cancelled"
                  ? "Download cancelled; partial files removed"
                  : "Download failed",
            s.state !== "done"
          );
        }
      }
    }, 700);
  }

  function stateWord(item, replay) {
    switch (item.state) {
      case "queued":
        return "waiting";
      case "running":
        return "downloading";
      case "done":
        return "done";
      case "failed":
        return "failed";
      case "cancelled":
        return "cancelled";
      case "skipped":
        // A skipped track on an `exists` result was already in the library, not a failure.
        return replay ? "already in your library" : "not attempted";
      default:
        return item.state || "";
    }
  }

  function paintProgress() {
    const s = state.status;
    const body = $("store-dl-body");
    if (!body) return;
    if (!s) {
      body.replaceChildren(el("p", { cls: "muted", text: "Starting…" }));
      return;
    }
    const items = s.items || [];
    const parts = [];
    // Declared for both branches: the row list below labels skipped tracks differently on an
    // `exists` replay, so this must not live inside the finished-state branch.
    const replay = Boolean(s.unite && s.unite.state === "exists");

    if (state.phase === "running") {
      const total = s.totalBytes || 0;
      parts.push(
        el("p", {
          text: `Downloading ${items.filter((i) => i.state === "done").length} of ${items.length} tracks · ${Ui.fmtBytes(s.doneBytes || 0)}${total ? ` of ${Ui.fmtBytes(total)}` : ""}`,
        })
      );
      const bar = el("progress", { attrs: { "aria-label": "Download progress" } });
      if (total) {
        bar.setAttribute("max", "100");
        bar.setAttribute("value", String(Math.min(100, Math.round(((s.doneBytes || 0) / total) * 100))));
      }
      parts.push(bar);
    } else {
      const done = items.filter((i) => i.state === "done");
      const bad = items.filter((i) => i.state === "failed" || i.state === "skipped");
      if (s.state === "done" && replay && done.length === 0) {
        // Every track was skipped because the release is already here. That is not a
        // download that fetched nothing by accident.
        parts.push(
          el("p", { cls: "bc-result ok", role: "status", text: "Nothing was downloaded: this release is already in your library." })
        );
      } else if (s.state === "done") {
        parts.push(
          el("p", { cls: "bc-result ok", role: "status" }, [
            el("strong", { text: `Downloaded ${plural(done.length, "track", "tracks")}. ` }),
            el("span", { text: `${Ui.fmtBytes(s.doneBytes || 0)} fetched.` }),
          ])
        );
      } else if (s.state === "partial") {
        // A partial result must never look like success.
        parts.push(
          el("p", { cls: "msg bc-result partial", role: "alert" }, [
            el("strong", { text: `Only ${done.length} of ${plural(items.length, "track", "tracks")} were downloaded. ` }),
            el("span", { text: `${bad.length} did not arrive; they are listed below.` }),
          ])
        );
      } else if (s.state === "cancelled") {
        parts.push(
          el("p", { cls: "msg bc-result", role: "status", text: "Cancelled. Partial files were deleted; nothing was added to the library." })
        );
      } else {
        const partialPlace =
          s.unite && s.unite.state === "rejected" && s.unite.noudata
            ? " The items below say which tracks did arrive."
            : " Nothing was added to the library.";
        parts.push(
          el("p", { cls: "msg bc-result", role: "alert", text: `The download failed.${partialPlace}` })
        );
      }
    }

    const rows = items.map((i) => {
      const detail = [];
      if (i.state === "done") {
        detail.push(i.original ? "original file" : i.streamSubstituted ? "lossy stream substituted" : "file");
        if (i.format) detail.push(i.format.toUpperCase());
        if (i.quality) detail.push(i.quality);
        if (i.bitrateKbps) detail.push(`${i.bitrateKbps} kbps`);
        detail.push(Ui.fmtBytes(i.bytes || 0));
        if (i.durationMs != null) detail.push(fmtMs(i.durationMs));
      } else if (i.state === "running") {
        detail.push(`${Ui.fmtBytes(i.bytes || 0)}${i.totalBytes ? ` of ${Ui.fmtBytes(i.totalBytes)}` : ""}`);
      }
      if (i.error) detail.push(i.error);
      return el("li", { cls: `bc-item ${i.state}` }, [
        el("strong", { text: `${i.release} — ${i.track}` }),
        el("span", { cls: "muted", text: ` · ${stateWord(i, replay)}${detail.length ? ` · ${detail.join(" · ")}` : ""}` }),
      ]);
    });
    parts.push(el("ul", { cls: "bc-queue" }, rows));

    if ((s.warnings || []).length) {
      parts.push(
        el("ul", { cls: "preview-warn" }, s.warnings.map((w) => el("li", { text: w })))
      );
    }
    if ((s.errors || []).length) {
      parts.push(
        el("ul", { cls: "preview-warn" }, s.errors.map((w) => el("li", { text: w })))
      );
    }
    if (s.unite) {
      parts.push(
        el("p", {
          cls: s.unite.state === "rejected" ? "msg" : "note",
          text:
            s.unite.state === "rejected"
              ? `The download could not be added to the library: ${s.unite.detail}`
              : s.unite.detail,
        })
      );
    }
    if (s.unite && (s.unite.state === "placed" || s.unite.state === "exists")) {
      if (s.unite.state === "exists") {
        parts.push(el("p", { cls: "note", text: "Already in your library; nothing was downloaded again." }));
      }
      // Say which metadata the item carries. Store metadata is not a failure, but the
      // person should know the release is not linked and that linking is possible.
      const linked = s.unite.metadata === "noudata";
      parts.push(
        el("p", {
          cls: linked ? "note bc-linked" : "note bc-unlinked",
          text: linked
            ? "Metadata from the linked Noubin release."
            : "Metadata from Bandcamp (no Noubin release linked).",
        })
      );
      if (s.unite.canLink) {
        const link = el("button", {
          text: "Link a Noubin release…",
          attrs: { type: "button", id: "bc-link-release" },
          on: {
            click: () => {
              closeDialog();
              const field = $("bc-noubin");
              if (field) {
                field.focus();
                field.scrollIntoView({ block: "center" });
                toast("Paste this release's Noubin URL, then download it again to link the item.");
              }
            },
          },
        });
        parts.push(
          el("p", { cls: "muted", text: "The files are in the library; linking unites them against the release, which downloads them again." })
        );
        parts.push(el("div", { cls: "coll-actions" }, [link]));
      }
    }
    body.replaceChildren(...parts);
    updateButtons();
  }

  function updateButtons() {
    const ok = $("store-dl-ok");
    const cancel = $("store-dl-cancel");
    if (!ok || !cancel) return;
    if (state.phase === "running") {
      ok.textContent = "Downloading…";
      ok.disabled = true;
      cancel.textContent = "Cancel download";
      cancel.disabled = false;
    } else if (state.phase === "result") {
      ok.textContent = "Close";
      ok.disabled = false;
      cancel.textContent = "Close";
      cancel.disabled = false;
    } else {
      ok.textContent = state.consent
        ? `Download ${plural(state.consent.tracks, "track", "tracks")}`
        : "Download";
      ok.disabled = state.busy || !state.consent || state.consent.tracks === 0;
      cancel.textContent = "Cancel";
      cancel.disabled = false;
    }
  }

  async function cancelDownload() {
    if (!state.session) return;
    try {
      await api(
        `/api/stores/bandcamp/download/${encodeURIComponent(state.session)}/cancel`,
        jsonBody({})
      );
      toast("Cancelling; partial files are removed");
    } catch (e) {
      setError(codeText(e));
    }
  }

  function confirmOrClose() {
    if (state.phase === "consent") startDownload();
    else if (state.phase === "result") closeDialog();
  }

  function cancelOrClose() {
    if (state.phase === "running") cancelDownload();
    else closeDialog();
  }

  // ---- wiring -----------------------------------------------------------------------------

  function init() {
    const d = dlg();
    if (!d) return;
    $("store-dl-ok").addEventListener("click", confirmOrClose);
    $("store-dl-cancel").addEventListener("click", cancelOrClose);
    d.addEventListener("cancel", (ev) => {
      ev.preventDefault();
      if (state.phase === "running") {
        toast("Cancel the download before closing this window.", true);
      } else {
        closeDialog();
      }
    });
    d.addEventListener("close", () => {
      stopPoll();
      state.session = null;
      state.status = null;
      state.consent = null;
      state.consentId = null;
      state.phase = "idle";
      setError("");
    });
  }

  init();

  /**
   * WEB_01: the platform view. The connection row (`Stores.header`) is built once here and sits
   * above the album list / release detail, which keep painting into their own container below it
   * (so a repaint never removes the row).
   */
  function render(view, id) {
    const body = el("div", { cls: "store-body" });
    view.replaceChildren(
      el("div", { cls: "store-top" }, [
        el("a", { cls: "back", text: "← Distros", attrs: { href: "#/distros" } }),
        Stores.header(),
      ]),
      body
    );
    // The row can only paint once it is in the document. Without this the first paint shows the
    // built-in "not connected" text and live buttons until the next 30 s poll (review).
    Stores.updateHeader();
    return id ? renderDetail(body, id) : renderList(body);
  }

  /**
   * Repaint the album list from the current status. Called when a connect, a disconnect or a
   * sync changes the collection while this view is open. Does nothing when a release is open or
   * the view is no longer in the document.
   */
  function refresh() {
    const body = state.listView;
    if (!body || state.release || !document.contains(body)) return;
    // Rebuild the body when it is the "not connected" / unavailable state, or when it has no
    // list container yet (a connect just landed). A list container is only valid while there is
    // a connection to load from, so a disconnect must go back through `renderList` too.
    const s = Stores.summary();
    const empty = s.ready && (s.unavailable || !s.connected);
    if (empty || !$("bc-list")) {
      // The cached page belongs to the previous connection; drop it so a reconnect cannot paint
      // the old collection for a moment.
      state.page = null;
      renderList(body);
      return;
    }
    loadList(0);
  }

  return { render, refresh };
})();
