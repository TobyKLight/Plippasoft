/* Plippa media portal — PIN sign-in (SECFIX_02). Classic script, global `Signin`.
   The portal asks for the six-digit PIN shown on the device (System > Network). The answer is a
   session cookie the page cannot read (HttpOnly); this file only shows the form and reacts to a
   401. No inline script and no innerHTML: every string reaches the page through textContent. */

const Signin = (() => {
  let shown = false;

  function byId(id) {
    return document.getElementById(id);
  }

  /** The one place a 401 from any call ends up (fetch in app.js, XHR in ui.js). */
  function expired() {
    show("Your session has ended. Enter the PIN again.");
  }

  function show(message) {
    const box = byId("signin");
    if (!box) return;
    if (!shown) {
      shown = true;
      box.classList.remove("hidden");
      const input = byId("signin-pin");
      if (input) input.focus();
    }
    setMessage(message || "");
  }

  function setMessage(text) {
    const m = byId("signin-msg");
    if (m) m.textContent = text;
  }

  async function submit(ev) {
    ev.preventDefault();
    const input = byId("signin-pin");
    const pin = (input.value || "").trim();
    if (!/^[0-9]{6}$/.test(pin)) {
      setMessage("The PIN is six digits.");
      return;
    }
    const button = byId("signin-go");
    button.disabled = true;
    try {
      const res = await fetch("/api/portal/login", {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Plippa": "1" },
        body: JSON.stringify({ pin }),
      });
      let data = null;
      try {
        data = await res.json();
      } catch {
        data = null;
      }
      if (res.ok) {
        input.value = "";
        location.reload();
        return;
      }
      input.value = "";
      setMessage((data && data.error) || `Sign-in failed (HTTP ${res.status})`);
      input.focus();
    } catch {
      setMessage("Could not reach the player.");
    } finally {
      button.disabled = false;
    }
  }

  async function signOut() {
    try {
      await fetch("/api/portal/logout", { method: "POST", headers: { "X-Plippa": "1" } });
    } catch {
      // Offline: the reload below shows whatever is true.
    }
    location.reload();
  }

  /**
   * Called once at start-up. Resolves true when the page may load its data (signed in, or the
   * device does not ask for a PIN) and false when the form is up instead.
   */
  async function gate() {
    const form = byId("signin-form");
    if (form) form.addEventListener("submit", submit);
    const out = byId("signout-btn");
    if (out) out.addEventListener("click", signOut);
    let st = null;
    try {
      const res = await fetch("/api/status");
      st = res.ok ? await res.json() : null;
    } catch {
      st = null;
    }
    if (!st) return true; // let the normal error banner explain a dead server
    note(st);
    if (st.pin_required && !st.signed_in) {
      show("");
      return false;
    }
    return true;
  }

  /** The warning and the sign-out button follow what the server says about the PIN. */
  function note(status) {
    const warn = byId("pin-warning");
    if (warn) warn.classList.toggle("hidden", status.pin_required !== false);
    // SECFIX_03: while the portal is served in the clear, say so wherever it is shown —
    // the Bandcamp password on this page would cross the network unencrypted.
    const clear = byId("https-warning");
    if (clear) clear.classList.toggle("hidden", status.https !== false);
    const out = byId("signout-btn");
    if (out) out.classList.toggle("hidden", !(status.pin_required && status.signed_in));
  }

  return { gate, expired, note };
})();
