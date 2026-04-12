"""Web-based dashboard for running-process daemon.

Launches a lightweight HTTP server and opens a browser to display a live
tree view of all tracked processes.  Data is fetched from the
running-process-daemon via its ``list --json`` CLI command.

Usage::

    running-process-dashboard          # default port 8787
    running-process-dashboard --port 9000
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import threading
import webbrowser
from http.server import HTTPServer, BaseHTTPRequestHandler
from typing import Sequence

_DEFAULT_PORT = 8787
_REFRESH_INTERVAL_MS = 3000

# ---------------------------------------------------------------------------
# Data fetching
# ---------------------------------------------------------------------------


def _fetch_processes_json() -> list[dict]:
    """Query the daemon for the current process list (JSON)."""
    daemon_bin = shutil.which("running-process-daemon")
    if daemon_bin is None:
        return []
    try:
        result = subprocess.run(
            [daemon_bin, "list", "--json"],
            capture_output=True,
            text=True,
            timeout=5.0,
        )
        if result.returncode != 0:
            return []
        return json.loads(result.stdout)
    except (subprocess.TimeoutExpired, json.JSONDecodeError, OSError):
        return []


# ---------------------------------------------------------------------------
# HTML template
# ---------------------------------------------------------------------------

_HTML_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>running-process dashboard</title>
<style>
  :root {
    --bg: #0d1117; --fg: #e6edf3; --accent: #58a6ff;
    --green: #3fb950; --red: #f85149; --yellow: #d29922;
    --border: #30363d; --surface: #161b22;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: 'Segoe UI', system-ui, -apple-system, sans-serif;
         background: var(--bg); color: var(--fg); padding: 24px; }
  h1 { font-size: 1.4rem; font-weight: 600; margin-bottom: 16px;
       display: flex; align-items: center; gap: 10px; }
  h1 .dot { width: 10px; height: 10px; border-radius: 50%;
            background: var(--green); display: inline-block; }
  h1 .dot.offline { background: var(--red); }
  .meta { color: #8b949e; font-size: 0.85rem; margin-bottom: 20px; }
  table { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
  th { text-align: left; padding: 10px 12px; border-bottom: 2px solid var(--border);
       color: #8b949e; font-weight: 500; text-transform: uppercase;
       font-size: 0.75rem; letter-spacing: 0.05em; }
  td { padding: 8px 12px; border-bottom: 1px solid var(--border); }
  tr:hover td { background: var(--surface); }
  .pid { font-family: 'Cascadia Code', 'Fira Code', monospace; color: var(--accent); }
  .state { padding: 2px 8px; border-radius: 12px; font-size: 0.8rem; font-weight: 500; }
  .state-alive { background: #0d2818; color: var(--green); }
  .state-dead { background: #2d1214; color: var(--red); }
  .state-zombie { background: #2d2208; color: var(--yellow); }
  .state-unknown { background: #1c1c1c; color: #8b949e; }
  .cmd { font-family: 'Cascadia Code', 'Fira Code', monospace;
         font-size: 0.85rem; max-width: 500px; overflow: hidden;
         text-overflow: ellipsis; white-space: nowrap; }
  .empty { text-align: center; padding: 48px; color: #8b949e; }
  .uptime { color: #8b949e; font-family: monospace; }
  #error { color: var(--red); margin-bottom: 12px; display: none; }
</style>
</head>
<body>

<h1><span class="dot" id="statusDot"></span> running-process dashboard</h1>
<div class="meta" id="meta">loading...</div>
<div id="error"></div>

<table>
<thead>
  <tr><th>PID</th><th>State</th><th>Kind</th><th>Uptime</th><th>Command</th></tr>
</thead>
<tbody id="tbody"></tbody>
</table>
<div class="empty" id="empty" style="display:none;">No tracked processes</div>

<script>
const REFRESH_MS = """ + str(_REFRESH_INTERVAL_MS) + r""";

function stateName(s) {
  return {1:'alive', 2:'dead', 3:'zombie'}[s] || 'unknown';
}
function stateClass(s) {
  return 'state state-' + stateName(s);
}
function fmtUptime(sec) {
  sec = Math.floor(sec);
  if (sec < 60) return sec + 's';
  if (sec < 3600) return Math.floor(sec/60) + 'm ' + (sec%60) + 's';
  return Math.floor(sec/3600) + 'h ' + Math.floor((sec%3600)/60) + 'm';
}

async function refresh() {
  try {
    const resp = await fetch('/api/processes');
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    const data = await resp.json();
    const procs = data.processes || [];

    document.getElementById('statusDot').className = 'dot';
    document.getElementById('meta').textContent =
      procs.length + ' process(es) tracked — refreshing every ' + (REFRESH_MS/1000) + 's';
    document.getElementById('error').style.display = 'none';

    const tbody = document.getElementById('tbody');
    const empty = document.getElementById('empty');
    if (procs.length === 0) {
      tbody.innerHTML = '';
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    tbody.innerHTML = procs.map(p =>
      '<tr>' +
        '<td class="pid">' + p.pid + '</td>' +
        '<td><span class="' + stateClass(p.state) + '">' + stateName(p.state) + '</span></td>' +
        '<td>' + (p.kind || '') + '</td>' +
        '<td class="uptime">' + fmtUptime(p.uptime_seconds || 0) + '</td>' +
        '<td class="cmd" title="' + (p.command||'').replace(/"/g,'&quot;') + '">' +
          (p.command || '') + '</td>' +
      '</tr>'
    ).join('');
  } catch(e) {
    document.getElementById('statusDot').className = 'dot offline';
    document.getElementById('error').textContent = 'Failed to fetch: ' + e.message;
    document.getElementById('error').style.display = '';
  }
}

refresh();
setInterval(refresh, REFRESH_MS);
</script>
</body>
</html>"""


# ---------------------------------------------------------------------------
# HTTP handler
# ---------------------------------------------------------------------------


class _DashboardHandler(BaseHTTPRequestHandler):
    """Serve the dashboard HTML and JSON API."""

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/" or self.path == "/index.html":
            self._send_html()
        elif self.path == "/api/processes":
            self._send_json()
        else:
            self.send_error(404)

    def _send_html(self) -> None:
        body = _HTML_TEMPLATE.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self) -> None:
        processes = _fetch_processes_json()
        payload = json.dumps({"processes": processes}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format: str, *_args: object) -> None:
        # Suppress default access logs
        pass


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Launch the running-process web dashboard"
    )
    parser.add_argument(
        "--port", type=int, default=_DEFAULT_PORT,
        help=f"HTTP server port (default: {_DEFAULT_PORT})",
    )
    parser.add_argument(
        "--no-browser", action="store_true",
        help="Don't auto-open the browser",
    )
    args = parser.parse_args(argv)

    url = f"http://localhost:{args.port}"
    server = HTTPServer(("127.0.0.1", args.port), _DashboardHandler)

    if not args.no_browser:
        threading.Timer(0.5, webbrowser.open, args=(url,)).start()

    print(f"[dashboard] serving at {url}")
    print("[dashboard] press Ctrl+C to stop")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[dashboard] shutting down")
        server.shutdown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
