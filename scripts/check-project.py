#!/usr/bin/env python3
"""Cheap repository invariants. No application window or network required."""
import ast
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
for directory in ('scripts', 'examples'):
    base = ROOT / directory
    if base.is_dir():
        for script in base.rglob('*.py'):
            ast.parse(script.read_text(encoding='utf-8'), filename=str(script))

# The UI talks to playback through player::Session commands only.
ui_files = [ROOT / 'crates/iris/src/ui.rs', *sorted((ROOT / 'crates/iris/src/ui').glob('*.rs'))]
for path in ui_files:
    text = path.read_text(encoding='utf-8')
    for needle in ('command_async(', 'get_property_string(', 'set_property', 'self.mpv', 'this.mpv'):
        assert needle not in text, f'{path.relative_to(ROOT)} uses {needle}'

assert 'feature = "devtools"' in (ROOT / 'crates/iris/src/debug.rs').read_text(encoding='utf-8')

metadata = json.loads(subprocess.run(
    ['cargo', 'metadata', '--offline', '--locked', '--no-deps', '--format-version', '1'],
    cwd=ROOT, check=True, capture_output=True, text=True).stdout)
core = next(p for p in metadata['packages'] if p['name'] == 'iris-core')
assert not any(d['name'] == 'gpui' for d in core['dependencies']), 'iris-core must stay headless'
api = next(p for p in metadata['packages'] if p['name'] == 'iris-api')
assert not any(d['name'] in ('gpui', 'iris-mpv') for d in api['dependencies']), 'iris-api must stay pure control-plane'
print('Python syntax, UI playback boundary, devtools gate, headless iris-core & iris-api, Cargo metadata: OK')
