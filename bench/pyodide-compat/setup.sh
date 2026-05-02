#!/usr/bin/env bash
# Phase 0 Spike 2 setup: install Node deps + create CPython venv with pandas/sklearn.
set -euo pipefail
cd "$(dirname "$0")"

echo "==> npm install pyodide..."
npm install --no-audit --no-fund

echo "==> Creating CPython venv at .venv..."
python3 -m venv .venv
.venv/bin/pip install --upgrade pip setuptools wheel

echo "==> Installing pandas / numpy / scikit-learn into venv..."
.venv/bin/pip install --quiet 'numpy>=2.0' 'pandas>=2.2' 'scikit-learn>=1.5'

echo "==> Versions:"
.venv/bin/python3 -c "import sys, numpy, pandas, sklearn; print(f'  python  {sys.version.split()[0]}'); print(f'  numpy   {numpy.__version__}'); print(f'  pandas  {pandas.__version__}'); print(f'  sklearn {sklearn.__version__}')"

echo "==> Done. Run with: npm test"
