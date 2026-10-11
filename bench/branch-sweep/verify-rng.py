#!/usr/bin/env python3
"""Compare pre/post reactor RNG bytes and states, then check branch isolation.

Only one daemon runs at a time. The reference is the saved pre-change reactor,
not the host's different NumPy release. Run from the repository root.
"""
import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
REACTOR = ROOT / 'runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm'
WORKLOAD = '''
import numpy as np, hashlib, json, copy
records = []
def record(label, values, bitgen):
    a = np.asarray(values)
    records.append([label, a.dtype.str, list(a.shape),
                    hashlib.sha256(a.tobytes()).hexdigest(), copy.deepcopy(bitgen.state)])
for seed in [0, 7, 2**128-1, [0, 7, 4294967295]]:
    bg = np.random.PCG64(seed)
    rng = np.random.Generator(bg)
    for label, fn in [
        ('cached32', lambda: rng.integers(0, 2**32, 3, dtype=np.uint32)),
        ('normal64', lambda: rng.standard_normal(200000)),
        ('normal32', lambda: rng.standard_normal(200000, dtype=np.float32)),
        ('raw', lambda: bg.random_raw(200000)),
        ('uniform', lambda: rng.random(100000)),
        ('integers', lambda: rng.integers(-123, 456, 100000, dtype=np.int32)),
        ('exponential', lambda: rng.standard_exponential(100000)),
        ('gamma', lambda: rng.standard_gamma(.7, 100000)),
        ('scalar', lambda: [rng.standard_normal() for _ in range(1000)]),
        ('empty', lambda: rng.standard_normal(0)),
    ]:
        record(str(seed)+label, fn(), bg)
    state = copy.deepcopy(bg.state)
    whole = rng.standard_normal(10003)
    end = copy.deepcopy(bg.state)
    bg.state = state
    chunks = np.concatenate([rng.standard_normal(n) for n in [1, 2, 0, 5000, 5000]])
    assert whole.tobytes() == chunks.tobytes() and bg.state == end
    record(str(seed)+'chunks', chunks, bg)
    bg.advance(2**80 + 123)
    record(str(seed)+'advance', rng.standard_normal(10000), bg)
    jumped = bg.jumped()
    record(str(seed)+'jumped', np.random.Generator(jumped).standard_normal(10000), jumped)
    bg.state = state
    record(str(seed)+'restore', rng.standard_normal(10003), bg)
    assert bg.state == end
for hi in [0, 2**64-1]:
    for lo in [0, 2**32-1, 2**64-1]:
        for inc in [1, 2**128-1]:
            bg = np.random.PCG64(7)
            bg.state = {'bit_generator':'PCG64', 'state':{'state':(hi<<64)|lo, 'inc':inc},
                        'has_uint32':1, 'uinteger':4294967295}
            rng = np.random.Generator(bg)
            record(str((hi,lo,inc)), rng.standard_normal(10000), bg)
            record(str((hi,lo,inc))+'cache', rng.integers(0, 2**32, 3, dtype=np.uint32), bg)
rng = np.random.default_rng(7)
record('benchmark-normal', rng.standard_normal(2000000), rng.bit_generator)
# Separate generators are controls for changes outside PCG64's single step.
for cls in [np.random.PCG64DXSM, np.random.MT19937, np.random.Philox, np.random.SFC64]:
    bg = cls(7)
    a = np.random.Generator(bg).standard_normal(100000)
    records.append([cls.__name__, hashlib.sha256(a.tobytes()).hexdigest()])
print(json.dumps(records, sort_keys=True))
'''
PARENT = '''
import numpy as np, hashlib, json, copy
rng = np.random.default_rng(7)
rng.integers(0, 2**32, 3, dtype=np.uint32)
saved = copy.deepcopy(rng.bit_generator.state)
probe = np.random.default_rng()
probe.bit_generator.state = saved
expected = hashlib.sha256(probe.standard_normal(200000).tobytes()).hexdigest()
expected_state = copy.deepcopy(probe.bit_generator.state)
'''
CHILD = '''
a = rng.standard_normal(200000)
assert hashlib.sha256(a.tobytes()).hexdigest() == expected
assert rng.bit_generator.state == expected_state
rng.standard_normal(10001)
print(expected)
'''


