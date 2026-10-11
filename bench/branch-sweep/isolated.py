#!/usr/bin/env python3
"""macOS warm-service comparison: one fresh COW child per parameter, on both sides.

Run from the repository root: python3 bench/branch-sweep/isolated.py --output PATH
The script starts and stops its own Wisp daemon. No Docker image is required.
"""
import argparse
import ctypes
import hashlib
import errno
import json
import os
import pathlib
import platform
import random
import select
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request

from demo import make_grid, child_code

PROFILE = '(version 1)(deny default)(allow sysctl-read)'
THREAD_ENV = {k: '1' for k in ('OMP_NUM_THREADS', 'OPENBLAS_NUM_THREADS', 'VECLIB_MAXIMUM_THREADS', 'MKL_NUM_THREADS')}


def setup_code(bars, dataset, profile=False):
    if profile and dataset == 'original':
        stages = [
            ('rng_init', 'rng = np.random.default_rng(7)'),
            ('normal', f'_prices = rng.standard_normal({bars})'),
            ('return_scale', '_next = _prices * 0.001; del _prices; _prices = _next; del _next'),
            ('price_cumsum', '_next = np.cumsum(_prices); del _prices; _prices = _next; del _next'),
            ('exp', '_next = np.exp(_prices); del _prices; _prices = _next; del _next'),
            ('price_scale', 'px = 100.0 * _prices; del _prices'),
            ('shared_cumsum', 'cs = np.cumsum(px)'),
        ]
        lines = ['import numpy as np, time, json', '_parent_timings = {}']
        for name, code in stages:
            lines += ['_parent_start = time.perf_counter()', code,
                      f'_parent_timings[{name!r}] = (time.perf_counter() - _parent_start) * 1000000']
        return '\n'.join(lines)
    if dataset == 'original':
        return f'import numpy as np\nrng = np.random.default_rng(7)\npx = 100.0 * np.exp(np.cumsum(rng.standard_normal({bars}) * 0.001))\ncs = np.cumsum(px)'
    # Binary-exact prices and sums avoid differences between NumPy RNG/libm versions.
    return f'import numpy as np\npx = 100.0 + (np.arange({bars}, dtype=np.float64) % 1024) / 1024\ncs = np.cumsum(px)'


def compute_code(fast, slow, profile=False):
    code = child_code(fast, slow).rsplit('print(', 1)[0]
    if not profile:
        return code
    lines = ['import time, json', '_array_timings = {}']
    for line in code.strip().splitlines():
        name = line.split('=', 1)[0].strip()
        lines += ['_array_start = time.perf_counter()', line,
                  f'_array_timings[{name!r}] = (time.perf_counter() - _array_start) * 1000000']
    return '\n'.join(lines)


def restrict(profile):
    error = ctypes.c_char_p()
    if LIB.sandbox_init(profile.encode(), 0, ctypes.byref(error)):
        message = error.value.decode() if error.value else 'unknown sandbox error'
        if error.value:
            LIB.sandbox_free_error(error)
        raise RuntimeError(message)


def denied(fn):
    try:
        fn()
    except OSError as e:
        if e.errno in (errno.EPERM, errno.EACCES):
            return True
        raise
    return False


def probes(canary, port):
    def read():
        with open(canary) as f:
            f.read()
    def write():
        with open(canary, 'a') as f:
            f.write('FAIL')
    def connect():
        with socket.socket() as s:
            s.connect(('127.0.0.1', port))
    return {'file_read': denied(read), 'file_write': denied(write),
            'network_connect': denied(connect),
            'process_exec': denied(lambda: os.posix_spawn('/usr/bin/true', ['true'], {}))}


