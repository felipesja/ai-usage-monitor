const { invoke } = window.__TAURI__.core;
const notification = window.__TAURI__.notification;

const INTERVAL_MS = 60_000;
// Percentages a limit fires a notification at, read from config.json via the
// backend (falls back to these until it answers). A level re-arms only after
// usage falls REARM_MARGIN points below it, so a meter parked on a boundary
// does not re-notify every refresh.
let alertThresholds = [80, 90, 95, 98, 100];
const REARM_MARGIN = 5;
// Highest threshold already announced per meter (a high-water mark).
const firedLevels = new Map();

const ACCENT = { Claude: "var(--claude)", Codex: "var(--openai)", Cursor: "var(--cursor)", Grok: "var(--grok)" };

function resetRemaining(value) {
  if (!value) return "";
  const seconds = Math.max(0, Math.floor((new Date(value) - Date.now()) / 1000));
  if (Number.isNaN(seconds)) return "";
  if (seconds >= 86_400) return `${Math.floor(seconds / 86_400)}d ${Math.floor((seconds % 86_400) / 3600)}h`;
  if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
  return `${Math.floor(seconds / 60)}m`;
}

function meterColor(percent, providerName) {
  if (percent == null) return "var(--muted)";
  if (percent >= 85) return "var(--red)";
  if (percent >= 65) return "var(--amber)";
  return ACCENT[providerName] || "var(--text)";
}

// macOS requires an explicit permission grant (UNUserNotificationCenter);
// Windows happens to allow sending without one. Ask once, lazily, on the
// first alert.
async function notify(payload) {
  let granted = await notification.isPermissionGranted();
  if (!granted) {
    granted = (await notification.requestPermission()) === "granted";
  }
  if (granted) notification.sendNotification(payload);
}

// Highest threshold at or below `percent`, or 0 if below them all.
function reachedLevel(percent) {
  let level = 0;
  for (const t of alertThresholds) if (percent >= t) level = t;
  return level;
}

function checkAlerts(providers) {
  if (!alertThresholds.length) return;
  for (const provider of providers) {
    for (const meter of provider.meters || []) {
      if (meter.percent == null) continue;
      // reset_at is kept out of the key because some providers jitter it
      // between fetches.
      // Provider is part of the identity: the same account can use Claude and
      // Codex, and both expose meters such as "Weekly". Without it, a lower
      // meter from one provider can re-arm an alert fired by the other.
      const key = `${provider.name}|${provider.email || provider.account}|${meter.label}`;
      let mark = firedLevels.get(key) || 0;
      // Re-arm: forget any announced level the meter dropped clearly below
      // (window renewed, or usage genuinely fell). A real reset lands at ~0,
      // far past the margin, so every level re-arms.
      if (mark && meter.percent < mark - REARM_MARGIN) {
        mark = reachedLevel(meter.percent);
        firedLevels.set(key, mark);
      }
      const level = reachedLevel(meter.percent);
      if (level <= mark) continue;
      firedLevels.set(key, level);
      const remaining = resetRemaining(meter.reset_at);
      notify({
        title: `${provider.name} · ${provider.email || provider.account}`,
        body: `${meter.label} at ${Math.round(meter.percent)}%` + (remaining ? ` · renews in ${remaining}` : ""),
      });
    }
  }
}

async function loadAlertThresholds() {
  try {
    const values = await invoke("alert_thresholds");
    if (Array.isArray(values) && values.length) alertThresholds = values;
  } catch (error) {
    console.error(error);
  }
}

// Providers that were never set up on this machine render as noise (Codex
// or Grok without a local session, Cursor without a key). Keep them out of
// the panel — the accounts view (a) still reports their status. A provider
// that WAS set up and then breaks keeps showing its error.
function isUnconfigured(provider) {
  if (!provider.error || provider.meters?.length) return false;
  if (provider.name === "Cursor") return provider.error.startsWith("set it up with");
  if (provider.name === "Grok") {
    return provider.error.includes("no local session found");
  }
  // Registered Codex profiles (account is the profile dir, not "ChatGPT")
  // stay visible when their copy cannot refresh. Only the auto-detected
  // live CLI is hidden until `codex login`.
  if (provider.name === "Codex") {
    return provider.account === "ChatGPT" && provider.error.includes("no local session found");
  }
  return false;
}

