/* Plippa media portal — unite dialog (NB_09): add a release to the library.
 *
 * Case A  free release: look up (consent) -> overview -> download (second consent listing every
 *         URL) -> add to library.
 * Case B  files from elsewhere: look up (consent) -> pick files -> proposal -> fix by hand -> add.
 * Case C  the files already include a `_<release>.noudata`: no lookup, they are only associated.
 * Refresh manually update an existing item's online metadata (consent each time).
 *
 * Security: every server-provided string (titles, file names, URLs, errors) goes through
 * textContent / createElement only — never innerHTML. Uses the helpers of app.js.
 * Minimal on purpose: NB_11 brings the polished modal patterns; keep the markup reusable.
 */

const unite = { sid: null, key: "", overview: null, proposal: null, files: [], poll: null };

function uniteBox() {
  return $("unite");
}

function uniteShow(title) {
  $("unite-title").textContent = title;
  uniteBox().classList.remove("hidden");
  clearNode($("unite-body"));
  clearNode($("unite-actions"));
  $("unite-error").textContent = "";
}

function uniteError(e) {
  $("unite-error").textContent = e && e.message ? e.message : String(e);
}

function uniteButton(label, onClick, primary) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  if (primary) b.className = "primary";
  b.addEventListener("click", () => {
    Promise.resolve()
      .then(onClick)
      .catch(uniteError);
  });
  $("unite-actions").appendChild(b);
  return b;
}

function uniteClose() {
  if (unite.poll) clearInterval(unite.poll);
  unite.poll = null;
  if (unite.sid) {
    // Forgetting the session deletes everything staged for it.
    api(`/api/unite/${unite.sid}`, { method: "DELETE" }).catch(() => {});
  }
  unite.sid = null;
  uniteBox().classList.add("hidden");
}

function fmtBytes(n) {
  if (n == null) return "";
  if (n >= 1048576) return (n / 1048576).toFixed(1) + " MB";
  if (n >= 1024) return (n / 1024).toFixed(0) + " kB";
  return n + " B";
}

function jsonPost(path, body) {
  return api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
}

// ---- step 1: which release? -------------------------------------------------------------

function uniteStart(prefillKey) {
  uniteShow("Add a release to your library");
  const body = $("unite-body");
  body.appendChild(
    text(
      "p",
      "Paste the release's Noubin address (or tap the tag first). Looking it up contacts a " +
        "server on the internet, and only after you confirm. Local playback never does.",
      "muted"
    )
  );
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = "https://artist.example/noubin/release/";
  input.value = prefillKey || "";
  input.setAttribute("aria-label", "Noubin address");
  body.appendChild(input);
  const consent = document.createElement("div");
  consent.className = "hidden";
  body.appendChild(consent);

  uniteButton("Look up online…", async () => {
    unite.key = input.value.trim();
    const p = await jsonPost("/api/web/prepare", { noubinKey: unite.key });
    clearNode(consent);
    consent.appendChild(text("p", p.consent_message));
    const line = document.createElement("p");
    line.appendChild(text("span", "Address ", "label"));
    line.appendChild(text("code", p.fetch_url));
    consent.appendChild(line);
    const go = document.createElement("button");
    go.type = "button";
    go.textContent = "Contact server";
    go.addEventListener("click", () => {
      go.disabled = true;
      jsonPost("/api/unite/start", { noubinKey: unite.key, consent: true })
        .then((r) => {
          unite.sid = r.session;
          unite.overview = r.overview;
          uniteOverview();
        })
        .catch((e) => {
          go.disabled = false;
          uniteError(e);
        });
    });
    consent.appendChild(go);
    consent.classList.remove("hidden");
  });
  uniteButton("I already have the release folder (no lookup)…", async () => {
    unite.key = input.value.trim();
    const r = await jsonPost("/api/unite/start", {
      associateOnly: true,
      noubinKey: unite.key || undefined,
    });
    unite.sid = r.session;
    unite.overview = null;
    uniteFiles();
  });
  uniteButton("Close", uniteClose);
}

// ---- step 2: overview ------------------------------------------------------------------

