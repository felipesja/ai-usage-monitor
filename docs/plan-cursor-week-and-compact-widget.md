# Plan — Cursor weekly pace meter + widget vertical compaction

Execution plan written to be followed literally. Two independent parts; do Part 1 fully,
then Part 2. Every snippet below is ready to paste. Repo rule (`CLAUDE.md`): all comments,
user-visible strings and docs in **English**.

Files touched:

| File | Part |
| --- | --- |
| `cli/usage_monitor.py` | 1 |
| `widget/src-tauri/src/collector/cursor.rs` | 1 |
| `CLAUDE.md` | 1 |
| `widget/src/index.html` | 2 |
| `widget/src/main.js` | 2 |
| `widget/src-tauri/src/main.rs` | 2 |

---

## Background (already verified against the live account — do not re-probe)

### Problem 1 — Cursor has no pacing signal

Cursor's quota is monthly and the single `Usage` bar saturates: the account sits at
`$80.20 / $80.20 · 100%` on day 17 of a 31-day cycle, so the bar says nothing about whether
the burn rate is sustainable. Claude already shows a weekly window in the same panel; Cursor
gets an equivalent pace gauge — the monthly quota divided into the weeks of the cycle, with
the bar reading "am I ahead of or behind the sustainable rate".

### Problem 2 — the panel is too tall

With 2 Claude accounts + Codex + Cursor + Grok the provider list overflows the 560px window
and scrolls. Each provider costs ~109px, much of it chrome (a dedicated email line, a 7px
bar on its own row with margins, an 18px gap between providers) plus a 20px static subtitle
row. Adding a weekly Cursor meter makes this worse, so both land together.

### The endpoint (probed live, works)

`POST https://cursor.com/api/dashboard/get-filtered-usage-events`

- **Headers**: the existing `Cookie: WorkosCursorSessionToken=<token>` +
  `Content-Type: application/json` **plus `Origin: https://cursor.com`**. Without `Origin`
  it is a hard `403 {"error":"Invalid origin for state-changing request"}`; `Referer` alone
  does not satisfy it. The app's own `ai-usage-monitor` User-Agent is accepted (no browser
  UA needed).
- **Body**: `{"startDate": "<epoch_ms>", "endDate": "<epoch_ms>", "page": 1, "pageSize": 1000}`.
  The dates are **strings** holding epoch milliseconds (that is the shape that was probed).
  `pageSize` is capped at 1000 server-side (400 above that).
- **Response**:

  ```json
  {
    "totalUsageEventsCount": 380,
    "usageEventsDisplay": [
      {
        "timestamp": "1786634126000",
        "kind": "USAGE_EVENT_KIND_FREE_CREDIT",
        "tokenUsage": { "totalCents": 13.27 },
        "owningUser": "..."
      }
    ]
  }
  ```

  `timestamp` is a **string** of epoch ms; `tokenUsage.totalCents` is a float.
- **Scope**: user-only (all 555 events over 30 days carried the same `owningUser`).
- **Latency**: 0.4s for a 7-day window, 0.8s for a full 17-day cycle (380 events).
- `usage-summary` itself carries no weekly field — it only has `billingCycleStart` /
  `billingCycleEnd` (ISO-8601 strings on the cookie path), so the events endpoint is the
  only retroactive source.

### The metric (cycle-aligned weeks)

Weeks are aligned to the billing cycle so the weekly budget divides the monthly quota
exactly.

1. `week_index = floor((now - cycle_start) / 7d)`
2. window = `[cycle_start + 7d*week_index, min(that + 7d, cycle_end))`
3. `weekly_budget_cents = plan_total_cents * window_days / cycle_days` — a plain 7/31 share;
   the trailing partial window gets a proportionally smaller budget instead of a full one.
4. `window_cost` = sum of `tokenUsage.totalCents` over events inside the window.
5. Convert raw cost into plan units with the cycle's own exchange rate
   `rate = plan_used_cents / cycle_total_event_cents`, then `weekly_used = window_cost * rate`.
   **Why**: the endpoint reports raw model cost (~$253.93 this cycle) while the plan meter
   counts quota dollars ($80.20), so the raw sum cannot be compared to the quota directly.
   A single cycle-wide rate is an approximation — the two event kinds
   (`USAGE_EVENT_KIND_FREE_CREDIT`, `USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS`) draw on
   different pools at different effective rates. It is the right trade for a pace gauge; a
   per-kind mapping would have to infer the pool behind each `kind` string and would break
   on any kind we have not seen. **Say this in a comment at the conversion, in both
   languages.**
