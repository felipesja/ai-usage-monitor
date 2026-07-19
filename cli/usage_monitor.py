#!/usr/bin/env python3
"""Terminal dashboard for Claude, Codex, and Cursor subscription usage."""

from __future__ import annotations

import argparse
import base64
import calendar
import concurrent.futures
import curses
import datetime as dt
import getpass
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any


APP = "ai-usage-monitor"
CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / APP
CLAUDE_DIR = CONFIG_DIR / "claude"
CURSOR_CONFIG = CONFIG_DIR / "cursor.json"
CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
CLAUDE_PROFILE_URL = "https://api.anthropic.com/api/oauth/profile"
CLAUDE_TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
CLAUDE_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"


@dataclass
class Meter:
    label: str
    percent: float | None = None
    reset_at: str | None = None
    used: str | None = None
    limit: str | None = None


@dataclass
class Provider:
    name: str
    account: str
    plan: str = ""
    email: str = ""
    standby: bool = False
    meters: list[Meter] = field(default_factory=list)
    details: list[str] = field(default_factory=list)
    error: str | None = None

    def label(self) -> str:
        return self.email or self.account


def ensure_private_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    for candidate in (CONFIG_DIR, CLAUDE_DIR, path):
        if candidate.exists():
            candidate.chmod(0o700)


def write_private_json(path: Path, value: dict[str, Any]) -> None:
    ensure_private_dir(path.parent)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    temp.chmod(0o600)
    temp.replace(path)


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def request_json(
    url: str,
    *,
    method: str = "GET",
    headers: dict[str, str] | None = None,
    body: dict[str, Any] | None = None,
    timeout: float = 12,
) -> dict[str, Any]:
    data = json.dumps(body).encode() if body is not None else None
    all_headers = {"Accept": "application/json", **(headers or {})}
    if body is not None:
        all_headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=all_headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        message = f"HTTP {exc.code}"
        try:
            payload = json.loads(exc.read().decode("utf-8"))
            message += f": {payload.get('error', payload.get('message', 'falha na API'))}"
        except Exception:
            pass
        raise RuntimeError(message) from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"rede indisponível: {exc.reason}") from None


def safe_name(value: str) -> str:
    value = re.sub(r"[^a-zA-Z0-9_.-]+", "-", value.strip()).strip("-.")
    if not value:
        raise ValueError("nome de perfil inválido")
    return value


def claude_add(name: str, source: Path) -> None:
    name = safe_name(name)
    data = read_json(source)
    oauth = data.get("claudeAiOauth") or {}
    if not oauth.get("accessToken") or not oauth.get("refreshToken"):
        raise RuntimeError("o arquivo não contém uma sessão OAuth completa do Claude")
    target = CLAUDE_DIR / name / ".credentials.json"
    write_private_json(target, data)
    print(f"Perfil Claude '{name}' cadastrado em {target.parent}")


def claude_login(name: str, email: str | None) -> None:
    name = safe_name(name)
    with tempfile.TemporaryDirectory(prefix="ai-usage-claude-login-") as temp:
        temp_dir = Path(temp)
        temp_dir.chmod(0o700)
        env = os.environ.copy()
        env["CLAUDE_CONFIG_DIR"] = str(temp_dir)
        command = ["claude", "auth", "login", "--claudeai"]
        if email:
            command.extend(["--email", email])
        try:
            subprocess.run(command, env=env, check=True)
        except subprocess.CalledProcessError as exc:
            raise RuntimeError(f"login do Claude terminou com código {exc.returncode}") from None
        source = temp_dir / ".credentials.json"
        if not source.exists():
            raise RuntimeError("o login terminou sem criar uma credencial")
        claude_add(name, source)


def refresh_claude(path: Path, data: dict[str, Any]) -> dict[str, Any]:
    oauth = data["claudeAiOauth"]
    expires_at = float(oauth.get("expiresAt") or 0)
    if expires_at > time.time() * 1000 + 120_000:
        return data
    scopes = oauth.get("scopes") or []
    response = request_json(
        CLAUDE_TOKEN_URL,
        method="POST",
        headers={"User-Agent": "claude-code/2"},
        body={
            "grant_type": "refresh_token",
            "refresh_token": oauth["refreshToken"],
            "client_id": CLAUDE_CLIENT_ID,
            "scope": " ".join(scopes),
        },
        timeout=30,
    )
    oauth["accessToken"] = response["access_token"]
    oauth["refreshToken"] = response.get("refresh_token", oauth["refreshToken"])
    oauth["expiresAt"] = int(time.time() * 1000 + int(response.get("expires_in", 28_800)) * 1000)
    if response.get("refresh_token_expires_in"):
        oauth["refreshTokenExpiresAt"] = int(
            time.time() * 1000 + int(response["refresh_token_expires_in"]) * 1000
        )
    if response.get("scope"):
        oauth["scopes"] = response["scope"].split()
    write_private_json(path, data)
    return data