function uniteOverview() {
  const o = unite.overview;
  uniteShow(o.title || "(untitled release)");
  const body = $("unite-body");
  if (o.artist) body.appendChild(text("p", o.artist, "muted"));
  const ol = document.createElement("ol");
  (o.tracks || []).forEach((t) => {
    const li = document.createElement("li");
    li.textContent =
      (t.title || "(untitled)") +
      (t.expected_length_ms != null ? "  " + fmtMs(t.expected_length_ms) : "") +
      (t.free_download ? "  (free download)" : "");
    ol.appendChild(li);
  });
  body.appendChild(ol);
  if (o.platforms && o.platforms.length) {
    body.appendChild(text("h4", "Where to get it"));
    const ul = document.createElement("ul");
    o.platforms.forEach((p) => {
      const li = document.createElement("li");
      li.appendChild(safeLink(p.label, p.url));
      ul.appendChild(li);
    });
    body.appendChild(ul);
  }
  if (o.free_download && o.free_download.available) {
    uniteButton("Download the free files…", () => uniteDownloadConsent({ media: true, images: true }, uniteAfterDownload), true);
  }
  uniteButton("I have the files…", uniteFiles, !(o.free_download && o.free_download.available));
  uniteButton("Close", uniteClose);
}

// ---- consent + progress for downloads (also used by refresh) ------------------------------

async function uniteDownloadConsent(opts, done) {
  const p = await jsonPost(`/api/unite/${unite.sid}/download/prepare`, opts);
  uniteShow("Download from the internet?");
  const body = $("unite-body");
  body.appendChild(text("p", p.message));
  body.appendChild(text("h4", `Every address that will be contacted (${p.urls.length})`));
  const ul = document.createElement("ul");
  ul.className = "urls";
  p.urls.forEach((u) => {
    const li = document.createElement("li");
    li.appendChild(text("code", u.url));
    li.appendChild(text("span", "  " + u.kind + (u.optional ? " (optional)" : ""), "muted"));
    ul.appendChild(li);
  });
  body.appendChild(ul);
  if (p.refused && p.refused.length) {
    body.appendChild(text("p", `${p.refused.length} address(es) were refused as unsafe and will not be contacted:`, "muted"));
    const rl = document.createElement("ul");
    p.refused.forEach((r) => rl.appendChild(text("li", r.url + " — " + r.reason, "muted")));
    body.appendChild(rl);
  }
  uniteButton("Download", async () => {
    await jsonPost(`/api/unite/${unite.sid}/download`, { consent: true, consentId: p.consent_id });
    uniteProgress(done);
  }, true);
  uniteButton("Cancel", () => (unite.overview ? uniteOverview() : uniteClose()));
}

function uniteProgress(done) {
  uniteShow("Downloading…");
  const body = $("unite-body");
  const list = document.createElement("ul");
  body.appendChild(list);
  const summary = text("p", "", "muted");
  body.appendChild(summary);
  const cancel = uniteButton("Cancel download", () =>
    api(`/api/unite/${unite.sid}/download/cancel`, { method: "POST" })
  );
  const tick = () =>
    api(`/api/unite/${unite.sid}`)
      .then((s) => {
        const d = s.download;
        clearNode(list);
        d.items.forEach((i) => {
          const li = document.createElement("li");
          li.textContent =
            `${i.kind}: ${i.state}` +
            (i.received ? ` ${fmtBytes(i.received)}${i.total ? " / " + fmtBytes(i.total) : ""}` : "") +
            (i.error ? ` — ${i.error}` : "");
          list.appendChild(li);
        });
        summary.textContent = (d.warnings || []).join(" ");
        if (d.state !== "running") {
          clearInterval(unite.poll);
          unite.poll = null;
          cancel.disabled = true;
          done(d);
        }
      })
      .catch(uniteError);
  if (unite.poll) clearInterval(unite.poll);
  unite.poll = setInterval(tick, 1000);
  tick();
}

function uniteAfterDownload(d) {
  if (d.state === "done") {
    return uniteProposal();
  }
  uniteShow(d.state === "cancelled" ? "Download cancelled" : "Download failed");
  $("unite-body").appendChild(text("p", d.error || "Nothing was added to your library.", "muted"));
  uniteButton("Try again", () => uniteDownloadConsent({ media: true, images: true }, uniteAfterDownload));
  uniteButton("I have the files…", uniteFiles);
  uniteButton("Close", uniteClose);
}

// ---- step 3: pick files (Case B / C) ------------------------------------------------------

