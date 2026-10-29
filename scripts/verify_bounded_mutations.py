#!/usr/bin/env python3
"""Exercise six production defects, restore exact bytes, then rerun the formal checks."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', default='.audit/bounded-review-inputs/mutations')
args = parser.parse_args()
evidence = (ROOT / args.evidence).resolve()
evidence.mkdir(parents=True, exist_ok=True)


def function_span(text, name):
    match = re.search(r'\bfn\s+' + re.escape(name) + r'\s*\(', text)
    if match is None:
        raise AssertionError(f'missing production function {name}')
    start = text.index('{', match.start())
    depth = 1
    end = start + 1
    while depth:
        if text[end] == '{':
            depth += 1
        elif text[end] == '}':
            depth -= 1
        end += 1
    return start, end


variants = [
    ('round-reset', 'src/review_checkpoint.rs', 'refresh_round',
     r'review_round\(source_round,\s*1,\s*budget\)', 'review_round(source_round, 1, budget).map(|_| 1)',
     'formal_refresh_readiness', 'refresh global review budget reset'),
    ('stale-head', 'src/review_checkpoint.rs', 'identity_matches',
     r'saved\s*==\s*current', 'saved.head != current.head || saved == current',
     'formal_checkpoint_reachability', 'stale checkpoint identity accepted'),
    ('old-approval', 'src/review_checkpoint.rs', 'initialize',
     r'config\.resume\s*=\s*Some\(state\);', '''let mut state = state;
        for role in 0..2 {
            let source_path = self.state.directory(&self.root).join(format!("{}.report.json", ROLES[role].0));
            if source_path.try_exists()? {
                state.reserve_role(&config.artifacts, role)?;
                let mut saved: RoleCheckpoint = durable::json(&source_path, durable::REPORT_LIMIT)?;
                saved.version = VERSION;
                saved.inputs = state.inputs.digest()?;
                saved.attempt = state.attempts[role];
                durable::write_json(&state.directory(&config.artifacts).join(format!("{}.report.json", ROLES[role].0)), &saved, false)?;
            }
        }
        config.resume = Some(state);''',
     'formal_refresh_old_reports_are_evidence_only', 'refresh imported old report as approval'),
    ('fix-intent', 'src/review_checkpoint.rs', 'can_resume',
     r'CheckpointPhase::ReviewPending\s*\|\s*CheckpointPhase::ReviewReportsReady',
     'CheckpointPhase::ReviewPending | CheckpointPhase::ReviewReportsReady | CheckpointPhase::FixIntentRecorded',
     'formal_refresh_readiness', 'refresh accepted stale source or incomplete snapshot readiness'),
    ('incomplete-snapshot', 'src/review_checkpoint.rs', 'refresh_eligible',
     r'&&\s*snapshot_verified', '&& { let _ = snapshot_verified; true }',
     'formal_refresh_readiness', 'refresh accepted stale source or incomplete snapshot readiness'),
    ('prompt-cap', 'src/review_inputs.rs', 'validate_prompt',
     r'prompt\.len\(\)\s*<=\s*PROMPT_LIMIT', 'true',
     'formal_snapshot_final_prompt_bound', 'final rendered prompt cap omitted'),
]
originals = {name: (ROOT / name).read_bytes() for _, name, *_ in variants}
results = []
try:
    for name, source, function, pattern, replacement, test, expected in variants:
        for path, original in originals.items():
            assert (ROOT / path).read_bytes() == original, f'concurrent source change in {path}'
        original = originals[source]
        text = original.decode()
        start, end = function_span(text, function)
        changed, matches = re.subn(pattern, lambda _: replacement, text[start:end])
        assert matches == 1, f'{name}: expected one mutation target, got {matches}'
        mutated = (text[:start] + changed + text[end:]).encode()
        path = ROOT / source
        try:
            path.write_bytes(mutated)
            run = subprocess.run(
                ['cargo', 'test', '--locked', test, '--', '--nocapture'],
                cwd=ROOT, capture_output=True, text=True,
            )
            transcript = run.stdout + run.stderr
            (evidence / f'{name}.txt').write_text(transcript)
            caught = run.returncode != 0 and expected in transcript
            results.append({
                'mutation': name, 'source': source, 'function': function,
                'source_sha256': hashlib.sha256(original).hexdigest(),
                'mutated_sha256': hashlib.sha256(mutated).hexdigest(),
                'test': test, 'exit_code': run.returncode, 'caught': caught,
                'expected_counterexample': expected,
            })
            print(f'{name}: {"caught" if caught else "NOT CAUGHT"}', flush=True)
            assert caught, f'{name}: expected property did not fail; inspect {evidence / (name + ".txt")}'
        finally:
            assert path.read_bytes() == mutated, f'concurrent edit while mutating {source}; do not overwrite it'
            path.write_bytes(original)
finally:
    restored = {name: (ROOT / name).read_bytes() == original for name, original in originals.items()}
    (evidence / 'result.json').write_text(json.dumps({
        'source_sha256': {name: hashlib.sha256(original).hexdigest() for name, original in originals.items()},
        'restored': restored, 'mutations': results,
    }, indent=2) + '\n')
subprocess.run(['python3', 'scripts/verify_formal.py', '--evidence', str(evidence.parent / 'formal-restored')], cwd=ROOT, check=True)
