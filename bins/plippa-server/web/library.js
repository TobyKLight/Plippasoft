// NB_10: category library (two-level navigation), collection views, playlist editing and the
// per-track "⋮" menu. Plain script, no build step; talks only to the NB_03/NB_04 endpoints
// (`/api/kinds`, `/api/collections…`, `/api/playlists…`, `/api/items/:id/cues`).
//
// Everything that comes from files/tags/metadata (titles, artists, cue text, playlist names,
// error messages) is put on the page with textContent / createElement only — never innerHTML
// (NBSecurityRequirements §5.1). Relies on globals from app.js: api, $, idStr, showBanner,
// fmtMs, openDetail, refreshNowPlaying.

const Library = (() => {
  /** Long lists show this many rows, then a "Show more" button (no virtual scrolling needed). */
  const PAGE = 200;
  const NAV_KEY = "plippa.nav";

  // The library views, in the order of devPlippaSW/libraryUIHeirarchy.md. `flat` views list
  // individual tracks (GET /api/tracks), the others list releases / playlists
  // (GET /api/collections?kind=…). (Artists and Files are not implemented yet.)
  const TABS = [
    { key: "tracks", kind: "track", label: "Tracks", flat: true },
    { key: "albums", kind: "album", label: "Albums" },
    { key: "playlists", kind: "playlist", label: "Playlists" },
    { key: "podcasts", kind: "podcast", label: "Podcasts" },
    { key: "audiobooks", kind: "audiobook", label: "Audiobooks" },
    { key: "mixes", kind: "mix", label: "Mixes", flat: true },
    // WEB_01: distribution platforms (Bandcamp today) live under Distros, not as a tab.
    { key: "distros", label: "Distros" },
  ];
  /**
   * WEB_01: views reachable by hash but not shown in the tab strip. Bandcamp became a platform
   * inside Distros, and `#/bandcamp` / `#/bandcamp/<release>` still resolve (bookmarks, and the
   * "Show in library" links a download produces).
   */
  const HIDDEN_VIEWS = ["bandcamp"];
  const DEFAULT_VIEW = "tracks";

  /** One platform per row in the Distros view. Adding one means adding an entry here and a view. */
  const PLATFORMS = [{ key: "bandcamp", label: "Bandcamp" }];

  function knownView(key) {
    return TABS.some((t) => t.key === key) || HIDDEN_VIEWS.includes(key);
  }
  const EMPTY_HINT = {
    tracks: "No tracks yet.",
    albums: "No albums yet.",
    playlists: "No playlists yet. Use “Create playlist” to make one.",
    podcasts: "No podcasts yet.",
    audiobooks: "No audiobooks yet.",
    mixes: "No mixes yet.",
  };

  const state = {
    tab: "tracks",
    id: null, // open collection (decimal string) or null
    editing: false,
    filter: "",
    shown: PAGE,
    counts: {}, // kind -> count
    storeConnected: null, // last Bandcamp connection state seen by the Distros row
    detail: null, // last loaded CollectionDetail
    np: null, // last now-playing snapshot (for the "playing" highlight)
    creating: false,
    editNext: false, // open the next collection straight in edit mode (new playlist)
    cueNote: null, // {id, text}: shown in the collection view after a timestamps save (NB_16)
  };
  let seq = 0; // drops answers of superseded renders
  let headCovers = null; // ART_05: slider of the open collection's header (destroyed on re-render)
  function releaseCovers() {
    if (headCovers) headCovers.destroy();
    headCovers = null;
  }

  // ---- tiny DOM helpers -------------------------------------------------------------

  function el(tag, opts, children) {
    const n = document.createElement(tag);
    if (opts) {
      if (opts.cls) n.className = opts.cls;
      if (opts.text != null) n.textContent = Ui.clean(String(opts.text));
      if (opts.attrs) for (const [k, v] of Object.entries(opts.attrs)) n.setAttribute(k, v);
      if (opts.on) for (const [k, fn] of Object.entries(opts.on)) n.addEventListener(k, fn);
    }
    for (const c of children || []) if (c) n.appendChild(c);
    return n;
  }

  function button(label, onClick, opts) {
    const o = opts || {};
    return el("button", {
      text: label,
      cls: o.cls,
      attrs: { type: "button", ...(o.attrs || {}) },
      on: { click: onClick },
    });
  }

  function fmtDuration(ms) {
    return ms == null ? "" : fmtMs(ms);
  }

  function pluralItems(n) {
    return `${n} ${n === 1 ? "item" : "items"}`;
  }

  function toast(message, isError) {
    const box = $("toast");
    if (!box) return;
    box.textContent = Ui.clean(String(message));
    box.className = isError ? "toast error" : "toast";
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => {
      box.className = "toast hidden";
      box.textContent = "";
    }, isError ? 7000 : 3500);
  }

  function tabInfo(key) {
    return TABS.find((t) => t.key === key) || TABS.find((t) => t.key === DEFAULT_VIEW) || TABS[0];
  }

  // ---- routing ------------------------------------------------------------------------

  function loadSavedTab() {
    try {
      const v = JSON.parse(localStorage.getItem(NAV_KEY) || "null");
      // WEB_01: a saved "bandcamp" is a tab that no longer exists; land on Distros instead.
      const key = v && v.tab === "bandcamp" ? "distros" : v && v.tab;
      if (key && knownView(key)) return key;
    } catch {
      /* storage unavailable or corrupt: default tab */
    }
    return DEFAULT_VIEW;
  }

  function saveTab(tab) {
    try {
      localStorage.setItem(NAV_KEY, JSON.stringify({ tab }));
    } catch {
      /* storage unavailable */
    }
  }

  /**
   * `#/albums/42` -> {tab:"albums", id:"42"}; `#/bandcamp/a%3A1` -> id "a:1".
   * Store release ids are opaque and may hold `:` or `/`, so the segment is decoded here.
   * WEB_01: `bandcamp` is still accepted even though it is no longer a tab.
   */
  function parseHash(hash) {
    const m = /^#\/([a-z]+)(?:\/([^/]+))?\/?$/.exec(hash || "");
    if (!m || !knownView(m[1])) return null;
    let id = m[2] || null;
    if (id != null) {
      try {
        id = decodeURIComponent(id);
      } catch {
        return null;
      }
    }
    return { tab: m[1], id };
  }

  function go(tab, id, replace) {
    const hash = id ? `#/${tab}/${encodeURIComponent(id)}` : `#/${tab}`;
    if (replace) {
      history.replaceState(null, "", hash);
      route();
    } else if (location.hash === hash) {
      route();
    } else {
      location.hash = hash; // hashchange -> route()
    }
  }

  function route() {
    const r = parseHash(location.hash) || { tab: loadSavedTab(), id: null };
    const changed = r.tab !== state.tab || r.id !== state.id;
    state.tab = r.tab;
    state.id = r.id;
    if (changed) {
      state.editing = state.editNext && !!r.id;
      state.editNext = false;
      state.filter = "";
      state.shown = PAGE;
      state.creating = false;
      state.cueNote = null;
    }
    saveTab(state.tab);
    renderNav();
    if (changed) {
      // Never leave the previous view (with its handlers for the old id) clickable while the
      // new one loads.
      const view = $("lib-view");
      if (view) view.replaceChildren(el("p", { cls: "muted", text: "Loading…" }));
    }
    return render();
  }

  // ---- navigation ---------------------------------------------------------------------

  function renderNav() {
    const nav = $("lib-nav");
    if (!nav) return;
    const tabs = el("div", { cls: "lib-tabs", attrs: { role: "tablist", "aria-label": "Library" } });
    // WEB_01: every tab is always visible. Distros owns the Bandcamp view, so it reads as
    // selected from inside it.
    for (const t of TABS) {
      const on = t.key === state.tab || (t.key === "distros" && state.tab === "bandcamp");
      const n = state.counts[t.kind];
      tabs.appendChild(
        button(n != null ? `${t.label} (${n})` : t.label, () => go(t.key, null), {
          cls: on ? "tab-btn on" : "tab-btn",
          attrs: { role: "tab", "aria-selected": String(on), "data-tab": t.key },
        })
      );
    }
    const parts = [tabs];
    // WEB_01: same condition as before the tabs changed — the pointer is the only way to the
    // connect dialog from outside the Distros tab, so it must not depend on the first response.
    if (!distroSummary().connected) {
      parts.push(
        button("Bandcamp: connect to browse your collection.", () => {
          if (typeof Stores !== "undefined" && Stores.openConnect) Stores.openConnect();
        }, { cls: "link store-connect-pointer" })
      );
    }
    nav.replaceChildren(...parts);
  }

  /** stores.js's view of Bandcamp, or a neutral "not known yet" before the first response. */
  function distroSummary() {
    if (typeof Stores === "undefined" || !Stores.summary) return { ready: false, connected: false };
    return Stores.summary();
  }

  async function refreshCounts() {
    try {
      const r = await api("/api/kinds");
      const counts = {};
      for (const k of r.kinds || []) counts[k.kind] = k.count;
      state.counts = counts;
      renderNav();
    } catch {
      /* the tab labels just stay without counts */
    }
  }

  // ---- rendering entry ----------------------------------------------------------------

  /** WEB_01: the Distros row's right-hand text, in the words the old bar used. */
  function distroStateText(s) {
    if (!s.ready) return "Checking…";
    if (s.unavailable) return "Unavailable on this Plippa";
    if (!s.connected) return s.rejected ? "Not connected (credentials rejected)" : "Not connected";
    // `releases === null` means "no cache yet", which is not the same as an empty collection.
    if (s.releases === null || s.releases === undefined) return "Connected";
    const n = s.releases;
    return n ? `Connected · ${n} ${n === 1 ? "release" : "releases"}` : "Connected · 0 releases";
  }

  /**
   * WEB_01: Distros — one row per distribution platform, with its state. Bandcamp is the only
   * platform today; opening a row shows that platform's view (stores.js owns Bandcamp's).
   */
  function renderDistros() {
    const s = distroSummary();
    const box = el("div", { cls: "distros" });
    box.appendChild(el("h2", { text: "Distros" }));
    box.appendChild(
      el("p", { cls: "muted", text: "Music from the platforms your Plippa can download from." })
    );
    const list = el("div", { cls: "distro-list" });
    for (const p of PLATFORMS) {
      list.appendChild(
        el(
          "button",
          {
            cls: "distro-row",
            attrs: { type: "button", "data-platform": p.key },
            on: { click: () => go(p.key, null) },
          },
          [
            el("span", { cls: "distro-name", text: p.label }),
            el("span", { cls: "muted", text: distroStateText(s) }),
          ]
        )
      );
    }
    box.appendChild(list);
    // The reason belongs to the platform that reported it, not to Bandcamp by name.
    if (s.ready && s.unavailable) {
      const platform = PLATFORMS[0] || { label: "Bandcamp" };
      box.appendChild(
        el("p", {
          cls: "note",
          attrs: { role: "status" },
          text: `${platform.label} is unavailable on this Plippa: ${s.reason}. Everything else keeps working.`,
        })
      );
    }
    return box;
  }

  async function render() {
    const my = ++seq;
    if (!state.id) releaseCovers();
    const view = $("lib-view");
    if (!view) return;
    // The tab labels carry counts on every tab, including after a reload straight onto a
    // platform tab (WEB_01 review). Started once before the branch so the early returns get it
    // too; it repaints the nav when it answers.
    refreshCounts().catch(() => {});
    if (state.tab === "distros") {
      // WEB_01: the platform list. No API call: stores.js keeps the one status we show.
      view.replaceChildren(renderDistros());
      return;
    }
    if (state.tab === "bandcamp") {
      // stores.js owns this view (collection, detail, consent, progress).
      if (typeof Bandcamp !== "undefined") {
        await Bandcamp.render(view, state.id);
      } else {
        view.replaceChildren(el("p", { cls: "muted", text: "Bandcamp is not available yet." }));
      }
      return;
    }
    try {
      if (state.id) {
        const detail = await api(`/api/collections/${encodeURIComponent(state.id)}`);
        if (my !== seq) return;
        state.detail = detail;
        view.replaceChildren(renderCollection(detail));
      } else {
        const info = tabInfo(state.tab);
        const r = info.flat
          ? await api(`/api/tracks?view=${encodeURIComponent(info.kind)}`)
          : await api(`/api/collections?kind=${encodeURIComponent(info.kind)}`);
        if (my !== seq) return;
        state.detail = null;
        renderList(view, (info.flat ? r.tracks : r.collections) || []);
      }
    } catch (e) {
      if (my !== seq) return;
      const box = el("div", { cls: "empty" }, [el("p", { text: e.message })]);
      if (state.id) box.appendChild(button("Back to list", () => go(state.tab, null)));
      view.replaceChildren(box);
    }
  }

  // ---- list views ---------------------------------------------------------------------

  let listCache = [];

  function matches(c, f) {
    if (!f) return true;
    const q = f.toLowerCase();
    return (
      (c.title || "").toLowerCase().includes(q) ||
      (c.artist || "").toLowerCase().includes(q) ||
      (c.collection_title || "").toLowerCase().includes(q)
    );
  }

  function renderList(view, items) {
    listCache = items;
    const info = tabInfo(state.tab);
    const head = el("div", { cls: "view-head" }, [el("h2", { text: info.label })]);
    if (state.tab === "playlists") {
      head.appendChild(button("Create playlist", () => {
        state.creating = true;
        renderListBody();
        const inp = $("new-playlist-title");
        if (inp) inp.focus();
      }, { cls: "primary", attrs: { id: "create-playlist-btn" } }));
    }
    view.replaceChildren(head, el("div", { attrs: { id: "lib-list-body" } }));
    renderListBody();
  }

  function renderListBody() {
    const body = $("lib-list-body");
    if (!body) return;
    const info = tabInfo(state.tab);
    const parts = [];
    if (state.tab === "playlists" && state.creating) parts.push(createPlaylistForm());
    if (!listCache.length) {
      parts.push(emptyState(info));
      body.replaceChildren(...parts);
      return;
    }
    const filtered = listCache.filter((c) => matches(c, state.filter));
    parts.push(
      el("input", {
        cls: "filter",
        attrs: { type: "search", placeholder: `Filter ${info.label.toLowerCase()}…`, "aria-label": "Filter", value: state.filter, id: "lib-filter" },
        on: {
          input: (ev) => {
            state.filter = ev.target.value;
            state.shown = PAGE;
            renderListBody();
            const f = $("lib-filter");
            f.focus();
            f.setSelectionRange(f.value.length, f.value.length);
          },
        },
      })
    );
    if (!filtered.length) {
      parts.push(el("p", { cls: "muted", text: "Nothing matches the filter." }));
    } else if (info.flat) {
      parts.push(trackTable(filtered.slice(0, state.shown)));
    } else {
      parts.push(cardGrid(filtered.slice(0, state.shown), info));
    }
    if (filtered.length > state.shown) {
      parts.push(
        button(`Show more (${filtered.length - state.shown} more)`, () => {
          state.shown += PAGE;
          renderListBody();
        }, { cls: "more" })
      );
    }
    body.replaceChildren(...parts);
  }

  function emptyState(info) {
    const long = ["podcasts", "audiobooks", "mixes"].includes(info.key);
    return el("div", { cls: "empty", attrs: { "data-empty": info.key } }, [
      el("p", { cls: "empty-title", text: EMPTY_HINT[info.key] }),
      info.key !== "playlists"
        ? el("p", {
            cls: "muted",
            text:
              "Use “Add media” above to add some" +
              (long ? " (pick the matching kind)." : "."),
          })
        : null,
    ]);
  }

  function createPlaylistForm() {
    const input = el("input", {
      attrs: { type: "text", id: "new-playlist-title", placeholder: "Playlist name", maxlength: "200", "aria-label": "New playlist name" },
    });
    const submit = async () => {
      const title = input.value.trim();
      if (!title) return;
      try {
        const d = await api("/api/playlists", jsonBody("POST", { title }));
        state.creating = false;
        toast(`Created playlist “${d.title}”`);
        state.editNext = true;
        go("playlists", idStr(d.id));
      } catch (e) {
        toast(e.message, true);
      }
    };
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") submit();
      if (ev.key === "Escape") {
        state.creating = false;
        renderListBody();
      }
    });
    return el("div", { cls: "inline-form" }, [
      input,
      button("Create", submit, { cls: "primary" }),
      button("Cancel", () => {
        state.creating = false;
        renderListBody();
      }),
    ]);
  }

  /** Flat list of tracks (Tracks and Mixes views): one row per `mediaList` item. */
  function trackTable(rows) {
    const tbody = el("tbody");
    for (const c of rows) {
      const id = idStr(c.id);
      const pos = c.position || 0;
      const multi = c.collection_items > 1;
      // The title opens the release / file the track belongs to (its chapters, timestamps and
      // "Details…" live there).
      const titleLine = el("span", { cls: "title-line" }, [
        el("a", { cls: "link", text: c.title || "(untitled)", attrs: { href: `#/${tabInfo(state.tab).key}/${id}`, title: multi ? `Open “${c.collection_title || ""}”` : "Open" } }),
        c.available === false ? el("span", { cls: "flag", text: "missing" }) : null,
      ]);
      // D16: a thumbnail only when the API sent one (the track's art differs from its album's).
      const titleCell = el("td", { cls: "title-cell" }, [
        c.cover_thumb ? el("div", { cls: "row-with-cover" }, [Covers.thumb(c.cover_thumb, { cls: "row-cover", size: 128 }), titleLine]) : titleLine,
      ]);
      const tr = el("tr", { attrs: { "data-id": id, "data-pos": String(pos) } }, [
        titleCell,
        el("td", { text: c.artist || "" }),
        el("td", { cls: "muted col-from", text: multi ? c.collection_title || "" : "" }),
        el("td", { cls: "num", text: fmtDuration(c.duration_ms) }),
        el("td", { cls: "muted col-lib", text: c.library_label || "" }),
        el("td", { cls: "row-actions" }, [
          button("▶", () => playCollection(id, multi ? { item: pos } : {}), { cls: "icon", attrs: { title: "Play", "aria-label": `Play ${c.title || "track"}` } }),
          menuButton({ track_id: id, item: pos }, c.title || "(untitled)", {
            detailId: id,
            cues: cueAccess(c, pos, c.title),
            covers: multi ? null : coversAccess(c, c.title),
            del: deleteAccess(c),
          }),
        ]),
      ]);
      if (isNowPlayingRow(id, pos, multi)) tr.className = "playing";
      tbody.appendChild(tr);
    }
    // The wrapper keeps an over-wide table scrolling inside the library column.
    return el("div", { cls: "table-scroll" }, [
      el("table", { cls: "tracks" }, [
        el("thead", null, [
          el("tr", null, [
            el("th", { text: "Title" }),
            el("th", { text: "Artist" }),
            el("th", { cls: "col-from", text: "From" }),
            el("th", { cls: "num", text: "Length" }),
            el("th", { cls: "col-lib", text: "Library" }),
            el("th", { text: "" }),
          ]),
        ]),
        tbody,
      ]),
    ]);
  }

  function isNowPlayingRow(id, pos, multi) {
    const np = state.np;
    if (!np || np.playable_id !== id) return false;
    return !multi || np.track_index == null || np.track_index === pos;
  }

  function cardGrid(rows, info) {
    const grid = el("div", { cls: "cards" });
    for (const c of rows) {
      const id = idStr(c.id);
      const meta = [pluralItems(c.item_count)];
      if (c.duration_ms != null) meta.push(fmtDuration(c.duration_ms));
      const card = el("a", {
        cls: "card",
        attrs: { href: `#/${info.key}/${id}`, "data-id": id },
      }, [
        Covers.thumb(c.cover_thumb, { cls: "card-cover", alt: "" }),
        el("div", { cls: "card-text" }, [
          el("strong", { text: c.title || "(untitled)" }),
          c.artist ? el("span", { cls: "muted", text: c.artist }) : null,
          el("span", { cls: "muted", text: meta.join(" · ") }),
          c.available === false ? el("span", { cls: "flag", text: "some files missing" }) : null,
        ]),
      ]);
      // The ⋮ menu sits beside the link (a button must not be nested in an <a>).
      const cell = el("div", { cls: "card-cell" }, [card]);
      const menu = menuButton({ track_id: id, item: 0 }, c.title || "(untitled)", {
        detailId: id,
        collectionMenu: true,
        covers: coversAccess(c),
        del: deleteAccess(c),
      });
      menu.classList.add("card-menu");
      cell.appendChild(menu);
      grid.appendChild(cell);
    }
    return grid;
  }

  // ---- collection view ----------------------------------------------------------------

  function jsonBody(method, obj) {
    return { method, headers: { "Content-Type": "application/json" }, body: JSON.stringify(obj) };
  }

  function renderCollection(d) {
    releaseCovers();
    const id = idStr(d.id);
    const info = tabInfo(state.tab);
    const editable = !!d.editable;
    // Albums/releases are reorderable but not editable: edit mode shows ↑/↓ + drag only.
    const reorderable = !!d.reorderable;
    const editing = reorderable && state.editing;
    const root = el("div", { cls: "collection", attrs: { "data-id": id } });

    root.appendChild(
      el("a", { cls: "back", text: `← ${info.label}`, attrs: { href: `#/${info.key}` } })
    );

    const meta = [];
    if (d.artist) meta.push(d.artist);
    // `kind` is what the file is; `group` the tab that lists it. A tagless mix upload is a loose
    // track (kind "track") listed under Mixes (group "mix").
    const kindLabel = (k) => k;
    meta.push(d.group && d.group !== d.kind ? `${kindLabel(d.group)} (loose track)` : kindLabel(d.kind));
    meta.push(pluralItems(d.item_count));
    if (d.duration_ms != null) meta.push(fmtDuration(d.duration_ms));

    const titleBox = el("div", { cls: "coll-title" });
    if (editing && editable) {
      titleBox.appendChild(renameForm(d));
    } else {
      titleBox.appendChild(el("h2", { text: d.title || "(untitled)", attrs: { id: "coll-title" } }));
    }
    titleBox.appendChild(el("p", { cls: "muted", text: meta.join(" · ") }));
    if (d.available === false) titleBox.appendChild(el("p", { cls: "flag", text: "Some media files are missing." }));
    if (d.created_by && d.kind === "playlist") {
      titleBox.appendChild(el("p", { cls: "muted", text: `Created by ${d.created_by}` }));
    }

    const actions = el("div", { cls: "coll-actions" }, [
      button("▶ Play", () => playCollection(id, {}), { cls: "primary", attrs: { id: "coll-play" } }),
      button("Details…", () => openDetail(id).catch((e) => showBanner(e.message)), { attrs: { id: "coll-details", title: "Metadata, Noubin association" } }),
      button("Associate…", () => associateCollection(id), {
        attrs: {
          id: "coll-associate",
          title: "Write this item's key to a Noubin: tap it on the player when asked",
        },
      }),
    ]);
    const headCoversAccess = coversAccess(d);
    if (headCoversAccess) {
      actions.appendChild(
        menuButton({ track_id: id, item: 0 }, d.title || "(untitled)", {
          collectionMenu: true,
          covers: headCoversAccess,
          del: deleteAccess(d),
        })
      );
    }
    if (reorderable) {
      actions.appendChild(
        button(editing ? "Done" : "Edit", () => {
          state.editing = !state.editing;
          rerenderCollection();
        }, { attrs: { id: "coll-edit" } })
      );
      if (editing && editable) {
        actions.appendChild(button("Delete playlist", () => deletePlaylist(d), { cls: "danger", attrs: { id: "coll-delete" } }));
      }
    }

    // NB_16: timestamps live in the ⋮ menu of each item row (a collection has no own timestamps
    // and a single-track release is reached there too). Only an orphan needs a button here.
    if (d.is_orphan) {
      actions.appendChild(
        button("Create metadata…", (ev) => createMetadata(cueAccess(d, 0, d.title), ev.currentTarget), {
          attrs: { id: "coll-create-meta", title: "This file has no metadata (.noudata) yet" },
        })
      );
    }

    const coverSlot = el("div", { cls: "coll-cover" }, [Covers.thumb(d.cover_thumb, { cls: "cover-stage", size: 512 })]);
    const head = el("div", { cls: "coll-head" }, [coverSlot, titleBox, actions]);
    root.appendChild(head);
    if (d.cover_count >= 1) {
      headCovers = Covers.mount(coverSlot, id, { size: 512 });
      head.classList.add("has-cover-slider");
    }
    if (editing && !editable) {
      root.appendChild(
        el("p", {
          cls: "muted",
          attrs: { id: "coll-reorder-note" },
          text: "Reordering changes the track order in the album file; filenames are not changed.",
        })
      );
    }
    if (d.is_orphan) {
      root.appendChild(
        el("p", {
          cls: "muted",
          attrs: { id: "coll-orphan-note" },
          text: "This file has no metadata (.noudata) yet, so it cannot have timestamps. Use “Create metadata…” first.",
        })
      );
    }
    if (state.cueNote && state.cueNote.id === id) {
      root.appendChild(el("p", { cls: "note", attrs: { id: "coll-cue-note", role: "status" }, text: state.cueNote.text }));
    }

    if (!d.items.length) {
      root.appendChild(
        el("div", { cls: "empty" }, [
          el("p", { cls: "empty-title", text: editable ? "This playlist is empty." : "Nothing in here." }),
          editable ? el("p", { cls: "muted", text: "Use the ⋮ menu on any track and choose “Add to playlist…”." }) : null,
        ])
      );
      return root;
    }
    root.appendChild(itemList(d, id, editing));
    return root;
  }

  function rerenderCollection() {
    if (!state.detail) return route();
    $("lib-view").replaceChildren(renderCollection(state.detail));
  }

  /** Collection-view "Associate…": same flow as the details panel, for the open collection. */
  async function associateCollection(id) {
    toast("Tap a Noubin on the player now…");
    try {
      toast(await associatePlayable(id, false));
    } catch (e) {
      toast(e.message, true);
    }
    // The tag may have rewritten the file (a link track becomes a release, titles move).
    route();
  }

  function renameForm(d) {
    const input = el("input", {
      attrs: { type: "text", maxlength: "200", value: d.title, "aria-label": "Playlist name", id: "rename-input" },
    });
    const submit = async () => {
      const title = input.value.trim();
      if (!title || title === d.title) return;
      try {
        const nd = await api(`/api/playlists/${encodeURIComponent(idStr(d.id))}`, jsonBody("PATCH", { title, revision: d.revision || undefined }));
        toast(`Renamed to “${nd.title}”`);
        // The file (and so the id) may change with the name: always continue with the new id.
        state.editNext = true; // stay in edit mode under the new id
        go("playlists", idStr(nd.id), true);
      } catch (e) {
        conflictOr(e);
      }
    };
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") submit();
    });
    return el("div", { cls: "inline-form" }, [input, button("Rename", submit, { attrs: { id: "rename-btn" } })]);
  }

  async function deletePlaylist(d) {
    if (!window.confirm(`Delete the playlist “${d.title}”? The tracks themselves are not deleted.`)) return;
    try {
      await api(`/api/playlists/${encodeURIComponent(idStr(d.id))}${d.revision ? `?revision=${encodeURIComponent(d.revision)}` : ""}`, { method: "DELETE" });
      toast(`Deleted playlist “${d.title}”`);
      state.editing = false;
      go("playlists", null);
    } catch (e) {
      conflictOr(e);
    }
  }

  /** A 409 means the file changed elsewhere: say so and reload what is on disk. */
  function conflictOr(e) {
    toast(e.message, true);
    route();
  }

  function itemList(d, id, editing) {
    const ol = el("ol", { cls: editing ? "items editing" : "items", attrs: { "aria-label": "Items" } });
    const limit = Math.min(d.items.length, state.shown);
    for (let i = 0; i < limit; i++) ol.appendChild(itemRow(d, id, i, editing));
    const wrap = el("div", null, [ol]);
    if (d.items.length > limit) {
      wrap.appendChild(
        button(`Show more (${d.items.length - limit} more)`, () => {
          state.shown += PAGE;
          rerenderCollection();
        }, { cls: "more" })
      );
    }
    return wrap;
  }

  function itemRow(d, id, i, editing) {
    const editable = !!d.editable;
    const it = d.items[i];
    const li = el("li", { cls: "item", attrs: { "data-pos": String(it.position), "data-index": String(i) } });
    if (!it.available) li.classList.add("unavailable");
    if (state.np && state.np.playable_id === id && state.np.track_index === i) li.classList.add("playing");

    const main = el("div", { cls: "item-main" }, [
      el("span", { cls: "item-no", text: String(i + 1) }),
      it.cover_thumb ? Covers.thumb(it.cover_thumb, { cls: "row-cover", size: 128 }) : null,
      el("span", { cls: "item-title" }, [
        el("strong", { text: it.title || "(untitled)" }),
        it.artist ? el("span", { cls: "muted", text: ` — ${it.artist}` }) : null,
        it.available ? null : el("span", { cls: "flag", text: "missing" }),
      ]),
      el("span", { cls: "num muted", text: fmtDuration(it.duration_ms) }),
    ]);
    const acts = el("div", { cls: "row-actions" });
    if (editing) {
      li.draggable = true;
      acts.appendChild(el("span", { cls: "grip", text: "⠿", attrs: { title: "Drag to reorder", "aria-hidden": "true" } }));
      acts.appendChild(button("↑", () => moveItem(d, i, i - 1), { cls: "icon", attrs: { title: "Move up", "aria-label": `Move ${it.title} up`, ...(i === 0 ? { disabled: "" } : {}) } }));
      acts.appendChild(button("↓", () => moveItem(d, i, i + 1), { cls: "icon", attrs: { title: "Move down", "aria-label": `Move ${it.title} down`, ...(i === d.items.length - 1 ? { disabled: "" } : {}) } }));
      if (editable) {
        acts.appendChild(button("✕", () => removeItem(d, it), { cls: "icon danger", attrs: { title: "Remove from playlist", "aria-label": `Remove ${it.title}` } }));
      }
      wireDrag(li, d, i);
    } else {
      acts.appendChild(button("▶", () => playCollection(id, { item: it.position }), { cls: "icon", attrs: { title: "Play from here", "aria-label": `Play ${it.title}` } }));
    }
    // FIX_01E: timestamps live in the ⋮ menu only (no per-row button).
    acts.appendChild(
      menuButton({ track_id: id, item: it.position }, it.title || "(untitled)", {
        cues: cueAccess(d, it.position, it.title),
        del: deleteAccess(d, it.position, it.title),
      })
    );
    li.appendChild(el("div", { cls: "item-line" }, [main, acts]));

    if (!editing && it.cue_count > 0 && it.has_navigable_cues) {
      li.appendChild(cueToggle(id, it));
    }
    return li;
  }

  // ---- timestamps editing (NB_16) ---------------------------------------------------

  const PLAYLIST_CUES_HINT =
    "Playlists have no timestamps of their own. Open the original mix, album or track to edit its timestamps.";

  /** What the ⋮ menu / buttons may do with timestamps for this playable (`rec` = summary/detail). */
  function cueAccess(rec, item, title) {
    const id = idStr(rec.id);
    if (rec.kind === "playlist") return { blocked: PLAYLIST_CUES_HINT };
    if (rec.is_orphan) return { orphan: true, id, title: title || rec.title };
    return { id, item: item || 0, title: title || rec.title };
  }

  function isLoadedOnPlayer(id) {
    return !!(state.np && state.np.playable_id === id);
  }

  function editTimestamps(acc, opener) {
    const openerId = opener && opener.id;
    EditCues.open({
      id: acc.id,
      item: acc.item,
      title: acc.title,
      opener,
      isLoaded: () => isLoadedOnPlayer(acc.id),
      onSaved: () => {
        state.cueNote = isLoadedOnPlayer(acc.id)
          ? {
              id: acc.id,
              text: "Timestamps saved. This item is loaded on the player: the new timestamps apply the next time it is loaded (press Play on it again).",
            }
          : null;
        refreshCounts().catch(() => {});
        // The view is rebuilt: put the focus back on the same button.
        Promise.resolve(route()).then(() => {
          const n = openerId && document.getElementById(openerId);
          if (n) n.focus();
        });
      },
    });
  }

  function createMetadata(acc, opener) {
    EditCues.openCreate({
      id: acc.id,
      title: acc.title,
      opener,
      onCreated: (made) => {
        go(made.tab, made.id);
        if (made.longForm) {
          editTimestamps({ id: made.id, item: 0, title: made.title }, null);
        }
      },
    });
  }

  // ---- cues -----------------------------------------------------------------------

  function cueToggle(id, it) {
    const holder = el("div", { cls: "cues-holder" });
    const btn = button(`Cues (${it.cue_count})`, async () => {
      if (holder.dataset.open === "1") {
        holder.dataset.open = "0";
        holder.querySelector(".cue-list")?.remove();
        btn.setAttribute("aria-expanded", "false");
        return;
      }
      holder.dataset.open = "1";
      btn.setAttribute("aria-expanded", "true");
      const list = el("ul", { cls: "cue-list" }, [el("li", { cls: "muted", text: "Loading cues…" })]);
      holder.appendChild(list);
      try {
        const r = await api(`/api/items/${encodeURIComponent(id)}/cues?item=${it.position}`);
        list.replaceChildren(...cueRows(id, it, r));
      } catch (e) {
        list.replaceChildren(el("li", { cls: "muted", text: e.message }));
      }
    }, { cls: "cues-toggle", attrs: { "aria-expanded": "false" } });
    holder.appendChild(btn);
    return holder;
  }

  function cueRows(id, it, r) {
    const rows = [];
    for (const l of r.cue_lists || []) {
      if (!l.navigable) continue;
      if (l.title && (r.cue_lists || []).filter((x) => x.navigable).length > 1) {
        rows.push(el("li", { cls: "cue-list-title muted", text: l.title }));
      }
      for (const c of l.cues) {
        rows.push(
          el("li", { cls: "cue" }, [
            el("button", {
              cls: "cue-btn",
              attrs: { type: "button", title: "Play from this cue" },
              on: { click: () => playCollection(id, { item: it.position, cue_ms: c.start_ms }) },
            }, [el("span", { cls: "cue-time", text: fmtMs(c.start_ms) }), el("span", { cls: "cue-text", text: c.text || "(no text)" })]),
          ])
        );
      }
    }
    return rows.length ? rows : [el("li", { cls: "muted", text: "No navigable cues." })];
  }

  // ---- playlist item edits ---------------------------------------------------------

  async function removeItem(d, it) {
    try {
      const rev = d.revision ? `&revision=${encodeURIComponent(d.revision)}` : "";
      const nd = await api(
        `/api/playlists/${encodeURIComponent(idStr(d.id))}/items/${it.position}?path=${encodeURIComponent(it.media_path)}${rev}`,
        { method: "DELETE" }
      );
      applyDetail(nd);
      toast(`Removed “${it.title}”`);
    } catch (e) {
      conflictOr(e);
    }
  }

  /** Reorder: move the row at `from` to `to`. */
  async function moveItem(d, from, to) {
    if (from === to || to < 0 || to >= d.items.length) return;
    const order = d.items.map((_, i) => i);
    const [x] = order.splice(from, 1);
    order.splice(to, 0, x);
    try {
      const nd = await api(
        `/api/collections/${encodeURIComponent(idStr(d.id))}/order`,
        jsonBody("PUT", { positions: order, revision: d.revision || undefined })
      );
      applyDetail(nd);
      // Keep keyboard focus on the moved row's mover button.
      const row = document.querySelector(`#lib-view li.item[data-index="${to}"]`);
      const sel = to < from ? 'button[title="Move up"]' : 'button[title="Move down"]';
      const b = row && (row.querySelector(`${sel}:not([disabled])`) || row.querySelector("button:not([disabled])"));
      if (b) b.focus();
    } catch (e) {
      conflictOr(e);
    }
  }

  function applyDetail(nd) {
    state.detail = nd;
    rerenderCollection();
    refreshCounts();
  }

  let dragFrom = null;

  function wireDrag(li, d, i) {
    li.addEventListener("dragstart", (ev) => {
      dragFrom = i;
      li.classList.add("dragging");
      if (ev.dataTransfer) {
        ev.dataTransfer.effectAllowed = "move";
        ev.dataTransfer.setData("text/plain", String(i));
      }
    });
    li.addEventListener("dragend", () => {
      dragFrom = null;
      li.classList.remove("dragging");
      document.querySelectorAll("#lib-view li.drop-target").forEach((n) => n.classList.remove("drop-target"));
    });
    li.addEventListener("dragover", (ev) => {
      if (dragFrom == null) return;
      ev.preventDefault();
      if (ev.dataTransfer) ev.dataTransfer.dropEffect = "move";
      li.classList.add("drop-target");
    });
    li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
    li.addEventListener("drop", (ev) => {
      ev.preventDefault();
      li.classList.remove("drop-target");
      const from = dragFrom;
      dragFrom = null;
      if (from != null) moveItem(d, from, i);
    });
  }

  // ---- playing ------------------------------------------------------------------------

  async function playCollection(id, body) {
    try {
      if (body.cue_ms || body.item != null) toast("Starting…");
      await api(`/api/collections/${encodeURIComponent(id)}/play`, jsonBody("POST", body));
      setTimeout(() => refreshNowPlaying().catch(() => {}), 300);
    } catch (e) {
      toast(e.message, true);
    }
  }

  // ---- cover art editing (ART_06) -----------------------------------------------------

  /** What the ⋮ menus pass to the cover editor, or null when this collection / track cannot have
   *  its art edited (a dynamic playlist has no art of its own; unavailable media and a media file
   *  without metadata are fixed first). `rec` = a collection / track summary. */
  function coversAccess(rec, title) {
    if (rec.is_dynamic || rec.is_orphan || rec.available === false) return null;
    return { id: idStr(rec.id), title: title || rec.title || "" };
  }

  /** What the ⋮ menus pass to the delete dialog, or null when this record cannot be deleted
   *  here (a playlist has its own Delete, and the last media file of a release cannot go
   *  on its own). `item` = a `mediaList` position for a single-file delete. */
  function deleteAccess(rec, item, itemTitle) {
    const acc = DeleteItem.access(rec, item);
    if (!acc) return null;
    return { ...acc, itemTitle: itemTitle || "", collectionTitle: rec.title || "" };
  }

  function deleteItem(acc) {
    DeleteItem.open({
      id: acc.id,
      item: acc.item,
      title: acc.item == null ? acc.collectionTitle : acc.itemTitle || acc.collectionTitle,
      opener: document.activeElement,
      onDeleted: (id) => {
        // Closing the open item: fall back to the list; otherwise just refresh what is shown.
        if (state.id === id) go(state.tab, null);
        else route();
      },
    });
  }

  function editCovers(acc, opener) {
    EditCovers.open({
      id: acc.id,
      title: acc.title,
      opener,
      onSaved: () => coversSaved(acc.id),
    });
  }

  /** Redraw everything that shows the art of `id` after a Save. */
  function coversSaved(id) {
    if (typeof selectedId !== "undefined" && selectedId === id) showDetailCovers(id);
    return route();
  }

  // ---- "⋮" menu + Add to playlist -------------------------------------------------------

  let openMenu = null; // {node, anchor}

  function closeMenu(refocus) {
    if (!openMenu) return;
    const { node, anchor } = openMenu;
    openMenu = null;
    node.remove();
    document.removeEventListener("mousedown", onOutside, true);
    document.removeEventListener("keydown", onMenuKey, true);
    if (anchor) anchor.setAttribute("aria-expanded", "false");
    if (refocus && anchor && document.contains(anchor)) anchor.focus();
  }

  function onOutside(ev) {
    if (!openMenu) return;
    const { node, anchor } = openMenu;
    // A press on the anchor itself is the anchor's own toggle (menuButton / pickPlaylist).
    if (anchor && anchor.contains(ev.target)) return;
    if (!node.contains(ev.target)) closeMenu(false);
  }

  function onMenuKey(ev) {
    if (!openMenu) return;
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeMenu(true);
      return;
    }
    if (ev.key === "ArrowDown" || ev.key === "ArrowUp" || ev.key === "Home" || ev.key === "End") {
      const items = [...openMenu.node.querySelectorAll('[role="menuitem"]:not([disabled])')];
      if (!items.length) return;
      ev.preventDefault();
      const cur = items.indexOf(document.activeElement);
      let next;
      if (ev.key === "Home") next = 0;
      else if (ev.key === "End") next = items.length - 1;
      else if (ev.key === "ArrowDown") next = cur < 0 ? 0 : (cur + 1) % items.length;
      else next = cur <= 0 ? items.length - 1 : cur - 1;
      items[next].focus();
    }
  }

  function menuButton(ref, title, opts) {
    const b = el("button", {
      cls: "icon kebab",
      text: "⋮",
      attrs: { type: "button", "aria-haspopup": "menu", "aria-expanded": "false", title: "More", "aria-label": `More actions for ${title}` },
    });
    b.addEventListener("click", (ev) => {
      ev.stopPropagation();
      if (openMenu && openMenu.anchor === b) {
        closeMenu(true);
        return;
      }
      closeMenu(false);
      showMenu(b, ref, title, opts || {});
    });
    return b;
  }

  const NO_META_HINT = "This file has no metadata (.noudata) yet, so it cannot have timestamps. Choose “Create metadata…” first.";

  /** `opts.disabled` = explanation: the item stays focusable (aria-disabled) and shows it as tooltip/toast. */
  function menuItem(label, onClick, opts) {
    const attrs = { type: "button", role: "menuitem" };
    if (opts && opts.disabled) {
      attrs["aria-disabled"] = "true";
      attrs.title = opts.disabled;
    }
    return el("button", { cls: opts && opts.disabled ? "menuitem disabled-look" : "menuitem", text: label, attrs, on: { click: onClick } });
  }

  function showMenu(anchor, ref, title, opts) {
    const node = el("div", { cls: "menu", attrs: { role: "menu", "aria-label": `Actions for ${title}` } });
    openMenu = { node, anchor };
    anchor.setAttribute("aria-expanded", "true");
    const main = () => {
      const items = opts.collectionMenu ? [] : [menuItem("Add to playlist…", () => playlistPicker(node, ref, title))];
      if (opts.detailId) {
        items.push(menuItem("Details / associate…", () => {
          closeMenu(false);
          openDetail(opts.detailId).catch((e) => showBanner(e.message));
        }));
      }
      const cues = opts.cues;
      if (cues && cues.blocked) {
        items.push(menuItem("Edit timestamps…", () => toast(cues.blocked), { disabled: cues.blocked }));
      } else if (cues && cues.orphan) {
        items.push(menuItem("Edit timestamps…", () => toast(NO_META_HINT), { disabled: NO_META_HINT }));
        items.push(menuItem("Create metadata…", () => {
          closeMenu(false);
          createMetadata(cues, anchor);
        }));
      } else if (cues) {
        items.push(menuItem("Edit timestamps…", () => {
          closeMenu(false);
          editTimestamps(cues, anchor);
        }));
      }
      if (opts.covers) {
        items.push(menuItem("Edit cover art…", () => {
          closeMenu(false);
          editCovers(opts.covers, anchor);
        }));
      }
      if (opts.del) {
        // BCS_10: preview first; the dialog is the only place that can actually delete.
        const single = opts.del.item != null ? "Delete this file…" : "Delete…";
        items.push(menuItem(single, () => {
          closeMenu(false);
          deleteItem(opts.del);
        }));
      }
      node.replaceChildren(...items);
      items[0].focus();
    };
    document.body.appendChild(node);
    placeMenu(node, anchor);
    document.addEventListener("mousedown", onOutside, true);
    document.addEventListener("keydown", onMenuKey, true);
    main();
  }

  function placeMenu(node, anchor) {
    const r = anchor.getBoundingClientRect();
    const w = Math.min(320, window.innerWidth - 16);
    node.style.width = `${w}px`;
    const left = Math.max(8, Math.min(window.innerWidth - w - 8, r.right - w));
    node.style.left = `${left + window.scrollX}px`;
    node.style.top = `${r.bottom + window.scrollY + 4}px`;
  }

  async function playlistPicker(node, ref, title) {
    node.replaceChildren(el("div", { cls: "menu-head muted", text: "Add to playlist…" }), el("div", { cls: "menuitem muted", text: "Loading playlists…" }));
    let lists = [];
    let anyPlaylists = false;
    try {
      const r = await api("/api/collections?kind=playlist");
      // A playlist cannot be added to itself (the server refuses it): leave it out rather than
      // offering an action that must fail.
      const self = ref && ref.track_id != null ? idStr(ref.track_id) : null;
      const editable = (r.collections || []).filter((c) => c.editable);
      anyPlaylists = editable.length > 0;
      lists = editable.filter((c) => idStr(c.id) !== self);
    } catch (e) {
      toast(e.message, true);
      closeMenu(true);
      return;
    }
    if (!openMenu || openMenu.node !== node) return;
    const rows = lists.map((p) =>
      menuItem(p.title || "(untitled)", async () => {
        try {
          await api(`/api/playlists/${encodeURIComponent(idStr(p.id))}/items`, jsonBody("POST", { track_id: ref.track_id, ...(ref.item != null ? { item: ref.item } : {}) }));
          toast(`Added “${title}” to “${p.title}”`);
          closeMenu(true);
          refreshCounts();
          if (state.id === idStr(p.id)) route();
        } catch (e) {
          toast(e.message, true);
          closeMenu(true);
        }
      })
    );
    const newItem = menuItem("New playlist…", () => newPlaylistInMenu(node, ref, title));
    const empty = anyPlaylists
      ? el("div", { cls: "menuitem muted", text: "Only this playlist (it cannot be added to itself)." })
      : el("div", { cls: "menuitem muted", text: "No playlists yet." });
    node.replaceChildren(
      el("div", { cls: "menu-head muted", text: "Add to playlist…" }),
      ...(rows.length ? rows : [empty]),
      newItem
    );
    (rows[0] || newItem).focus();
  }

  /**
   * Open the playlist picker for playable `id`, anchored to a button outside this module (the
   * details panel). Every item of the playable is appended to the chosen playlist — a track is
   * one entry, a collection contributes all its tracks in album order.
   */
  function pickPlaylist(anchor, id, title) {
    if (openMenu && openMenu.anchor === anchor) {
      closeMenu(true);
      return;
    }
    closeMenu(false);
    const named = title || "this item";
    const node = el("div", { cls: "menu", attrs: { role: "menu", "aria-label": `Add ${named} to a playlist` } });
    openMenu = { node, anchor };
    anchor.setAttribute("aria-expanded", "true");
    document.body.appendChild(node);
    placeMenu(node, anchor);
    document.addEventListener("mousedown", onOutside, true);
    document.addEventListener("keydown", onMenuKey, true);
    playlistPicker(node, { track_id: idStr(id) }, named);
  }

  function newPlaylistInMenu(node, ref, title) {
    const input = el("input", { attrs: { type: "text", maxlength: "200", placeholder: "Playlist name", "aria-label": "New playlist name" } });
    const submit = async () => {
      const name = input.value.trim();
      if (!name) return;
      try {
        const d = await api("/api/playlists", jsonBody("POST", { title: name, tracks: [ref] }));
        toast(`Added “${title}” to new playlist “${d.title}”`);
        closeMenu(true);
        refreshCounts();
        if (state.tab === "playlists" && !state.id) route();
      } catch (e) {
        toast(e.message, true);
      }
    };
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        submit();
      }
    });
    node.replaceChildren(
      el("div", { cls: "menu-head muted", text: "New playlist" }),
      el("div", { cls: "menu-form" }, [input, button("Create + add", submit, { cls: "primary", attrs: { role: "menuitem" } })])
    );
    input.focus();
  }

  // ---- public ---------------------------------------------------------------------------

  /** Called by app.js with every now-playing poll: keeps the "playing" highlight current. */
  function noteNowPlaying(np) {
    const prev = state.np;
    state.np = np;
    if (!np) return;
    const same =
      prev &&
      prev.playable_id === np.playable_id &&
      prev.track_index === np.track_index &&
      prev.list_seq === np.list_seq;
    if (same) return;
    document.querySelectorAll("#lib-view .playing").forEach((n) => n.classList.remove("playing"));
    if (state.id && state.id === np.playable_id && np.track_index != null) {
      const row = document.querySelector(`#lib-view li.item[data-index="${np.track_index}"]`);
      if (row) row.classList.add("playing");
    } else if (!state.id && tabInfo(state.tab).flat && np.playable_id) {
      for (const row of document.querySelectorAll(`#lib-view tr[data-id="${np.playable_id}"]`)) {
        const multi = row.querySelector(".title-cell") && row.children[2] && row.children[2].textContent !== "";
        if (isNowPlayingRow(np.playable_id, Number(row.getAttribute("data-pos") || 0), multi)) row.classList.add("playing");
      }
    }
  }

  function init() {
    window.addEventListener("hashchange", () => {
      closeMenu(false);
      route();
    });
    window.addEventListener("resize", () => closeMenu(false));
    return route();
  }

  /**
   * stores.js calls this after every GET /api/stores. WEB_01: Distros is always visible, so this
   * only keeps the status text (tab strip pointer and the Distros row) current. An unchanged
   * value is ignored: the 30 s poll must not rebuild the nav under a keyboard user's focus.
   */
  function setStoreConnected(connected) {
    const next = !!connected;
    if (next === state.storeConnected) return;
    state.storeConnected = next;
    renderNav();
    refreshDistros();
  }

  /** Re-render the Distros row in place. Nothing happens on any other view. */
  function refreshDistros() {
    if (state.tab !== "distros") return;
    const view = $("lib-view");
    if (view) view.replaceChildren(renderDistros());
  }

  return {
    init,
    refresh: () => route(),
    noteNowPlaying,
    parseHash,
    setStoreConnected,
    refreshDistros,
    TABS,
    pickPlaylist,
    editCovers,
    coversAccess,
    deleteAccess,
  };
})();