def collect_claude(profile_dir: Path) -> Provider:
    name = profile_dir.name
    try:
        path = profile_dir / ".credentials.json"
        data = refresh_claude(path, read_json(path))
        oauth = data["claudeAiOauth"]
        auth_headers = {
            "Authorization": f"Bearer {oauth['accessToken']}",
            "anthropic-beta": "oauth-2025-04-20",
            "User-Agent": "claude-code/2",
        }
        usage = request_json(CLAUDE_USAGE_URL, headers=auth_headers)
        email = ""
        try:
            profile = request_json(CLAUDE_PROFILE_URL, headers=auth_headers)
            email = str((profile.get("account") or {}).get("email") or "")
        except Exception:
            pass
        result = Provider("Claude", name, str(oauth.get("subscriptionType", "")).title(), email)
        labels = (("five_hour", "Session"), ("seven_day", "Weekly"), ("seven_day_sonnet", "Weekly Sonnet"))
        for key, label in labels:
            block = usage.get(key)
            if block:
                result.meters.append(Meter(label, float(block.get("utilization", 0)), block.get("resets_at")))
        extra = usage.get("extra_usage") or {}
        if extra.get("is_enabled"):
            result.details.append(f"Uso extra: {extra.get('utilization', 0)}%")
        return result
    except Exception as exc:
        return Provider("Claude", name, error=str(exc))


