#!/usr/bin/env python3
"""Show local Codex account-pool readiness and live account usage."""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import math
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Mapping
from zoneinfo import ZoneInfo


def mapping(value: Any) -> Mapping[str, Any]:
    return value if isinstance(value, Mapping) else {}


def safe_text(value: Any, default: str = "-", limit: int = 120) -> str:
    if value is None:
        return default
    text = str(value).strip()
    if not text:
        return default
    text = "".join(character for character in text if ord(character) >= 32)
    if not text:
        return default
    return text[:limit]


def number(value: Any) -> float | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        result = float(value)
    except (TypeError, ValueError):
        return None
    return result if math.isfinite(result) else None


def integer(value: Any) -> int | None:
    result = number(value)
    return None if result is None else int(result)


def parse_datetime(value: Any) -> datetime | None:
    if value is None or value == "":
        return None
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        try:
            timestamp = float(value)
            if abs(timestamp) >= 100_000_000_000:
                timestamp /= 1000.0
            return datetime.fromtimestamp(timestamp, UTC)
        except (OverflowError, OSError, ValueError):
            return None
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    try:
        if text.replace(".", "", 1).lstrip("+-").isdigit():
            return parse_datetime(float(text))
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except (OverflowError, ValueError):
        return None
    return parsed.replace(tzinfo=UTC) if parsed.tzinfo is None else parsed.astimezone(UTC)


def display_datetime(value: Any, timezone: ZoneInfo, *, with_time: bool = True) -> str:
    parsed = parse_datetime(value)
    if parsed is None:
        return "-"
    local = parsed.astimezone(timezone)
    return local.strftime("%m-%d %H:%M" if with_time else "%Y-%m-%d")


def format_percent(value: float | None) -> str:
    if value is None:
        return "-"
    return str(int(value)) if value.is_integer() else f"{value:.1f}"


def format_tokens(value: Any) -> str:
    count = integer(value)
    if count is None:
        return "-"
    magnitude = abs(count)
    if magnitude >= 1_000_000_000:
        return f"{count / 1_000_000_000:.2f}B"
    if magnitude >= 1_000_000:
        return f"{count / 1_000_000:.2f}M"
    if magnitude >= 1_000:
        return f"{count / 1_000:.2f}K"
    return f"{count:,}"


def duration_label(value: Any) -> str:
    duration = integer(value)
    if duration == 300:
        return "5h"
    if duration == 10_080:
        return "7d"
    if duration is None:
        return "window"
    if duration % 60 == 0:
        return f"{duration // 60}h"
    return f"{duration}m"


def local_auth(auth_path: Path) -> dict[str, Any]:
    result: dict[str, Any] = {
        "status": "missing",
        "auth_mode": "-",
        "last_refresh": "-",
        "plan": "-",
        "account_id": None,
        "user_id": None,
        "email": None,
    }
    if not auth_path.is_file():
        return result
    try:
        raw = auth_path.read_bytes()
        data = json.loads(raw)
    except (OSError, UnicodeDecodeError):
        result["status"] = "unreadable"
        return result
    except json.JSONDecodeError:
        result["status"] = "invalid-json"
        return result
    if not isinstance(data, Mapping):
        result["status"] = "invalid-json"
        return result
    auth_mode = str(data.get("auth_mode") or "unknown")
    result["auth_mode"] = auth_mode
    result["last_refresh"] = safe_text(data.get("last_refresh"), "-")[:10]
    tokens = mapping(data.get("tokens"))
    result["account_id"] = tokens.get("account_id") or None
    # Token claims provide a local display hint; a live response takes priority.
    # Never expose the token itself or use these claims to authorize requests.
    id_token = tokens.get("id_token")
    if isinstance(id_token, str):
        parts = id_token.split(".")
        if len(parts) == 3:
            try:
                claims = mapping(json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4))))
                account = mapping(claims.get("https://api.openai.com/auth"))
                result["plan"] = safe_text(account.get("chatgpt_plan_type"), "-")
                result["account_id"] = result["account_id"] or account.get("chatgpt_account_id") or None
                result["user_id"] = account.get("chatgpt_user_id") or account.get("user_id") or None
                result["email"] = claims.get("email") or mapping(claims.get("https://api.openai.com/profile")).get("email")
            except (ValueError, UnicodeDecodeError, binascii.Error):
                pass
    if auth_mode == "chatgpt":
        ready = bool(tokens.get("access_token") and tokens.get("refresh_token"))
    elif auth_mode in {"api_key", "apikey"}:
        ready = bool(data.get("OPENAI_API_KEY"))
    else:
        ready = bool(data)
    result["status"] = "ready" if ready else "incomplete"
    return result


