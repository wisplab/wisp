#!/usr/bin/env bash
# scripts/pandas/01-cython.sh
#
# Stage 1: cythonize all pandas 1.5.3 .pyx → .c via pandas's OWN
# setup.py path, not a bare `cython` CLI.
#
# Why: bare `cython parsers.pyx` fails with "Converting to Python
# object not allowed without gil" on `kh_get_str_starts_item` —
# .pxd declares the param non-const but the C header has it const,
# and Cython refuses the implicit widening in nogil context. None
# of the obvious patches (add const to .pxd / drop from .pyx / cast
# at call site) satisfy it. But `maybe_cythonize(extensions, ...)`
# inside pandas/setup.py, which is what pandas's CI uses, compiles
# cleanly. There's some Cython.Compiler.Options state set by
# cythonize() that our bare CLI invocation misses.
#
# Pragmatic choice: just call the path that works.
#
# Requires Cython 0.29 + numpy + setuptools<70 (for pkg_resources)
# in the venv at $CYTHON_VENV.
set -euo pipefail
cd "$(dirname "$0")"

ROOT="$(cd ../../runtime/cpython-wasi && pwd)"
PANDAS_DIR="$ROOT/vendor/pandas-1.5.3"
CYTHON_VENV="${CYTHON_VENV:-/tmp/cython-venv}"

if [ ! -x "$CYTHON_VENV/bin/cython" ]; then
  echo "FATAL: Cython venv not found at $CYTHON_VENV. Create with:" >&2
  echo "  python3 -m venv $CYTHON_VENV" >&2
  echo "  HTTP_PROXY=... $CYTHON_VENV/bin/pip install 'cython<3' numpy 'setuptools<70'" >&2
  exit 1
fi

echo "==> Running pandas's own maybe_cythonize() to generate .c files"
cd "$PANDAS_DIR"
"$CYTHON_VENV/bin/python" -c "
import sys
sys.path.insert(0, '.')
import setup
result = setup.maybe_cythonize(setup.extensions, compiler_directives=setup.directives)
print('cythonized', len(result), 'extensions')
"

echo
echo "==> .c files generated:"
find "$PANDAS_DIR/pandas/_libs" -name "*.c" | wc -l
echo "  (counted .c files in pandas/_libs)"
echo
echo "Stage 1 done."
