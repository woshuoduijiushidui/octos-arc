#!/usr/bin/env python3
"""Fetch a task's official Playwright specs into arc/public-tests/<task>/.

`public-tests/<task>/` is byte-identical to what the platform serves at
    GET /api/requirements/<task>/tests?catalog=competition
(verified 2026-09-30 against arc-bench-web--keep: 33/33 files identical), and it
is the same suite the grader runs. So this is the tool that turns a task the
platform does NOT mount into /workspace/tests into a task `main.locate_acceptance_tests`
can still find — via the bundle fallback (`bundle_dir/public-tests`, matched by
the requirement tree's root name).

Demo/competition tracks are anonymous. The `hackathon` competition is official and
returns 401 without a session, so a logged-in cookie jar (or email/password) is
required there. Nothing here bypasses that gate; it only reuses a session you own.

usage:
  python3 arc/fetch-tests.py hackathon--sheet hackathon--github
  python3 arc/fetch-tests.py --list
  python3 arc/fetch-tests.py --all
  python3 arc/fetch-tests.py arc-bench-web--keep --out /tmp/pt     # public smoke check

auth (first one that works):
  --cookie-jar PATH   Netscape cookie jar with the arcbench_session cookie;
                      defaults to $ARC_COOKIE_JAR then ~/.arc-cookies
  --email/--password  site login (NOT the model API key); also $ARC_EMAIL/$ARC_PASSWORD

Nothing is written unless the whole download succeeded, and every existing spec
dictates its own filename, so a re-run is idempotent. `manifest.json` gets one
entry per fetched task, mapping task id -> the requirement tree's root name
(which is the key `locate_acceptance_tests` matches on).
"""
from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ARC_DIR = Path(__file__).resolve().parent
BASE = "https://arc-bench.com/api"
DEFAULT_OUT = ARC_DIR / "public-tests"
TASKS_DIR = ARC_DIR / "tasks"
MANIFEST = "manifest.json"


# ------------------------------------------------------------------ http

def load_jar(path: str | None) -> http.cookiejar.CookieJar:
    """Parse a Netscape cookie jar by hand.

    MozillaCookieJar asserts on host-only cookies written by some exporters
    (#HttpOnly_ prefix, no leading dot), which is what browsers hand out — the
    same reason `scoreboard.load_jar` does not use it.
    """
    jar = http.cookiejar.CookieJar()
    if not path or not os.path.isfile(path):
        return jar
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            raw = line.rstrip("\n")
            if not raw or raw.startswith("#") and not raw.startswith("#HttpOnly_"):
                continue
            if raw.startswith("#HttpOnly_"):
                raw = raw[len("#HttpOnly_"):]
            parts = raw.split("\t")
            if len(parts) != 7:
                continue
            domain, _, path_, secure, expires, name, value = parts
            try:
                jar.set_cookie(http.cookiejar.Cookie(
                    0, name, value, None, False, domain, True, domain.startswith("."),
                    path_, True, secure == "TRUE", expires.isdigit() and int(expires) or None,
                    False, None, None, {}))
            except Exception:  # a single malformed line must not lose the session
                continue
    return jar


class Client:
    def __init__(self, jar: http.cookiejar.CookieJar | None = None) -> None:
        self.jar = jar or http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.jar))

    def get_json(self, path: str, **params) -> object:
        url = BASE + path
        if params:
            url += ("&" if "?" in url else "?") + urllib.parse.urlencode(params)
        with self.opener.open(url, timeout=60) as resp:
            return json.load(resp)

    def login(self, email: str, password: str) -> None:
        body = json.dumps({"email": email, "password": password}).encode()
        req = urllib.request.Request(BASE + "/auth/login", data=body,
                                     headers={"Content-Type": "application/json"}, method="POST")
        with self.opener.open(req, timeout=60) as resp:
            json.load(resp)  # login sets the session cookie on the jar
        if not any(c.name == "arcbench_session" for c in self.jar):
            raise SystemExit("login succeeded but no arcbench_session cookie was set")


def requirement_ids(client: Client, catalog: str) -> list[str]:
    return [r["id"] for r in client.get_json("/requirements", catalog=catalog)]