def query_account(home: Path, profile_name: str, binary: Path, timeout: float) -> dict[str, Any]:
    socket_path = home / "app-server-control/app-server-control.sock"
    if not socket_path.is_socket():
        raise RuntimeError("Background server unavailable; open an account session, or use list --local.")
    environment = os.environ.copy()
    environment.update({"CODEX_HOME": str(home), "CODEX_SQLITE_HOME": str(home), "RUST_LOG": "error"})
    # This CLI command only exchanges RPC with the shared daemon. It never loads
    # credentials, so list calls share the daemon's per-profile refresh locks.
    for inherited_name in (
        "CODEX_AUTH_PROFILE", "CODEX_ACCOUNT_PROFILE", "CODEX_ACCESS_TOKEN",
        "CODEX_API_KEY", "OPENAI_API_KEY",
    ):
        environment.pop(inherited_name, None)
    command = [
        str(binary), "app-server", "daemon", "account-status", profile_name,
        "--timeout-ms", str(max(1, int(timeout * 1000))),
    ]
    try:
        result = subprocess.run(
            command, env=environment, capture_output=True, text=True, timeout=timeout + 2,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError("Background server query timed out") from exc
    if result.returncode:
        raise RuntimeError(safe_text(result.stderr, "Background server query failed", 180))
    try:
        payload = json.loads(result.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RuntimeError("Invalid background server query response") from exc
    if not isinstance(payload, dict) or not isinstance(payload.get("limits"), Mapping):
        raise RuntimeError("Missing quota data in background server query response")
    return payload


def normalize_live(payload: Mapping[str, Any]) -> dict[str, Any]:
    by_id = payload.get("rateLimitsByLimitId", payload.get("rate_limits_by_limit_id"))
    buckets: list[tuple[str, Mapping[str, Any]]] = []
    if isinstance(by_id, Mapping) and by_id:
        for raw_id, raw_bucket in by_id.items():
            if isinstance(raw_bucket, Mapping):
                buckets.append((str(raw_id), raw_bucket))
    else:
        historical = payload.get("rateLimits", payload.get("rate_limits"))
        if isinstance(historical, Mapping):
            buckets.append((str(historical.get("limitId") or "codex"), historical))

    windows: list[dict[str, Any]] = []
    plan = "-"
    credit: dict[str, Any] | None = None
    for raw_id, bucket in buckets:
        bucket_id = safe_text(bucket.get("limitId") or raw_id, raw_id, 50)
        if bucket_id.casefold() == "bengalfox":
            continue
        bucket_plan = safe_text(bucket.get("planType"), "-")
        if plan == "-" and bucket_plan != "-":
            plan = bucket_plan
        if credit is None and isinstance(bucket.get("credits"), Mapping):
            credits = bucket["credits"]
            balance = credits.get("balance")
            if credits.get("unlimited"):
                value = "∞"
            elif balance is not None:
                value = safe_text(balance)
            elif credits.get("hasCredits"):
                value = "available"
            else:
                value = "0"
            credit = {
                "value": value,
                "has_credits": bool(credits.get("hasCredits")),
                "unlimited": bool(credits.get("unlimited")),
                "balance": None if balance is None else safe_text(balance),
            }
        for kind in ("primary", "secondary"):
            window = bucket.get(kind)
            if not isinstance(window, Mapping):
                continue
            used = number(window.get("usedPercent", window.get("used_percent")))
            if used is None:
                continue
            windows.append(
                {
                    "limit_id": bucket_id,
                    "kind": kind,
                    "duration_minutes": integer(
                        window.get("windowDurationMins", window.get("window_duration_mins"))
                    ),
                    "used_percent": used,
                    "remaining_percent": max(0.0, min(100.0, 100.0 - used)),
                    "resets_at": window.get("resetsAt", window.get("resets_at")),
                }
            )
        individual = bucket.get("individualLimit", bucket.get("individual_limit"))
        if isinstance(individual, Mapping):
            remaining = number(
                individual.get("remainingPercent", individual.get("remaining_percent"))
            )
            if remaining is not None:
                windows.append(
                    {
                        "limit_id": bucket_id,
                        "kind": "individual",
                        "duration_minutes": None,
                        "used_percent": max(0.0, min(100.0, 100.0 - remaining)),
                        "remaining_percent": max(0.0, min(100.0, remaining)),
                        "resets_at": individual.get("resetsAt", individual.get("resets_at")),
                    }
                )

    reset_summary = mapping(
        payload.get("rateLimitResetCredits", payload.get("rate_limit_reset_credits"))
    )
    reset_cards = integer(reset_summary.get("availableCount", reset_summary.get("available_count")))
    usage = {}
    return {
        "plan": plan,
        "windows": windows,
        "reset_cards": None if reset_cards is None else max(0, reset_cards),
        "credits": credit,
        "ordinary_usage_allowed": payload.get("ordinaryUsageAllowed"),
        "usage": usage,
    }


def normalize_usage(payload: Mapping[str, Any]) -> dict[str, Any]:
    summary = mapping(payload.get("summary"))
    buckets = payload.get("dailyUsageBuckets", payload.get("daily_usage_buckets"))
    latest = buckets[-1] if isinstance(buckets, list) and buckets else {}
    return {
        "lifetime_tokens": integer(summary.get("lifetimeTokens", summary.get("lifetime_tokens"))),
        "peak_daily_tokens": integer(summary.get("peakDailyTokens", summary.get("peak_daily_tokens"))),
        "current_streak_days": integer(summary.get("currentStreakDays", summary.get("current_streak_days"))),
        "longest_streak_days": integer(summary.get("longestStreakDays", summary.get("longest_streak_days"))),
        "latest_daily_usage": (
            {
                "start_date": safe_text(latest.get("startDate", latest.get("start_date"))),
                "tokens": integer(latest.get("tokens")),
            }
            if isinstance(latest, Mapping)
            else None
        ),
    }


def load_deadline_data() -> tuple[dict[str, Mapping[str, Any]], str]:
    config_path = Path(
        os.environ.get("DEADLINE_CONFIG", str(Path.home() / ".config/deadline/config.json"))
    ).expanduser()
    try:
        data = json.loads(config_path.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return {}, "America/Los_Angeles"
    if not isinstance(data, Mapping):
        return {}, "America/Los_Angeles"
    records: dict[str, Mapping[str, Any]] = {}
    for account in data.get("accounts", []):
        if not isinstance(account, Mapping) or account.get("provider") != "codex":
            continue
        collector = mapping(account.get("collector"))
        name = collector.get("auth_profile") or account.get("label")
        if name:
            records[str(name).casefold()] = account
    preferences = mapping(data.get("preferences"))
    timezone = safe_text(preferences.get("display_timezone"), "America/Los_Angeles")
    return records, timezone


def extract_recharge(value: Any) -> Mapping[str, Any] | None:
    if isinstance(value, list):
        candidates = [item for item in value if isinstance(item, Mapping)]
        if not candidates:
            return None
        candidates.sort(
            key=lambda item: parse_datetime(
                item.get("at", item.get("date", item.get("recharged_at", item.get("created_at"))))
            )
            or datetime.min.replace(tzinfo=UTC)
        )
        return candidates[-1]
    if not isinstance(value, Mapping):
        return None
    for key in ("last_recharge", "lastRecharge", "recharges", "history", "records"):
        if key in value:
            result = extract_recharge(value[key])
            if result is not None:
                return result
    if any(key in value for key in ("at", "date", "recharged_at", "created_at", "timestamp")):
        return value
    return None


def load_recharge_records(home: Path) -> tuple[dict[str, Mapping[str, Any]], list[str]]:
    candidates = [
        home / "account-recharge-history.json",
        home / "recharge-history.json",
        Path.home() / ".config/deadline/recharge-history.json",
        Path.home() / ".local/share/deadline/recharge-history.json",
    ]
    records: dict[str, Mapping[str, Any]] = {}
    found: list[str] = []
    for path in candidates:
        if not path.is_file():
            continue
        try:
            data = json.loads(path.read_text())
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        found.append(str(path))
        root = mapping(data)
        account_values = root.get("accounts", root.get("recharges", root.get("history", data)))
        if isinstance(account_values, list):
            for item in account_values:
                if not isinstance(item, Mapping):
                    continue
                name = item.get("account") or item.get("account_id") or item.get("label") or item.get("name")
                record = extract_recharge(item)
                if name and record:
                    records[str(name).casefold()] = record
        elif isinstance(account_values, Mapping):
            for name, value in account_values.items():
                record = extract_recharge(value)
                if record:
                    records[str(name).casefold()] = record
    return records, found


def format_recharge(record: Mapping[str, Any] | None, timezone: ZoneInfo) -> str:
    if record is None:
        return "-"
    moment = record.get("at", record.get("date", record.get("recharged_at", record.get("created_at", record.get("timestamp")))))
    date = display_datetime(moment, timezone, with_time=False)
    amount = record.get("amount", record.get("amount_usd", record.get("value")))
    currency = safe_text(record.get("currency"), "USD", 10)
    amount_text = ""
    if amount is not None:
        amount_text = f" {currency} {safe_text(amount, limit=24)}"
    source = safe_text(record.get("source"), "", 20)
    return safe_text(f"{date}{amount_text}{(' ' + source) if source else ''}", "-", 28)


def subscription_for(record: Mapping[str, Any] | None, timezone: ZoneInfo) -> tuple[str, dict[str, Any] | None]:
    subscription = mapping(record.get("subscription")) if record else {}
    if not subscription:
        return "-", None
    expires = subscription.get("expires_at", subscription.get("expiresAt"))
    renews = subscription.get("renews_at", subscription.get("renewsAt"))
    boundary = expires or renews
    if boundary is None:
        return "-", dict(subscription)
    text = display_datetime(boundary, timezone, with_time=False)
    return safe_text(text, "-", 28), dict(subscription)


def quota_text(windows: list[Mapping[str, Any]], timezone: ZoneInfo) -> str:
    if not windows:
        return "-"
    rows = []
    for window in windows:
        label = duration_label(window.get("duration_minutes"))
        if window.get("kind") == "individual":
            label = "individual"
        limit_id = safe_text(window.get("limit_id"), "codex", 24)
        if limit_id.casefold() != "codex":
            label = f"{limit_id}/{label}"
        remaining = format_percent(number(window.get("remaining_percent")))
        reset = display_datetime(window.get("resets_at"), timezone)
        rows.append(f"{label} {remaining}%@{reset}")
    return "; ".join(rows)


def cell(value: Any, width: int) -> str:
    text = safe_text(value)
    if len(text) > width:
        text = text[: max(1, width - 1)] + "…"
    return f"{text:<{width}}"


def build_rows_parallel(
    home: Path,
    binary: Path,
    timeout: float,
    local_only: bool,
    workers: int,
) -> tuple[list[dict[str, Any]], str, list[str]]:
    accounts_dir = home / "accounts"
    account_dirs = {
        path.name.casefold(): path
        for path in accounts_dir.iterdir()
        if path.is_dir()
    } if accounts_dir.is_dir() else {}
    deadline_accounts, timezone_name = load_deadline_data()
    # Include expected profiles even when their credential directory is missing.
    # The ledger is metadata only: credentials still come from the account pool.
    for key, record in deadline_accounts.items():
        if record.get("enabled") is False:
            continue
        collector = mapping(record.get("collector"))
        name = collector.get("auth_profile") or record.get("label")
        if (
            isinstance(name, str)
            and name not in {"", ".", ".."}
            and "/" not in name
            and "\\" not in name
            and name.isprintable()
        ):
            account_dirs.setdefault(key, accounts_dir / name)
    default_auth = local_auth(home / "auth.json")
    try:
        timezone = ZoneInfo(timezone_name)
    except (KeyError, ValueError):
        timezone_name = "UTC"
        timezone = ZoneInfo("UTC")
    recharge_records, recharge_sources = load_recharge_records(home)
    rows: list[dict[str, Any]] = []
    jobs: dict[Any, dict[str, Any]] = {}
    with ThreadPoolExecutor(max_workers=max(1, min(workers, len(account_dirs) or 1))) as executor:
        for account_dir in sorted(account_dirs.values(), key=lambda path: path.name.casefold()):
            auth_path = account_dir / "auth.json"
            auth = local_auth(auth_path)
            selected = bool(default_auth["account_id"]) and (
                auth["account_id"] == default_auth["account_id"]
                and (
                    auth["user_id"] == default_auth["user_id"]
                    if default_auth["user_id"]
                    else bool(default_auth["email"]) and str(auth["email"]).casefold() == str(default_auth["email"]).casefold()
                )
            )
            subscription_text, subscription = subscription_for(
                deadline_accounts.get(account_dir.name.casefold()), timezone
            )
            row: dict[str, Any] = {
                "active": selected,  # Compatibility: this flag identifies the default account.
                "default": selected,
                "account": account_dir.name,
                "status": "local" if local_only and auth["status"] == "ready" else auth["status"],
                "auth_mode": auth["auth_mode"],
                "auth_status": auth["status"],
                "last_refresh": auth["last_refresh"],
                "plan": auth["plan"],
                "quota": [],
                "reset_cards": None,
                "credits": None,
                "usage": {},
                "last_recharge": None,
                "subscription": subscription,
                "subscription_display": subscription_text,
                "error": None,
            }
            if auth["status"] == "missing":
                row["error"] = (
                    f"Credentials missing at {auth_path}; "
                    f"run codex-account-next {account_dir.name} login."
                )
            recharge = recharge_records.get(account_dir.name.casefold())
            row["last_recharge"] = dict(recharge) if recharge is not None else None
            rows.append(row)
            if not local_only and auth["status"] == "ready":
                jobs[executor.submit(query_account, home, account_dir.name, binary, timeout)] = row
        for future in as_completed(jobs):
            row = jobs[future]
            try:
                live = future.result()
                normalized = normalize_live(live["limits"])
                row["status"] = "live"
                if normalized["plan"] != "-":
                    row["plan"] = normalized["plan"]
                row["quota"] = normalized["windows"]
                row["reset_cards"] = normalized["reset_cards"]
                row["credits"] = normalized["credits"]
                row["usage"] = normalize_usage(live["usage"])
                if live.get("usage_error"):
                    row["error"] = live["usage_error"]
            except Exception as exc:  # one account must not hide the rest of the pool
                row["status"] = "error"
                row["error"] = safe_text(exc, "live query failed", 180)
    return rows, timezone_name, recharge_sources


def render_table(rows: list[dict[str, Any]], timezone_name: str, local_only: bool) -> None:
    print(f"Accounts: {len(rows)}; nearest quota reset first, free accounts last.")
    print("* = default account (sessions may use other accounts); QUOTA is remaining%@reset time")
    print(f"  {'ACCOUNT':<24} {'STATUS':<8} {'AUTH':<8} {'PLAN':<8} {'QUOTA':<34} {'RESET_CARD':<10} {'CREDIT':<10} {'SUBSCRIPTION':<28}")
    for row in rows:
        marker = "*" if row["default"] else " "
        credits = mapping(row.get("credits"))
        credit_text = credits.get("value", "-")
        reset_cards = row.get("reset_cards")
        print(
            f"{marker} {cell(row['account'], 24)} "
            f"{cell(row['status'], 8)} "
            f"{cell(row['auth_mode'], 8)} "
            f"{cell(row['plan'], 8)} "
            f"{cell(quota_text(row['quota'], ZoneInfo(timezone_name)), 34)} "
            f"{cell('-' if reset_cards is None else reset_cards, 10)} "
            f"{cell(credit_text, 10)} "
            f"{cell(row['subscription_display'], 28)}"
        )
    if not rows:
        print("No matching account profiles found.")
        return
    errors = [f"{row['account']}: {row['error']}" for row in rows if row.get("error")]
    if errors:
        print("\nAccount notes:")
        for error in errors:
            print(f"  - {error}")
    if local_only:
        print("\nLocal mode: use `codex-account-next list` for live quota and credit data.")
    else:
        print(f"\nLive source: shared daemon account/rateLimits/read + account/usage/read ({timezone_name}).")
    print("Subscription dates come from the local deadline renewal record and are separate from quota resets.")


def json_ready(rows: list[dict[str, Any]], timezone_name: str, recharge_sources: list[str], local_only: bool) -> dict[str, Any]:
    return {
        "generated_at": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "timezone": timezone_name,
        "live": not local_only,
        "recharge_sources": recharge_sources,
        "accounts": rows,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Show Codex account-pool status and live quota")
    parser.add_argument("--home", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--timeout", type=float, default=12.0, help="per-account app-server timeout in seconds")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--local", action="store_true", help="skip network calls and show local auth status")
    parser.add_argument("--json", action="store_true", dest="as_json")
    parser.add_argument("--include-free", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    home = args.home.expanduser()
    binary = args.binary.expanduser()
    if not home.is_dir():
        parser.error(f"Codex home does not exist: {home}")
    if not args.local and not binary.is_file():
        parser.error(f"Codex binary does not exist: {binary}")
    rows, timezone_name, recharge_sources = build_rows_parallel(
        home, binary, max(0.1, args.timeout), args.local, max(1, args.workers)
    )
    # Compare reset timestamps, not formatted dates or remaining quota. When
    # several windows are shown, use the earliest known reset for that account.
    rows.sort(
        key=lambda row: (
            str(row["plan"]).casefold() == "free",
            min(
                (
                    reset
                    for window in row["quota"]
                    if (reset := parse_datetime(window.get("resets_at"))) is not None
                ),
                default=datetime.max.replace(tzinfo=UTC),
            ),
            str(row["account"]).casefold(),
        )
    )
    if args.as_json:
        print(json.dumps(json_ready(rows, timezone_name, recharge_sources, args.local), ensure_ascii=False, indent=2))
    else:
        render_table(rows, timezone_name, args.local)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