def native_worker(mode, canary, port):
    import numpy as np
    global LIB
    LIB = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
    LIB.sandbox_init.argtypes = [ctypes.c_char_p, ctypes.c_uint64, ctypes.POINTER(ctypes.c_char_p)]
    LIB.sandbox_init.restype = ctypes.c_int
    LIB.sandbox_free_error.argtypes = [ctypes.c_char_p]
    # Trusted bootstrap, matching the daemon's initialized Python+NumPy snapshot.
    np.cumsum(np.arange(10, dtype=np.float64))
    os.environ.clear()
    checks = {}
    if mode == 'seatbelt':
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                os.close(r)
                os.close(0)
                restrict(PROFILE)
                checks = probes(canary, port)
                checks['process_fork'] = denied(os.fork)
                os.write(w, json.dumps(checks).encode())
                os._exit(0)
            except BaseException:
                os._exit(1)
        os.close(w)
        data = os.read(r, 65536)
        os.close(r)
        _, status = os.waitpid(pid, 0)
        if status:
            raise RuntimeError('sandbox probe child failed')
        checks = json.loads(data)
    if mode == 'seatbelt' and not all(checks.values()):
        raise RuntimeError(f'sandbox probe failed: {checks}')
    print(json.dumps({'ready': True, 'numpy': np.__version__, 'probes': checks}), flush=True)
    for line in sys.stdin:
        request = json.loads(line)
        start = time.perf_counter()
        parent = {'np': np, '__builtins__': __builtins__}
        exec(setup_code(request['bars'], request['dataset'], request.get('profile_arrays', False)), parent)
        setup_ms = (time.perf_counter() - start) * 1000
        begin = time.perf_counter()
        running, results, next_index = {}, {}, 0
        array_timings = {}
        grid = request['grid']
        while next_index < len(grid) or running:
            while next_index < len(grid) and len(running) < request['workers']:
                index = next_index
                next_index += 1
                r, w = os.pipe()
                pid = os.fork()
                if pid == 0:
                    try:
                        os.close(r)
                        # Close all inherited pipes, including the control channel.
                        os.close(0)
                        for fd in running:
                            os.close(fd)
                        if mode == 'seatbelt':
                            restrict(PROFILE)
                        scope = parent.copy()
                        fast, slow = grid[index]
                        exec(compute_code(fast, slow, request.get('profile_arrays', False)), scope)
                        payload = {'index': index, 'pnl': scope['pnl'], 'array_timings': scope.get('_array_timings')}
                        os.write(w, json.dumps(payload).encode())
                        os._exit(0)
                    except BaseException as e:
                        os.write(w, json.dumps({'index': index, 'error': repr(e)}).encode())
                        os._exit(1)
                os.close(w)
                running[r] = pid
            readable, _, _ = select.select(list(running), [], [], 60)
            if not readable:
                raise RuntimeError('native child timed out')
            for fd in readable:
                chunks = []
                while data := os.read(fd, 65536):
                    chunks.append(data)
                os.close(fd)
                pid = running.pop(fd)
                _, status = os.waitpid(pid, 0)
                result = json.loads(b''.join(chunks))
                if status or 'error' in result:
                    raise RuntimeError(f'native child failed: {result}')
                results[result['index']] = result['pnl']
                array_timings[result['index']] = result['array_timings']
        branch_ms = (time.perf_counter() - begin) * 1000
        assert 'pnl' not in parent and 'ma_f' not in parent
        print(json.dumps({'setup_ms': setup_ms, 'branch_ms': branch_ms,
                          'total_ms': setup_ms + branch_ms,
                          'results': [results[i] for i in range(len(grid))],
                          'array_timings': [array_timings[i] for i in range(len(grid))],
                          'parent_timings': parent.get('_parent_timings')}), flush=True)
        del parent


def post(url, body):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=120) as response:
        return json.load(response)