function render(providers) {
  const root = document.getElementById("providers");
  root.replaceChildren();
  for (const provider of providers.filter((p) => !isUnconfigured(p))) {
    const box = document.createElement("div");
    box.className = "provider";
    box.setAttribute("data-tauri-drag-region", "");

    const head = document.createElement("div");
    head.className = "provider-head";
    // Name, standby badge and email share one line — the panel is height-bound.
    const nameRow = document.createElement("div");
    nameRow.className = "name-row";
    const name = document.createElement("span");
    name.className = `provider-name ${provider.name}`;
    name.textContent = provider.name;
    nameRow.appendChild(name);
    // With 2+ Claude accounts, the collector flags standby on the ones that are
    // not logged into the CLI — the unbadged one is the account in use.
    if (provider.standby) {
      const badge = document.createElement("span");
      badge.className = "badge standby";
      badge.textContent = "◉ standby";
      nameRow.appendChild(badge);
    }
    const email = document.createElement("span");
    email.className = "email";
    email.textContent = provider.email || provider.account;
    nameRow.appendChild(email);
    head.appendChild(nameRow);
    if (provider.plan) {
      const plan = document.createElement("span");
      plan.className = "plan";
      plan.textContent = provider.plan.toUpperCase();
      head.appendChild(plan);
    }
    box.appendChild(head);

    if (provider.error) {
      const error = document.createElement("div");
      error.className = "error";
      error.textContent = "! " + provider.error;
      box.appendChild(error);
    }

    for (const meter of provider.meters || []) {
      const row = document.createElement("div");
      const remaining = resetRemaining(meter.reset_at);
      // No reset_at = window not open yet (the 5h session only starts on the
      // first request). Render the row dimmed instead of "0% idle".
      const idle = !remaining && !meter.percent;
      row.className = idle ? "meter idle" : "meter";
      const color = idle ? "var(--dim)" : meterColor(meter.percent, provider.name);

      const label = document.createElement("span");
      label.className = "label";
      label.textContent = meter.label;
      if (meter.used != null && meter.limit != null) {
        const amount = document.createElement("span");
        amount.className = "amount";
        amount.textContent = ` ${meter.used}/${meter.limit}`;
        label.appendChild(amount);
      }

      const pct = document.createElement("span");
      pct.className = "pct";
      if (!idle) pct.style.color = color;
      pct.textContent = meter.percent == null ? "--" : `${Math.round(meter.percent)}%`;

      const reset = document.createElement("span");
      reset.className = "reset";
      reset.textContent = idle ? "-" : remaining;

      const bar = document.createElement("div");
      bar.className = "bar";
      const fill = document.createElement("div");
      fill.style.width = `${Math.min(100, Math.max(0, meter.percent || 0))}%`;
      fill.style.background = color;
      bar.appendChild(fill);

      row.append(label, pct, reset, bar);
      box.appendChild(row);
    }
    for (const detail of provider.details || []) {
      const line = document.createElement("div");
      line.className = detail.startsWith("⚠") ? "detail warn" : "detail";
      line.textContent = detail;
      box.appendChild(line);
    }
    root.appendChild(box);
  }
}

function setStatus(icon, text, fetching) {
  const status = document.getElementById("status");
  status.className = fetching ? "status fetching" : "status";
  status.replaceChildren();
  const iconSpan = document.createElement("span");
  iconSpan.className = "icon";
  iconSpan.textContent = icon;
  const textSpan = document.createElement("span");
  textSpan.className = "text";
  textSpan.textContent = text;
  status.append(iconSpan, textSpan);
}

let lastProviders = [];
let refreshPromise = null;

// Concurrent callers share the in-flight run instead of skipping — callers
// that await refresh() (the accounts view) need current data afterwards.
function refresh() {
  if (!refreshPromise) {
    refreshPromise = doRefresh().finally(() => {
      refreshPromise = null;
    });
  }
  return refreshPromise;
}

// The window is sized to its content: measure how much the visible scroll
// container overflows and ask the shell to grow/shrink by exactly that much.
// The 2px deadband keeps the 60s refresh from nudging the window every tick.
function fitWindow() {
  const el = accountsOpen()
    ? document.getElementById("accounts")
    : document.getElementById("providers");
  if (!el) return;
  const overflow = el.scrollHeight - el.clientHeight;
  if (Math.abs(overflow) <= 2) return;
  invoke("resize_to_content", { height: window.outerHeight + overflow }).catch(console.error);
}