6. `percent = weekly_used / weekly_budget * 100`, **not clamped** — >100% is the whole point
   (current data: ~$33.55 used against a ~$18.11 weekly budget → ~185%, i.e. burning at
   1.85x the sustainable rate).
7. `reset_at` = end of the current window (ISO-8601), so the existing countdown column works
   unchanged.

### Non-negotiable constraints

- Failure is **non-fatal and silent**: if the events call fails, omit the `Week` meter and
  keep the monthly `Usage` bar. No log, no `details` line, no `error` on the provider. This
  is an undocumented endpoint.
- Only the `dashboard_cookie` path gets the meter. The `admin_key` path is untouched.
- Python and Rust must emit the **identical** meter (`CLAUDE.md`: two implementations of one
  contract) — same label `Week`, same money formatting, same `reset_at`.
- The meter participates in alerts automatically via the existing high-water-mark latch in
  `alert_meters` / `checkAlerts`. **No alert code changes.**
- No new dependency in either language.

---

# PART 1 — Cursor weekly pace meter

## 1.1 `cli/usage_monitor.py`

### Step 1.1.1 — constants

Find the constants block near the top (it ends around the `GROK_REFRESH_BUFFER` line, ~line
60). Append after `GROK_REFRESH_BUFFER = 120`:

```python
CURSOR_EVENTS_URL = "https://cursor.com/api/dashboard/get-filtered-usage-events"
# Cursor caps pageSize at 1000 (400 above that); 5 pages covers any real cycle.
CURSOR_EVENTS_PAGE_SIZE = 1000
CURSOR_EVENTS_MAX_PAGES = 5
WEEK_SECONDS = 7 * 24 * 3600
```

### Step 1.1.2 — helpers

Insert these **immediately after `cursor_plan_usage`** (which ends with
`return min(used, total), total, included`, ~line 369) and before `extra_usage_detail`:

```python
def cursor_epoch(value: Any) -> float | None:
    """`billingCycleStart`/`End` as epoch seconds. The dashboard payload uses
    ISO-8601 strings; accept a raw epoch-ms number too, defensively."""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        return float(value) / 1000
    try:
        return dt.datetime.fromisoformat(str(value).replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def cursor_events(token: str, start: float, end: float) -> list[tuple[float, float]]:
    """(epoch seconds, raw cost in cents) for every usage event in the window.

    Undocumented dashboard endpoint. It rejects the request without an `Origin`
    header (403 "Invalid origin for state-changing request") — `Referer` alone
    does not satisfy it."""
    headers = {
        "Cookie": f"WorkosCursorSessionToken={token}",
        "User-Agent": APP,
        "Origin": "https://cursor.com",
    }
    events: list[tuple[float, float]] = []
    page = 1
    while page <= CURSOR_EVENTS_MAX_PAGES:
        payload = request_json(
            CURSOR_EVENTS_URL,
            method="POST",
            headers=headers,
            body={
                "startDate": str(int(start * 1000)),
                "endDate": str(int(end * 1000)),
                "page": page,
                "pageSize": CURSOR_EVENTS_PAGE_SIZE,
            },
        )
        for item in payload.get("usageEventsDisplay") or []:
            try:
                moment = float(item.get("timestamp")) / 1000
                cost = float((item.get("tokenUsage") or {}).get("totalCents") or 0)
            except (TypeError, ValueError):
                continue
            events.append((moment, cost))
        total = int(payload.get("totalUsageEventsCount") or 0)
        if page * CURSOR_EVENTS_PAGE_SIZE >= total:
            break
        page += 1
    return events


def cursor_week_window(cycle_start: float, cycle_end: float, now: float) -> tuple[float, float]:
    """The cycle-aligned week `now` falls in. Aligning to the cycle keeps the
    weekly budget an exact share of the monthly quota; the trailing week is
    short and gets a proportionally smaller budget."""
    index = max(0, int((now - cycle_start) // WEEK_SECONDS))
    start = cycle_start + WEEK_SECONDS * index
    return start, min(start + WEEK_SECONDS, cycle_end)


def cursor_weekly_meter(
    cycle_start: float,
    cycle_end: float,
    now: float,
    plan_used: float,
    plan_total: float,
    events: list[tuple[float, float]],
) -> Meter | None:
    """Pace gauge: this cycle-week's spend against its share of the monthly
    quota. The percent is deliberately not clamped — over 100% is the signal
    that the burn rate is above what the cycle can sustain. Pure function over
    its inputs, so the arithmetic stays out of the HTTP path."""
    if cycle_end <= cycle_start or plan_total <= 0:
        return None
    start, end = cursor_week_window(cycle_start, cycle_end, now)
    if end <= start:
        return None
    budget = plan_total * (end - start) / (cycle_end - cycle_start)
    if budget <= 0:
        return None
    cycle_cost = sum(cost for _, cost in events)
    window_cost = sum(cost for moment, cost in events if start <= moment < end)
    # The events endpoint reports raw model cost while the plan meter counts
    # quota dollars, so the raw sum cannot be compared to the quota directly:
    # rescale it with the cycle's own exchange rate. One rate for every event
    # kind is an approximation — included and bonus credits draw on different
    # pools at different effective rates — but it is the right trade for a pace
    # gauge; a per-kind mapping would have to infer the pool behind each `kind`
    # string and would break on any kind we have not seen.
    rate = plan_used / cycle_cost if cycle_cost > 0 else 0.0
    used = window_cost * rate
    reset_at = dt.datetime.fromtimestamp(end, dt.timezone.utc).isoformat()
    return Meter("Week", used * 100 / budget, reset_at, cursor_money(used), cursor_money(budget))
```

