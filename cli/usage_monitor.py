#!/usr/bin/env python3
"""Terminal dashboard for Claude, Codex, Cursor, and Grok subscription usage."""

from __future__ import annotations

import argparse
import base64
import calendar
import concurrent.futures
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
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

try:
    import curses
except ModuleNotFoundError:
    # Windows Python ships without curses. Only the TUI (`watch`) needs it; the
    # other commands — credential setup included — keep working.
    curses = None  # type: ignore[assignment]


APP = "ai-usage-monitor"
CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / APP
CLAUDE_DIR = CONFIG_DIR / "claude"
CURSOR_CONFIG = CONFIG_DIR / "cursor.json"
CONFIG_FILE = CONFIG_DIR / "config.json"
# Percentages at which a limit fires a notification, each level once as usage
# rises through it. Editable in config.json ("alert_thresholds").
DEFAULT_ALERT_THRESHOLDS = [80, 90, 95, 98, 100]
# A level only re-arms once usage falls this many points below it, so a meter
# hovering on a boundary (e.g. Codex jittering around 80%) does not re-notify
# every refresh. A real reset drops usage to ~0, far past this margin.
ALERT_REARM_MARGIN = 5
CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
CLAUDE_PROFILE_URL = "https://api.anthropic.com/api/oauth/profile"
CLAUDE_TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
CLAUDE_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
CLAUDE_SESSION_SECONDS = 5 * 3600  # the 5h quota window, also the standby horizon
CLAUDE_CONFIG_TIE_SECONDS = 600  # environments this close apart are both in use
CLAUDE_ACTIVE_ACCOUNT_FILE = CONFIG_DIR / "claude-active-account.json"
GROK_BILLING_URL = "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
GROK_SETTINGS_URL = "https://cli-chat-proxy.grok.com/v1/settings"
GROK_TOKEN_URL = "https://auth.x.ai/oauth2/token"
# Refresh the Grok CLI token when under this many seconds remain, matching Claude.
GROK_REFRESH_BUFFER = 120


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


def normalize_thresholds(values: Any) -> list[int]:
    """Sanitize a thresholds list: ints in 1..100, unique, ascending.

    Falls back to the defaults when the value is missing or unusable, so a
    hand-edited config that goes wrong still notifies instead of going silent.
    """
    if not isinstance(values, list):
        return list(DEFAULT_ALERT_THRESHOLDS)
    cleaned = set()
    for value in values:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            continue
        level = round(value)
        if 1 <= level <= 100:
            cleaned.add(level)
    return sorted(cleaned) if cleaned else list(DEFAULT_ALERT_THRESHOLDS)


def load_alert_thresholds() -> list[int]:
    """Notification levels from config.json, or the defaults if absent/broken."""
    try:
        return normalize_thresholds(read_json(CONFIG_FILE).get("alert_thresholds"))
    except (OSError, ValueError):
        return list(DEFAULT_ALERT_THRESHOLDS)


def ensure_config_file() -> None:
    """Write config.json with the default thresholds so the user has one to edit."""
    if CONFIG_FILE.exists():
        return
    try:
        write_private_json(CONFIG_FILE, {"alert_thresholds": list(DEFAULT_ALERT_THRESHOLDS)})
    except OSError:
        pass


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
            message += f": {payload.get('error', payload.get('message', 'API failure'))}"
        except Exception:
            pass
        raise RuntimeError(message) from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"network unavailable: {exc.reason}") from None