function uniteFiles() {
  uniteShow("Add your files");
  const body = $("unite-body");
  body.appendChild(
    text(
      "p",
      "Choose the audio files (and cover image). If a release .noudata file (named _<release>.noudata) " +
        "is included, the files are only associated with it.",
      "muted"
    )
  );
  const input = document.createElement("input");
  input.type = "file";
  input.multiple = true;
  body.appendChild(input);
  const list = document.createElement("ul");
  body.appendChild(list);
  const upload = async () => {
    const files = Array.from(input.files || []);
    for (const f of files) {
      const li = text("li", `${f.name} …`);
      list.appendChild(li);
      try {
        const r = await api(`/api/unite/${unite.sid}/files?name=${encodeURIComponent(f.name)}`, {
          method: "POST",
          headers: { "Content-Type": "application/octet-stream" },
          body: f,
        });
        li.textContent = `${f.name} → ${r.name} (${r.kind}, ${fmtBytes(r.size)})`;
      } catch (e) {
        li.textContent = `${f.name} — not added: ${e.message}`;
      }
    }
    input.value = "";
  };
  uniteButton("Upload chosen files", upload);
  if (unite.overview && unite.overview.free_download && unite.overview.free_download.available) {
    uniteButton("Download images only…", () => uniteDownloadConsent({ media: false, images: true }, () => uniteFiles()));
  } else if (unite.overview) {
    uniteButton("Download the cover image…", () => uniteDownloadConsent({ media: false, images: true }, () => uniteFiles()));
  }
  uniteButton("Next: match files", uniteProposal, true);
  uniteButton("Close", uniteClose);
}

// ---- step 4: proposal + manual assignment -------------------------------------------------

async function uniteProposal() {
  const p = await api(`/api/unite/${unite.sid}/proposal`);
  const s = await api(`/api/unite/${unite.sid}`);
  unite.proposal = p;
  unite.files = s.files.filter((f) => f.kind === "audio");
  uniteShow(p.case === "C" ? "Add release files" : "Match the files");
  const body = $("unite-body");
  (p.warnings || []).forEach((w) => body.appendChild(text("p", w, "note")));
  (s.rejected || []).forEach((r) => body.appendChild(text("p", `Not used: ${r.name} — ${r.reason}`, "muted")));
  (p.unsupported || []).forEach((r) => body.appendChild(text("p", `Not a format Plippa can play: ${r.name || r.reason}`, "muted")));
  if (p.case === "C") {
    body.appendChild(text("p", `The release file ${p.case_c.name} was supplied: your files will be associated with it, nothing is re-matched.`));
    uniteButton("Add to library", () => uniteApply(), true);
    uniteButton("Close", uniteClose);
    return;
  }
  const items = (unite.overview && unite.overview.tracks) || [];
  const table = document.createElement("table");
  table.className = "unite-table";
  const head = document.createElement("tr");
  ["#", "Track", "File"].forEach((h) => head.appendChild(text("th", h)));
  table.appendChild(head);
  const matchOf = {};
  (p.matches || []).forEach((m) => (matchOf[m.item] = m));
  const selects = [];
  items.forEach((t, i) => {
    const tr = document.createElement("tr");
    tr.appendChild(text("td", String(i + 1)));
    tr.appendChild(text("td", t.title || "(untitled)"));
    const td = document.createElement("td");
    const sel = document.createElement("select");
    sel.appendChild(new Option("— none —", ""));
    unite.files.forEach((f) => sel.appendChild(new Option(f.name, String(f.id))));
    const m = matchOf[i];
    if (m && m.files.length) sel.value = String(m.files[0].id);
    sel.dataset.item = String(i);
    sel.dataset.extra = m ? JSON.stringify(m.files.slice(1).map((f) => f.id)) : "[]";
    sel.dataset.first = m && m.files.length ? String(m.files[0].id) : "";
    td.appendChild(sel);
    if (m && m.level === "low") {
      td.appendChild(text("span", "  check this one", "warn"));
      tr.className = "low";
    }
    if (m && m.files.length > 1) td.appendChild(text("span", "  + other formats of the same track", "muted"));
    tr.appendChild(td);
    table.appendChild(tr);
    selects.push(sel);
  });
  body.appendChild(table);
  const left = document.createElement("label");
  const allow = document.createElement("input");
  allow.type = "checkbox";
  left.appendChild(allow);
  left.appendChild(document.createTextNode(" Finish even if some tracks or files are left over"));
  body.appendChild(left);
  uniteButton("Add to library", async () => {
    const assignments = selects
      .filter((s) => s.value !== "" || s.dataset.first !== "")
      .map((s) => {
        const files = s.value === "" ? [] : [Number(s.value)];
        if (s.value === s.dataset.first) JSON.parse(s.dataset.extra).forEach((x) => files.push(x));
        return { item: Number(s.dataset.item), files };
      });
    const preview = await jsonPost(`/api/unite/${unite.sid}/confirm`, {
      assignments,
      allowIncomplete: allow.checked,
    });
    if (preview.warnings && preview.warnings.length) {
      $("unite-error").textContent = preview.warnings.join(" ");
    }
    await uniteApply();
  }, true);
  uniteButton("Close", uniteClose);
}

