#!/usr/bin/env python3
"""Verify the Docker React monitor with Playwright and a real live CLI run."""
import argparse
import json
import os
from pathlib import Path
import socket
import signal
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
                                   cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
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
        'status': 'in_progress', 'image': image, 'browser_image': browser_image,
        'live_run_id': snapshot['run_id'], 'cli_exit_code': process.returncode,
        'checks': ['production Docker app', 'artifact-driven browser journeys',
                   'text visible before CLI completion', 'monitor restart during active CLI',
                   'real CLI terminal result'],
    }, indent=2) + '\n')
    run(['docker', 'stop', name])
    review_export = evidence / 'review-cli'
    review_export.mkdir(exist_ok=True)
    review_continue = evidence / 'review-continue'
    review_release = evidence / 'review-release'
    review_done = evidence / 'review-done'
    for marker in [review_continue, review_release, review_done, review_export / 'snapshot.json']:
        marker.unlink(missing_ok=True)
    review_environment = dict(os.environ, CRABBER_VERIFY_ARTIFACTS=str(review_export),
        CRABBER_VERIFY_REVIEW_CONTINUE_FILE=str(review_continue),
        CRABBER_VERIFY_REVIEW_RELEASE_FILE=str(review_release),
        CRABBER_VERIFY_REVIEW_DONE_FILE=str(review_done), TMPDIR=str(scratch))
    with (evidence / 'review-cli.txt').open('w') as output:
        process = subprocess.Popen(['cargo', 'test', '--locked', '--test', 'cli',
            'cli_review_recovery_browser_journey', '--', '--ignored', '--nocapture'],
            cwd=ROOT, env=review_environment, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        wait_until(lambda: (review_export / 'snapshot.json').is_file() or process.poll() is not None,
            'failed review checkpoint artifacts', timeout=180)
        if process.poll() is not None:
            raise RuntimeError('Review journey exited early. Read review-cli.txt.')
        review_snapshot = json.loads((review_export / 'snapshot.json').read_text())
        actual_review = Path(review_snapshot['artifacts'])
        start_server(actual_review.parent, host_port, name)
        browser(host_port, actual_review.parent, 'node /scripts/browser_review_recovery.mjs',
            [f'LIVE_RUN_ID={review_snapshot["run_id"]}'])
        if process.wait(timeout=45):
            raise RuntimeError('Review recovery CLI journey failed. Read review-cli.txt.')
    result = json.loads((evidence / 'result.json').read_text())
    result['status'] = 'passed'
    result['review_run_id'] = review_snapshot['run_id']
    result['checks'].extend(['failed reviewer preserved', 'same workflow resumed',
        'completed role reused', 'new generation live status', 'fresh approval after fixes'])
    run(['docker', 'stop', name])
    refresh_export = evidence / 'refresh-cli'
    refresh_export.mkdir(exist_ok=True)
    refresh_markers = [evidence / f'refresh-{stage}' for stage in ('continue', 'release', 'done')]
    for marker in [*refresh_markers, refresh_export / 'snapshot.json', refresh_export / 'refresh.json']:
        marker.unlink(missing_ok=True)
    refresh_environment = dict(os.environ, CRABBER_VERIFY_ARTIFACTS=str(refresh_export),
        CRABBER_VERIFY_REFRESH_CONTINUE_FILE=str(refresh_markers[0]),
        CRABBER_VERIFY_REFRESH_RELEASE_FILE=str(refresh_markers[1]),
        CRABBER_VERIFY_REFRESH_DONE_FILE=str(refresh_markers[2]), TMPDIR=str(scratch))
    with (evidence / 'refresh-cli.txt').open('w') as output:
        process = subprocess.Popen(['cargo', 'test', '--locked', '--test', 'cli',
            'cli_review_refresh_browser_journey', '--', '--ignored', '--nocapture'],
            cwd=ROOT, env=refresh_environment, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        wait_until(lambda: (refresh_export / 'snapshot.json').is_file() or process.poll() is not None,
            'failed refresh-source artifacts', timeout=180)
        if process.poll() is not None:
            raise RuntimeError('Refresh journey exited early. Read refresh-cli.txt.')
        refresh_snapshot = json.loads((refresh_export / 'snapshot.json').read_text())
        start_server(Path(refresh_snapshot['artifacts']).parent, host_port, name)
        browser(host_port, Path(refresh_snapshot['artifacts']).parent, 'node /scripts/browser_review_refresh.mjs',
            [f'LIVE_RUN_ID={refresh_snapshot["run_id"]}', 'REFRESH_EXPORT_DIR=/evidence/refresh-cli'])
        if process.wait(timeout=60):
            raise RuntimeError('Review refresh CLI journey failed. Read refresh-cli.txt.')
    result['refresh_source_run_id'] = refresh_snapshot['run_id']
    result['checks'].extend(['failed source retained', 'linked child review at global round four',
        'both refreshed reviewers fresh', 'child terminal approval', 'parent navigation retains failure'])
    (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
finally:
    release.touch()
    finished.touch()
    for marker in ['review-continue', 'review-release', 'review-done',
                   'refresh-continue', 'refresh-release', 'refresh-done']:
        (evidence / marker).touch()
    subprocess.run(['docker', 'stop', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if process is not None and process.poll() is None:
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)
print(f'Verified. Evidence survives cleanup at {evidence}')