def post(url, path, body):
    request = urllib.request.Request(url + path, data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=180) as response:
        return json.load(response)


def evaluate(url, path, code):
    result = post(url, path + '/eval', {'code': code})
    assert result['rc'] == 0 and not result.get('error'), result
    return result['stdout']


def save_rows(path, header, key, rows):
    body = json.dumps(header, indent=2)[:-2]
    lines = ',\n'.join('    '+json.dumps(row, separators=(',', ':')) for row in rows)
    path.write_text(body+',\n  '+json.dumps(key)+': [\n'+lines+'\n  ]\n}\n')


@contextlib.contextmanager
def daemon(reactor, backend, accel):
    with tempfile.TemporaryDirectory(prefix='wisp-rng-verify-') as tmp:
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        url = f'http://127.0.0.1:{port}'
        env = {'PATH': os.environ['PATH'], 'WISP_BIND': f'127.0.0.1:{port}',
               'WISP_PYTHON_WASM': str(reactor), 'WISP_SNAPSHOT_PATH': tmp+'/snapshot.bin',
               'WISP_ACCEL': accel, 'WISP_BRANCH_SNAPSHOT': backend,
               'WISP_WORKERS': '8', 'WISP_BRANCH_PARALLEL': '8',
               'VECLIB_MAXIMUM_THREADS':'1', 'OMP_NUM_THREADS':'1', 'OPENBLAS_NUM_THREADS':'1'}
        with open(tmp+'/daemon.log', 'w') as log:
            process = subprocess.Popen([str(ROOT/'target/release/wisp-runtime')], cwd=ROOT, env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic()+180
                while True:
                    if process.poll() is not None:
                        raise RuntimeError(pathlib.Path(tmp+'/daemon.log').read_text())
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=.2):
                            break
                    except OSError:
                        if time.monotonic()>deadline:
                            raise RuntimeError('daemon startup timeout')
                        time.sleep(.1)
                yield url
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-reactor', type=pathlib.Path, required=True)
    parser.add_argument('--reactor', type=pathlib.Path, default=REACTOR)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    configs = [('reference', args.reference_reactor.resolve(), 'auto', '1')]
    configs += [('candidate', args.reactor.resolve(), backend, accel)
                for backend in ['auto', 'file'] for accel in ['1', '0']]
    reference = None
    reports = []
    for name, reactor, backend, accel in configs:
        with daemon(reactor, backend, accel) as url:
            sid = post(url, '/v1/session', {})['session_id']
            path = '/v1/session/'+sid
            records = json.loads(evaluate(url, path, WORKLOAD))
            if reference is None:
                reference = records
            assert records == reference, (name, backend, accel, 'RNG bytes/state changed')
            evaluate(url, path, PARENT)
            for repeat in range(2):
                result = post(url, path+'/branch', {'codes': [CHILD]*8})
                assert result['snapshot_backend'] == ('mach-cow' if backend=='auto' else 'file'), result
                for child in result['results']:
                    assert child['rc'] == 0 and not child.get('error'), child
                assert len({child['stdout'] for child in result['results']}) == 1
                evaluate(url, path, 'assert rng.bit_generator.state == saved')
            evaluate(url, path, CHILD)
            with urllib.request.urlopen(urllib.request.Request(url+path, method='DELETE')):
                pass
            reports.append({'name':name, 'backend':backend, 'accel':accel,
                            'reactor_sha256':hashlib.sha256(reactor.read_bytes()).hexdigest(),
                            'records_equal':True, 'branch_isolation':True})
            print(f'PASS: {name} {backend} accel={accel}, {len(records)} sequence/state records, 16 isolated branches', flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    save_rows(args.output, {'checks':reports}, 'reference_records', reference)


if __name__ == '__main__':
    main()
