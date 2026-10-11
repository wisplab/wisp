#!/usr/bin/env python3
"""RNG-only diagnostic: sequential old/new daemons, randomized block order.

This does not replace isolated.py's complete fair benchmark. Allocation is
included in draw timing; hashing, HTTP, service bootstrap are outside it.
"""
import argparse
import importlib.util
import json
import pathlib
import random
import statistics

spec = importlib.util.spec_from_file_location('verify_rng', pathlib.Path(__file__).with_name('verify-rng.py'))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-reactor', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--blocks', type=int, default=3)
    parser.add_argument('--repeats', type=int, default=9)
    args = parser.parse_args()
    assert args.blocks >= 2 and args.repeats >= 3
    rows = []
    order_rng = random.Random(71)
    for block in range(args.blocks):
        order = [('before', args.reference_reactor.resolve()), ('after', verify.REACTOR)]
        order_rng.shuffle(order)
        for name, reactor in order:
            with verify.daemon(reactor, 'auto', '1') as url:
                sid = verify.post(url, '/v1/session', {})['session_id']
                path = '/v1/session/'+sid
                code = f'''
import numpy as np, time, json, hashlib, random
methods = ['raw', 'normal64', 'normal32', 'uniform']
samples = []
order_rng = random.Random(71)
for repeat in range({args.repeats+1}):
    order_rng.shuffle(methods)
    for method in methods:
        rng = np.random.default_rng(7)
        start = time.perf_counter()
        if method == 'raw': a = rng.bit_generator.random_raw(2000000)
        elif method == 'normal64': a = rng.standard_normal(2000000)
        elif method == 'normal32': a = rng.standard_normal(2000000, dtype=np.float32)
        else: a = rng.random(2000000)
        elapsed = (time.perf_counter()-start)*1000
        if repeat:
            samples.append({{'method':method, 'ms':elapsed,
                            'digest':hashlib.sha256(a.tobytes()).hexdigest(),
                            'state':rng.bit_generator.state}})
        del a
print(json.dumps(samples))
'''
                samples = json.loads(verify.evaluate(url, path, code))
                rows.append({'block':block, 'version':name, 'samples':samples})
                with verify.urllib.request.urlopen(verify.urllib.request.Request(url+path, method='DELETE')):
                    pass
                print(f'{name} block {block}: '+str({m:round(statistics.median(s['ms'] for s in samples if s['method']==m),3) for m in ['raw','normal64','normal32','uniform']}), flush=True)
    summary = {}
    for method in ['raw', 'normal64', 'normal32', 'uniform']:
        data = [s for row in rows for s in row['samples'] if s['method']==method]
        assert len({json.dumps([s['digest'],s['state']], sort_keys=True) for s in data}) == 1
        summary[method] = {}
        for name in ['before','after']:
            times = [s['ms'] for row in rows if row['version']==name for s in row['samples'] if s['method']==method]
            q = statistics.quantiles(times, n=4, method='inclusive')
            summary[method][name] = {'median_ms':statistics.median(times), 'q1':q[0], 'q3':q[2], 'min':min(times), 'max':max(times)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    verify.save_rows(args.output, {'diagnostic_only':True, 'draws':2000000, 'blocks':args.blocks, 'repeats':args.repeats,
                                      'before_sha256':verify.hashlib.sha256(args.reference_reactor.read_bytes()).hexdigest(),
                                      'after_sha256':verify.hashlib.sha256(verify.REACTOR.read_bytes()).hexdigest(),
                                      'summary':summary}, 'rows', rows)


if __name__ == '__main__':
    main()
