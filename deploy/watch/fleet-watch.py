#!/usr/bin/env python3
"""fleet-watch: the fleet's one alarm (replaces Uptime-Kuma + Beszel, 2026-10-19).

Runs on caguaserver every 2 minutes (fleet-watch.timer). Each check is either
ok or failing; only *transitions* push to the phone, plus a reminder every
REMIND_HOURS while something stays broken, plus a recovery notice. Pushes go
to cagua-push /api/notify (tag "fleet"); if cagua-push itself is the thing
that is down, the alert goes out through the mini's wa-notify instead.

  fleet-watch.py            run all checks, alert on changes
  fleet-watch.py --status   print every check, alert on nothing

State: ~/.local/state/fleet-watch.json (last result per check + when it broke).
Env:   CAGUA_PUSH_URL, CAGUA_PUSH_TOKEN (from /etc/cagua-push.env).
"""
import glob
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request

STATE = os.path.expanduser('~/.local/state/fleet-watch.json')
MINI = 'caguabot@100.124.66.77'
REMIND_HOURS = 6
# A check must fail this many runs in a row before it alerts (2 runs ≈ 4 min),
# so a service restart or a slow request never pages.
FAIL_RUNS = 2

# name -> (url, ok_codes). Any HTTP answer in ok_codes means "up"; 401 counts
# for services that sit behind auth.
HTTP = {
    'command-center': ('http://127.0.0.1:8787/api/projects?limit=1', {200}),
    'cuentas': ('http://127.0.0.1:8789/health', {200, 401}),
    'hermeshub': ('http://127.0.0.1:8796/health', {200}),
    'cagua-push': ('http://127.0.0.1:8812/health', {200}),
    'caguastore': ('http://127.0.0.1:8099/', {200}),
}
TCP = {
    'soft-serve': ('127.0.0.1', 23231),
    'mini-ssh': ('100.124.66.77', 22),
}
DISKS = {'/': 85, '/data': 90}
BACKUP_GLOB = '/data/backups/caguaserver/caguaserver-*.tgz'
BACKUP_MAX_DAYS = 8
# The mini's wa-watchdog touches this every run; stale = WhatsApp unwatched.
MINI_HEARTBEAT = '~/.local/state/wa-watchdog.json'
MINI_HEARTBEAT_MAX_MIN = 15


def http_ok(url, codes):
    try:
        with urllib.request.urlopen(url, timeout=8) as r:
            code = r.status
    except urllib.error.HTTPError as e:
        code = e.code
    except Exception as e:
        return False, f'sin respuesta ({type(e).__name__})'
    return code in codes, f'HTTP {code}'


def tcp_ok(host, port):
    try:
        with socket.create_connection((host, port), timeout=6):
            return True, 'ok'
    except OSError as e:
        return False, f'sin conexión ({e.strerror or e})'


def failed_units():
    out = subprocess.run(['systemctl', 'list-units', '--failed', '--no-legend', '--plain'],
                         capture_output=True, text=True).stdout
    units = sorted(line.split()[0] for line in out.splitlines() if line.strip())
    return (not units), (', '.join(units) or 'ninguna')


def disk(path, limit):
    if path != '/' and not os.path.ismount(path):
        return False, 'no montado'
    u = shutil.disk_usage(path)
    pct = round(u.used * 100 / u.total)
    return pct < limit, f'{pct}% usado'


def backup_fresh():
    files = glob.glob(BACKUP_GLOB)
    if not files:
        return False, 'no hay respaldos'
    age = (time.time() - max(os.path.getmtime(f) for f in files)) / 86400
    return age < BACKUP_MAX_DAYS, f'último hace {age:.1f} días'


def mini_heartbeat():
    cmd = f'stat -f %m {MINI_HEARTBEAT} 2>/dev/null || echo 0'
    try:
        out = subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', MINI, cmd],
                             capture_output=True, text=True, timeout=20).stdout.strip()
        age = (time.time() - int(out or 0)) / 60
    except Exception as e:
        return False, f'no pude revisar ({type(e).__name__})'
    return age < MINI_HEARTBEAT_MAX_MIN, f'último latido hace {age:.0f} min'


def run_checks():
    results = {}
    for name, (url, codes) in HTTP.items():
        results[name] = http_ok(url, codes)
    for name, (host, port) in TCP.items():
        results[name] = tcp_ok(host, port)
    results['systemd'] = failed_units()
    for path, limit in DISKS.items():
        results[f'disco {path}'] = disk(path, limit)
    results['respaldo semanal'] = backup_fresh()
    if results['mini-ssh'][0]:
        results['whatsapp watchdog'] = mini_heartbeat()
    return results


# ---------------------------------------------------------------- delivery

def push(title, body, priority, dedupe):
    url = os.environ.get('CAGUA_PUSH_URL', 'http://127.0.0.1:8812').rstrip('/')
    token = os.environ.get('CAGUA_PUSH_TOKEN', '')
    data = json.dumps({'title': title, 'body': body, 'tag': 'fleet',
                       'priority': priority, 'dedupe_key': dedupe}).encode()
    req = urllib.request.Request(f'{url}/api/notify', data=data, headers={
        'Content-Type': 'application/json', 'Authorization': f'Bearer {token}'})
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.status < 300


def notify(title, body, priority, dedupe):
    try:
        if push(title, body, priority, dedupe):
            return
    except Exception as e:
        print(f'cagua-push failed ({e}), trying wa-notify on the mini', file=sys.stderr)
    msg = f'{title}: {body}'.replace("'", '')
    subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', MINI,
                    f"/opt/homebrew/bin/wa-notify fleet '{msg}'"], timeout=60)


def main():
    status_only = '--status' in sys.argv
    results = run_checks()
    if status_only:
        for name, (ok, detail) in results.items():
            print(f"{'ok  ' if ok else 'FAIL'} {name}: {detail}")
        return 0

    try:
        state = json.load(open(STATE))
    except (OSError, ValueError):
        state = {}
    now = time.time()
    for name, (ok, detail) in results.items():
        s = state.setdefault(name, {'fails': 0, 'alerted': 0, 'since': 0})
        if ok:
            if s['alerted']:
                down_min = (now - s['since']) / 60
                notify(f'✅ {name} volvió', f'{detail} · estuvo caído {down_min:.0f} min',
                       'default', f'fleet:{name}:up:{int(now)}')
            state[name] = {'fails': 0, 'alerted': 0, 'since': 0}
            continue
        s['fails'] += 1
        s['since'] = s['since'] or now
        due = not s['alerted'] or now - s['alerted'] > REMIND_HOURS * 3600
        if s['fails'] >= FAIL_RUNS and due:
            again = ' (sigue)' if s['alerted'] else ''
            notify(f'🔴 {name}{again}', detail, 'high', f'fleet:{name}:down:{int(now)}')
            s['alerted'] = now
    os.makedirs(os.path.dirname(STATE), exist_ok=True)
    with open(STATE, 'w') as f:
        json.dump(state, f, indent=1)
    return 0


if __name__ == '__main__':
    sys.exit(main())
