#!/usr/bin/env python3
"""Prove recovery checks reject five defects, restoring exact source bytes."""
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
source = ROOT / 'src/review_checkpoint.rs'
evidence = ROOT / '.audit/review-recovery/mutations'
evidence.mkdir(parents=True, exist_ok=True)
original = source.read_bytes()
variants = [
    ('identity', 'saved == current', 'true', 'stale checkpoint identity accepted'),
    ('one-report', 'valid.into_iter().all(|v| v)', 'valid[0]', 'incomplete publication counted'),
    ('round-reset', '.checked_add(first)', '.checked_add(1)', 'global review budget reset'),
    ('fix-intent', 'CheckpointPhase::ReviewPending | CheckpointPhase::ReviewReportsReady',
        'CheckpointPhase::ReviewPending | CheckpointPhase::ReviewReportsReady | CheckpointPhase::FixIntentRecorded', 'ready state before durable paired reports'),
    ('premature-ready', 'PhaseEvent::Reports(valid) if can_resume(phase) && reports_ready(valid)',
        'PhaseEvent::Reports(_valid) if can_resume(phase)', 'ready state before durable paired reports'),
]
results = []
try:
    for name, old, new, expected in variants:
        text = original.decode()
        assert text.count(old) == 1, name
        source.write_text(text.replace(old, new))
        run = subprocess.run(['cargo', 'test', '--locked', 'formal_checkpoint_reachability', '--', '--nocapture'],
            cwd=ROOT, capture_output=True, text=True)
        transcript = run.stdout + run.stderr
        (evidence / f'{name}.txt').write_text(transcript)
        caught = run.returncode != 0 and expected in transcript
        results.append({'mutation': name, 'exit': run.returncode, 'caught': caught})
        assert caught, f'{name}: checker did not catch expected property; read evidence'
finally:
    source.write_bytes(original)
    (evidence / 'result.json').write_text(json.dumps({'source_sha256': hashlib.sha256(original).hexdigest(),
        'restored': source.read_bytes() == original, 'mutations': results}, indent=2) + '\n')
subprocess.run(['python3', 'scripts/verify_formal.py', '--evidence', '.audit/review-recovery/formal'], cwd=ROOT, check=True)
