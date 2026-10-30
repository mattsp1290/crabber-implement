#!/usr/bin/env python3
"""Check all abstract workflow paths for every supported review budget."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', default='.audit/bounded-review-inputs/formal')
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
evidence = (root / args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)
sources = ['src/review_dispatch.rs', 'src/workflow.rs', 'src/review_checkpoint.rs', 'src/review_inputs.rs', 'src/review_artifacts.rs', 'src/config.rs', 'src/git.rs', 'src/durable.rs', 'src/review.rs', 'src/formal_verification.rs']
source_hashes = {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources}
result = subprocess.run(
    ['cargo', 'test', '--locked', 'formal_', '--', '--nocapture'],
    cwd=root, text=True, capture_output=True,
)
transcript = result.stdout + result.stderr
(evidence / 'transcript.txt').write_text(transcript)
summary = {
    'checker': 'crabber-codex-delegation-workflow-v4',
    'sync_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
    'working_tree_sources': True,
    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'cargo': subprocess.check_output(['cargo', '--version'], text=True).strip(),
    'python': sys.version.split()[0],
    'source_sha256': source_hashes,
    'sources_unchanged': all(hashlib.sha256((root / name).read_bytes()).hexdigest() == digest for name, digest in source_hashes.items()),
    'budget_min': 1,
    'budget_max': 100,
    'exit_code': result.returncode,
    'marker_present': 'crabber-delegation-checker-v1' in transcript and 'crabber-workflow-checker-v3' in transcript and 'crabber-refresh-checker-v1' in transcript and 'crabber-checkpoint-checker-v1' in transcript and 'safety=PASS termination=PASS' in transcript,
}
(evidence / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
print(transcript, end='')
print(json.dumps(summary))
if result.returncode or not summary['marker_present'] or not summary['sources_unchanged']:
    sys.exit(1)
