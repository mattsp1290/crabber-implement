#!/usr/bin/env python3
"""Check all abstract workflow paths for every supported review budget."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', default='.audit/formal')
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
evidence = (root / args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)
result = subprocess.run(
    ['cargo', 'test', '--locked', 'formal_workflow_reachability', '--', '--nocapture'],
    cwd=root, text=True, capture_output=True,
)
transcript = result.stdout + result.stderr
(evidence / 'transcript.txt').write_text(transcript)
sources = ['src/workflow.rs', 'src/formal_verification.rs']
summary = {
    'checker': 'crabber-workflow-checker-v1',
    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'cargo': subprocess.check_output(['cargo', '--version'], text=True).strip(),
    'python': sys.version.split()[0],
    'source_sha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    'budget_min': 1,
    'budget_max': 100,
    'exit_code': result.returncode,
    'marker_present': 'safety=PASS termination=PASS' in transcript,
}
(evidence / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
print(transcript, end='')
print(json.dumps(summary))
if result.returncode or not summary['marker_present']:
    sys.exit(1)
