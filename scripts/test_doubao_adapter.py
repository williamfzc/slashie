#!/usr/bin/env python3
"""Verify Doubao adapter input/session/evidence boundaries with a fake provider."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

adapter = Path(__file__).with_name('doubao_session.py')
with tempfile.TemporaryDirectory(prefix='slashie-adapter-') as folder:
    work = Path(folder)
    fake = work / 'provider'
    fake.write_text('#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps({"subtype":"success","session_id":"synthetic_session","result":{"content":json.dumps(sys.argv[1:])}}))\n')
    fake.chmod(0o700)
    env = dict(os.environ, DOUBAO_CLI=str(fake), SLASHIE_SESSION_DIR=str(work),
               SLASHIE_PROMPT='default\nrequest\nlink: https://example.test/source', SLASHIE_ARGS='args only')
    env.pop('SLASHIE_RECORD_TURNS', None)
    env.pop('SLASHIE_CONTINUE', None)
    env.pop('DOUBAO_PERMISSION_MODE', None)
    def run():
        result = subprocess.run([sys.executable, str(adapter)], env=env, capture_output=True, text=True, timeout=5)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)
    argv = run()
    assert argv[argv.index('-p') + 1] == env['SLASHIE_PROMPT']
    assert not (work / 'doubao-turns.jsonl').exists()
    assert json.loads((work / 'doubao-session.json').read_text())['session_id'] == 'synthetic_session'
    assert '--permission-mode' not in argv
    env['DOUBAO_PERMISSION_MODE'] = 'always_allow'
    argv = run()
    assert argv[argv.index('--permission-mode') + 1] == 'always_allow'
    env['DOUBAO_PERMISSION_MODE'] = 'not-a-mode'
    result = subprocess.run([sys.executable, str(adapter)], env=env, capture_output=True, text=True, timeout=5)
    assert result.returncode == 2 and 'Invalid' in result.stderr
    env.pop('DOUBAO_PERMISSION_MODE')
    env.update(SLASHIE_CONTINUE='1', SLASHIE_RECORD_TURNS='1')
    argv = run()
    assert argv[argv.index('--resume') + 1] == 'synthetic_session'
    assert json.loads((work / 'doubao-turns.jsonl').read_text())['prompt'] == env['SLASHIE_PROMPT']
print('PASS combined prompt, exact resume, default privacy, opt-in evidence')