### Step 1.1.3 — wire it into `collect_cursor`

In `collect_cursor()` (~line 586), in the `dashboard_cookie` branch, **right after** the
block that appends the `Usage` meter:

```python
            result.meters.append(
                Meter(
                    "Usage",
                    percent,
                    data.get("billingCycleEnd"),
                    cursor_money(used),
                    cursor_money(total),
                )
            )
```

insert:

```python
            # Pace gauge over the cycle-aligned week. Best effort: the events
            # endpoint is undocumented, so a failure just drops the meter and
            # leaves the monthly bar alone.
            try:
                cycle_start = cursor_epoch(data.get("billingCycleStart"))
                cycle_end = cursor_epoch(data.get("billingCycleEnd"))
                if cycle_start is not None and cycle_end is not None:
                    week = cursor_weekly_meter(
                        cycle_start,
                        cycle_end,
                        time.time(),
                        used,
                        total,
                        cursor_events(token, cycle_start, cycle_end),
                    )
                    if week is not None:
                        result.meters.append(week)
            except Exception:
                pass
```

Leave the `Extra usage` detail, the `Auto usage` meter and the `On demand` detail exactly
where they are (the `Week` meter lands between `Usage` and `Auto usage` — that is intended).

**Do not touch** `plain()`, `tui()`, `draw_compact_tui()` or `alert_meters()` — they iterate
meters generically and pick the new one up for free.

## 1.2 `widget/src-tauri/src/collector/cursor.rs`

### Step 1.2.1 — imports and constants

Change the import line

```rust
use super::date::next_month_iso;
```

to

```rust
use super::date::{epoch_to_iso, iso_to_epoch, next_month_iso};
```

and add, right after the `use super::{Meter, Provider};` line:

```rust
const EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
/// Cursor caps pageSize at 1000 (400 above that); 5 pages covers any real cycle.
const EVENTS_PAGE_SIZE: i64 = 1000;
const EVENTS_MAX_PAGES: i64 = 5;
const WEEK_SECONDS: i64 = 7 * 24 * 3600;
```

### Step 1.2.2 — wire it into `by_cookie`

In `by_cookie`, right after

```rust
    provider.meters.push(usage_meter);
```

insert:

```rust
    // Pace gauge over the cycle-aligned week. Best effort: the events endpoint
    // is undocumented, so a failure just drops the meter and leaves the monthly
    // bar alone.
    if let (Some(cycle_start), Some(cycle_end)) = (
        data.get("billingCycleStart").and_then(Value::as_str).and_then(iso_to_epoch),
        billing_end.as_deref().and_then(iso_to_epoch),
    ) {
        if let Ok(events) = fetch_events(&client, &cookie, cycle_start, cycle_end) {
            if let Some(week) =
                weekly_meter(cycle_start, cycle_end, now_epoch(), used, total, &events)
            {
                provider.meters.push(week);
            }
        }
    }
```