def request_form(
    url: str,
    body: dict[str, str],
    *,
    headers: dict[str, str] | None = None,
    timeout: float = 30,
) -> dict[str, Any]:
    """POST application/x-www-form-urlencoded — OIDC token refresh."""
    data = urllib.parse.urlencode(body).encode()
    all_headers = {
        "Accept": "application/json",
        "Content-Type": "application/x-www-form-urlencoded",
        **(headers or {}),
    }
    request = urllib.request.Request(url, data=data, headers=all_headers, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        message = f"HTTP {exc.code}"
        try:
            payload = json.loads(exc.read().decode("utf-8"))
            message += f": {payload.get('error', payload.get('message', 'API failure'))}"
        except Exception:
            pass
        raise RuntimeError(message) from None
    except urllib.error.URLError as exc:
        raise RuntimeError(f"network unavailable: {exc.reason}") from None


def safe_name(value: str) -> str:
    value = re.sub(r"[^a-zA-Z0-9_.-]+", "-", value.strip()).strip("-.")
    if not value:
        raise ValueError("invalid profile name")
    return value


def claude_keychain_read() -> dict[str, Any] | None:
    """The Claude Code session from the macOS Keychain, or None.

    On macOS the CLI stores OAuth credentials in the Keychain (service
    "Claude Code-credentials") instead of `.credentials.json` — even when
    CLAUDE_CONFIG_DIR points elsewhere. The user may see a Keychain
    permission prompt on the first read.
    """
    if sys.platform != "darwin":
        return None
    try:
        result = subprocess.run(
            ["security", "find-generic-password", "-s", "Claude Code-credentials", "-w"],
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(result.stdout)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
        return None


def register_claude(name: str, data: dict[str, Any]) -> None:
    oauth = data.get("claudeAiOauth") or {}
    if not oauth.get("accessToken") or not oauth.get("refreshToken"):
        raise RuntimeError("the source does not contain a complete Claude OAuth session")
    target = CLAUDE_DIR / name / ".credentials.json"
    write_private_json(target, data)
    print(f"Claude profile '{name}' registered at {target.parent}")


def claude_add(name: str, source: Path) -> None:
    name = safe_name(name)
    if source.exists():
        data = read_json(source)
    else:
        # macOS: the active session lives in the Keychain, not on disk.
        data = claude_keychain_read()
        if data is None:
            raise RuntimeError(f"{source}: not found")
    register_claude(name, data)


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
            raise RuntimeError(f"Claude login exited with code {exc.returncode}") from None
        source = temp_dir / ".credentials.json"
        if source.exists():
            claude_add(name, source)
            return
        # macOS: the login lands in the Keychain even with CLAUDE_CONFIG_DIR
        # set, so capture it from there. Note that in this case the CLI also
        # replaced the default session's Keychain entry — the "does not disturb
        # open sessions" promise only holds where the temp profile works.
        data = claude_keychain_read()
        if data is None:
            raise RuntimeError("login finished without creating a credential")
        register_claude(name, data)


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
        detail = extra_usage_detail(usage.get("extra_usage") or {})
        if detail:
            result.details.append(detail)
        return result
    except Exception as exc:
        return Provider("Claude", name, error=str(exc))


CURRENCY_SYMBOLS = {"USD": "$", "BRL": "R$", "EUR": "€", "GBP": "£"}


def money(minor: float, currency: str, places: int) -> str:
    """Minor units (`used_credits`, `monthly_limit`) as a readable amount."""
    symbol = CURRENCY_SYMBOLS.get(currency.upper()) or (f"{currency.upper()} " if currency else "")
    return f"{symbol}{minor / (10 ** places):.{places}f}"


def cursor_money(cents: float) -> str:
    """Format Cursor's included-usage cents as dollars."""
    if cents.is_integer() and cents % 100 == 0:
        return f"${cents / 100:.0f}"
    return f"${cents / 100:.2f}"


def cursor_plan_usage(plan: dict[str, Any]) -> tuple[float, float, float]:
    """Cursor's `plan.used`/`limit` saturate at the included allowance, so an
    account with bonus credits sits at a permanent 100%. `breakdown` carries the
    real balance (included + bonus) and `totalPercentUsed` the real share of it.
    Returns (used_cents, total_cents, included_cents); falls back to the plain
    used/limit pair when the payload has no breakdown."""
    breakdown = plan.get("breakdown") or {}
    included = float(breakdown.get("included", plan.get("limit", 0)) or 0)
    total = float(breakdown.get("total", included) or 0)
    percent = plan.get("totalPercentUsed")
    if percent is not None and total > 0:
        used = float(percent) / 100 * total
    else:
        used = float(plan.get("used", 0) or 0)
    return min(used, total), total, included


def extra_usage_detail(extra: dict[str, Any]) -> str:
    """One line describing the extra-usage credits, or "" when there are none.

    `utilization` is only filled in when the account has a monthly cap — with
    pay-as-you-go credits it comes back null, so the amount spent
    (`used_credits`, in minor units) is the field that always carries meaning.
    Credits already spent still show up after the account turns extra usage off
    (or gets it capped), flagged with `off`.
    """
    used = extra.get("used_credits")
    if used is None:
        return ""
    enabled = bool(extra.get("is_enabled"))
    if not enabled and not used:
        return ""
    places = int(extra.get("decimal_places") or 0)
    currency = str(extra.get("currency") or "")
    text = f"Extra usage: {money(float(used), currency, places)}"
    limit = extra.get("monthly_limit")
    if limit:
        percent = extra.get("utilization")
        if percent is None:
            percent = float(used) / float(limit) * 100
        text += f" / {money(float(limit), currency, places)} ({float(percent):.0f}%)"
    return text if enabled else f"{text} · off"


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
    raise RuntimeError("Codex did not respond in time")


def codex_bin() -> str:
    if os.name == "nt":
        # Some npm versions install only the extensionless POSIX shim and
        # codex.ps1 in the global bin directory. Prefer a regular executable
        # from PATH, then the native binary shipped by @openai/codex.
        found = shutil.which("codex.exe")
        if found:
            return found
        npm_dir = Path(os.environ.get("APPDATA", Path.home() / "AppData" / "Roaming")) / "npm"
        cmd = npm_dir / "codex.cmd"
        if cmd.is_file():
            return str(cmd)
        openai = npm_dir / "node_modules" / "@openai"
        package_roots = [openai / "codex" / "node_modules" / "@openai", openai]
        for root in package_roots:
            candidates = sorted(root.glob("codex-win32-*/vendor/**/bin/codex.exe"), reverse=True)
            if candidates:
                return str(candidates[0])
        return "codex.exe"

    # On WSL the nvm PATH is not loaded in non-interactive shells, and the
    # Windows PATH offers an npm shim from /mnt/c that cannot run there —
    # only accept `which` if it points at a native Linux binary.
    found = shutil.which("codex")
    if found and not found.startswith("/mnt/"):
        return found
    candidates = sorted(Path.home().glob(".nvm/versions/node/*/bin/codex"))
    return str(candidates[-1]) if candidates else (found or "codex")


def codex_command(binary: str) -> list[str]:
    # On Windows `which` resolves to the npm shim `codex.CMD`, which
    # CreateProcess cannot execute directly — it has to go through cmd.exe.
    if os.name == "nt" and binary.lower().endswith((".cmd", ".bat")):
        return ["cmd.exe", "/C", binary, "app-server", "--stdio"]
    return [binary, "app-server", "--stdio"]


def codex_live() -> dict[str, Any]:
    binary = codex_bin()
    env = os.environ.copy()
    # codex is a Node script: make sure the node from the same dir is on PATH.
    env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH', '')}"
    proc = subprocess.Popen(
        codex_command(binary),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
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
            raise RuntimeError(f"{exc} (init phase, exit={proc.poll()})") from None
        proc.stdin.write('{"method":"initialized"}\n')
        proc.stdin.write('{"method":"account/rateLimits/read","id":2}\n')
        proc.stdin.flush()
        try:
            return rpc_read(proc, messages, 2, 15)
        except RuntimeError as exc:
            raise RuntimeError(f"{exc} (rateLimits phase, exit={proc.poll()})") from None
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
        raise RuntimeError("no limits were found in the local sessions")
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
                log_path = Path(tempfile.gettempdir()) / "ai-usage-codex.log"
                with log_path.open("a", encoding="utf-8") as log:
                    log.write(f"{dt.datetime.now():%H:%M:%S} codex_live failed: {live_error}\n")
            except OSError:
                pass
        raw = data.get("rateLimits") or {}
        plan = raw.get("planType", raw.get("plan_type", ""))
        result = Provider("Codex", "ChatGPT", str(plan).replace("_", " ").title(), codex_email())
        if live_error:
            result.details.append(f"⚠ local cache · app-server: {live_error}")
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
            result.details.append(f"Credits: {credits.get('balance', '?')}")
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
        return Provider("Cursor", "Business", "Team", error="set it up with: ai-usage cursor-cookie or cursor-admin")
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
            used, total, included = cursor_plan_usage(plan)
            percent = used * 100 / total if total else None

            result.meters.append(
                Meter(
                    "Usage",
                    percent,
                    data.get("billingCycleEnd"),
                    cursor_money(used),
                    cursor_money(total),
                )
            )
            if used - included > 0:
                result.details.append(f"Extra usage: {cursor_money(used - included)}")
            auto_percent = plan.get("autoPercentUsed")
            if auto_percent:
                result.meters.append(Meter("Auto usage", float(auto_percent), data.get("billingCycleEnd")))
            demand = (data.get("individualUsage") or {}).get("onDemand") or {}
            if demand.get("enabled"):
                result.details.append(f"On demand: ${float(demand.get('used', 0)) / 100:.2f}")
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
            raise RuntimeError("user not found in the team response")
        reset = next_month(int(data["subscriptionCycleStart"])) if data.get("subscriptionCycleStart") else None
        result = Provider("Cursor", "Business", "Team", email)
        for key_name, label in (("totalPercentUsed", "Total usage"), ("autoPercentUsed", "Auto")):
            if member.get(key_name) is not None:
                result.meters.append(Meter(label, float(member[key_name]), reset))
        spent = float(member.get("spendCents", 0)) / 100
        limit = member.get("monthlyLimitDollars") or member.get("hardLimitOverrideDollars")
        result.details.append(f"Spend: ${spent:.2f}" + (f" / ${float(limit):.2f}" if limit else ""))
        return result
    except Exception as exc:
        return Provider("Cursor", "Business", error=str(exc))


def grok_home() -> Path:
    override = os.environ.get("GROK_HOME")
    return Path(override) if override else Path.home() / ".grok"


def grok_auth_path() -> Path:
    return grok_home() / "auth.json"


def grok_session(data: dict[str, Any]) -> tuple[str, dict[str, Any]]:
    """The most recently expiring Grok CLI session in auth.json."""
    best: tuple[str, dict[str, Any], float] | None = None
    for key, account in data.items():
        if not isinstance(account, dict) or not account.get("key"):
            continue
        expires = grok_expires_epoch(account) or 0.0
        if best is None or expires >= best[2]:
            best = (key, account, expires)
    if best is None:
        raise RuntimeError("no local session found")
    return best[0], best[1]


def grok_expires_epoch(account: dict[str, Any]) -> float | None:
    text = account.get("expires_at")
    if not text:
        return None
    try:
        return dt.datetime.fromisoformat(str(text).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def grok_numeric(value: Any) -> float:
    if isinstance(value, dict):
        value = value.get("val")
    try:
        return float(value or 0)
    except (TypeError, ValueError):
        return 0.0


def refresh_grok(path: Path, data: dict[str, Any], session_key: str, *, force: bool = False) -> dict[str, Any]:
    """Renew the Grok CLI access token when it is close to expiry, writing it back.

    The file belongs to the Grok CLI; persisting the rotated pair is required
    because OIDC refresh tokens are single-use — keeping the old one would
    lock the CLI out of the next refresh.
    """
    account = data[session_key]
    expires = grok_expires_epoch(account)
    if not force and expires is not None and expires > time.time() + GROK_REFRESH_BUFFER:
        return data
    refresh_token = account.get("refresh_token")
    client_id = account.get("oidc_client_id")
    if not refresh_token or not client_id:
        if expires is not None and expires <= time.time():
            raise RuntimeError("Grok session expired; run grok login")
        return data
    response = request_form(
        GROK_TOKEN_URL,
        {
            "grant_type": "refresh_token",
            "refresh_token": str(refresh_token),
            "client_id": str(client_id),
        },
        headers={"User-Agent": APP},
    )
    access = response.get("access_token")
    if not access:
        raise RuntimeError("Grok token refresh returned no access_token")
    account["key"] = access
    if response.get("refresh_token"):
        account["refresh_token"] = response["refresh_token"]
    expires_in = int(response.get("expires_in") or 28_800)
    account["expires_at"] = (
        dt.datetime.now(dt.timezone.utc) + dt.timedelta(seconds=expires_in)
    ).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    data[session_key] = account
    write_private_json(path, data)
    return data


def grok_from_billing(config: dict[str, Any], email: str, plan: str) -> Provider:
    result = Provider("Grok", "SuperGrok", plan, email)
    period = config.get("currentPeriod") or {}
    period_type = str(period.get("type") or "")
    if "WEEKLY" in period_type.upper():
        label = "Weekly"
    elif "MONTHLY" in period_type.upper():
        label = "Monthly"
    else:
        label = "Credits"

    percent = config.get("creditUsagePercent")
    if percent is None:
        for item in config.get("productUsage") or []:
            if str(item.get("product") or "") == "GrokBuild" and item.get("usagePercent") is not None:
                percent = item["usagePercent"]
                break
    if percent is None:
        cap = grok_numeric(config.get("onDemandCap"))
        used = grok_numeric(config.get("onDemandUsed"))
        if cap:
            percent = used / cap * 100
        elif period:
            percent = 0.0

    reset = period.get("end") or config.get("billingPeriodEnd")
    if percent is not None:
        result.meters.append(Meter(label, float(percent), reset))

    on_used = grok_numeric(config.get("onDemandUsed"))
    on_cap = grok_numeric(config.get("onDemandCap"))
    if on_used:
        result.details.append(f"On demand: {on_used:g}" + (f" / {on_cap:g}" if on_cap else ""))
    prepaid = grok_numeric(config.get("prepaidBalance"))
    if prepaid:
        result.details.append(f"Prepaid: {prepaid:g}")
    return result


def collect_grok() -> Provider:
    path = grok_auth_path()
    if not path.exists():
        return Provider("Grok", "SuperGrok", error="no local session found")
    try:
        data = read_json(path)
        session_key, _ = grok_session(data)
        data = refresh_grok(path, data, session_key)
        account = data[session_key]
        token = str(account["key"])
        email = str(account.get("email") or "")
        headers = {
            "Authorization": f"Bearer {token}",
            "Accept": "application/json",
            "x-xai-token-auth": "xai-grok-cli",
            "User-Agent": APP,
        }
        try:
            billing = request_json(GROK_BILLING_URL, headers=headers)
        except RuntimeError as exc:
            if "HTTP 401" not in str(exc):
                raise
            data = refresh_grok(path, data, session_key, force=True)
            headers["Authorization"] = f"Bearer {data[session_key]['key']}"
            billing = request_json(GROK_BILLING_URL, headers=headers)
        plan = "SuperGrok"
        try:
            settings = request_json(GROK_SETTINGS_URL, headers=headers)
            plan = str(settings.get("subscription_tier_display") or plan)
        except Exception:
            pass
        return grok_from_billing(billing.get("config") or billing, email, plan)
    except Exception as exc:
        return Provider("Grok", "SuperGrok", error=str(exc))


def session_active(provider: Provider) -> bool:
    """Whether the account has a live 5h session window — i.e. it burned quota
    recently enough that the window has not rolled over yet."""
    meter = next((item for item in provider.meters if item.label == "Session"), None)
    if meter is None or not meter.percent:
        return False
    if not meter.reset_at:
        return True
    try:
        moment = dt.datetime.fromisoformat(meter.reset_at.replace("Z", "+00:00"))
    except ValueError:
        return True
    return moment > dt.datetime.now(dt.timezone.utc)


def wsl_claude_configs() -> list[tuple[Path, Path]]:
    """The CLI configs inside WSL, seen from Windows.

    Only *running* distros are listed: reaching into `\\\\wsl.localhost\\<name>`
    of a stopped one would boot its VM on every refresh. `wsl.exe --list` reads
    the registry, boots nothing, and answers in ~200 ms.
    """
    try:
        proc = subprocess.run(
            ["wsl.exe", "--list", "--running", "--quiet"],
            capture_output=True,
            timeout=5,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
    except Exception:
        return []
    # wsl.exe writes UTF-16LE.
    names = [line.strip() for line in proc.stdout.decode("utf-16-le", "ignore").splitlines()]
    paths: list[Path] = []
    for name in filter(None, names):
        root = Path(f"\\\\wsl.localhost\\{name}")
        paths.append(default_claude_source(root / "root"))
        paths.extend(custom_dir_claude_configs(root / "root"))
        try:
            for home in sorted((root / "home").iterdir()):
                paths.append(default_claude_source(home))
                paths.extend(custom_dir_claude_configs(home))
        except OSError:
            pass
    return paths


def windows_claude_configs() -> list[tuple[Path, Path]]:
    """The CLI configs on the Windows profiles, seen from WSL through /mnt."""
    if not Path("/mnt").is_dir():
        return []
    try:
        paths = []
        for home in sorted(Path("/mnt").glob("*/Users/*")):
            paths.append(default_claude_source(home))
            paths.extend(custom_dir_claude_configs(home))
        return paths
    except OSError:
        return []


def default_claude_source(home: Path) -> tuple[Path, Path]:
    """The default layout: `.claude.json` next to the `~/.claude` dir."""
    return (home / ".claude.json", home / ".claude" / "history.jsonl")


def custom_dir_claude_configs(base: Path) -> list[tuple[Path, Path]]:
    """(config, history) inside each `.claude*` dir under `base`.

    A CLI running with a custom `CLAUDE_CONFIG_DIR` keeps both files *inside*
    that dir (the default layout keeps them next to `~/.claude` / in it) — the
    `~/.claude*` naming is the discoverable convention for those setups.
    """
    try:
        return sorted(
            (p / ".claude.json", p / "history.jsonl") for p in base.glob(".claude*") if p.is_dir()
        )
    except OSError:
        return []


def claude_config_sources() -> list[tuple[Path, Path]]:
    """Every (`.claude.json`, `history.jsonl`) pair the Claude Code CLI may have
    written on this machine, on both sides of WSL — the answer has to be the
    same from Windows or Linux; macOS has no second side. The config carries
    the account, the history carries liveness (see `claude_active_emails`)."""
    sources = []
    override = os.environ.get("CLAUDE_CONFIG_DIR")
    if override:
        sources.append((Path(override) / ".claude.json", Path(override) / "history.jsonl"))
    sources.append(default_claude_source(Path.home()))
    sources.extend(custom_dir_claude_configs(Path.home()))
    if os.name == "nt":
        sources.extend(wsl_claude_configs())
    elif sys.platform.startswith("linux"):
        sources.extend(windows_claude_configs())
    return list(dict.fromkeys(sources))


def claude_active_emails() -> set[str]:
    """Accounts the CLI is logged into right now, lowercased.

    Each environment's freshness is the newest of its `.claude.json` (rewritten
    on session events) and its `history.jsonl` (appended on every prompt) — the
    config alone goes untouched during a long-running session, which would make
    an actively working account look idle. The freshest environment names the
    session in use; anything within `CLAUDE_CONFIG_TIE_SECONDS` of it counts
    too: with a CLI working on each side, both accounts really are burning
    quota, and without the tolerance the badge would bounce between them.
    An environment untouched for longer than a session window says nothing
    about what is running now — it is dropped, and the caller falls back to
    the meters.
    """
    stale = time.time() - CLAUDE_SESSION_SECONDS
    found: list[tuple[float, str]] = []
    for config, history in claude_config_sources():
        try:
            stamp = config.stat().st_mtime
        except OSError:
            continue
        try:
            stamp = max(stamp, history.stat().st_mtime)
        except OSError:
            pass
        if stamp < stale:
            continue
        try:
            email = str((read_json(config).get("oauthAccount") or {}).get("emailAddress") or "")
        except Exception:
            continue
        if email:
            found.append((stamp, email.lower()))
    if not found:
        return set()
    newest = max(stamp for stamp, _ in found)
    return {email for stamp, email in found if stamp >= newest - CLAUDE_CONFIG_TIE_SECONDS}


def external_active_claude_emails() -> set[str]:
    """An optional active-account hint supplied by an external tool.

    The app does not require or create this file. Standard Claude Code setups
    keep using `claude_active_emails`.
    """
    try:
        hint = read_json(CLAUDE_ACTIVE_ACCOUNT_FILE)
        updated_at = int(hint.get("updated_at") or 0)
        email = str(hint.get("email") or "").strip().lower()
    except (OSError, ValueError, TypeError, json.JSONDecodeError):
        return set()
    if not email or updated_at <= 0 or time.time() - updated_at > CLAUDE_SESSION_SECONDS:
        return set()
    return {email}


def mark_standby(results: list[Provider]) -> None:
    """Flag the Claude accounts that are not the one in use.

    An optional external activity hint takes precedence when present. This lets
    launchers or routers report the upstream account without making them a
    dependency of the app. Without a fresh hint, use the most recently touched
    `.claude.json` across environments.
    When no `.claude.json` names a known account (usage driven from claude.ai or
    the desktop app), fall back to the session window: an account whose 5h
    window already rolled over cannot be the one burning quota. If nothing
    distinguishes the accounts, nothing is flagged.
    """
    claude = [item for item in results if item.name == "Claude" and item.error is None]
    if len(claude) < 2:
        return
    active_emails = external_active_claude_emails() or claude_active_emails()
    in_use = {item.account for item in claude if item.email.lower() in active_emails}
    if not in_use:
        in_use = {item.account for item in claude if session_active(item)}
    if not in_use or len(in_use) == len(claude):
        return
    for item in claude:
        item.standby = item.account not in in_use


def collect_all() -> list[Provider]:
    claude_profiles = sorted(path.parent for path in CLAUDE_DIR.glob("*/.credentials.json")) if CLAUDE_DIR.exists() else []
    jobs = [("claude", profile) for profile in claude_profiles] + [
        ("codex", None),
        ("cursor", None),
        ("grok", None),
    ]

    def run(job: tuple[str, Path | None]) -> Provider:
        kind, path = job
        if kind == "claude":
            assert path is not None
            return collect_claude(path)
        if kind == "codex":
            return collect_codex()
        if kind == "cursor":
            return collect_cursor()
        return collect_grok()

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
    return f"renews {local:%b %d %H:%M} ({remaining})" if remaining else value


WINDOWS_POWERSHELL = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"


def notify_windows(title: str, body: str) -> None:
    """Native Windows toast via WinRT; no-op outside WSL."""
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


def notify_macos(title: str, body: str) -> None:
    """Native macOS notification via osascript."""
    # json.dumps produces valid AppleScript string literals (double quotes,
    # backslash escapes), keeping the values inert inside the script.
    # ensure_ascii=False: AppleScript does not understand \uXXXX escapes,
    # and the meter separator is a literal "·".
    def quote(value: str) -> str:
        return json.dumps(value, ensure_ascii=False)

    script = f"display notification {quote(body)} with title {quote(title)}"
    try:
        subprocess.Popen(
            ["osascript", "-e", script],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        pass


def notify(title: str, body: str) -> None:
    if sys.platform == "darwin":
        notify_macos(title, body)
    else:
        notify_windows(title, body)


def alert_meters(
    results: list[Provider],
    thresholds: list[int],
    fired: dict[tuple[str, str], int],
) -> None:
    """Notify once as a meter rises through each configured threshold.

    `fired` holds the highest threshold already announced per meter (a
    high-water mark). A level re-arms only after usage falls `ALERT_REARM_MARGIN`
    points below it, so a meter parked on a boundary is not announced on every
    refresh; a genuine window renewal drops usage far enough to re-arm every
    level. reset_at is kept out of the key because some providers jitter it
    between fetches.
    """
    if not thresholds:
        return
    for provider in results:
        for meter in provider.meters:
            if meter.percent is None:
                continue
            key = (provider.label(), meter.label)
            mark = fired.get(key, 0)
            # Re-arm: forget any announced level the meter has now dropped
            # clearly below (window renewed, or usage genuinely fell).
            if mark and meter.percent < mark - ALERT_REARM_MARGIN:
                mark = max((t for t in thresholds if meter.percent >= t), default=0)
                fired[key] = mark
            level = max((t for t in thresholds if meter.percent >= t), default=0)
            if level <= mark:
                continue
            fired[key] = level
            remaining = reset_remaining(meter.reset_at)
            body = f"{meter.label} at {meter.percent:.0f}%" + (f" · renews in {remaining}" if remaining else "")
            notify(f"{provider.name} · {provider.label()}", body)


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
            "grok": 180 if rich else curses.COLOR_YELLOW,
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
        return {name: 0 for name in ("cyan", "claude", "openai", "cursor", "grok", "amber", "green", "red", "muted", "dim", "text")}


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


PROVIDER_COLOR = {"Claude": "claude", "Codex": "openai", "Cursor": "cursor", "Grok": "grok"}


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
        reset = reset_remaining(meter.reset_at) or "idle"
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
            reset = reset_remaining(meter.reset_at) or "idle"
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


def tui(screen: Any, interval: int, thresholds: list[int]) -> None:
    curses.curs_set(0)
    screen.timeout(150)
    colors = tui_colors()

    lock = threading.Lock()
    state: dict[str, Any] = {"results": [], "updated_at": None, "fetching": True}
    wake_event = threading.Event()
    stop_event = threading.Event()
    fired: dict[tuple[str, str], int] = {}

    def fetch_loop() -> None:
        while not stop_event.is_set():
            with lock:
                state["fetching"] = True
            results = collect_all()
            with lock:
                state["results"] = results
                state["updated_at"] = dt.datetime.now()
                state["fetching"] = False
            alert_meters(results, thresholds, fired)
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
            # Repaint from scratch after a resize: drop the curses diff and
            # clear the artifacts left on screen.
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
            status = f"{SPINNER_FRAMES[frame % len(SPINNER_FRAMES)]} refreshing"
            status_attr = colors["amber"]
        else:
            status = f"● {updated_at:%H:%M:%S}" if updated_at else "○ loading"
            status_attr = colors["green"]
        tui_add(screen, 1, max(2, width - len(status) - 3), status, len(status), status_attr)
        info = f"{len(results)} accounts · auto {interval}s" if width < 60 else f"{len(results)} subscriptions · auto-refresh every {interval}s"
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

        footer = "r refresh · q quit"
        tui_add(screen, height - 2, 2, footer, width - 4, colors["muted"])
        if force_full:
            force_full = False
            screen.redrawwin()
        # Synchronized output (DEC 2026): the terminal composes the frame
        # atomically, with no tearing mid-redraw.
        sys.stdout.write("\x1b[?2026h")
        sys.stdout.flush()
        screen.refresh()
        sys.stdout.write("\x1b[?2026l")
        sys.stdout.flush()
        frame += 1
        if handle_key(screen.getch()):
            return


def cursor_cookie() -> None:
    value = getpass.getpass("WorkosCursorSessionToken (hidden input): ").strip()
    value = value.removeprefix("WorkosCursorSessionToken=")
    if not value:
        raise RuntimeError("empty cookie")
    # Pasting into a hidden prompt fails silently on some consoles (the value
    # arrives truncated). Without this check the error would only surface later,
    # as an opaque HTTP 400 during collection.
    if len(value) < 100 or not ("%3A%3A" in value or "::" in value):
        raise RuntimeError(
            "the cookie does not look like a complete WorkosCursorSessionToken "
            f"(got {len(value)} characters); check whether the paste worked"
        )
    write_private_json(CURSOR_CONFIG, {"method": "dashboard_cookie", "session_cookie": value})
    print(f"Cursor session saved to {CURSOR_CONFIG}")


def cursor_admin(email: str) -> None:
    key = getpass.getpass("Cursor Admin API Key (hidden input): ").strip()
    if not key.startswith("key_"):
        raise RuntimeError("the key must start with key_")
    write_private_json(CURSOR_CONFIG, {"method": "admin_key", "admin_key": key, "email": email})
    print(f"Cursor admin key saved to {CURSOR_CONFIG}")


def doctor() -> None:
    ensure_config_file()
    print(f"Config: {CONFIG_DIR}")
    print(f"Claude: {len(list(CLAUDE_DIR.glob('*/.credentials.json'))) if CLAUDE_DIR.exists() else 0} profile(s)")
    print(f"Codex CLI: {'ok' if shutil.which('codex') else 'not found'}")
    print(f"Cursor: {'configured' if CURSOR_CONFIG.exists() else 'not configured'}")
    print(f"Grok: {'logged in' if grok_auth_path().exists() else 'no local session'}")
    print(f"Alerts: {', '.join(f'{t}%' for t in load_alert_thresholds())} ({CONFIG_FILE.name})")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    sub = result.add_subparsers(dest="command")
    watch = sub.add_parser("watch", help="open the TUI")
    watch.add_argument("--interval", type=int, default=60)
    watch.add_argument(
        "--alert",
        type=int,
        default=None,
        help="notify only from N%% up, overriding config.json (0 disables notifications)",
    )
    once = sub.add_parser("once", help="print a single reading and exit")
    once.add_argument("--json", action="store_true")
    add = sub.add_parser("claude-add", help="capture the currently active Claude session")
    add.add_argument("name")
    add.add_argument("--source", type=Path, default=Path.home() / ".claude" / ".credentials.json")
    login = sub.add_parser("claude-login", help="authenticate a Claude account without changing the default session")
    login.add_argument("name")
    login.add_argument("--email")
    sub.add_parser("claude-list", help="list Claude profiles")
    sub.add_parser("cursor-cookie", help="register the Cursor dashboard cookie")
    admin = sub.add_parser("cursor-admin", help="register a Cursor Admin API Key")
    admin.add_argument("--email", required=True)
    sub.add_parser("doctor", help="check the configuration")
    return result


def main() -> int:
    # The Windows console defaults to a legacy code page, and the bars, badges
    # and currency symbols are not in it — force UTF-8 so `once` prints the same
    # everywhere instead of dying with an encoding error.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]
        except (AttributeError, OSError):
            pass
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
        else:
            if curses is None:
                raise RuntimeError("the TUI needs the curses module, missing in this Python; use 'once' or the widget")
            ensure_config_file()
            # --alert overrides the config file: a value pins a single threshold,
            # 0 turns notifications off. Without it, use the configured levels.
            alert = getattr(args, "alert", None)
            thresholds = load_alert_thresholds() if alert is None else ([alert] if alert > 0 else [])
            curses.wrapper(tui, getattr(args, "interval", 60), thresholds)
        return 0
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
