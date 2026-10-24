#!/usr/bin/env python3
"""Verify the Docker React monitor with Playwright and a real live CLI run."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', default='.audit/web')
args = parser.parse_args()
evidence = (ROOT / args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)
web = ROOT / 'web'
(web / 'node_modules').mkdir(exist_ok=True)
image = 'crabber-monitor:verify'
browser_image = 'mcr.microsoft.com/playwright:v1.64.1-noble'


def run(command, **kwargs):
    subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def wait_until(predicate, description, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, ValueError):
            pass
        time.sleep(0.25)
    raise RuntimeError(f'Timed out waiting for {description}')


def start_server(artifacts, host_port, name):
    run(['docker', 'run', '--detach', '--rm', '--name', name,
         '-p', f'127.0.0.1:{host_port}:4173',
         '-v', f'{artifacts}:/artifacts:ro', '-e', 'ARTIFACTS_DIR=/artifacts', image])
    wait_until(lambda: urllib.request.urlopen(f'http://127.0.0.1:{host_port}/api/runs').status == 200,
               'Docker monitor readiness')


def browser(host_port, artifacts, command, extra_env=(), restart=None):
    browser_name = f'crabber-browser-{os.getpid()}'
    invocation = ['docker', 'run', '--rm', '--name', browser_name,
                  '--add-host=host.docker.internal:host-gateway',
                  '-v', f'{web}:/work:ro', '-v', '/work/node_modules',
                  '-v', f'{evidence}:/evidence', '-v', f'{artifacts}:/artifacts',
                  '-v', f'{ROOT / "scripts"}:/scripts:ro', '-w', '/work',
                  '-e', f'PLAYWRIGHT_BASE_URL=http://host.docker.internal:{host_port}',
                  '-e', 'TEST_ARTIFACTS_DIR=/artifacts',
                  '-e', 'PLAYWRIGHT_OUTPUT_DIR=/evidence/results',
                  '-e', 'PLAYWRIGHT_REPORT_DIR=/evidence/report']
    for entry in extra_env:
        invocation.extend(['-e', entry])
    invocation.extend([browser_image, 'sh', '-c', f'npm ci --ignore-scripts && {command}'])
    if restart is None:
        run(invocation)
        return
    worker = subprocess.Popen(invocation, cwd=ROOT)
    try:
        wait_until(lambda: (evidence / 'restart-request').exists() or worker.poll() is not None,
                   'browser live observation')
        if worker.poll() is not None:
            raise RuntimeError('Browser exited before live monitor restart')
        restart()
        (evidence / 'restarted').touch()
        if worker.wait(timeout=90):
            raise RuntimeError('Live browser journey failed')
    finally:
        if worker.poll() is None:
            subprocess.run(['docker', 'stop', browser_name],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            worker.wait(timeout=10)


run(['docker', 'build', '-t', image, str(web)])
run(['docker', 'pull', browser_image])
fixtures = evidence / 'fixtures'
fixtures.mkdir(exist_ok=True)
name = f'crabber-monitor-{os.getpid()}'
host_port = port()
process = None
release = evidence / 'release'
finished = evidence / 'finished'
for marker in [release, finished, evidence / 'cli' / 'snapshot.json',
               evidence / 'restart-request', evidence / 'restarted', evidence / 'result.json']:
    marker.unlink(missing_ok=True)
try:
    start_server(fixtures, host_port, name)
    browser(host_port, fixtures, 'npm run test:browser')
    run(['docker', 'stop', name])
    exported = evidence / 'cli'
    exported.mkdir(exist_ok=True)
    scratch = evidence / 'tmp'
    scratch.mkdir(exist_ok=True)
    environment = dict(os.environ, CRABBER_VERIFY_ARTIFACTS=str(exported),
                       CRABBER_VERIFY_HOLD_FILE=str(release),
                       CRABBER_VERIFY_DONE_FILE=str(finished), TMPDIR=str(scratch))
    with (evidence / 'cli.txt').open('w') as output:
        process = subprocess.Popen(['cargo', 'test', '--locked', '--test', 'cli',
                                    'cli_streams_live_artifacts_before_completion', '--', '--nocapture'],
                                   cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
        wait_until(lambda: (exported / 'snapshot.json').is_file() or process.poll() is not None,
                   'live CLI artifacts', timeout=180)
        if process.poll() is not None:
            raise RuntimeError('CLI live journey exited early. Read cli.txt.')
        snapshot = json.loads((exported / 'snapshot.json').read_text())
        actual = Path(snapshot['artifacts'])
        start_server(actual.parent, host_port, name)
        def restart_monitor():
            run(['docker', 'stop', name])
            start_server(actual.parent, host_port, name)

        browser(host_port, actual.parent, 'node /scripts/browser_live.mjs',
                [f'LIVE_RUN_ID={snapshot["run_id"]}'], restart=restart_monitor)
        if process.wait(timeout=90):
            raise RuntimeError('CLI live journey failed. Read cli.txt.')
    (evidence / 'result.json').write_text(json.dumps({
        'status': 'passed', 'image': image, 'browser_image': browser_image,
        'live_run_id': snapshot['run_id'], 'cli_exit_code': process.returncode,
        'checks': ['production Docker app', 'artifact-driven browser journeys',
                   'text visible before CLI completion', 'monitor restart during active CLI',
                   'real CLI terminal result'],
    }, indent=2) + '\n')
finally:
    release.touch()
    finished.touch()
    subprocess.run(['docker', 'stop', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if process is not None and process.poll() is None:
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)
print(f'Verified. Evidence survives cleanup at {evidence}')