`billing_end` is already an `Option<String>` in scope (cloned for the `Usage` meter), so
`billing_end.as_deref()` is valid here — keep the existing
`Meter::new("Auto usage", Some(auto), billing_end)` below untouched (it consumes the value).

### Step 1.2.3 — new functions

Add these **above** the `#[cfg(test)] mod tests` block:

```rust
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

/// (epoch seconds, raw cost in cents) for every usage event in the window.
/// Undocumented dashboard endpoint: it rejects the request without an `Origin`
/// header (403 "Invalid origin for state-changing request") — `Referer` alone
/// does not satisfy it.
fn fetch_events(
    client: &reqwest::blocking::Client,
    cookie: &str,
    start: i64,
    end: i64,
) -> Result<Vec<(i64, f64)>, String> {
    let mut events: Vec<(i64, f64)> = Vec::new();
    let mut page: i64 = 1;
    while page <= EVENTS_MAX_PAGES {
        let payload: Value = client
            .post(EVENTS_URL)
            .header("Cookie", cookie)
            .header("User-Agent", "ai-usage-monitor")
            .header("Origin", "https://cursor.com")
            .json(&json!({
                "startDate": (start * 1000).to_string(),
                "endDate": (end * 1000).to_string(),
                "page": page,
                "pageSize": EVENTS_PAGE_SIZE,
            }))
            .send()
            .map_err(|err| err.to_string())?
            .error_for_status()
            .map_err(|err| err.to_string())?
            .json()
            .map_err(|err| err.to_string())?;

        for item in payload
            .get("usageEventsDisplay")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            // `timestamp` is epoch ms inside a string; `totalCents` is a float.
            let moment = item
                .get("timestamp")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|text| text.parse::<i64>().ok())
                        .or_else(|| value.as_f64().map(|number| number as i64))
                })
                .map(|ms| ms / 1000);
            let cost = item
                .get("tokenUsage")
                .and_then(|usage| usage.get("totalCents"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if let Some(moment) = moment {
                events.push((moment, cost));
            }
        }

        let total = payload
            .get("totalUsageEventsCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if page * EVENTS_PAGE_SIZE >= total {
            break;
        }
        page += 1;
    }
    Ok(events)
}

/// The cycle-aligned week `now` falls in. Aligning to the cycle keeps the
/// weekly budget an exact share of the monthly quota; the trailing week is
/// short and gets a proportionally smaller budget.
fn week_window(cycle_start: i64, cycle_end: i64, now: i64) -> (i64, i64) {
    let index = (now - cycle_start).max(0) / WEEK_SECONDS;
    let start = cycle_start + WEEK_SECONDS * index;
    (start, (start + WEEK_SECONDS).min(cycle_end))
}

/// Pace gauge: this cycle-week's spend against its share of the monthly quota.
/// The percent is deliberately not clamped — over 100% is the signal that the
/// burn rate is above what the cycle can sustain. Pure over its inputs, so the
/// arithmetic stays out of the HTTP path and can be unit-tested.
fn weekly_meter(
    cycle_start: i64,
    cycle_end: i64,
    now: i64,
    plan_used: f64,
    plan_total: f64,
    events: &[(i64, f64)],
) -> Option<Meter> {
    if cycle_end <= cycle_start || plan_total <= 0.0 {
        return None;
    }
    let (start, end) = week_window(cycle_start, cycle_end, now);
    if end <= start {
        return None;
    }
    let budget = plan_total * (end - start) as f64 / (cycle_end - cycle_start) as f64;
    if budget <= 0.0 {
        return None;
    }
    let cycle_cost: f64 = events.iter().map(|(_, cost)| cost).sum();
    let window_cost: f64 = events
        .iter()
        .filter(|(moment, _)| *moment >= start && *moment < end)
        .map(|(_, cost)| cost)
        .sum();
    // The events endpoint reports raw model cost while the plan meter counts
    // quota dollars, so the raw sum cannot be compared to the quota directly:
    // rescale it with the cycle's own exchange rate. One rate for every event
    // kind is an approximation — included and bonus credits draw on different
    // pools at different effective rates — but it is the right trade for a pace
    // gauge; a per-kind mapping would have to infer the pool behind each `kind`
    // string and would break on any kind we have not seen.
    let rate = if cycle_cost > 0.0 { plan_used / cycle_cost } else { 0.0 };
    let used = window_cost * rate;
    let mut meter = Meter::new("Week", Some(used * 100.0 / budget), Some(epoch_to_iso(end as f64)));
    meter.used = Some(display_money(used));
    meter.limit = Some(display_money(budget));
    Some(meter)
}
```