def rpc_read(proc: subprocess.Popen[str], messages: queue.Queue[dict[str, Any]], wanted: int, timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            message = messages.get(timeout=max(0.1, deadline - time.monotonic()))
        except queue.Empty:
            break
        if message.get("id") == wanted:
            if "error" in message:
                raise RuntimeError(str(message["error"]))
            return message["result"]
    raise RuntimeError("Codex não respondeu a tempo")


def codex_bin() -> str:
    # Em shells não-interativos o PATH do nvm não carrega, e o PATH do
    # Windows (via WSL) oferece um shim npm de /mnt/c que não roda aqui —
    # só aceita o which se for um binário do próprio Linux.
    found = shutil.which("codex")
    if found and not found.startswith("/mnt/"):
        return found
    candidates = sorted(Path.home().glob(".nvm/versions/node/*/bin/codex"))
    return str(candidates[-1]) if candidates else (found or "codex")


def codex_live() -> dict[str, Any]:
    binary = codex_bin()
    env = os.environ.copy()
    # O codex é um script Node: garante o node do mesmo diretório no PATH.
    env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH', '')}"
    stderr_log = open("/tmp/ai-usage-codex-stderr.log", "ab")
    proc = subprocess.Popen(
        [binary, "app-server", "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=stderr_log,
        text=True,
        bufsize=1,
        start_new_session=True,
        env=env,
    )
    messages: queue.Queue[dict[str, Any]] = queue.Queue()

    def reader() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            try:
                messages.put(json.loads(line))
            except json.JSONDecodeError:
                continue

    threading.Thread(target=reader, daemon=True).start()
    assert proc.stdin is not None
    try:
        init = {
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {"name": APP, "title": "AI Usage Monitor", "version": "0.1.0"},
                "capabilities": None,
            },
        }
        proc.stdin.write(json.dumps(init) + "\n")
        proc.stdin.flush()
        try:
            rpc_read(proc, messages, 1, 5)
        except RuntimeError as exc:
            raise RuntimeError(f"{exc} (fase init, exit={proc.poll()})") from None
        proc.stdin.write('{"method":"initialized"}\n')
        proc.stdin.write('{"method":"account/rateLimits/read","id":2}\n')
        proc.stdin.flush()
        try:
            return rpc_read(proc, messages, 2, 15)
        except RuntimeError as exc:
            raise RuntimeError(f"{exc} (fase rateLimits, exit={proc.poll()})") from None
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()


def codex_cached() -> dict[str, Any]:
    session_dir = Path.home() / ".codex" / "sessions"
    latest = max(session_dir.rglob("*.jsonl"), key=lambda path: path.stat().st_mtime)
    found: dict[str, Any] | None = None
    with latest.open(encoding="utf-8") as stream:
        for line in stream:
            try:
                payload = json.loads(line).get("payload", {})
                if payload.get("type") == "token_count" and payload.get("rate_limits"):
                    found = payload["rate_limits"]
            except json.JSONDecodeError:
                continue
    if found is None:
        raise RuntimeError("nenhum limite foi encontrado nas sessões locais")
    return {"rateLimits": found}


def codex_email() -> str:
    try:
        auth = read_json(Path.home() / ".codex" / "auth.json")
        token = (auth.get("tokens") or {}).get("id_token") or ""
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
        return str(claims.get("email") or "")
    except Exception:
        return ""


def collect_codex() -> Provider:
    try:
        live_error = ""
        try:
            data = codex_live()
        except Exception as exc:
            data = codex_cached()
            live_error = str(exc) or type(exc).__name__
            try:
                with open("/tmp/ai-usage-codex.log", "a", encoding="utf-8") as log:
                    log.write(f"{dt.datetime.now():%H:%M:%S} codex_live falhou: {live_error}\n")
            except OSError:
                pass
        raw = data.get("rateLimits") or {}
        plan = raw.get("planType", raw.get("plan_type", ""))
        result = Provider("Codex", "ChatGPT", str(plan).replace("_", " ").title(), codex_email())
        if live_error:
            result.details.append(f"⚠ cache local · app-server: {live_error}")
        for key in ("primary", "secondary"):
            block = raw.get(key)
            if not block:
                continue
            minutes = block.get("windowDurationMins", block.get("window_minutes"))
            label = "Session" if minutes == 300 else "Weekly" if minutes == 10_080 else f"{minutes} min"
            percent = block.get("usedPercent", block.get("used_percent"))
            reset = block.get("resetsAt", block.get("resets_at"))
            if isinstance(reset, (int, float)):
                reset = dt.datetime.fromtimestamp(reset, dt.timezone.utc).isoformat()
            result.meters.append(Meter(label, float(percent), reset))
        credits = raw.get("credits") or {}
        if credits.get("hasCredits", credits.get("has_credits")):
            result.details.append(f"Créditos: {credits.get('balance', '?')}")
        return result
    except Exception as exc:
        return Provider("Codex", "ChatGPT", error=str(exc))


def next_month(epoch_ms: int) -> str:
    value = dt.datetime.fromtimestamp(epoch_ms / 1000, dt.timezone.utc)
    year, month = value.year, value.month + 1
    if month == 13:
        year, month = year + 1, 1
    day = min(value.day, calendar.monthrange(year, month)[1])
    return value.replace(year=year, month=month, day=day).isoformat()


def collect_cursor() -> Provider:
    if not CURSOR_CONFIG.exists():
        return Provider("Cursor", "Business", "Team", error="configure com: ai-usage cursor-cookie ou cursor-admin")
    try:
        config = read_json(CURSOR_CONFIG)
        if config["method"] == "dashboard_cookie":
            token = config["session_cookie"]
            cookie_headers = {"Cookie": f"WorkosCursorSessionToken={token}", "User-Agent": APP}
            data = request_json("https://cursor.com/api/usage-summary", headers=cookie_headers)
            email = ""
            try:
                email = str(request_json("https://cursor.com/api/auth/me", headers=cookie_headers).get("email") or "")
            except Exception:
                pass
            result = Provider("Cursor", "Business", str(data.get("membershipType", "Team")).title(), email)
            plan = (data.get("individualUsage") or {}).get("plan") or {}
            raw_used = float(plan.get("used", 0))
            raw_limit = float(plan.get("limit", 0))
            percent = raw_used * 100 / raw_limit if raw_limit else None

            # The team dashboard presents included-request units at 1/4 of the
            # internal values returned by usage-summary (576/2000 -> 144/500).
            request_scale = 4 if data.get("limitType") == "team" else 1
            request_used = raw_used / request_scale
            request_limit = raw_limit / request_scale

            def display_number(value: float) -> str:
                return str(int(value)) if value.is_integer() else f"{value:g}"

            result.meters.append(
                Meter(
                    "Usage",
                    percent,
                    data.get("billingCycleEnd"),
                    display_number(request_used),
                    display_number(request_limit),
                )
            )
            auto_percent = plan.get("autoPercentUsed")
            if auto_percent:
                result.meters.append(Meter("Uso do Auto", float(auto_percent), data.get("billingCycleEnd")))
            demand = (data.get("individualUsage") or {}).get("onDemand") or {}
            if demand.get("enabled"):
                result.details.append(f"Sob demanda: ${float(demand.get('used', 0)) / 100:.2f}")
            return result

        key = config["admin_key"]
        auth = base64.b64encode(f"{key}:".encode()).decode()
        data = request_json(
            "https://api.cursor.com/teams/spend",
            method="POST",
            headers={"Authorization": f"Basic {auth}"},
            body={"searchTerm": config.get("email", ""), "page": 1, "pageSize": 10},
        )
        members = data.get("teamMemberSpend") or []
        email = config.get("email", "")
        member = next((item for item in members if item.get("email", "").lower() == email.lower()), members[0] if members else {})
        if not member:
            raise RuntimeError("usuário não encontrado no retorno da equipe")
        reset = next_month(int(data["subscriptionCycleStart"])) if data.get("subscriptionCycleStart") else None
        result = Provider("Cursor", "Business", "Team", email)
        for key_name, label in (("totalPercentUsed", "Uso total"), ("autoPercentUsed", "Auto")):
            if member.get(key_name) is not None:
                result.meters.append(Meter(label, float(member[key_name]), reset))
        spent = float(member.get("spendCents", 0)) / 100
        limit = member.get("monthlyLimitDollars") or member.get("hardLimitOverrideDollars")
        result.details.append(f"Gasto: ${spent:.2f}" + (f" / ${float(limit):.2f}" if limit else ""))
        return result
    except Exception as exc:
        return Provider("Cursor", "Business", error=str(exc))


def claude_active_email() -> str:
    try:
        data = read_json(Path.home() / ".claude.json")
        return str((data.get("oauthAccount") or {}).get("emailAddress") or "")
    except Exception:
        return ""


def mark_standby(results: list[Provider]) -> None:
    claude = [item for item in results if item.name == "Claude"]
    if len(claude) < 2:
        return
    email = claude_active_email().lower()
    if not email:
        return
    for item in claude:
        item.standby = item.email.lower() != email


def collect_all() -> list[Provider]:
    claude_profiles = sorted(path.parent for path in CLAUDE_DIR.glob("*/.credentials.json")) if CLAUDE_DIR.exists() else []
    jobs = [("claude", profile) for profile in claude_profiles] + [("codex", None), ("cursor", None)]

    def run(job: tuple[str, Path | None]) -> Provider:
        kind, path = job
        if kind == "claude":
            assert path is not None
            return collect_claude(path)
        return collect_codex() if kind == "codex" else collect_cursor()

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(jobs))) as pool:
        results = list(pool.map(run, jobs))
    mark_standby(results)
    return results