def wisp_run(url, bars, grid, dataset, profile_arrays=False):
    start = time.perf_counter()
    sid = post(url + '/v1/session', {})['session_id']
    path = url + '/v1/session/' + sid
    try:
        code = setup_code(bars, dataset, profile_arrays)
        if profile_arrays and dataset == 'original':
            code += '\nprint(json.dumps(_parent_timings))'
        result = post(path + '/eval', {'code': code})
        if result['rc']:
            raise RuntimeError(result)
        setup_ms = (time.perf_counter() - start) * 1000
        begin = time.perf_counter()
        codes = [compute_code(f, s, profile_arrays) + f'\nprint({f}, {s}, repr(pnl))'
                 + ('\nprint(json.dumps(_array_timings))' if profile_arrays else '') for f, s in grid]
        response = post(path + '/branch', {'codes': codes})
        branch_ms = (time.perf_counter() - begin) * 1000
        if len(response['results']) != len(grid):
            raise RuntimeError('incomplete Wisp results')
        values = []
        array_timings = []
        for expected, item in zip(grid, response['results']):
            if item['rc'] or item.get('error'):
                raise RuntimeError(item)
            lines = item['stdout'].splitlines()
            fast, slow, pnl = lines[0].split()
            array_timings.append(json.loads(lines[1]) if profile_arrays else None)
            if [int(fast), int(slow)] != expected:
                raise RuntimeError('Wisp result order mismatch')
            values.append(float(pnl))
        check = post(path + '/eval', {'code': "assert 'pnl' not in globals() and 'ma_f' not in globals()"})
        if check['rc']:
            raise RuntimeError(check)
        return {'setup_ms': setup_ms, 'branch_ms': branch_ms, 'total_ms': setup_ms + branch_ms,
                'snapshot_ms': response['snapshot_us'] / 1000, 'results': values,
                'snapshot_bytes': response.get('snapshot_bytes'),
                'snapshot_backend': response.get('snapshot_backend'), 'array_timings': array_timings,
                'parent_timings': json.loads(result['stdout']) if result['stdout'] else None,
                'child_timings': [item.get('timings') for item in response['results']]}
    finally:
        with urllib.request.urlopen(urllib.request.Request(path, method='DELETE')):
            pass


def vm_counters():
    import re
    output = subprocess.check_output(['vm_stat'], text=True)
    keys = ('Pages free', 'Pages occupied by compressor', 'Swapins', 'Swapouts', 'Pageins', 'Pageouts')
    values = dict(re.findall(r'^([\w -]+):\s+(\d+)\.', output, re.MULTILINE))
    return {key: int(values[key]) for key in keys}