### Step 1.2.4 — unit tests

Extend the existing `mod tests` — change its import line to

```rust
    use super::{display_money, plan_usage, week_window, weekly_meter};
```

and append these tests inside the same module:

```rust
    // 2026-08-01T00:00:00Z .. 2026-09-01T00:00:00Z (31 days).
    const CYCLE_START: i64 = 1_785_542_400;
    const CYCLE_END: i64 = CYCLE_START + 31 * 86_400;

    #[test]
    fn aligns_weeks_to_the_billing_cycle() {
        // Day 0 → the first week.
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, CYCLE_START),
            (CYCLE_START, CYCLE_START + 7 * 86_400)
        );
        // Day 17 → the third week (index 2).
        let day17 = CYCLE_START + 17 * 86_400;
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, day17),
            (CYCLE_START + 14 * 86_400, CYCLE_START + 21 * 86_400)
        );
        // The trailing week is clipped to the cycle end (31 = 4*7 + 3).
        let day29 = CYCLE_START + 29 * 86_400;
        assert_eq!(
            week_window(CYCLE_START, CYCLE_END, day29),
            (CYCLE_START + 28 * 86_400, CYCLE_END)
        );
    }

    #[test]
    fn scales_the_weekly_budget_by_the_window_length() {
        // A full week of a 31-day cycle: 7/31 of the quota, nothing spent.
        let meter = weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START, 0.0, 3_100.0, &[]).unwrap();
        assert_eq!(meter.limit.as_deref(), Some("$7"));
        assert_eq!(meter.percent, Some(0.0));
        // The trailing 3-day window gets 3/31, not a full week.
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 29 * 86_400, 0.0, 3_100.0, &[]).unwrap();
        assert_eq!(meter.limit.as_deref(), Some("$3"));
    }

    #[test]
    fn converts_raw_event_cost_into_plan_units() {
        // Cycle cost 400 raw cents mapped onto 100 spent plan cents → rate 0.25.
        // Only the two events inside week 0 count: (100 + 60) * 0.25 = 40 cents
        // against a 7/31 * 3100 = 700 cent budget → 5.71%.
        let events = [
            (CYCLE_START + 3_600, 100.0),
            (CYCLE_START + 2 * 86_400, 60.0),
            (CYCLE_START + 10 * 86_400, 240.0),
        ];
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 86_400, 100.0, 3_100.0, &events)
                .unwrap();
        assert_eq!(meter.used.as_deref(), Some("$0.40"));
        assert_eq!(meter.limit.as_deref(), Some("$7"));
        assert!(
            (meter.percent.unwrap() - 5.714).abs() < 0.01,
            "percent = {:?}",
            meter.percent
        );
    }

    #[test]
    fn does_not_clamp_a_burn_rate_above_the_sustainable_pace() {
        // Everything spent in week 0: 1400 raw cents at rate 1.0 against a 700
        // cent budget is 200%, and must stay 200%.
        let events = [(CYCLE_START + 3_600, 1_400.0)];
        let meter =
            weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START + 86_400, 1_400.0, 3_100.0, &events)
                .unwrap();
        assert!(
            (meter.percent.unwrap() - 200.0).abs() < 0.01,
            "percent = {:?}",
            meter.percent
        );
    }

    #[test]
    fn refuses_a_degenerate_cycle() {
        assert!(weekly_meter(CYCLE_END, CYCLE_START, CYCLE_START, 10.0, 100.0, &[]).is_none());
        assert!(weekly_meter(CYCLE_START, CYCLE_END, CYCLE_START, 10.0, 0.0, &[]).is_none());
    }
```

If a constant above turns out slightly off when the tests run (e.g. `CYCLE_START` is not
exactly 2026-08-01T00:00:00Z), **fix only the epoch constant, never the assertions' intent**.
What is under test: window alignment, budget scaling, rate conversion, and the absence of
clamping.

## 1.3 `CLAUDE.md`

Extend the **Cursor** bullet under "Python collector". Keep the existing text about the two
methods, `cursor_plan_usage` and `Extra usage:`, and append:

