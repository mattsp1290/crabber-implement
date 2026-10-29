#!/usr/bin/env python3
"""Drive review recovery through the production monitor with local Playwright."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', default='.audit/review-recovery/browser-local')
args = parser.parse_args()
evidence = (ROOT / args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)
export = evidence / 'cli'
export.mkdir(exist_ok=True)
markers = [evidence / f'review-{name}' for name in ('continue', 'release', 'done')]
for path in [*markers, export / 'snapshot.json', evidence / 'result.json', evidence / 'review-browser.json']:
    path.unlink(missing_ok=True)

def wait_for(predicate, timeout=180):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.1)
    raise RuntimeError('Timed out waiting for review browser journey')

subprocess.run(['npm', '--prefix', 'web', 'run', 'build'], cwd=ROOT, check=True)
env = dict(os.environ, CRABBER_VERIFY_ARTIFACTS=str(export),
    CRABBER_VERIFY_REVIEW_CONTINUE_FILE=str(markers[0]),
    CRABBER_VERIFY_REVIEW_RELEASE_FILE=str(markers[1]),
    CRABBER_VERIFY_REVIEW_DONE_FILE=str(markers[2]))
server = None
with (evidence / 'cli.txt').open('w') as log:
    cli = subprocess.Popen(['cargo', 'test', '--locked', '--test', 'cli',
        'cli_review_recovery_browser_journey', '--', '--ignored', '--nocapture'],
        cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        wait_for(lambda: (export / 'snapshot.json').exists() or cli.poll() is not None)
        if cli.poll() is not None:
            raise RuntimeError('CLI exited before browser; read cli.txt')
        snapshot = json.loads((export / 'snapshot.json').read_text())
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        url = f'http://127.0.0.1:{port}'
        server = subprocess.Popen(['node', 'web/server/index.js'], cwd=ROOT,
            env=dict(os.environ, ARTIFACTS_DIR=str(Path(snapshot['artifacts']).parent), PORT=str(port)),
            stdout=subprocess.DEVNULL)
        def ready():
            try:
                return urllib.request.urlopen(f'{url}/api/runs').status == 200
            except OSError:
                return False
        wait_for(ready, 30)
        subprocess.run(['node', 'scripts/browser_review_recovery.mjs'], cwd=ROOT, check=True,
            env=dict(os.environ, PLAYWRIGHT_BASE_URL=url, LIVE_RUN_ID=snapshot['run_id'],
                PLAYWRIGHT_PACKAGE_JSON=str(ROOT / 'web/package.json'),
                PLAYWRIGHT_EVIDENCE_DIR=str(evidence)))
        if cli.wait(timeout=30):
            raise RuntimeError('CLI recovery failed; read cli.txt')
        (evidence / 'result.json').write_text(json.dumps({'status': 'passed',
            'runtime': 'native production server', 'cli_exit': cli.returncode,
            'run_id': snapshot['run_id']}, indent=2) + '\n')
    finally:
        for marker in markers:
            marker.touch()
        if cli.poll() is None:
            try:
                cli.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(cli.pid, signal.SIGTERM)
                cli.wait(timeout=10)
        if server is not None:
            server.terminate()
            server.wait(timeout=10)
assert (evidence / 'review-completed.png').is_file()
assert (export / 'snapshot.json').is_file()
print(f'Browser evidence survives cleanup: {evidence}')
