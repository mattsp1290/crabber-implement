#!/usr/bin/env python3
"""Run the installed CLI against a scratch repository using the real provider."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', default='target/debug/crabber-implement')
parser.add_argument('--evidence', default='.audit/live')
args = parser.parse_args()
binary = Path(args.binary).resolve()
evidence = Path(args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)
if not os.environ.get('OPENCODE_GO_API_KEY'):
    raise SystemExit('Export OPENCODE_GO_API_KEY before running live verification.')


def run(argv, cwd, **kwargs):
    return subprocess.run(argv, cwd=cwd, text=True, capture_output=True, **kwargs)


with tempfile.TemporaryDirectory(prefix='crabber-live-') as scratch:
    repo = Path(scratch)
    for argv in [
        ['git', 'init', '-b', 'main'],
        ['git', 'config', 'user.name', 'Crabber verification'],
        ['git', 'config', 'user.email', 'verification@example.invalid'],
    ]:
        result = run(argv, repo)
        result.check_returncode()
    (repo / 'README.md').write_text('# Scratch verification repository\n')
    for argv in [['git', 'add', 'README.md'], ['git', 'commit', '-m', 'Initialize verification repository']]:
        result = run(argv, repo)
        result.check_returncode()
    plan = repo / 'plan.md'
    plan.write_text('''# Add a greeting program

Status: Ready

Create hello.py in the repository root. Running python3 hello.py must print
exactly Hello from Crabber. followed by one newline. Use only Python's standard
library. Run the program and verify its output before finishing. No other
product behavior is required.
''')
    for argv in [['git', 'add', 'plan.md'], ['git', 'commit', '-m', 'Add greeting implementation plan']]:
        result = run(argv, repo)
        result.check_returncode()
    started = time.time()
    result = run([str(binary), str(plan), '--repo', str(repo), '--max-reviews', '3', '--session-timeout', '240'], repo, timeout=1600)
    (evidence / 'stdout.txt').write_text(result.stdout)
    (evidence / 'stderr.txt').write_text(result.stderr)
    actual = run(['python3', 'hello.py'], repo)
    (evidence / 'program.txt').write_text(actual.stdout + actual.stderr)
    (evidence / 'git-log.txt').write_text(run(['git', 'log', '--oneline', '--all'], repo).stdout)
    (evidence / 'git-status.txt').write_text(run(['git', 'status', '--porcelain'], repo).stdout)
    if (repo / 'hello.py').exists():
        (evidence / 'hello.py').write_text((repo / 'hello.py').read_text())
    import shutil
    for source in [repo / 'reviews', repo / '.git' / 'crabber-implement']:
        if source.is_dir():
            shutil.copytree(source, evidence / source.name, dirs_exist_ok=True)
    manifests = sorted((repo / 'reviews').glob('*/manifest.json'))
    head = run(['git', 'rev-parse', 'HEAD'], repo).stdout.strip()
    final_manifest = json.loads(manifests[-1].read_text()) if manifests else {}
    review_files_present = bool(manifests) and all(
        (manifests[-1].parent / reviewer['slug'] / filename).is_file()
        for reviewer in final_manifest.get('reviewers', [])
        for filename in ['00-overview.md', '01-critical-and-important.md', '02-suggestions.md', '03-positive-notes.md', '04-action-items.md']
    ) and len(final_manifest.get('reviewers', [])) == 2
    workflow_results = list((evidence / 'crabber-implement').glob('*/result.json'))
    workflow_completed = bool(workflow_results) and json.loads(workflow_results[-1].read_text()).get('status') == 'completed'
    summary = {
        'exit_code': result.returncode,
        'program_exit_code': actual.returncode,
        'program_output_matches': actual.stdout == 'Hello from Crabber.\n',
        'clean_repository': not run(['git', 'status', '--porcelain'], repo).stdout.strip(),
        'review_matches_head': final_manifest.get('head_sha') == head,
        'review_files_present': review_files_present,
        'workflow_completed': workflow_completed,
        'elapsed_seconds': round(time.time() - started, 1),
        'scratch_repository_removed_on_exit': True,
    }
    (evidence / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary))
    assert result.returncode == 0, 'CLI failed. Read evidence stdout.txt and stderr.txt.'
    assert actual.returncode == 0 and summary['program_output_matches'], 'Plan acceptance criterion failed.'
    assert all(summary[key] for key in ['clean_repository', 'review_matches_head', 'review_files_present', 'workflow_completed']), 'Review or repository evidence failed.'
assert (evidence / 'result.json').is_file(), 'Evidence did not survive cleanup.'