> The cookie path also fetches `dashboard/get-filtered-usage-events` for the current billing
> cycle and derives a `Week` pace meter: cycle-aligned week
> (`week_index = floor((now - cycle_start) / 7d)`), budget = the quota's share of that window
> (`plan_total * window_days / cycle_days`), used = the window's raw `tokenUsage.totalCents`
> rescaled by the cycle's own exchange rate `plan_used / cycle_total_event_cents` — the
> endpoint reports raw model cost while the plan meter counts quota dollars. The percent is
> **not** clamped: above 100% means burning faster than the cycle sustains. The endpoint
> requires an `Origin: https://cursor.com` header (403 otherwise) and is undocumented, so a
> failure silently drops the meter and leaves the monthly bar intact. `admin_key` accounts
> get no `Week` meter.

Mirror the same sentence in the `cursor.rs` mention under the widget's "Native collector"
bullet if it reads as out of date.

## 1.4 Part 1 verification

1. `python3 cli/usage_monitor.py once --json` — the Cursor provider carries both `Usage` and
   `Week`; `Week.used`/`limit` are dollar strings and `reset_at` is within 7 days of
   `billingCycleStart + 7d*k`. Cross-check `Week.used` against a manual sum of the window's
   `tokenUsage.totalCents` scaled by `plan_used / cycle_total`.
2. `python3 cli/usage_monitor.py watch` — the new meter renders in both TUI layouts (resize
   below 96 columns to hit `draw_compact_tui`).
3. `cd widget/src-tauri && cargo test` — the new tests pass alongside the existing
   `plan_usage` ones.
4. `cd widget && cargo run --manifest-path src-tauri/Cargo.toml -- --probe` — the Rust JSON
   matches the Python JSON for Cursor (same `Week` percent within rounding).
5. Break the endpoint on purpose (temporarily send a bad `Origin`) and confirm the monthly
   `Usage` bar still renders, with `Week` simply absent and no `error` on the provider.

---

# PART 2 — Widget vertical compaction

Goal: roughly 35% shorter per provider, plus a window that auto-fits its content. Purely
presentational — no collector changes.

## 2.1 `widget/src/index.html`

### Step 2.1.1 — delete the subtitle row, move the count into the header

Replace:

```html
  <header data-tauri-drag-region>
    <span class="brand">◆ AI USAGE</span>
    <span class="status" id="status"><span class="icon">○</span><span class="text">loading</span></span>
  </header>
  <div class="subtitle-row">
    <span class="subtitle" id="subtitle"></span>
    <span class="autorefresh" id="autorefresh">auto-refresh every 60s</span>
  </div>
```

with:

```html
  <header data-tauri-drag-region>
    <span class="brand">◆ AI USAGE</span>
    <span class="subtitle" id="subtitle"></span>
    <span class="status" id="status" title="auto-refresh every 60s"><span class="icon">○</span><span class="text">loading</span></span>
  </header>
```

The `#subtitle` id survives — `doRefresh()` keeps writing the count to it in its new
position. `#autorefresh` disappears as an element and becomes the `title` tooltip on
`#status`.

### Step 2.1.2 — CSS edits

In the `<style>` block:

| Current | New |
| --- | --- |
| `header { … padding-bottom: 2px; }` | `header { … padding-bottom: 6px; }` |
| `.subtitle-row { … }` | **delete the rule** |
| `/* Right-aligned … */` + `.autorefresh { … }` | **delete the rule and its comment** |
| `.provider { margin-bottom: 18px; }` | `.provider { margin-bottom: 9px; padding-bottom: 9px; border-bottom: 1px solid rgba(255, 255, 255, 0.06); }` |
| — | add `.provider:last-child { margin-bottom: 0; padding-bottom: 0; border-bottom: none; }` |
| `.provider-head { … margin-bottom: 5px; }` | `.provider-head { … margin-bottom: 3px; }` |
| `.identity { flex: 1; min-width: 0; }` | **delete the rule** |
| `.name-row { display: flex; align-items: baseline; gap: 8px; }` | `.name-row { display: flex; align-items: baseline; gap: 8px; flex: 1; min-width: 0; }` |
| `.email { … margin-top: 1px; overflow: hidden; … }` | `.email { color: var(--muted); font-size: 10.5px; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }` |
| `.meter { … gap: 3px 8px; … padding: 1px 0; }` | `.meter { display: grid; grid-template-columns: 1fr auto auto; gap: 0 8px; align-items: center; padding: 0; }` |
| `.meter + .meter { margin-top: 6px; }` | `.meter + .meter { margin-top: 4px; }` |
| `.meter .bar { … height: 7px; … margin-top: 1px; }` | same rule with `height: 4px;` and `margin-top: 2px;` |

