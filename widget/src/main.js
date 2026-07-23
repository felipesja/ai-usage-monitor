const { invoke } = window.__TAURI__.core;
const notification = window.__TAURI__.notification;

const INTERVAL_MS = 60_000;
const ALERT_PERCENT = 80; // 0 disables
const alerted = new Set();

const ACCENT = { Claude: "var(--claude)", Codex: "var(--openai)", Cursor: "var(--cursor)" };

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

function checkAlerts(providers) {
  if (!ALERT_PERCENT) return;
  for (const provider of providers) {
    for (const meter of provider.meters || []) {
      if (meter.percent == null) continue;
      // Hysteresis: alert once when crossing the threshold, and only re-arm
      // when usage drops back below it (e.g. the window renewed). reset_at is
      // kept out of the key because some providers jitter it between fetches.
      const key = `${provider.email || provider.account}|${meter.label}`;
      if (meter.percent < ALERT_PERCENT) {
        alerted.delete(key);
        continue;
      }
      if (alerted.has(key)) continue;
      alerted.add(key);
      const remaining = resetRemaining(meter.reset_at);
      notify({
        title: `${provider.name} · ${provider.email || provider.account}`,
        body: `${meter.label} at ${Math.round(meter.percent)}%` + (remaining ? ` · renews in ${remaining}` : ""),
      });
    }
  }
}

function render(providers) {
  const root = document.getElementById("providers");
  root.replaceChildren();
  for (const provider of providers) {
    const box = document.createElement("div");
    box.className = "provider";
    box.setAttribute("data-tauri-drag-region", "");

    const head = document.createElement("div");
    head.className = "provider-head";
    const identity = document.createElement("div");
    identity.className = "identity";
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
    identity.appendChild(nameRow);
    const email = document.createElement("div");
    email.className = "email";
    email.textContent = provider.email || provider.account;
    identity.appendChild(email);
    head.appendChild(identity);
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
      label.textContent =
        meter.used != null && meter.limit != null ? `${meter.label} ${meter.used}/${meter.limit}` : meter.label;

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

let refreshing = false;

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

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  setStatus("↻", "refreshing", true);
  try {
    const raw = await invoke("fetch_usage");
    const providers = JSON.parse(raw);
    render(providers);
    checkAlerts(providers);
    // en-GB keeps the 24h HH:MM:SS shape the compact layout is sized for.
    setStatus("●", new Date().toLocaleTimeString("en-GB"), false);
    document.getElementById("subtitle").textContent = `${providers.length} subscriptions`;
  } catch (error) {
    setStatus("!", "refresh failed", true);
    console.error(error);
  } finally {
    refreshing = false;
  }
}

document.getElementById("refresh").addEventListener("click", refresh);
document.getElementById("close").addEventListener("click", () => invoke("hide_to_tray"));
document.addEventListener("keydown", (event) => {
  if (event.key === "q" || event.key === "Escape") invoke("hide_to_tray");
  if (event.key === "r") refresh();
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

document.getElementById("autorefresh").textContent = `auto-refresh every ${INTERVAL_MS / 1000}s`;

refresh();
setInterval(refresh, INTERVAL_MS);
