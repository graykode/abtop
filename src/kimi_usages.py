#!/usr/bin/env python3
"""Fetch Kimi Code account quota for abtop's local cache.

# abtop-kimi-usages-v2

Installed explicitly by `abtop --setup`. The abtop process only reads the
resulting JSON file; this companion owns the optional network and OAuth work.
"""

import json
import os
import stat
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime
from pathlib import Path

CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098"
TOKEN_URL = "https://auth.kimi.com/api/oauth/token"
USAGES_URL = "https://api.kimi.com/coding/v1/usages"
TIMEOUT_SECONDS = 8
MAX_RESPONSE_BYTES = 1024 * 1024


def kimi_home():
    explicit = os.environ.get("KIMI_CODE_HOME", "").strip()
    if explicit:
        return Path(explicit).expanduser()

    home = Path.home()
    for candidate in (home / ".kimi-code", home / ".kimi"):
        if (candidate / "credentials" / "kimi-code.json").is_file():
            return candidate
    return home / ".kimi-code"


def read_json(path):
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else None
    except (OSError, ValueError):
        return None


def read_response(response):
    payload = response.read(MAX_RESPONSE_BYTES + 1)
    if len(payload) > MAX_RESPONSE_BYTES:
        raise RuntimeError("response exceeded 1 MiB")
    value = json.loads(payload.decode("utf-8"))
    if not isinstance(value, dict):
        raise RuntimeError("response was not a JSON object")
    return value


def atomic_write_json(path, value, mode=None):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    temporary_path = Path(temporary)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(value, output, separators=(",", ":"))
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        if mode is not None:
            os.chmod(temporary_path, mode)
        os.replace(temporary_path, path)
    finally:
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass


def credentials_expired(credentials):
    try:
        expires_at = float(credentials.get("expires_at"))
    except (TypeError, ValueError):
        return False
    return expires_at > 0 and expires_at <= time.time() + 30


def refresh_credentials(credentials, path):
    refresh_token = str(credentials.get("refresh_token") or "")
    if not refresh_token:
        raise RuntimeError("OAuth credentials expired; run `kimi login`")

    body = urllib.parse.urlencode(
        {
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }
    ).encode()
    request = urllib.request.Request(
        TOKEN_URL,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/x-www-form-urlencoded",
            "X-Msh-Platform": "kimi_cli",
        },
    )
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        refreshed = read_response(response)

    access_token = str(refreshed.get("access_token") or "")
    if not access_token:
        raise RuntimeError("OAuth refresh did not return an access token")

    try:
        expires_in = max(1.0, float(refreshed.get("expires_in") or 900))
    except (TypeError, ValueError):
        expires_in = 900.0

    next_credentials = dict(credentials)
    next_credentials.update(refreshed)
    next_credentials["access_token"] = access_token
    next_credentials["refresh_token"] = str(
        refreshed.get("refresh_token") or refresh_token
    )
    next_credentials["expires_at"] = time.time() + expires_in

    try:
        existing_mode = stat.S_IMODE(path.stat().st_mode)
    except OSError:
        existing_mode = 0o600
    atomic_write_json(path, next_credentials, existing_mode)
    return next_credentials


def fetch_usages(access_token):
    request = urllib.request.Request(
        USAGES_URL,
        headers={
            "Authorization": "Bearer " + access_token,
            "Accept": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
        return read_response(response)


def number(value):
    if value is None or value == "":
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def reset_timestamp(value):
    if not isinstance(value, str) or not value:
        return None
    try:
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
    except (TypeError, ValueError):
        return None


def window_from_usage(value, window_minutes=None):
    if not isinstance(value, dict):
        return None
    limit = number(value.get("limit"))
    if limit is None or limit <= 0:
        return None
    used = number(value.get("used"))
    if used is None:
        remaining = number(value.get("remaining"))
        if remaining is None:
            return None
        used = limit - remaining

    window = {
        "used_percentage": max(0.0, min(100.0, used / limit * 100.0)),
        "resets_at": reset_timestamp(
            value.get("resetTime") or value.get("reset_at") or value.get("resetAt")
        )
        or 0,
    }
    if window_minutes is not None:
        window["window_minutes"] = window_minutes
    return window


def window_minutes(value):
    if not isinstance(value, dict) or not isinstance(value.get("window"), dict):
        return None
    duration = number(value["window"].get("duration"))
    if duration is None or duration <= 0:
        return None
    unit = str(value["window"].get("timeUnit") or "TIME_UNIT_MINUTE")
    duration = int(duration)
    if unit in ("TIME_UNIT_SECOND", "SECOND", "seconds"):
        return max(1, (duration + 59) // 60)
    if unit in ("TIME_UNIT_HOUR", "HOUR", "hours"):
        return duration * 60
    if unit in ("TIME_UNIT_DAY", "DAY", "days"):
        return duration * 24 * 60
    return duration


def normalize_usages(body):
    short = None
    limits = body.get("limits")
    if isinstance(limits, list) and limits:
        entry = limits[0] if isinstance(limits[0], dict) else None
        if entry is not None:
            detail = entry.get("detail")
            if not isinstance(detail, dict):
                detail = entry
            short = window_from_usage(detail, window_minutes(entry) or 300)

    long = window_from_usage(body.get("usage"))
    if short is None and long is None:
        return None

    result = {"source": "kimi", "updated_at": int(time.time())}
    if short is not None:
        result["five_hour"] = short
    if long is not None:
        result["seven_day"] = long
    return result


def main():
    home = kimi_home()
    credentials_path = home / "credentials" / "kimi-code.json"
    output_path = home / "abtop-rate-limits.json"
    credentials = read_json(credentials_path)
    if not credentials or not str(credentials.get("access_token") or ""):
        print("abtop-usages: Kimi is not logged in; run `kimi login`", file=sys.stderr)
        return 1

    try:
        if credentials_expired(credentials):
            credentials = refresh_credentials(credentials, credentials_path)
        try:
            body = fetch_usages(str(credentials["access_token"]))
        except urllib.error.HTTPError as error:
            if error.code != 401:
                raise
            credentials = refresh_credentials(credentials, credentials_path)
            body = fetch_usages(str(credentials["access_token"]))

        result = normalize_usages(body)
        if result is None:
            raise RuntimeError("usage response contained no supported quota windows")
        atomic_write_json(output_path, result, 0o600)
        return 0
    except Exception as error:
        print(f"abtop-usages: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
