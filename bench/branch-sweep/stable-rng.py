#!/usr/bin/env python3
"""Run the unchanged fair workload in balanced, single-service ABBA blocks."""
import argparse
import json
import pathlib
import statistics
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def distribution(values):
    q = statistics.quantiles(values, n=4, method='inclusive')
    return {'median': statistics.median(values), 'q1': q[0], 'q3': q[2],
            'min': min(values), 'max': max(values), 'n': len(values)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference-reactor', type=pathlib.Path, required=True)
    parser.add_argument('--output-prefix', type=pathlib.Path, required=True)
    parser.add_argument('--repeats', type=int, default=9)
    parser.add_argument('--idle-timeout', type=int, default=600)
    args = parser.parse_args()
    assert args.repeats >= 3
    args.output_prefix.parent.mkdir(parents=True, exist_ok=True)
    batches = []
    for block, version in enumerate(['before', 'after', 'after', 'before']):
        path = pathlib.Path(str(args.output_prefix)+f'-b{block}-{version}.json')
        command = [sys.executable, str(HERE/'isolated.py'), '--bars', '2000000',
                   '--grid', '40', '--workers', '8', '--repeats', str(args.repeats),
                   '--idle-guard', '--idle-timeout', str(args.idle_timeout), '--output', str(path)]
        if version == 'before':
            command += ['--wisp-reactor', str(args.reference_reactor.resolve())]
        print(f'block {block}: {version}', flush=True)
        subprocess.run(command, cwd=ROOT, check=True)
        result = json.loads(path.read_text())
        batches.append({'block':block, 'version':version, 'path':str(path), 'data':result})
    assert len({b['data']['wisp_binary_sha256'] for b in batches}) == 1
    for version in ['before', 'after']:
        assert len({b['data']['wisp_reactor_sha256'] for b in batches if b['version']==version}) == 1
    assert batches[0]['data']['wisp_reactor_sha256'] != batches[1]['data']['wisp_reactor_sha256']
    values = [r['wisp']['results'] for b in batches for r in b['data']['rounds']]
    assert all(value==values[0] for value in values)
    summary = {}
    for version in ['before', 'after']:
        rounds = [r for b in batches if b['version']==version for r in b['data']['rounds']]
        summary[version] = {mode:{metric:distribution([r[mode][metric] for r in rounds])
                            for metric in ['setup_ms', 'branch_ms', 'total_ms']}
                            for mode in ['plain', 'seatbelt', 'wisp']}
        summary[version]['paired_wisp_native_ratio'] = distribution(
            [r['wisp']['total_ms']/r['seatbelt']['total_ms'] for r in rounds])
    before = summary['before']['wisp']['total_ms']['median']
    after = summary['after']['wisp']['total_ms']['median']
    pairs = []
    for first, second in [(batches[0],batches[1]),(batches[3],batches[2])]:
        before_data, after_data = first['data'], second['data']
        bw = before_data['summary']['wisp']['total_ms']['median']
        aw = after_data['summary']['wisp']['total_ms']['median']
        bn = before_data['summary']['seatbelt']['total_ms']['median']
        an = after_data['summary']['seatbelt']['total_ms']['median']
        pairs.append({'before_block':first['block'], 'after_block':second['block'],
                      'wisp_after_before_ratio':aw/bw, 'native_after_before_ratio':an/bn,
                      'normalized_after_before_ratio':(aw/an)/(bw/bn)})
    result = {'order':['before','after','after','before'], 'repeats_per_block':args.repeats,
              'bars':2000000, 'grid_count':40, 'workers':8,
              'summary':summary, 'comparison':{'after_before_ratio_of_medians':after/before,
              'total_time_reduction_pct':100*(1-after/before), 'saved_ms':before-after,
              'after_wisp_native_ratio_of_medians':after/summary['after']['seatbelt']['total_ms']['median']},
              'adjacent_block_pairs':pairs,
              'batches':[{k:v for k,v in b.items() if k!='data'} for b in batches]}
    output = pathlib.Path(str(args.output_prefix)+'-summary.json')
    output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result['comparison'], indent=2), flush=True)


if __name__ == '__main__':
    main()