async function doRefresh() {
  setStatus("↻", "refreshing", true);
  try {
    const raw = await invoke("fetch_usage");
    const providers = JSON.parse(raw);
    lastProviders = providers;
    render(providers);
    checkAlerts(providers);
    // en-GB keeps the 24h HH:MM:SS shape the compact layout is sized for.
    setStatus("●", new Date().toLocaleTimeString("en-GB"), false);
    const visible = providers.filter((p) => !isUnconfigured(p)).length;
    document.getElementById("subtitle").textContent = `${visible} subscription${visible === 1 ? "" : "s"}`;
    requestAnimationFrame(fitWindow);
  } catch (error) {
    setStatus("!", "refresh failed", true);
    console.error(error);
  }
}

// ---- Accounts view (⚙) -----------------------------------------------------

function accountsOpen() {
  return document.body.classList.contains("show-accounts");
}

// The message survives renderAccounts() rebuilding the view; it is cleared
// when the view closes.
let accountsMsg = { text: "", kind: "" };

function setAccountsMsg(text, kind) {
  accountsMsg = { text: text || "", kind: kind || "" };
  const msg = document.getElementById("acct-msg");
  if (!msg) return;
  msg.textContent = accountsMsg.text;
  msg.className = `acct-msg${accountsMsg.kind ? ` ${accountsMsg.kind}` : ""}`;
}

function acctRow(...children) {
  const row = document.createElement("div");
  row.className = "acct-row";
  row.append(...children);
  return row;
}

function acctSpan(className, text) {
  const span = document.createElement("span");
  span.className = className;
  span.textContent = text;
  return span;
}