def reset_remaining(value: str | None) -> str:
    if not value:
        return ""
    try:
        moment = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return ""
    seconds = max(0, int((moment - dt.datetime.now(dt.timezone.utc)).total_seconds()))
    if seconds >= 86_400:
        return f"{seconds // 86_400}d {(seconds % 86_400) // 3600}h"
    if seconds >= 3600:
        return f"{seconds // 3600}h {(seconds % 3600) // 60}m"
    return f"{seconds // 60}m"


def reset_text(value: str | None) -> str:
    if not value:
        return ""
    remaining = reset_remaining(value)
    try:
        local = dt.datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone()
    except ValueError:
        return value
    return f"renova {local:%d/%m %H:%M} ({remaining})" if remaining else value


WINDOWS_POWERSHELL = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"


def notify_windows(title: str, body: str) -> None:
    """Toast nativo do Windows via WinRT; no-op fora do WSL."""
    powershell = WINDOWS_POWERSHELL if os.path.exists(WINDOWS_POWERSHELL) else shutil.which("powershell.exe")
    if not powershell:
        return

    def quote(value: str) -> str:
        return "'" + value.replace("'", "''") + "'"

    script = (
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;"
        "$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent("
        "[Windows.UI.Notifications.ToastTemplateType]::ToastText02);"
        "$texts = $template.GetElementsByTagName('text');"
        f"$null = $texts.Item(0).AppendChild($template.CreateTextNode({quote(title)}));"
        f"$null = $texts.Item(1).AppendChild($template.CreateTextNode({quote(body)}));"
        "$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe';"
        "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show("
        "[Windows.UI.Notifications.ToastNotification]::new($template))"
    )
    try:
        subprocess.Popen(
            [powershell, "-NoProfile", "-NonInteractive", "-Command", script],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        pass


def alert_meters(results: list[Provider], threshold: int, alerted: set[tuple[str, str]]) -> None:
    if threshold <= 0:
        return
    for provider in results:
        for meter in provider.meters:
            if meter.percent is None:
                continue
            # Histerese: alerta 1x ao cruzar o limiar e só re-arma quando o
            # uso cai abaixo dele (ex.: janela renovada). reset_at não entra
            # na chave porque alguns provedores oscilam o valor entre fetches.
            key = (provider.label(), meter.label)
            if meter.percent < threshold:
                alerted.discard(key)
                continue
            if key in alerted:
                continue
            alerted.add(key)
            remaining = reset_remaining(meter.reset_at)
            body = f"{meter.label} em {meter.percent:.0f}%" + (f" · renova em {remaining}" if remaining else "")
            notify_windows(f"{provider.name} · {provider.label()}", body)


def bar(percent: float | None, width: int = 24) -> str:
    if percent is None:
        return "[" + "?" * width + "]"
    used = min(width, max(0, round(percent * width / 100)))
    return "[" + "█" * used + "░" * (width - used) + "]"


def plain(results: list[Provider]) -> str:
    lines: list[str] = []
    for provider in results:
        title = f"{provider.name} · {provider.label()}" + (f" · {provider.plan}" if provider.plan else "")
        if provider.standby:
            title += " · STANDBY"
        lines.append(title)
        if provider.error:
            lines.append(f"  ! {provider.error}")
        for meter in provider.meters:
            percent = "--" if meter.percent is None else f"{meter.percent:.0f}%"
            raw = f" ({meter.used}/{meter.limit})" if meter.used not in (None, "None") and meter.limit not in (None, "None") else ""
            lines.append(f"  {meter.label:<15} {bar(meter.percent)} {percent:>4}{raw} {reset_text(meter.reset_at)}".rstrip())
        lines.extend(f"  {item}" for item in provider.details)
        lines.append("")
    return "\n".join(lines).rstrip()


def tui_colors() -> dict[str, int]:
    try:
        curses.start_color()
        curses.use_default_colors()
        rich = getattr(curses, "COLORS", 0) >= 256
        palette = {
            "cyan": 80 if rich else curses.COLOR_CYAN,
            "claude": 173 if rich else curses.COLOR_RED,
            "openai": 36 if rich else curses.COLOR_GREEN,
            "cursor": 252 if rich else curses.COLOR_WHITE,
            "amber": 214 if rich else curses.COLOR_YELLOW,
            "green": 78 if rich else curses.COLOR_GREEN,
            "red": 203 if rich else curses.COLOR_RED,
            "muted": 245 if rich else curses.COLOR_WHITE,
            "dim": 240 if rich else curses.COLOR_WHITE,
            "text": 255 if rich else curses.COLOR_WHITE,
        }
        for number, color in enumerate(palette.values(), start=1):
            curses.init_pair(number, color, -1)
        return {name: curses.color_pair(number) for number, name in enumerate(palette, start=1)}
    except curses.error:
        return {name: 0 for name in ("cyan", "claude", "openai", "cursor", "amber", "green", "red", "muted", "dim", "text")}


def tui_add(screen: Any, y: int, x: int, value: str, width: int, attr: int = 0) -> None:
    height, screen_width = screen.getmaxyx()
    if y < 0 or y >= height or x < 0 or x >= screen_width or width <= 0:
        return
    try:
        screen.addnstr(y, x, value, min(width, screen_width - x - 1), attr)
    except curses.error:
        pass


def tui_value(meter: Meter) -> str:
    if meter.used not in (None, "None") and meter.limit not in (None, "None"):
        base = f"{meter.used}/{meter.limit}"
        return f"{base} · {meter.percent:.0f}%" if meter.percent is not None else base
    return "--" if meter.percent is None else f"{meter.percent:.0f}%"


def tui_bar(percent: float | None, width: int) -> str:
    width = max(4, width)
    if percent is None:
        return "?" * width
    filled = min(width, max(0, round(percent * width / 100)))
    return "█" * filled + "░" * (width - filled)


PROVIDER_COLOR = {"Claude": "claude", "Codex": "openai", "Cursor": "cursor"}


def provider_accent(index: int, provider: Provider, colors: dict[str, int]) -> int:
    return colors[PROVIDER_COLOR.get(provider.name, "text")]


def meter_attr(percent: float | None, accent: int, colors: dict[str, int]) -> int:
    if percent is None:
        return colors["muted"]
    if percent >= 85:
        return colors["red"]
    if percent >= 65:
        return colors["amber"]
    return accent


def draw_card(
    screen: Any,
    y: int,
    x: int,
    height: int,
    width: int,
    provider: Provider,
    index: int,
    colors: dict[str, int],
) -> None:
    accent = provider_accent(index, provider, colors)
    border_attr = accent
    tui_add(screen, y, x, "╭" + "─" * (width - 2) + "╮", width, border_attr)
    for row in range(1, height - 1):
        tui_add(screen, y + row, x, "│", 1, border_attr)
        tui_add(screen, y + row, x + width - 1, "│", 1, border_attr)
    tui_add(screen, y + height - 1, x, "╰" + "─" * (width - 2) + "╯", width, border_attr)

    plan = provider.plan.upper() if provider.plan else ""
    tui_add(screen, y + 1, x + 2, provider.name, width - len(plan) - 5, accent | curses.A_BOLD)
    if provider.standby:
        tui_add(screen, y + 1, x + 3 + len(provider.name), "◉ STANDBY", width - len(plan) - 6 - len(provider.name), colors["cyan"] | curses.A_BOLD)
    if plan:
        tui_add(screen, y + 1, x + max(2, width - len(plan) - 3), plan, len(plan), colors["muted"])
    tui_add(screen, y + 2, x + 2, provider.label(), width - 4, colors["dim"])

    if provider.error:
        tui_add(screen, y + 4, x + 3, "! " + provider.error, width - 6, colors["red"])
        return

    inner_width = width - 6
    for meter_index, meter in enumerate(provider.meters):
        row = y + 3 + meter_index * 2
        if row + 1 > y + height - 2:
            break
        value = tui_value(meter)
        tui_add(screen, row, x + 3, meter.label, inner_width - len(value) - 1, colors["text"])
        tui_add(screen, row, x + width - len(value) - 3, value, len(value), curses.A_BOLD)
        attr = meter_attr(meter.percent, accent, colors)
        reset = reset_remaining(meter.reset_at) or "inativa"
        tui_add(screen, row + 1, x + 3, tui_bar(meter.percent, inner_width - len(reset) - 2), inner_width - len(reset) - 2, attr)
        tui_add(screen, row + 1, x + width - len(reset) - 3, reset, len(reset), colors["muted"])


def draw_compact_tui(screen: Any, results: list[Provider], top: int, colors: dict[str, int]) -> None:
    height, width = screen.getmaxyx()
    row = top
    inner_width = width - 4
    label_w = 16
    bar_x = 4 + label_w + 1
    reset_end = width - 2
    pct_end = reset_end - 8 - 2
    bar_w = pct_end - 4 - 1 - bar_x
    for index, provider in enumerate(results):
        if row >= height - 2:
            break
        accent = provider_accent(index, provider, colors)
        plan = provider.plan.upper() if provider.plan else ""
        tui_add(screen, row, 2, provider.name, inner_width - len(plan) - 1, accent | curses.A_BOLD)
        if provider.standby:
            tui_add(screen, row, 3 + len(provider.name), "◉ STANDBY", inner_width - len(plan) - len(provider.name) - 2, colors["cyan"] | curses.A_BOLD)
        if plan:
            tui_add(screen, row, max(2, width - len(plan) - 2), plan, len(plan), colors["muted"])
        row += 1
        tui_add(screen, row, 2, provider.label(), inner_width, colors["dim"])
        row += 1
        if provider.error:
            tui_add(screen, row, 2, "! " + provider.error, inner_width, colors["red"])
            row += 2
            continue
        for meter in provider.meters:
            if row >= height - 2:
                break
            label = meter.label
            has_fraction = meter.used not in (None, "None") and meter.limit not in (None, "None")
            pct = "--" if meter.percent is None else f"{meter.percent:.0f}%"
            if has_fraction:
                if bar_w >= 4:
                    label = f"{label} {meter.used}/{meter.limit}"
                else:
                    pct = f"{meter.used}/{meter.limit}"
            reset = reset_remaining(meter.reset_at) or "inativa"
            attr = meter_attr(meter.percent, accent, colors)
            tui_add(screen, row, 4, label, min(label_w, max(1, pct_end - len(pct) - 5)), colors["text"])
            if bar_w >= 4:
                tui_add(screen, row, bar_x, tui_bar(meter.percent, bar_w), bar_w, attr)
            tui_add(screen, row, max(4, pct_end - len(pct)), pct, len(pct), attr | curses.A_BOLD)
            tui_add(screen, row, max(4, reset_end - len(reset)), reset, len(reset), colors["muted"])
            row += 1
        row += 1


MAX_CARD_WIDTH = 52
SPINNER_FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"


def tui(screen: Any, interval: int, alert: int = 80) -> None:
    curses.curs_set(0)
    screen.timeout(150)
    colors = tui_colors()

    lock = threading.Lock()
    state: dict[str, Any] = {"results": [], "updated_at": None, "fetching": True}
    wake_event = threading.Event()
    stop_event = threading.Event()
    alerted: set[tuple[str, str]] = set()

    def fetch_loop() -> None:
        while not stop_event.is_set():
            with lock:
                state["fetching"] = True
            results = collect_all()
            with lock:
                state["results"] = results
                state["updated_at"] = dt.datetime.now()
                state["fetching"] = False
            alert_meters(results, alert, alerted)
            wake_event.wait(interval)
            wake_event.clear()

    threading.Thread(target=fetch_loop, daemon=True).start()

    frame = 0
    last_draw_key: Any = None
    force_full = False

    def handle_key(key: int) -> bool:
        nonlocal force_full, last_draw_key
        if key in (ord("q"), 27):
            stop_event.set()
            wake_event.set()
            return True
        if key == ord("r"):
            wake_event.set()
        elif key == curses.KEY_RESIZE:
            # Repinta do zero após resize: descarta o diff do curses e limpa
            # os artefatos que sobram na tela.
            curses.update_lines_cols()
            force_full = True
            last_draw_key = None
        return False

    while True:
        with lock:
            results = state["results"]
            updated_at = state["updated_at"]
            fetching = state["fetching"]
        spinner_frame = frame % len(SPINNER_FRAMES) if fetching else -1
        draw_key = (id(results), updated_at, fetching, spinner_frame, screen.getmaxyx(), int(time.monotonic() // 15))
        if draw_key == last_draw_key and not force_full:
            frame += 1
            if handle_key(screen.getch()):
                return
            continue
        last_draw_key = draw_key
        screen.erase()
        height, width = screen.getmaxyx()
        tui_add(screen, 1, 2, "◆ AI USAGE", 14, colors["cyan"] | curses.A_BOLD)
        if width >= 60:
            tui_add(screen, 1, 15, "/ overview", 15, colors["muted"])
        if fetching:
            status = f"{SPINNER_FRAMES[frame % len(SPINNER_FRAMES)]} atualizando"
            status_attr = colors["amber"]
        else:
            status = f"● {updated_at:%H:%M:%S}" if updated_at else "○ carregando"
            status_attr = colors["green"]
        tui_add(screen, 1, max(2, width - len(status) - 3), status, len(status), status_attr)
        info = f"{len(results)} contas · auto {interval}s" if width < 60 else f"{len(results)} assinaturas · atualização automática em {interval}s"
        tui_add(screen, 2, 2, info, width - 4, colors["muted"])

        content_top = 4
        footer_row = height - 2
        cards = results[:4]
        max_meters = max((len(p.meters) for p in cards if not p.error), default=1)
        card_height = 4 + 2 * max(1, min(max_meters, 4))
        gap = 2
        columns = 2 if width >= 96 else 1
        card_width = min(MAX_CARD_WIDTH, (width - 4 - gap * (columns - 1)) // columns)
        rows_of_cards = (len(cards) + columns - 1) // columns
        needed = content_top + rows_of_cards * (card_height + 1)
        if card_width >= 34 and needed <= footer_row:
            grid_width = columns * card_width + gap * (columns - 1)
            left = max(2, (width - grid_width) // 2)
            for index, provider in enumerate(cards):
                card_y = content_top + (index // columns) * (card_height + 1)
                card_x = left + (index % columns) * (card_width + gap)
                draw_card(screen, card_y, card_x, card_height, card_width, provider, index, colors)
        else:
            draw_compact_tui(screen, results, content_top, colors)

        footer = "r atualizar · q sair"
        tui_add(screen, height - 2, 2, footer, width - 4, colors["muted"])
        if force_full:
            force_full = False
            screen.redrawwin()
        # Synchronized output (DEC 2026): o terminal compõe o frame de forma
        # atômica, sem tearing no meio do redraw.
        sys.stdout.write("\x1b[?2026h")
        sys.stdout.flush()
        screen.refresh()
        sys.stdout.write("\x1b[?2026l")
        sys.stdout.flush()
        frame += 1
        if handle_key(screen.getch()):
            return


def cursor_cookie() -> None:
    value = getpass.getpass("WorkosCursorSessionToken (entrada oculta): ").strip()
    value = value.removeprefix("WorkosCursorSessionToken=")
    if not value:
        raise RuntimeError("cookie vazio")
    write_private_json(CURSOR_CONFIG, {"method": "dashboard_cookie", "session_cookie": value})
    print(f"Sessão do Cursor salva em {CURSOR_CONFIG}")


def cursor_admin(email: str) -> None:
    key = getpass.getpass("Cursor Admin API Key (entrada oculta): ").strip()
    if not key.startswith("key_"):
        raise RuntimeError("a chave deve começar com key_")
    write_private_json(CURSOR_CONFIG, {"method": "admin_key", "admin_key": key, "email": email})
    print(f"Chave administrativa do Cursor salva em {CURSOR_CONFIG}")


def doctor() -> None:
    print(f"Configuração: {CONFIG_DIR}")
    print(f"Claude: {len(list(CLAUDE_DIR.glob('*/.credentials.json'))) if CLAUDE_DIR.exists() else 0} perfil(is)")
    print(f"Codex CLI: {'ok' if shutil.which('codex') else 'não encontrado'}")
    print(f"Cursor: {'configurado' if CURSOR_CONFIG.exists() else 'não configurado'}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    sub = result.add_subparsers(dest="command")
    watch = sub.add_parser("watch", help="abrir o TUI")
    watch.add_argument("--interval", type=int, default=60)
    watch.add_argument("--alert", type=int, default=80, help="notificar quando um limite atingir N%% (0 desliga)")
    once = sub.add_parser("once", help="imprimir uma leitura e sair")
    once.add_argument("--json", action="store_true")
    add = sub.add_parser("claude-add", help="capturar a sessão Claude atualmente ativa")
    add.add_argument("name")
    add.add_argument("--source", type=Path, default=Path.home() / ".claude" / ".credentials.json")
    login = sub.add_parser("claude-login", help="autenticar uma conta Claude sem alterar a sessão padrão")
    login.add_argument("name")
    login.add_argument("--email")
    sub.add_parser("claude-list", help="listar perfis Claude")
    sub.add_parser("cursor-cookie", help="cadastrar cookie do dashboard Cursor")
    admin = sub.add_parser("cursor-admin", help="cadastrar Cursor Admin API Key")
    admin.add_argument("--email", required=True)
    sub.add_parser("doctor", help="verificar a configuração")
    sub.add_parser("bridge", help="loop stdin→JSON compacto (usado pelo widget)")
    return result


def main() -> int:
    args = parser().parse_args()
    command = args.command or ("watch" if sys.stdout.isatty() else "once")
    try:
        if command == "claude-add":
            claude_add(args.name, args.source)
        elif command == "claude-login":
            claude_login(args.name, args.email)
        elif command == "claude-list":
            for path in sorted(CLAUDE_DIR.glob("*/.credentials.json")) if CLAUDE_DIR.exists() else []:
                print(path.parent.name)
        elif command == "cursor-cookie":
            cursor_cookie()
        elif command == "cursor-admin":
            cursor_admin(args.email)
        elif command == "doctor":
            doctor()
        elif command == "once":
            results = collect_all()
            print(json.dumps([asdict(item) for item in results], indent=2) if getattr(args, "json", False) else plain(results))
        elif command == "bridge":
            # Uma linha no stdin = uma coleta; resposta em JSON de linha única.
            for _ in sys.stdin:
                results = collect_all()
                print(json.dumps([asdict(item) for item in results], separators=(",", ":")), flush=True)
        else:
            curses.wrapper(tui, getattr(args, "interval", 60), getattr(args, "alert", 80))
        return 0
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"erro: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