Keep `.subtitle { color: var(--muted); font-size: 11px; }` — it is now a header element.

## 2.2 `widget/src/main.js`

### Step 2.2.1 — one-line identity in `render()`

Replace this block (~lines 113-134):

```js
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
```

with:

```js
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
```

`.plan` is still appended to `head` afterwards and stays right-aligned via
`margin-left: auto`.

### Step 2.2.2 — auto-fit helper

Add near the other top-level helpers, before `doRefresh`:

```js
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
```

`accountsOpen()` is declared later in the file with `function`, so hoisting makes this fine.

### Step 2.2.3 — call it

- In `doRefresh()`, after the `subtitle` line (last statement of the `try`), add
  `requestAnimationFrame(fitWindow);` — one frame so layout has settled.
- In `toggleAccounts()`, at the end of the function (after the `if (show) … else …`), add
  `requestAnimationFrame(fitWindow);`.
- In `renderAccounts()`, at the very end, add `requestAnimationFrame(fitWindow);` — the
  accounts view rebuilds itself after add/remove and must re-fit.

### Step 2.2.4 — the dead line at the bottom

Replace:

```js
document.getElementById("autorefresh").textContent = `auto-refresh every ${INTERVAL_MS / 1000}s`;
```

with:

```js
document.getElementById("status").title = `auto-refresh every ${INTERVAL_MS / 1000}s`;
```

(keeps the tooltip in sync with the real interval instead of the hardcoded HTML text).

## 2.3 `widget/src-tauri/src/main.rs`

### Step 2.3.1 — the command

Add next to the other `#[tauri::command]` functions (e.g. after `hide_to_tray`):

```rust
/// Grows/shrinks the window to fit its content, keeping the bottom edge where
/// it is so the panel does not appear to jump. Deliberately not `apply_position`:
/// a window the user dragged elsewhere must not snap back to the corner on
/// every refresh.
#[tauri::command]
fn resize_to_content(app: AppHandle, height: f64) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("no main window")?;
    let scale = window.scale_factor().map_err(|err| err.to_string())?;
    // Matches `minHeight` in tauri.conf.json — change both together.
    let min = 320.0;
    let max = match window.current_monitor().map_err(|err| err.to_string())? {
        Some(monitor) => {
            (monitor.work_area().size.height as f64 / monitor.scale_factor()) - 2.0 * MARGIN as f64
        }
        None => height,
    };
    let target = height.clamp(min, max.max(min));

    let before = window.outer_size().map_err(|err| err.to_string())?;
    let position = window.outer_position().map_err(|err| err.to_string())?;
    window
        .set_size(tauri::LogicalSize::new(before.width as f64 / scale, target))
        .map_err(|err| err.to_string())?;
    let after = window.outer_size().map_err(|err| err.to_string())?;
    // Keep the bottom edge fixed (physical px; x untouched).
    let y = position.y + before.height as i32 - after.height as i32;
    window
        .set_position(tauri::PhysicalPosition::new(position.x, y))
        .map_err(|err| err.to_string())?;
    Ok(())
}
```

### Step 2.3.2 — register it

Add `resize_to_content,` to the `tauri::generate_handler![…]` list (after `hide_to_tray,`).

## 2.4 Part 2 verification

1. `cd widget && npm run dev` — the panel is visibly shorter and shows no scrollbar with all
   providers configured.
2. The window shrinks/grows to the content and stays anchored at the corner it was shown at;
   dragging it elsewhere and refreshing must not snap it back.
3. `a` opens the accounts view and the window resizes to it; `esc` restores the provider list
   and its height.
4. Temporarily remove an account and confirm the window shrinks rather than leaving empty
   space.
5. `cd widget/src-tauri && cargo build` — no warnings from the new command.

---

## Definition of done

- [ ] Python emits `Week` for cookie-based Cursor accounts, never for `admin_key`.
- [ ] Rust emits an identical `Week` meter (label, money strings, `reset_at` shape).
- [ ] `cargo test` green, including the new window/budget tests.
- [ ] A failing events endpoint leaves the provider with only its old meters and no error.
- [ ] The widget panel fits all configured providers without scrolling at the default size.
- [ ] `CLAUDE.md`'s Cursor bullet documents the `Week` meter, the `Origin` requirement and
      the raw-cost → plan-unit conversion.
