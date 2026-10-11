"""Observe macOS background load outside benchmark timing."""
import datetime
import json
import pathlib
import re
import subprocess
import time


def swaps():
    output = subprocess.check_output(['vm_stat'], text=True)
    return {name: int(re.search(r'^'+name+r':\s+(\d+)', output, re.M)[1])
            for name in ['Swapins', 'Swapouts']}


def sample():
    before = swaps()
    start = time.monotonic()
    output = subprocess.check_output(['top', '-l', '2', '-s', '2', '-n', '0'], text=True)
    after = swaps()
    seconds = time.monotonic()-start
    return {'at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'idle_pct': float(re.findall(r'([\d.]+)% idle', output)[-1]),
            'memory_pressure_level': int(subprocess.check_output(
                ['sysctl', '-n', 'kern.memorystatus_vm_pressure_level'], text=True)),
            'swapins_per_s': (after['Swapins']-before['Swapins'])/seconds,
            'swapouts_per_s': (after['Swapouts']-before['Swapouts'])/seconds}


def wait_until_idle(log_path, phase, timeout=600):
    """Require three consecutive quiet samples; keep every rejected sample."""
    deadline = time.monotonic()+timeout
    consecutive = 0
    samples = []
    next_message = 0
    pathlib.Path(log_path).parent.mkdir(parents=True, exist_ok=True)
    while True:
        value = sample()
        value['phase'] = phase
        value['quiet'] = (value['idle_pct'] >= 80
                          and value['memory_pressure_level'] == 1
                          and value['swapouts_per_s'] == 0
                          and value['swapins_per_s'] <= 64)
        samples.append(value)
        with pathlib.Path(log_path).open('a') as log:
            log.write(json.dumps(value)+'\n')
        consecutive = consecutive+1 if value['quiet'] else 0
        if consecutive >= 3:
            return {'accepted': samples[-3:], 'samples_observed': len(samples)}
        now = time.monotonic()
        if now >= next_message:
            print(f"load gate {phase}: idle={value['idle_pct']:.1f}% "
                  f"pressure={value['memory_pressure_level']} "
                  f"swap in/out={value['swapins_per_s']:.0f}/{value['swapouts_per_s']:.0f} pages/s", flush=True)
            next_message = now+30
        if now >= deadline:
            raise RuntimeError(f'background load did not stabilize for {phase}; observations: {log_path}')
        time.sleep(3)
