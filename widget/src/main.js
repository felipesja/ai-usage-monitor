const { invoke } = window.__TAURI__.core;
const notification = window.__TAURI__.notification;

const INTERVAL_MS = 60_000;
const ALERT_PERCENT = 80; // 0 desliga
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

function checkAlerts(providers) {
  if (!ALERT_PERCENT) return;
  for (const provider of providers) {
    for (const meter of provider.meters || []) {
      if (meter.percent == null) continue;
      // Histerese: alerta 1x ao cruzar o limiar e só re-arma quando o uso
      // cai abaixo dele (ex.: janela renovada). Não usa reset_at na chave
      // porque alguns provedores oscilam esse valor entre fetches.
      const key = `${provider.email || provider.account}|${meter.label}`;
      if (meter.percent < ALERT_PERCENT) {
        alerted.delete(key);
        continue;
      }
      if (alerted.has(key)) continue;
      alerted.add(key);
      const remaining = resetRemaining(meter.reset_at);
      notification.sendNotification({
        title: `${provider.name} · ${provider.email || provider.account}`,
        body: `${meter.label} em ${Math.round(meter.percent)}%` + (remaining ? ` · renova em ${remaining}` : ""),
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
    // Com 2+ contas do mesmo provedor, o coletor marca standby na que não
    // está logada no CLI — a sem selo é a ativa no momento.
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
      // Sem reset_at = janela ainda não aberta (a sessão de 5h só começa no
      // primeiro request). Renderiza a linha apagada em vez de "0% inativa".
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
  setStatus("↻", "atualizando", true);
  try {
    const raw = await invoke("fetch_usage");
    const providers = JSON.parse(raw);
    render(providers);
    checkAlerts(providers);
    setStatus("●", new Date().toLocaleTimeString("pt-BR"), false);
    document.getElementById("subtitle").textContent =
      `${providers.length} assinaturas · atualização automática em ${INTERVAL_MS / 1000}s`;
  } catch (error) {
    setStatus("!", "falha na atualização", true);
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

// A janela nasce oculta. Esperar um frame garante que o WebView já tenha
// composto o conteúdo antes de um click no tray poder mostrá-la.
requestAnimationFrame(() => invoke("frontend_ready").catch(console.error));

refresh();
setInterval(refresh, INTERVAL_MS);
