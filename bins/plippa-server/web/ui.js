/* Plippa media portal — shared UI helpers (NB_11): DOM builder, toast, byte formatting, drop zone,
   streamed upload with progress. Used by addmedia.js; the NB_09 unite dialog can adopt them.
   Everything untrusted goes in through textContent / setAttribute — never innerHTML
   (NBSecurityRequirements §5.1). Classic script (no modules), global `Ui`. */

const Ui = (() => {
  // NBSecurityRequirements §3 / NBSecurityReview_1 SEC-12: a .noudata that arrived on a stick is indexed
  // as is, so titles, artists and cue text may carry control, bidi-override, zero-width and tag
  // characters (display spoofing, invisible text). Everything that comes from the server, and every
  // text node the helpers create, goes through `clean` first. Newline and tab are kept.
  const INVISIBLE = /[\u0000-\u0008\u000B-\u001F\u007F-\u009F\u00AD\u061C\u180E\u200B-\u200F\u2028-\u202E\u2060-\u206F\uFEFF\uFFF9-\uFFFB]|\uDB40[\uDC00-\uDC7F]/g;
  function clean(s) {
    return typeof s === "string" ? s.replace(INVISIBLE, "") : s;
  }
  /** `clean` applied to every string inside a parsed JSON value. */
  function scrub(v) {
    if (typeof v === "string") return clean(v);
    if (Array.isArray(v)) return v.map(scrub);
    if (v && typeof v === "object") {
      const o = {};
      for (const k of Object.keys(v)) o[k] = scrub(v[k]);
      return o;
    }
    return v;
  }

  /** el("div", {cls, text, attrs, on}, [children]) — children may contain nulls. */
  function el(tag, opts, children) {
    const n = document.createElement(tag);
    if (opts) {
      if (opts.cls) n.className = opts.cls;
      if (opts.text != null) n.textContent = clean(String(opts.text));
      if (opts.attrs) for (const [k, v] of Object.entries(opts.attrs)) n.setAttribute(k, v);
      if (opts.on) for (const [k, fn] of Object.entries(opts.on)) n.addEventListener(k, fn);
    }
    for (const c of children || []) if (c) n.appendChild(c);
    return n;
  }

  function fmtBytes(n) {
    if (n == null || !Number.isFinite(n)) return "";
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
  }

  let toastTimer = null;
  function toast(message, isError) {
    const box = document.getElementById("toast");
    if (!box) return;
    box.textContent = clean(String(message));
    box.className = isError ? "toast error" : "toast";
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      box.className = "toast hidden";
      box.textContent = "";
    }, isError ? 7000 : 3500);
  }

  const AUDIO_EXT = /\.(mp3|flac|wav|ogg|opus|m4a|m4b|aac|mp4)$/i;
  /** Cheap client-side filter; the server sniffs the real content and has the last word. */
  function looksAudio(file) {
    return (file.type && file.type.startsWith("audio/")) || AUDIO_EXT.test(file.name || "");
  }

  // FIX_01A: filenames sort numerically ("9 x" before "10 x"). Created once at module scope;
  // the raw-string tie-break makes the order total and stable for case/accent variants.
  const NATURAL = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
  function naturalCompare(a, b) {
    const c = NATURAL.compare(String(a), String(b));
    if (c !== 0) return c;
    const x = String(a);
    const y = String(b);
    return x < y ? -1 : x > y ? 1 : 0;
  }

  /**
   * Make `zone` accept dropped files and (via the button/Enter/Space) a file picker.
   * `input` is a hidden <input type=file>. Calls onFiles(File[]) with the chosen files; only
   * drags that carry files react (an internal list-reorder drag does not).
   */
  function dropZone(zone, input, onFiles) {
    const hasFiles = (ev) => ev.dataTransfer && Array.from(ev.dataTransfer.types || []).includes("Files");
    zone.addEventListener("dragenter", (ev) => {
      if (!hasFiles(ev)) return;
      ev.preventDefault();
      zone.classList.add("hover");
    });
    zone.addEventListener("dragover", (ev) => {
      if (!hasFiles(ev)) return;
      ev.preventDefault();
      ev.dataTransfer.dropEffect = "copy";
      zone.classList.add("hover");
    });
    zone.addEventListener("dragleave", () => zone.classList.remove("hover"));
    zone.addEventListener("drop", (ev) => {
      if (!hasFiles(ev)) return;
      ev.preventDefault();
      zone.classList.remove("hover");
      const files = Array.from(ev.dataTransfer.files || []);
      if (files.length) onFiles(files);
    });
    input.addEventListener("change", () => {
      const files = Array.from(input.files || []);
      input.value = "";
      if (files.length) onFiles(files);
    });
    const open = () => input.click();
    return { open };
  }

  /**
   * Stream `file` (a File/Blob is sent as-is: the browser reads it from disk in chunks, it is
   * never loaded into JS memory) to `url` with upload progress. Returns {promise, abort}.
   * The promise resolves with the parsed JSON answer and rejects with Error(server message);
   * an abort rejects with an Error whose `.aborted` is true.
   */
  function upload(url, file, onProgress) {
    const xhr = new XMLHttpRequest();
    const promise = new Promise((resolve, reject) => {
      xhr.open("POST", url);
      // A cross-site form / no-cors fetch cannot send this header (NBSecurityRequirements §5.3).
      xhr.setRequestHeader("X-Plippa", "1");
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.upload.addEventListener("progress", (ev) => {
        if (onProgress && ev.lengthComputable) onProgress(ev.loaded, ev.total);
      });
      xhr.addEventListener("load", () => {
        let data = null;
        try {
          data = xhr.responseText ? scrub(JSON.parse(xhr.responseText)) : null;
        } catch {
          data = null;
        }
        if (xhr.status >= 200 && xhr.status < 300) return resolve(data);
        if (xhr.status === 401 && typeof Signin !== "undefined") Signin.expired();
        const err = new Error((data && data.error) || `Upload failed (HTTP ${xhr.status})`);
        err.status = xhr.status;
        reject(err);
      });
      xhr.addEventListener("error", () => reject(new Error("Connection lost during the upload")));
      xhr.addEventListener("timeout", () => reject(new Error("The upload timed out")));
      xhr.addEventListener("abort", () => {
        const err = new Error("Upload cancelled");
        err.aborted = true;
        reject(err);
      });
      xhr.send(file);
    });
    return { promise, abort: () => xhr.abort() };
  }

  /** Length of an audio file in ms via the browser's decoder, or null (unknown / too slow). */
  function audioDuration(file) {
    return new Promise((resolve) => {
      let url = null;
      const a = document.createElement("audio");
      const done = (v) => {
        clearTimeout(timer);
        a.removeAttribute("src");
        if (url) URL.revokeObjectURL(url);
        resolve(v);
      };
      const timer = setTimeout(() => done(null), 4000);
      a.preload = "metadata";
      a.addEventListener("loadedmetadata", () =>
        done(Number.isFinite(a.duration) && a.duration > 0 ? Math.round(a.duration * 1000) : null)
      );
      a.addEventListener("error", () => done(null));
      try {
        url = URL.createObjectURL(file);
        a.src = url;
      } catch {
        done(null);
      }
    });
  }

  /** Give a <dialog> its shared behaviour: restore focus on close, swallow stray file drops. */
  function modal(dlg, opener) {
    dlg.addEventListener("dragover", (ev) => {
      if (ev.dataTransfer && Array.from(ev.dataTransfer.types || []).includes("Files")) ev.preventDefault();
    });
    dlg.addEventListener("drop", (ev) => {
      // A file dropped beside the drop zone must not make the browser navigate to it.
      if (ev.dataTransfer && Array.from(ev.dataTransfer.types || []).includes("Files")) ev.preventDefault();
    });
    dlg.addEventListener("close", () => {
      const o = opener();
      if (o && document.contains(o)) o.focus();
    });
  }


  /**
   * Table + counts + warnings + ignored lines for a `POST /api/cues/preview` answer (or the
   * equivalent parts of a save report). Everything is textContent (NBSecurityRequirements §5.1).
   * `opts.emptyText` replaces the "no timestamps" line.
   */
  function renderCuePreview(box, data, opts) {
    box.replaceChildren();
    const n = data.cue_count;
    const ignored = (data.ignored_lines || []).length;
    const parts = [`${n} ${n === 1 ? "cue" : "cues"}`];
    if (ignored) parts.push(`${ignored} ignored ${ignored === 1 ? "line" : "lines"}`);
    box.appendChild(
      el("p", { cls: "preview-head" }, [
        el("strong", { text: parts[0] }),
        ignored ? el("span", { cls: "muted", text: ` · ${parts[1]}` }) : null,
      ])
    );
    if (n === 0) {
      box.appendChild(
        el("p", {
          cls: "muted",
          text: (opts && opts.emptyText) || "No timestamps found: this will be imported without cues.",
        })
      );
    } else {
      const rows = (data.cues || []).map((c) =>
        el("tr", null, [el("td", { cls: "cue-time", text: c.start }), el("td", { text: c.text })])
      );
      box.appendChild(
        el("div", { cls: "preview-scroll" }, [
          el("table", { cls: "preview-table" }, [
            el("thead", null, [el("tr", null, [el("th", { text: "Time" }), el("th", { text: "Title" })])]),
            el("tbody", null, rows),
          ]),
        ])
      );
    }
    const warns = data.warnings || [];
    if (warns.length) {
      box.appendChild(
        el("ul", { cls: "preview-warn" }, warns.slice(0, 20).map((w) => el("li", { text: w.message })))
      );
    }
    if (ignored) {
      const list = el(
        "ul",
        { cls: "muted preview-ignored" },
        data.ignored_lines.slice(0, 20).map((l) => el("li", { text: `line ${l.line}: ${l.text}` }))
      );
      box.appendChild(el("details", null, [el("summary", { text: "Ignored lines" }), list]));
    }
  }

  /**
   * The paste box + live preview used by the Add media dialog and by "Edit timestamps":
   * debounced `POST /api/cues/preview` into `box`. `getDuration()` gives the media length in ms
   * (or null); `active()` (default true) switches the preview off; `showEmpty` renders the
   * "0 cues" summary for a blank box instead of nothing; `onResult(data|null)` is told each
   * answer. A newer keystroke supersedes an older answer. Returns {soon, flush, reset}.
   */
  function cuePaste({ textarea, box, getDuration, active, showEmpty, emptyText, onResult }) {
    let seq = 0;
    let timer = null;
    async function run() {
      const text = textarea.value;
      const mine = ++seq;
      if ((active && !active()) || (!text.trim() && !showEmpty)) {
        box.replaceChildren();
        if (onResult) onResult(null);
        return;
      }
      if (!text.trim()) {
        renderCuePreview(box, { cue_count: 0 }, { emptyText });
        if (onResult) onResult({ cue_count: 0, cues: [], warnings: [], ignored_lines: [] });
        return;
      }
      let data;
      try {
        const ms = getDuration ? getDuration() : null;
        data = await api("/api/cues/preview", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ text, ...(ms ? { duration_ms: ms } : {}) }),
        });
      } catch (e) {
        if (mine !== seq) return;
        box.replaceChildren(el("p", { cls: "muted", text: `Preview unavailable: ${e.message}` }));
        if (onResult) onResult(null);
        return;
      }
      if (mine !== seq) return;
      renderCuePreview(box, data, { emptyText });
      if (onResult) onResult(data);
    }
    return {
      soon(delay) {
        clearTimeout(timer);
        timer = setTimeout(run, delay == null ? 300 : delay);
      },
      flush: run,
      reset() {
        clearTimeout(timer);
        seq++;
        box.replaceChildren();
      },
    };
  }

  return { clean, scrub, el, fmtBytes, toast, looksAudio, naturalCompare, dropZone, upload, audioDuration, modal, renderCuePreview, cuePaste };
})();