def root_name(task_id: str) -> str | None:
    """The requirement tree's root `name` — the manifest key the harness matches."""
    path = TASKS_DIR / task_id / "requirements.yaml"
    if not path.is_file():  # accept .yml too, like main.load_requirement_tree
        alt = path.with_suffix(".yml")
        if alt.is_file():
            path = alt
        else:
            return None
    try:
        import yaml
    except ImportError:
        # Do not add a hard dependency for a cosmetic manifest value: fall back
        # to a line scan so the tests themselves are still fetchable.
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.lstrip().startswith("name:"):
                return line.split(":", 1)[1].strip().strip("'\"") or None
        return None
    data = yaml.safe_load(path.read_text(encoding="utf-8"))
    if isinstance(data, dict):
        for wrapper in ("root", "requirement"):
            if isinstance(data.get(wrapper), dict):
                data = data[wrapper]
                break
        return str(data.get("name") or "").strip() or None
    return None


# ------------------------------------------------------------------ writing

def fetch_one(client: Client, task_id: str, catalog: str, out_dir: Path, dry_run: bool) -> tuple[int, str | None]:
    payload = client.get_json(f"/requirements/{urllib.parse.quote(task_id)}/tests", catalog=catalog)
    files = payload.get("files") or []
    if not files:
        return 0, None
    name = root_name(task_id)
    if not dry_run:
        target = out_dir / task_id
        for entry in files:
            rel = Path(str(entry["path"]))
            if rel.is_absolute() or ".." in rel.parts:
                raise SystemExit(f"{task_id}: refusing unsafe spec path {rel}")
            dest = target / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(str(entry["content"]), encoding="utf-8")
    return len(files), name


def update_manifest(out_dir: Path, entries: dict[str, str], dry_run: bool) -> None:
    manifest = out_dir / MANIFEST
    data: dict[str, str] = {}
    if manifest.is_file():
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            raise SystemExit(f"{manifest} is not valid JSON; fix it before adding entries")
    changed = [k for k, v in entries.items() if data.get(k) != v]
    data.update(entries)
    if dry_run or not changed:
        return
    manifest.write_text(json.dumps(data, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")


# ------------------------------------------------------------------ main

def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("tasks", nargs="*", help="task ids, e.g. hackathon--sheet")
    p.add_argument("--out", default=str(DEFAULT_OUT), help=f"spec root (default {DEFAULT_OUT})")
    p.add_argument("--catalog", default="competition", choices=("playground", "competition", "benchmark"))
    p.add_argument("--cookie-jar", default=None,
                   help="Netscape cookie jar; default $ARC_COOKIE_JAR or ~/.arc-cookies")
    p.add_argument("--email", default=os.environ.get("ARC_EMAIL"))
    p.add_argument("--password", default=os.environ.get("ARC_PASSWORD"))
    p.add_argument("--list", action="store_true", help="list the catalog's tasks and exit")
    p.add_argument("--all", action="store_true", help="fetch every task in the catalog")
    p.add_argument("--dry-run", action="store_true")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    out_dir = Path(args.out)
    jar_path = args.cookie_jar or os.environ.get("ARC_COOKIE_JAR") or str(Path.home() / ".arc-cookies")
    client = Client(load_jar(jar_path))

    if args.list:
        found = requirement_ids(client, args.catalog)
        print(f"{args.catalog}: {len(found)} task(s)")
        for tid in found:
            print(f"  {tid}")
        return 0

    tasks = list(args.tasks)
    if args.all:
        tasks = requirement_ids(client, args.catalog)
    if not tasks:
        raise SystemExit("no tasks given; pass task ids, --all, or --list")

    authed = any(c.name == "arcbench_session" for c in client.jar)
    if not authed and args.email and args.password:
        client.login(args.email, args.password)
        authed = True
    if not authed:
        print(f"note: no session in {jar_path}; official tracks (hackathon) will answer 401",
              file=sys.stderr)

    written: dict[str, str] = {}
    for task_id in tasks:
        try:
            count, name = fetch_one(client, task_id, args.catalog, out_dir, args.dry_run)
        except urllib.error.HTTPError as exc:
            detail = ""
            try:
                detail = json.loads(exc.read()).get("detail")
            except Exception:
                pass
            print(f"FAIL {task_id}: HTTP {exc.code} {detail or exc.reason}")
            continue
        if not count:
            print(f"FAIL {task_id}: no spec files returned")
            continue
        print(f"ok   {task_id}: {count} file(s) -> {out_dir / task_id}"
              + (f" (manifest: {name!r})" if name else " (no local requirements.yaml: manifest not updated)"))
        if name:
            written[task_id] = name
    update_manifest(out_dir, written, args.dry_run)
    return 0 if len(written) or args.dry_run else 1


if __name__ == "__main__":
    raise SystemExit(main())