async function uniteApply() {
  const r = await jsonPost(`/api/unite/${unite.sid}/apply`, {});
  unite.sid = null; // applied: staging is already gone
  uniteShow("Added to your library");
  $("unite-body").appendChild(text("p", r.noudata));
  $("unite-body").appendChild(text("p", "Tap the tag again: it now plays from your library without any internet access.", "muted"));
  uniteButton("Close", uniteClose, true);
  refreshList().catch(() => {});
}

// ---- refresh an existing item ---------------------------------------------------------------

async function uniteRefresh() {
  if (selectedId == null) return;
  unite.overview = null;
  const id = encodeURIComponent(selectedId);
  let url = null;
  let prep;
  try {
    prep = await jsonPost(`/api/playables/${id}/refresh/prepare`, {});
  } catch (e) {
    if (e.code !== "needs_url") throw e;
    uniteShow("Refresh online info");
    const body = $("unite-body");
    body.appendChild(text("p", e.message, "muted"));
    const input = document.createElement("input");
    input.type = "text";
    input.placeholder = "https://artist.example/noubin/release/";
    body.appendChild(input);
    uniteButton("Next", async () => {
      url = input.value.trim();
      prep = await jsonPost(`/api/playables/${id}/refresh/prepare`, { url });
      uniteRefreshConsent(id, url, prep);
    }, true);
    uniteButton("Close", uniteClose);
    return;
  }
  uniteRefreshConsent(id, url, prep);
}

function uniteRefreshConsent(id, url, prep) {
  uniteShow("Refresh online info");
  const body = $("unite-body");
  body.appendChild(text("p", prep.consent_message));
  const line = document.createElement("p");
  line.appendChild(text("span", "Address ", "label"));
  line.appendChild(text("code", prep.fetch_url));
  body.appendChild(line);
  uniteButton("Contact server", async () => {
    const r = await jsonPost(`/api/playables/${id}/refresh`, { url: url || undefined, consent: true });
    uniteShow("Refreshed");
    const b = $("unite-body");
    b.appendChild(text("p", r.changed.length ? "Updated: " + r.changed.join(", ") : "Nothing had changed."));
    (r.warnings || []).forEach((w) => b.appendChild(text("p", w, "muted")));
    if (r.images) {
      unite.sid = r.images.session;
      uniteButton("Download cover images…", () =>
        uniteDownloadConsent({ media: false, images: true }, async (d) => {
          if (d.state === "done") {
            const out = await jsonPost(`/api/playables/${id}/refresh/images`, { session: unite.sid });
            unite.sid = null;
            uniteShow("Cover images");
            $("unite-body").appendChild(text("p", `${out.installed} image(s) added.`));
            uniteButton("Close", uniteClose, true);
          } else {
            uniteShow("Download did not finish");
            uniteButton("Close", uniteClose);
          }
        })
      );
    }
    uniteButton("Close", uniteClose, true);
    refreshList().then(() => openDetail(selectedId)).catch(() => {});
  }, true);
  uniteButton("Cancel", uniteClose);
}

function wireUnite() {
  if (!$("unite")) return;
  $("unite-open-btn").addEventListener("click", () => uniteStart(""));
  $("refresh-btn").addEventListener("click", () => uniteRefresh().catch((e) => showDetailMsg(e.message)));
  const w = $("web-unite-btn");
  if (w) {
    // The lookup was already consented to in the banner: reuse its result, no second request.
    w.addEventListener("click", () => {
      if (!webToken) return uniteStart(webKey);
      unite.key = webKey || "";
      jsonPost("/api/unite/start", { token: webToken })
        .then((r) => {
          unite.sid = r.session;
          unite.overview = r.overview;
          uniteOverview();
        })
        .catch((e) => {
          uniteStart(webKey);
          uniteError(e);
        });
    });
  }
}

wireUnite();
