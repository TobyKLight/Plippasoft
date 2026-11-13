// ART_05: shared cover-art component. Square thumbnails, a slideshow and a lightbox, used by the
// library lists, the collection view and the details panel (and by the cover editor / Add media
// dialogs later). Plain script, no build step, loaded before library.js.
//
// Security: nothing from files, tags or the server is put on the page as markup. URLs are only
// used when they point at this portal's cover API; `alt` text goes through setAttribute only.
// The CSP forbids inline styles: classes and CSSOM property assignment only.
//
//   Covers.thumb(url, { size: 128 | 512, alt, cls })      -> .cover-box element
//   Covers.fromApi(list)                                  -> covers for slider / lightbox
//   Covers.slider(covers, { size, autoMs, fadeMs, onOpen }) -> { el, destroy(), index(), show(i) }
//   Covers.lightbox(covers, index, { returnFocus, onClose(i) }) -> { close() }
//   Covers.mount(slot, collectionId, { size })            -> { destroy() }  (fetch + slider)

const Covers = (() => {
  const PLACEHOLDER_CLS = "nocover";
  /** Open lightboxes: sliders do not auto-advance while one is up. */
  let lightboxesOpen = 0;

  const isCoverUrl = (u) => typeof u === "string" && /^\/api\/(?:(?:collections|items)\/[^/?#]+\/covers(?:\/|$|\?)|imports\/[0-9a-f]+\/art\/[0-9a-f]+(?:$|\?))/.test(u);

  /** `url` with its `size` parameter replaced (or added); `size` 128|512|1024|"full". */
  function sized(url, size) {
    const base = url.replace(/([?&])size=[^&]*&?/, "$1").replace(/[?&]$/, "");
    return base + (base.includes("?") ? "&" : "?") + "size=" + size;
  }

  function node(tag, cls, attrs) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (attrs) for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
    return n;
  }

  function reducedMotion() {
    try {
      return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    } catch {
      return false;
    }
  }

  // ---- thumbnail ------------------------------------------------------------------------

  /** Square image box; the "♪" placeholder (CSS) shows until the image has loaded, and again if
   *  it fails to load (a stale link must not leave a broken-image icon). */
  function thumb(url, opts) {
    const o = opts || {};
    const box = node("div", `cover-box ${o.cls || ""} ${PLACEHOLDER_CLS}`.trim());
    if (isCoverUrl(url)) {
      const size = o.size || (window.devicePixelRatio >= 2 ? 512 : 128);
      const img = node("img", null, { src: sized(url, size), alt: o.alt || "", loading: "lazy", decoding: "async" });
      img.addEventListener("load", () => box.classList.remove(PLACEHOLDER_CLS));
      img.addEventListener("error", () => {
        img.remove();
        box.classList.add(PLACEHOLDER_CLS);
      });
      box.appendChild(img);
    }
    return box;
  }

  /** `GET …/covers` answer -> the shape slider and lightbox take. */
  function fromApi(list) {
    return ((list && list.covers) || [])
      .filter((c) => isCoverUrl(c.url))
      .map((c) => ({ url: c.url, thumbUrl: c.thumb_url, mediumUrl: c.medium_url, alt: c.alt || "" }));
  }

  // ---- slider ---------------------------------------------------------------------------

  function slider(covers, opts) {
    const o = { size: 512, autoMs: 5000, fadeMs: 300, ...(opts || {}) };
    const items = (covers || []).filter((c) => c && isCoverUrl(c.url));
    const many = items.length > 1;
    const srcOf = (i) => (o.size === 512 && items[i].mediumUrl) || sized(items[i].url, o.size);

    const root = node("div", "cover-slider", { role: "group", "aria-roledescription": "carousel", "aria-label": "Cover art" });
    root.style.setProperty("--cover-fade", `${o.fadeMs}ms`);
    const stage = node("div", `cover-box cover-stage ${PLACEHOLDER_CLS}`);
    root.appendChild(stage);
    if (!items.length) {
      return { el: root, destroy() {}, index: () => 0, show() {} };
    }

    const open = node("button", "cover-open", { type: "button" });
    const layers = [node("img", "cover-layer is-active"), node("img", "cover-layer")];
    let cur = 0;
    let active = 0; // which layer is on top
    let timer = null;
    let hovered = false;
    let focused = false;
    let destroyed = false;
    let wasConnected = false;
    const cleanups = [];

    const listen = (target, ev, fn, opt) => {
      target.addEventListener(ev, fn, opt);
      cleanups.push(() => target.removeEventListener(ev, fn, opt));
    };

    layers.forEach((img, k) => {
      img.alt = "";
      img.decoding = "async";
      listen(img, "load", () => {
        img.classList.remove("broken");
        if (k === active) stage.classList.remove(PLACEHOLDER_CLS);
      });
      listen(img, "error", () => {
        img.classList.add("broken");
        if (k === active) stage.classList.add(PLACEHOLDER_CLS);
      });
      open.appendChild(img);
    });
    stage.appendChild(open);

    const next = (i) => (i + 1) % items.length;
    const prev = (i) => (i + items.length - 1) % items.length;

    function setSrc(img, i) {
      if (img.dataset.i === String(i)) return;
      img.dataset.i = String(i);
      img.classList.remove("broken");
      img.setAttribute("src", srcOf(i));
    }

    /** Warm the HTTP cache with the following image; the fading-out layer must not change. */
    function preload(i) {
      const im = new Image();
      im.src = srcOf(i);
    }

    function label() {
      const a = items[cur].alt;
      open.setAttribute("aria-label", `Open cover art ${cur + 1} of ${items.length}${a ? ": " + a : ""}`);
    }

    const dots = [];
    function paint() {
      label();
      layers[active].setAttribute("alt", items[cur].alt || "");
      layers[1 - active].setAttribute("alt", "");
      dots.forEach((d, i) => {
        if (i === cur) d.setAttribute("aria-current", "true");
        else d.removeAttribute("aria-current");
      });
    }

    /** Make image `i` current: it goes on the idle layer, which then fades in over the old one. */
    function show(i, instant) {
      if (destroyed) return;
      i = ((i % items.length) + items.length) % items.length;
      if (i === cur && layers[active].dataset.i === String(i)) return;
      const incoming = 1 - active;
      setSrc(layers[incoming], i);
      layers[incoming].classList.toggle("instant", !!instant || reducedMotion());
      layers[active].classList.toggle("instant", !!instant || reducedMotion());
      const old = active;
      active = incoming;
      cur = i;
      layers[active].classList.add("is-active");
      layers[old].classList.remove("is-active");
      stage.classList.toggle(PLACEHOLDER_CLS, layers[active].classList.contains("broken"));
      paint();
      if (many) preload(next(i));
    }

    function go(delta) {
      show(delta > 0 ? next(cur) : prev(cur));
      restart();
    }

    function paused() {
      return hovered || focused || document.hidden || lightboxesOpen > 0 || reducedMotion();
    }

    function tick() {
      if (destroyed) return;
      if (root.isConnected) wasConnected = true;
      else if (wasConnected) return destroy(); // the view was re-rendered without telling us
      if (!paused()) show(next(cur));
    }

    function restart() {
      clearInterval(timer);
      timer = null;
      if (many && !destroyed) timer = setInterval(tick, o.autoMs);
    }

    function destroy() {
      if (destroyed) return;
      destroyed = true;
      clearInterval(timer);
      timer = null;
      cleanups.splice(0).forEach((f) => f());
    }

    function openLightbox() {
      if (o.onOpen) o.onOpen(cur);
      Covers.lightbox(items, cur, {
        returnFocus: open,
        onClose: (i) => {
          if (!destroyed) {
            show(i, true);
            restart();
          }
        },
      });
    }

    layers[0].dataset.i = "0";
    layers[0].setAttribute("src", srcOf(0));
    if (many) preload(1);

    if (many) {
      const mkNav = (cls, glyph, name, delta) => {
        const b = node("button", `cover-nav ${cls}`, { type: "button", "aria-label": name });
        b.textContent = glyph;
        listen(b, "click", () => go(delta));
        return b;
      };
      const dotsBox = node("div", "cover-dots");
      items.forEach((_, i) => {
        const d = node("button", "cover-dot", { type: "button", "aria-label": `Show cover art ${i + 1}` });
        listen(d, "click", () => {
          show(i);
          restart();
        });
        dots.push(d);
        dotsBox.appendChild(d);
      });
      const controls = node("div", "cover-controls");
      controls.append(mkNav("cover-prev", "‹", "Previous cover art", -1), dotsBox, mkNav("cover-next", "›", "Next cover art", 1));
      root.appendChild(controls);
      listen(root, "mouseenter", () => (hovered = true));
      listen(root, "mouseleave", () => (hovered = false));
      listen(root, "focusin", () => (focused = true));
      listen(root, "focusout", (ev) => {
        if (!root.contains(ev.relatedTarget)) focused = false;
      });
      listen(root, "keydown", (ev) => {
        if (ev.key === "ArrowLeft") go(-1);
        else if (ev.key === "ArrowRight") go(1);
        else return;
        ev.preventDefault();
      });
      restart();
    }
    listen(open, "click", openLightbox);
    paint();

    return { el: root, destroy, index: () => cur, show: (i) => show(i, true) };
  }

  // ---- lightbox -------------------------------------------------------------------------

  function lightbox(covers, startIndex, opts) {
    const o = opts || {};
    const items = (covers || []).filter((c) => c && isCoverUrl(c.url));
    if (!items.length) return { close() {} };
    let cur = Math.min(Math.max(startIndex || 0, 0), items.length - 1);
    const opener = o.returnFocus || document.activeElement;

    const dlg = node("dialog", "cover-lightbox");
    const box = node("div", "cover-box lb-box");
    const img = node("img", "lb-img");
    box.appendChild(img);
    const close = node("button", "lb-close", { type: "button", "aria-label": "Close" });
    close.textContent = "✕";
    const count = node("p", "lb-count");
    dlg.append(box, close, count);
    const nav = [];
    if (items.length > 1) {
      for (const [cls, glyph, name, d] of [["lb-prev", "‹", "Previous cover art", -1], ["lb-next", "›", "Next cover art", 1]]) {
        const b = node("button", `lb-nav ${cls}`, { type: "button", "aria-label": name });
        b.textContent = glyph;
        b.addEventListener("click", () => step(d));
        dlg.appendChild(b);
        nav.push(b);
      }
    }

    function load() {
      const c = items[cur];
      box.classList.add(PLACEHOLDER_CLS);
      let triedFull = false;
      img.onload = () => box.classList.remove(PLACEHOLDER_CLS);
      img.onerror = () => {
        if (!triedFull) {
          triedFull = true;
          img.setAttribute("src", sized(c.url, "full"));
        } else {
          box.classList.add(PLACEHOLDER_CLS);
        }
      };
      img.setAttribute("alt", c.alt || "");
      img.setAttribute("src", sized(c.url, 1024));
      dlg.setAttribute("aria-label", `Cover art ${cur + 1} of ${items.length}`);
      count.textContent = items.length > 1 ? `${cur + 1} / ${items.length}` : "";
    }

    function step(d) {
      cur = Math.min(Math.max(cur + d, 0), items.length - 1);
      load();
    }

    let closed = false;
    function finish() {
      if (closed) return;
      closed = true;
      lightboxesOpen = Math.max(0, lightboxesOpen - 1);
      dlg.remove();
      if (opener && opener.isConnected && opener.focus) opener.focus();
      if (o.onClose) o.onClose(cur);
    }
    function shut() {
      if (dlg.open) dlg.close();
      else finish();
    }

    dlg.addEventListener("close", finish);
    close.addEventListener("click", shut);
    // The backdrop is the dialog element itself (it has no padding).
    dlg.addEventListener("click", (ev) => {
      if (ev.target === dlg) shut();
    });
    dlg.addEventListener("keydown", (ev) => {
      if (ev.key === "ArrowLeft") step(-1);
      else if (ev.key === "ArrowRight") step(1);
      else if (ev.key === "Tab") {
        const f = [...dlg.querySelectorAll("button")].filter((b) => !b.disabled);
        const first = f[0];
        const last = f[f.length - 1];
        if (ev.shiftKey && document.activeElement === first) last.focus();
        else if (!ev.shiftKey && document.activeElement === last) first.focus();
        else return;
      } else return;
      ev.preventDefault();
    });

    load();
    document.body.appendChild(dlg);
    lightboxesOpen++;
    dlg.showModal();
    close.focus();
    return { close: shut };
  }

  // ---- fetch + mount --------------------------------------------------------------------

  /** Fill `slot` (emptied first) with the slider of collection `id`; the placeholder stays when
   *  there are no servable images or the request fails. Safe to call again for the same slot
   *  (`destroy()` the previous handle first) and to destroy before the answer arrives. */
  function mount(slot, id, opts) {
    let slider_ = null;
    let gone = false;
    (async () => {
      let list;
      try {
        list = await api(`/api/collections/${encodeURIComponent(id)}/covers`);
      } catch {
        return;
      }
      if (gone || !slot.isConnected) return;
      const covers = fromApi(list);
      if (!covers.length) {
        slot.replaceChildren(thumb(null, { cls: "cover-stage" }));
        return;
      }
      slider_ = slider(covers, opts);
      slot.replaceChildren(slider_.el);
      slot.classList.add("has-slider");
    })();
    return {
      destroy() {
        gone = true;
        if (slider_) slider_.destroy();
        slider_ = null;
      },
    };
  }

  return { thumb, fromApi, slider, lightbox, mount, sized };
})();
