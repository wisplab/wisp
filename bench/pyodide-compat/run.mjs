#!/usr/bin/env node
// Phase 0 Spike 2: Pyodide pandas/sklearn/numpy compat vs CPython.
//
// For each snippet in corpus.json, run in Pyodide and in a local CPython venv,
// JSON-serialize `result`, diff. Report per-category correctness rate.

import { loadPyodide } from 'pyodide';
import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const CORPUS_PATH = join(HERE, 'corpus.json');
const VENV_PYTHON = join(HERE, '.venv', 'bin', 'python3');

const FILTER = process.argv[2]; // optional category filter

function fail(msg) {
  console.error(`FATAL: ${msg}`);
  process.exit(1);
}

if (!existsSync(VENV_PYTHON)) {
  fail(`venv not found at ${VENV_PYTHON}. Run ./setup.sh first.`);
}

const corpus = JSON.parse(readFileSync(CORPUS_PATH, 'utf8'));

// ---- CPython side ------------------------------------------------------

// Make any value JSON-serializable. Pandas produces tuple-keyed dicts (multi-level
// column index) that vanilla json.dumps rejects. Recursively stringify dict keys
// and round floats to handle signed-zero / FP cosmetic differences.
const PRELUDE = `
def __wisp_normalize(v):
    if isinstance(v, dict):
        return {str(k): __wisp_normalize(val) for k, val in v.items()}
    if isinstance(v, (list, tuple)):
        return [__wisp_normalize(x) for x in v]
    if isinstance(v, float):
        # round to 6 decimals; treat -0.0 as 0.0
        r = round(v, 6)
        return 0.0 if r == 0.0 else r
    return v
`;

function runInCPython(snippet) {
  const importLines = snippet.imports
    .map(spec => `import ${spec}`)
    .join('\n');
  const wrapped = `import json, sys
${importLines}
${PRELUDE}
try:
${snippet.code.split('\n').map(l => '    ' + l).join('\n')}
    print(json.dumps(__wisp_normalize(result), default=str, sort_keys=True))
except Exception as e:
    print(json.dumps({'__error__': type(e).__name__ + ': ' + str(e)}))
`;
  const t0 = process.hrtime.bigint();
  const r = spawnSync(VENV_PYTHON, ['-c', wrapped], { encoding: 'utf8' });
  const t1 = process.hrtime.bigint();
  if (r.status !== 0) {
    return { ok: false, error: `cpython exit ${r.status}: ${r.stderr.slice(0, 200)}`, ms: Number(t1 - t0) / 1e6 };
  }
  const out = r.stdout.trim().split('\n').pop();
  return { ok: true, output: out, ms: Number(t1 - t0) / 1e6 };
}

// ---- Pyodide side ------------------------------------------------------

async function runInPyodide(pyodide, snippet) {
  const importLines = snippet.imports
    .map(spec => `import ${spec}`)
    .join('\n');
  const wrapped = `import json
${importLines}
${PRELUDE}
${snippet.code}
__wisp_result = json.dumps(__wisp_normalize(result), default=str, sort_keys=True)
`;
  const t0 = process.hrtime.bigint();
  try {
    await pyodide.runPythonAsync(wrapped);
    const out = pyodide.globals.get('__wisp_result');
    const t1 = process.hrtime.bigint();
    return { ok: true, output: out, ms: Number(t1 - t0) / 1e6 };
  } catch (e) {
    const t1 = process.hrtime.bigint();
    return { ok: false, error: String(e).slice(0, 300), ms: Number(t1 - t0) / 1e6 };
  }
}

// ---- main --------------------------------------------------------------

function packagesFor(category) {
  const base = ['numpy'];
  if (category === 'pandas') return [...base, 'pandas'];
  if (category === 'sklearn') return [...base, 'pandas', 'scikit-learn'];
  return base;
}

async function main() {
  console.log('# Pyodide compat — Phase 0 Spike 2\n');
  const t0 = Date.now();
  console.log('Loading Pyodide...');
  const pyodide = await loadPyodide({ stdout: () => {}, stderr: () => {} });
  console.log(`Pyodide loaded in ${Date.now() - t0}ms\n`);

  const results = [];

  for (const [category, snippets] of Object.entries(corpus)) {
    if (FILTER && FILTER !== category) continue;
    console.log(`\n## ${category} (${snippets.length} snippets)\n`);

    const pkgs = packagesFor(category);
    console.log(`Loading Pyodide packages: ${pkgs.join(', ')}...`);
    const tp0 = Date.now();
    await pyodide.loadPackage(pkgs);
    console.log(`Loaded in ${Date.now() - tp0}ms\n`);

    for (const snippet of snippets) {
      const cpy = runInCPython(snippet);
      const pyo = await runInPyodide(pyodide, snippet);

      let verdict;
      if (!cpy.ok && !pyo.ok) verdict = 'BOTH_FAIL';
      else if (!cpy.ok) verdict = 'CPYTHON_FAIL';
      else if (!pyo.ok) verdict = 'PYODIDE_FAIL';
      else if (cpy.output === pyo.output) verdict = 'MATCH';
      else verdict = 'MISMATCH';

      results.push({ category, name: snippet.name, verdict, cpy, pyo });

      const status = verdict === 'MATCH' ? '✓' : '✗';
      console.log(`${status} ${snippet.name.padEnd(36)} ${verdict.padEnd(14)} cpy=${cpy.ms.toFixed(0)}ms pyo=${pyo.ms.toFixed(0)}ms`);
      if (verdict === 'MISMATCH') {
        console.log(`    cpy: ${cpy.output.slice(0, 120)}`);
        console.log(`    pyo: ${pyo.output.slice(0, 120)}`);
      } else if (verdict === 'PYODIDE_FAIL') {
        console.log(`    pyo error: ${pyo.error}`);
      } else if (verdict === 'CPYTHON_FAIL') {
        console.log(`    cpy error: ${cpy.error}`);
      }
    }
  }

  // ---- summary ---------------------------------------------------------
  console.log('\n## Summary\n');
  const cats = {};
  for (const r of results) {
    cats[r.category] ??= { total: 0, match: 0, pyo_fail: 0, mismatch: 0, both_fail: 0, cpy_fail: 0 };
    cats[r.category].total++;
    if (r.verdict === 'MATCH') cats[r.category].match++;
    else if (r.verdict === 'PYODIDE_FAIL') cats[r.category].pyo_fail++;
    else if (r.verdict === 'CPYTHON_FAIL') cats[r.category].cpy_fail++;
    else if (r.verdict === 'BOTH_FAIL') cats[r.category].both_fail++;
    else cats[r.category].mismatch++;
  }
  console.log('| Category | Total | Match | Mismatch | Pyodide fail | CPython fail | Both fail | Match % |');
  console.log('|---|---|---|---|---|---|---|---|');
  for (const [cat, s] of Object.entries(cats)) {
    const pct = ((s.match / s.total) * 100).toFixed(1);
    console.log(`| ${cat} | ${s.total} | ${s.match} | ${s.mismatch} | ${s.pyo_fail} | ${s.cpy_fail} | ${s.both_fail} | ${pct}% |`);
  }
  console.log('');
}

main().catch(e => fail(e.stack || e.message));