def save_result(path, result):
    # Keep every raw sample; one line per round makes generated-data diffs
    # manageable while leaving settings and summaries easy to inspect.
    header = {key: value for key, value in result.items() if key != 'rounds'}
    rows = ',\n'.join('    ' + json.dumps(row, separators=(',', ':'))
                       for row in result['rounds'])
    path.write_text(json.dumps(header, indent=2)[:-2]
                    + ',\n  "rounds": [\n' + rows + '\n  ]\n}\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bars', type=int, default=2_000_000)
    parser.add_argument('--grid', type=int, default=40)
    parser.add_argument('--workers', type=int, default=8)
    parser.add_argument('--repeats', type=int, default=7)
    parser.add_argument('--dataset', choices=('original', 'binary-exact'), default='original')
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--profile-arrays', action='store_true', help='diagnostic per-array timers; keep off for headline comparisons')
    parser.add_argument('--snapshot-backend', choices=('auto', 'file'), default='auto')
    parser.add_argument('--no-prelink', action='store_true')
    parser.add_argument('--wisp-binary', type=pathlib.Path, help='baseline or candidate release daemon')
    parser.add_argument('--wisp-reactor', type=pathlib.Path, help='explicit reactor for reproducible RNG comparisons')
    parser.add_argument('--idle-guard', action='store_true', help='require quiet CPU, normal memory pressure and minimal swapping before warmup/each round')
    parser.add_argument('--idle-timeout', type=int, default=600, help='maximum seconds to wait at each load gate')
    parser.add_argument('--baseline-binary', type=pathlib.Path, help='also interleave an older Wisp binary in every round')
    args = parser.parse_args()
    if platform.system() != 'Darwin':
        parser.error('this Seatbelt benchmark requires macOS')
    if args.bars <= 300 or not 1 <= args.grid <= 64 or args.workers < 1 or args.repeats < 3 or args.idle_timeout < 1:
        parser.error('invalid bars/grid/workers/repeats')
    root = pathlib.Path(__file__).resolve().parents[2]
    load_log = args.output.with_suffix('.load.jsonl')
    if args.idle_guard:
        from load_guard import wait_until_idle
        wait_until_idle(load_log, 'before-startup', args.idle_timeout)
    binary = (args.wisp_binary or root / 'target/release/wisp-runtime').resolve()
    reactor = (args.wisp_reactor or root / 'runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor.wasm').resolve()
    grid = [list(pair) for pair in make_grid(args.grid)]
    native = {}
    daemon = None
    baseline_daemon = None
    with tempfile.TemporaryDirectory(prefix='wisp-isolated-') as tmp:
        canary = pathlib.Path(tmp) / 'denied-canary'
        canary.write_text('not visible inside native sandbox')
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            probe_port = listener.getsockname()[1]
            with socket.socket() as reserve:
                reserve.bind(('127.0.0.1', 0))
                port = reserve.getsockname()[1]
            url = f'http://127.0.0.1:{port}'
            env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), **THREAD_ENV}
            try:
                metadata = {}
                for mode in ('plain', 'seatbelt'):
                    t = time.perf_counter()
                    native[mode] = subprocess.Popen([sys.executable, '-u', str(pathlib.Path(__file__).resolve()),
                        '--worker', mode, str(canary), str(probe_port)], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, text=True, env=env)
                    metadata[mode] = json.loads(native[mode].stdout.readline())
                    metadata[mode]['startup_ms'] = (time.perf_counter() - t) * 1000
                log_path = pathlib.Path(tmp) / 'daemon.log'
                with log_path.open('w') as log:
                    t = time.perf_counter()
                    daemon = subprocess.Popen([str(binary)], cwd=root,
                        env={**env, 'WISP_BIND': f'127.0.0.1:{port}', 'WISP_WORKERS': str(args.workers),
                             'WISP_BRANCH_PARALLEL': str(args.workers), 'WISP_ACCEL': '1',
                             'WISP_PYTHON_WASM': str(reactor),
                             'WISP_SNAPSHOT_PATH': str(pathlib.Path(tmp) / 'snapshot.bin'),
                             'WISP_BRANCH_SNAPSHOT': args.snapshot_backend,
                             'WISP_BRANCH_PRELINK': '0' if args.no_prelink else '1'}, stdout=log, stderr=log)
                    deadline = time.monotonic() + 120
                    while True:
                        if daemon.poll() is not None:
                            raise RuntimeError(log_path.read_text())
                        try:
                            with socket.create_connection(('127.0.0.1', port), timeout=.2):
                                break
                        except OSError:
                            if time.monotonic() > deadline:
                                raise RuntimeError('daemon startup timeout')
                            time.sleep(.1)
                    metadata['wisp'] = {'startup_ms': (time.perf_counter() - t) * 1000}
                    sid = post(url + '/v1/session', {})['session_id']
                    path = url + '/v1/session/' + sid
                    probe_code = f'''
import numpy as np, sys, json, wisp
def blocked(f):
    try:
        f()
    except wisp.WispError:
        return True
    return False
checks = {{
    'file_read': blocked(lambda: wisp.file_read({str(canary)!r})),
    'file_write': blocked(lambda: wisp.file_write({str(canary)!r}, 'FAIL')),
    'network_connect': blocked(lambda: wisp.web_fetch('http://127.0.0.1:{probe_port}')),
    'process_exec': blocked(lambda: wisp.shell(['/usr/bin/true'])),
}}
assert all(checks.values()), checks
print(json.dumps({{'python': sys.version.split()[0], 'numpy': np.__version__, 'probes': checks}}))
'''
                    try:
                        inspection = post(path + '/eval', {'code': probe_code})
                        if inspection['rc']:
                            raise RuntimeError(inspection)
                        metadata['wisp'].update(json.loads(inspection['stdout']))
                    finally:
                        with urllib.request.urlopen(urllib.request.Request(path, method='DELETE')):
                            pass
                    modes = ['plain', 'seatbelt', 'wisp']
                    baseline_url = None
                    if args.baseline_binary:
                        with socket.socket() as reserve:
                            reserve.bind(('127.0.0.1', 0))
                            baseline_port = reserve.getsockname()[1]
                        baseline_url = f'http://127.0.0.1:{baseline_port}'
                        baseline_log_path = pathlib.Path(tmp) / 'baseline.log'
                        t = time.perf_counter()
                        with baseline_log_path.open('w') as baseline_log:
                            baseline_daemon = subprocess.Popen([str(args.baseline_binary.resolve())], cwd=root,
                                env={**env, 'WISP_BIND': f'127.0.0.1:{baseline_port}', 'WISP_WORKERS': str(args.workers),
                                     'WISP_BRANCH_PARALLEL': str(args.workers), 'WISP_ACCEL': '1',
                                     'WISP_PYTHON_WASM': str(reactor),
                                     'WISP_SNAPSHOT_PATH': str(pathlib.Path(tmp) / 'baseline-snapshot.bin')},
                                stdout=baseline_log, stderr=baseline_log)
                        deadline = time.monotonic() + 120
                        while True:
                            if baseline_daemon.poll() is not None:
                                raise RuntimeError(baseline_log_path.read_text())
                            try:
                                with socket.create_connection(('127.0.0.1', baseline_port), timeout=.2):
                                    break
                            except OSError:
                                if time.monotonic() > deadline:
                                    raise RuntimeError('baseline daemon startup timeout')
                                time.sleep(.1)
                        metadata['wisp_before'] = {'startup_ms': (time.perf_counter() - t) * 1000,
                            'binary_sha256': hashlib.sha256(args.baseline_binary.read_bytes()).hexdigest()}
                        sid = post(baseline_url + '/v1/session', {})['session_id']
                        path = baseline_url + '/v1/session/' + sid
                        try:
                            inspection = post(path + '/eval', {'code': probe_code})
                            if inspection['rc']:
                                raise RuntimeError(inspection)
                            metadata['wisp_before'].update(json.loads(inspection['stdout']))
                        finally:
                            with urllib.request.urlopen(urllib.request.Request(path, method='DELETE')):
                                pass
                        modes.append('wisp_before')
                    def run(mode):
                        if mode in ('wisp', 'wisp_before'):
                            return wisp_run(baseline_url if mode == 'wisp_before' else url, args.bars, grid, args.dataset, args.profile_arrays)
                        p = native[mode]
                        t = time.perf_counter()
                        p.stdin.write(json.dumps({'bars': args.bars, 'grid': grid, 'workers': args.workers, 'dataset': args.dataset, 'profile_arrays': args.profile_arrays}) + '\n')
                        p.stdin.flush()
                        line = p.stdout.readline()
                        if not line:
                            raise RuntimeError(f'{mode} worker exited: {p.poll()}')
                        value = json.loads(line)
                        value['total_ms'] = (time.perf_counter() - t) * 1000
                        return value
                    # Discard one full sweep per backend, then shuffle order in every round.
                    if args.idle_guard:
                        wait_until_idle(load_log, 'before-warmup', args.idle_timeout)
                    for mode in modes:
                        run(mode)
                    rounds = []
                    rng = random.Random(71)
                    max_error = 0.0
                    for i in range(args.repeats):
                        order = modes.copy()
                        rng.shuffle(order)
                        row = {'order': order, 'vm_before': vm_counters()}
                        if args.idle_guard:
                            row['load_gate'] = wait_until_idle(load_log, f'round-{i+1}', args.idle_timeout)
                            row['vm_before'] = vm_counters()
                        for mode in order:
                            row[mode] = run(mode)
                        for mode in (m for m in modes if m != 'wisp'):
                            error = max(abs(a-b) for a,b in zip(row[mode]['results'], row['wisp']['results']))
                            max_error = max(max_error, error)
                            if not error <= 1e-9:
                                raise RuntimeError(f'result mismatch: {error}')
                        row['vm_after'] = vm_counters()
                        print(f'round {i+1}: ' + ', '.join(f"{m}={row[m]['total_ms']:.1f}ms" for m in modes), flush=True)
                        rounds.append(row)
                        if args.idle_guard:
                            checkpoint = args.output.with_suffix('.rounds.jsonl')
                            with checkpoint.open('a') as file:
                                file.write(json.dumps(row, separators=(',', ':'))+'\n')
                    summary = {}
                    for mode in modes:
                        summary[mode] = {}
                        for metric in ('setup_ms', 'branch_ms', 'total_ms'):
                            data = [r[mode][metric] for r in rounds]
                            q = statistics.quantiles(data, n=4, method='inclusive')
                            summary[mode][metric] = {'median': statistics.median(data), 'q1': q[0], 'q3': q[2], 'min': min(data), 'max': max(data)}
                    ratios = [r['wisp']['total_ms'] / r['seatbelt']['total_ms'] for r in rounds]
                    quartiles = statistics.quantiles(ratios, n=4, method='inclusive')
                    comparison = {'paired_total_ratio_median': statistics.median(ratios),
                                  'paired_total_ratio_q1': quartiles[0], 'paired_total_ratio_q3': quartiles[2],
                                  'total_ratio_of_medians': summary['wisp']['total_ms']['median'] / summary['seatbelt']['total_ms']['median']}
                    if args.baseline_binary:
                        paired = [r['wisp']['total_ms'] / r['wisp_before']['total_ms'] for r in rounds]
                        comparison['paired_after_before_ratio_median'] = statistics.median(paired)
                        comparison['after_before_ratio_of_medians'] = summary['wisp']['total_ms']['median'] / summary['wisp_before']['total_ms']['median']
                    result = {'hardware': {'platform': platform.platform(), 'cpu': subprocess.check_output(['sysctl', '-n', 'machdep.cpu.brand_string'], text=True).strip(),
                              'memory_bytes': int(subprocess.check_output(['sysctl', '-n', 'hw.memsize'], text=True)), 'cpus': os.cpu_count()},
                              'python': sys.version, 'git_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(), 'bars': args.bars, 'grid': grid, 'workers': args.workers,
                              'wisp_binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                              'wisp_reactor_path': str(reactor), 'wisp_reactor_sha256': hashlib.sha256(reactor.read_bytes()).hexdigest(),
                              'repeats': args.repeats, 'dataset': args.dataset,
                              'profile_arrays': args.profile_arrays, 'requested_snapshot_backend': args.snapshot_backend,
                              'idle_guard': args.idle_guard, 'idle_timeout': args.idle_timeout if args.idle_guard else None,
                              'prelink': not args.no_prelink,
                              'profile': PROFILE, 'metadata': metadata,
                              'max_absolute_error': max_error, 'summary': summary, 'comparison': comparison, 'rounds': rounds}
                    args.output.parent.mkdir(parents=True, exist_ok=True)
                    save_result(args.output, result)
                    print(json.dumps(summary, indent=2))
            finally:
                for p in native.values():
                    if p.stdin:
                        p.stdin.close()
                    try:
                        p.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        p.kill()
                        p.wait()
                for process in (daemon, baseline_daemon):
                    if process is not None:
                        process.terminate()
                        try:
                            process.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--worker':
        native_worker(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else:
        main()
