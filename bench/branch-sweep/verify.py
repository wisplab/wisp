#!/usr/bin/env python3
"""Integration checks for branch isolation and NumPy accel correctness.

Run against a local daemon, with WISP_ACCEL=1 and again with WISP_ACCEL=0.
Usage: WISP_DAEMON_URL=http://127.0.0.1:19000 python3 bench/branch-sweep/verify.py
"""
import json
import os
import urllib.request
import urllib.error
import concurrent.futures
import time

URL = os.environ.get("WISP_DAEMON_URL", "http://127.0.0.1:9000").rstrip("/")


def request(path, body=None, method=None):
    req = urllib.request.Request(
        URL + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method=method,
    )
    with urllib.request.urlopen(req, timeout=120) as response:
        data = response.read()
        return json.loads(data) if data else None


def ok(result):
    assert result["rc"] == 0 and not result.get("error"), result
    return result["stdout"]


def expect_error(path, body, status, message):
    try:
        request(path, body)
    except urllib.error.HTTPError as error:
        assert error.code == status, (error.code, error.read())
        assert message in json.loads(error.read())['error']
    else:
        raise AssertionError(f'expected HTTP {status}')


# Scalar accumulation is an independent reference for the CBLAS shim.
MATH = '''
import numpy as np
count = 0
for dtype in (np.float64, np.float32, np.complex128, np.complex64):
    a = (np.arange(48).reshape(6, 8) / 7).astype(dtype)
    b = (np.arange(40).reshape(8, 5) / 11).astype(dtype)
    if np.iscomplexobj(a):
        a += 0.25j
        b -= 0.125j
    cases = [(a, b), (np.asfortranarray(a), np.asfortranarray(b)),
             (a[::-1], b[:, ::-1]), (a[:, ::2], b[::2]),
             (a.T, a), (a, a.T), (a, a.T.copy()),
             (a[0], b), (a, b[:, 0]), (a[0], a[1])]
    for x, y in cases:
        x2 = x.reshape(1, -1) if x.ndim == 1 else x
        y2 = y.reshape(-1, 1) if y.ndim == 1 else y
        expected = [[sum(x2[i, k].item() * y2[k, j].item()
                         for k in range(x2.shape[1]))
                     for j in range(y2.shape[1])] for i in range(x2.shape[0])]
        actual = np.asarray(x @ y).reshape(x2.shape[0], y2.shape[1])
        np.testing.assert_allclose(actual, expected, rtol=2e-5, atol=2e-5)
        count += 1
    np.testing.assert_allclose(a @ np.zeros((8, 0), dtype=dtype),
                               np.zeros((6, 0), dtype=dtype))
print('math cases:', count)
'''

print('stateless:', ok(request('/v1/eval', {'code': MATH})).strip())
sid = request('/v1/session', {})['session_id']
try:
    path = f'/v1/session/{sid}'
    print('session:', ok(request(path + '/eval', {'code': MATH})).strip())
    ok(request(path + '/eval', {'code': 'x = [1, 2]; big = np.arange(1000000); print("parent")'}))
    for repeat in range(2):
        result = request(path + '/branch', {'codes': [
            'x.append(3); big[0] = 99; branch_only = True; print(x, int(big[0]))',
            'assert x == [1, 2]; assert big[0] == 0; print("isolated")',
            'raise ValueError("expected branch failure")',
            MATH,
        ]})
        assert len(result['results']) == 4, result
        children = result['results']
        assert ok(children[0]).strip() == '[1, 2, 3] 99', children[0]
        assert ok(children[1]).strip() == 'isolated', children[1]
        assert children[2]['rc'] != 0 and 'expected branch failure' in children[2]['stderr'], children[2]
        assert 'math cases: 40' in ok(children[3]), children[3]
        ok(request(path + '/eval', {'code': 'assert x == [1, 2]; assert big[0] == 0; assert "branch_only" not in globals()'}))
        print(f'branch round {repeat + 1}: order, memory growth, isolation, error recovery, math passed')
    expect_error(path + '/branch', {'codes': []}, 400, 'empty codes')
    expect_error(path + '/branch', {'codes': ['pass'] * 4097}, 400, 'exceeds cap')
    for stream in ('stdout', 'stderr'):
        for _ in range(2):
            result = request(path + '/eval', {'code': f'import sys; print("z" * 600000, file=sys.{stream})'})
            ok(result)
            assert result[stream] == 'z' * 600000 + '\n'
        result = request(path + '/eval', {'code': f'print("z" * 1200000, file=sys.{stream})'})
        ok(result)
        assert result[stream].endswith('[wisp: output truncated at 1048576 bytes]\n')
        assert len(result[stream]) < 1048700
        assert ok(request(path + '/eval', {'code': 'print("recovered")'})) == 'recovered\n'
    print('output: cumulative stdout/stderr, truncation and recovery passed')

    for code, close in [
        ("f = open('/Lib/json/__init__.py', 'rb', buffering=0)", 'f.close()'),
        ("import os; fd = os.open('/Lib/json/__init__.py', os.O_RDONLY)", 'os.close(fd)'),
    ]:
        ok(request(path + '/eval', {'code': code}))
        expect_error(path + '/branch', {'codes': ['pass']}, 409, 'close opened handles')
        ok(request(path + '/eval', {'code': close}))
        child = request(path + '/branch', {'codes': [
            "with open('/Lib/json/__init__.py', 'rb', buffering=0) as child_file: print(child_file.read(1))"
        ]})
        assert ok(child['results'][0]) == "b'r'\n"
    print('resources: Python and raw WASI handles rejected; close/reopen recovery passed')

    ok(request(path + '/eval', {'code': 'concurrent_value = 10'}))
    with concurrent.futures.ThreadPoolExecutor() as pool:
        branch = pool.submit(request, path + '/branch', {'codes': [
            'import time; time.sleep(2); print(concurrent_value, int(big[0]), int(big[-1])); child_growth = np.arange(6000000, dtype=np.float64); assert child_growth[-1] == 5999999; assert int(big.sum(dtype=np.int64)) == 499999500000'
        ]})
        time.sleep(0.3)
        assert not branch.done(), 'slow child must still be running'
        assert ok(request(path + '/eval', {'code': 'concurrent_value = 20; big.fill(-7); parent_growth = np.ones(4000000); print("parent free")'})) == 'parent free\n'
        assert not branch.done(), 'parent eval was blocked until child completion'
        assert ok(branch.result()['results'][0]) == '10 0 999999\n'
    assert ok(request(path + '/eval', {'code': 'assert (big == -7).all(); assert parent_growth[-1] == 1; assert "child_growth" not in globals(); print(concurrent_value)'})) == '20\n'
    print('concurrency: parent writes and parent/child growth preserve immutable snapshot')
finally:
    request(f'/v1/session/{sid}', method='DELETE')
print('PASS')