async function renderAccounts() {
  const root = document.getElementById("accounts");
  root.replaceChildren();

  let detection = { claude: [], cursor: { configured: false, method: null, email: null }, codex: { present: false, email: null } };
  try {
    detection = JSON.parse(await invoke("detect_accounts"));
  } catch (error) {
    console.error(error);
  }

  // Claude: registered profiles (from the last collection), then detected
  // credential sources. Adding reads the source — on macOS the Keychain may
  // ask for permission — and names the profile after the account's email.
  const claude = document.createElement("div");
  claude.className = "acct-section";
  claude.appendChild(acctSpan("acct-title provider-name Claude", "Claude"));
  const registered = lastProviders.filter((p) => p.name === "Claude");
  for (const provider of registered) {
    const remove = acctSpan("acct-act rm", "✕ remove");
    remove.addEventListener("click", async () => {
      try {
        await invoke("remove_claude_account", { profile: provider.account });
        setAccountsMsg(`removed ${provider.account}`, "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
      }
    });
    claude.appendChild(
      acctRow(
        acctSpan("who", provider.email || provider.account),
        acctSpan("meta", provider.plan || ""),
        remove,
      ),
    );
  }
  if (!registered.length) {
    claude.appendChild(acctSpan("acct-hint", "no profiles registered"));
  }
  // Hide sources whose logged-in account is already a registered profile;
  // sources with unknown identity stay visible (Add dedupes them anyway).
  const knownEmails = new Set(
    registered.map((p) => (p.email || "").toLowerCase()).filter(Boolean),
  );
  const candidates = detection.claude.filter(
    (c) => !c.email || !knownEmails.has(c.email.toLowerCase()),
  );
  if (!candidates.length) {
    claude.appendChild(acctSpan("acct-hint", "no new accounts detected"));
  }
  for (const candidate of candidates) {
    const add = acctSpan("acct-act add", "+ add");
    add.addEventListener("click", async () => {
      add.classList.add("disabled");
      add.textContent = "adding…";
      setAccountsMsg(`reading ${candidate.label}…`);
      try {
        const result = JSON.parse(await invoke("add_claude_account", { source: candidate.id }));
        setAccountsMsg(
          result.already
            ? `${result.email} is already registered as '${result.profile}'`
            : `added ${result.email} (${result.plan}) as '${result.profile}'`,
          "ok",
        );
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
        add.classList.remove("disabled");
        add.textContent = "+ add";
      }
    });
    claude.appendChild(acctRow(acctSpan("who", candidate.label), add));
  }
  root.appendChild(claude);

  // Codex: stored profiles (this app owns those copies) plus the live CLI
  // session. "+ add" captures the CLI login; "✕ remove" on a profile only
  // drops the copy. Logging the CLI out is a separate action.
  const codex = document.createElement("div");
  codex.className = "acct-section";
  codex.appendChild(acctSpan("acct-title provider-name Codex", "Codex"));
  const codexRegistered = lastProviders.filter(
    (p) => p.name === "Codex" && p.account && p.account !== "ChatGPT",
  );
  const knownCodexEmails = new Set(
    codexRegistered.map((p) => (p.email || "").toLowerCase()).filter(Boolean),
  );
  for (const provider of codexRegistered) {
    const remove = acctSpan("acct-act rm", "✕ remove");
    remove.addEventListener("click", async () => {
      try {
        await invoke("remove_codex_profile", { profile: provider.account });
        setAccountsMsg(`removed ${provider.account}`, "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
      }
    });
    const meta = provider.standby ? "standby" : provider.plan || "";
    codex.appendChild(
      acctRow(
        acctSpan("who", provider.email || provider.account),
        acctSpan("meta", meta),
        remove,
      ),
    );
  }
  if (!codexRegistered.length) {
    codex.appendChild(acctSpan("acct-hint", "no profiles registered"));
  }
  const liveCodex = lastProviders.find(
    (p) => p.name === "Codex" && p.account === "ChatGPT" && !p.error,
  );
  const liveEmail = (detection.codex?.email || liveCodex?.email || "").toLowerCase();
  const canAdd =
    Boolean(detection.codex?.present) &&
    (!liveEmail || !knownCodexEmails.has(liveEmail));
  if (canAdd) {
    const add = acctSpan("acct-act add", "+ add");
    add.addEventListener("click", async () => {
      add.classList.add("disabled");
      add.textContent = "adding…";
      setAccountsMsg("reading the Codex CLI session…");
      try {
        const result = JSON.parse(await invoke("add_codex_account"));
        setAccountsMsg(
          result.already
            ? `${result.email} is already registered as '${result.profile}'`
            : `added ${result.email} as '${result.profile}'`,
          "ok",
        );
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
        add.classList.remove("disabled");
        add.textContent = "+ add";
      }
    });
    const label = liveCodex
      ? `CLI · ${liveCodex.email || "logged in"}`
      : "CLI · current session";
    const logout = acctSpan("acct-act rm", "✕ logout");
    logout.addEventListener("click", async () => {
      logout.classList.add("disabled");
      logout.textContent = "logging out…";
      try {
        await invoke("remove_codex_account");
        setAccountsMsg("logged out of the Codex CLI", "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
        logout.classList.remove("disabled");
        logout.textContent = "✕ logout";
      }
    });
    codex.appendChild(acctRow(acctSpan("who", label), add, logout));
  } else if (!codexRegistered.length) {
    codex.appendChild(acctSpan("acct-hint", "auto-detected via the Codex CLI — none found"));
  }
  root.appendChild(codex);

  // Grok, like Codex: auto-detected from the CLI session. Remove shells
  // out to `grok logout` (or drops auth.json if the binary is missing).
  const grok = document.createElement("div");
  grok.className = "acct-section";
  grok.appendChild(acctSpan("acct-title provider-name Grok", "Grok"));
  const grokProvider = lastProviders.find((p) => p.name === "Grok");
  const grokDetected = Boolean(grokProvider);
  const grokHint = acctSpan(
    "acct-hint",
    grokProvider && !grokProvider.error
      ? `auto-detected · ${grokProvider.email || grokProvider.plan || "logged in"}`
      : "auto-detected via the Grok CLI — none found",
  );
  if (grokDetected) {
    const remove = acctSpan("acct-act rm", "✕ remove");
    remove.addEventListener("click", async () => {
      remove.classList.add("disabled");
      remove.textContent = "logging out…";
      try {
        await invoke("remove_grok_account");
        setAccountsMsg("logged out of the Grok CLI", "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
        remove.classList.remove("disabled");
        remove.textContent = "✕ remove";
      }
    });
    grok.appendChild(acctRow(grokHint, remove));
  } else {
    grok.appendChild(grokHint);
  }
  root.appendChild(grok);

  // Cursor: no local credential to detect; manual key/cookie entry.
  const cursor = document.createElement("div");
  cursor.className = "acct-section";
  cursor.appendChild(acctSpan("acct-title provider-name Cursor", "Cursor"));
  const cursorConfig = detection.cursor || { configured: false, method: null, email: null };

  function showCursorForm() {
    cursor.replaceChildren(acctSpan("acct-title provider-name Cursor", "Cursor"));
    const form = document.createElement("div");
    form.className = "acct-form";
    const method = document.createElement("select");
    method.className = "acct-select";
    method.append(new Option("Admin API key", "admin_key"), new Option("Dashboard cookie", "dashboard_cookie"));
    if (["admin_key", "dashboard_cookie"].includes(cursorConfig.method)) method.value = cursorConfig.method;
    const secret = document.createElement("input");
    secret.type = "password";
    secret.className = "acct-input";
    const email = document.createElement("input");
    email.type = "text";
    email.className = "acct-input";
    email.placeholder = "your-email@company.com";
    email.value = cursorConfig.email || "";
    const emailLabel = document.createElement("label");
    emailLabel.append("email", email);
    const syncMethod = () => {
      const admin = method.value === "admin_key";
      secret.placeholder = admin ? "key_…" : "WorkosCursorSessionToken";
      emailLabel.style.display = admin ? "" : "none";
    };
    method.addEventListener("change", syncMethod);
    syncMethod();
    const save = acctSpan("acct-save", "save");
    save.addEventListener("click", async () => {
      save.classList.add("disabled");
      try {
        await invoke("save_cursor_config", {
          method: method.value,
          secret: secret.value,
          email: email.value,
        });
        setAccountsMsg("Cursor configured", "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
        save.classList.remove("disabled");
      }
    });
    const actions = document.createElement("div");
    actions.className = "acct-form-actions";
    actions.appendChild(save);
    if (cursorConfig.configured) {
      const cancel = acctSpan("acct-act rm", "cancel");
      cancel.addEventListener("click", () => renderAccounts());
      actions.appendChild(cancel);
    }
    const methodLabel = document.createElement("label");
    methodLabel.append("method", method);
    const secretLabel = document.createElement("label");
    secretLabel.append("secret", secret);
    form.append(methodLabel, secretLabel, emailLabel, actions);
    cursor.appendChild(form);
    secret.focus();
  }

  if (cursorConfig.configured) {
    const edit = acctSpan("acct-act add", "edit");
    edit.addEventListener("click", showCursorForm);
    const remove = acctSpan("acct-act rm", "✕ remove");
    remove.addEventListener("click", async () => {
      try {
        await invoke("remove_cursor_config");
        setAccountsMsg("Cursor config removed", "ok");
        await refresh();
        renderAccounts();
      } catch (error) {
        setAccountsMsg(String(error), "err");
      }
    });
    cursor.appendChild(acctRow(acctSpan("who", "configured"), edit, remove));
  } else {
    showCursorForm();
  }
  root.appendChild(cursor);

  const msg = document.createElement("div");
  msg.id = "acct-msg";
  msg.className = `acct-msg${accountsMsg.kind ? ` ${accountsMsg.kind}` : ""}`;
  msg.textContent = accountsMsg.text;
  root.appendChild(msg);
  requestAnimationFrame(fitWindow);
}

function toggleAccounts(open) {
  const show = open ?? !accountsOpen();
  document.body.classList.toggle("show-accounts", show);
  document.getElementById("foothint").textContent = show ? "esc back" : "r refresh · a accounts · esc hide";
  if (show) renderAccounts();
  else accountsMsg = { text: "", kind: "" };
  requestAnimationFrame(fitWindow);
}

document.getElementById("refresh").addEventListener("click", refresh);
document.getElementById("settings").addEventListener("click", () => toggleAccounts());
document.getElementById("close").addEventListener("click", () => invoke("hide_to_tray"));
document.addEventListener("keydown", (event) => {
  const typing = ["INPUT", "SELECT", "TEXTAREA"].includes(event.target.tagName);
  if (event.key === "Escape") {
    if (accountsOpen()) toggleAccounts(false);
    else invoke("hide_to_tray");
    return;
  }
  if (typing) return;
  if (event.key === "q") invoke("hide_to_tray");
  if (event.key === "r") refresh();
  if (event.key === "a") toggleAccounts();
});

// The window starts hidden. Waiting one frame guarantees the WebView has
// composited the content before a tray click can show it. macOS (WKWebView)
// suspends requestAnimationFrame while the window is hidden, so a timer
// backs it up — the ready signal must not depend on ever being composited.
let readySent = false;
function sendReady() {
  if (readySent) return;
  readySent = true;
  invoke("frontend_ready").catch(console.error);
}
requestAnimationFrame(sendReady);
setTimeout(sendReady, 250);

document.getElementById("status").title = `auto-refresh every ${INTERVAL_MS / 1000}s`;

loadAlertThresholds();
refresh();
setInterval(refresh, INTERVAL_MS);
