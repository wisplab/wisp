# scripts/pandas/ — WIP

## Status as of 2026-05-17 early morning

Two sessions in. **Still stuck on `parsers.pyx`. Root cause now
identified; remediation is deep enough to need a dedicated day.**

### Root cause (confirmed)

`pandas/_libs/src/klib/khash_python.h:421` declares the real C function:
```c
khuint_t kh_get_str_starts_item(const kh_str_starts_t* table,
                                const char* key)
```
But `pandas/_libs/khash.pxd:104` declares it WITHOUT `const`:
```python
khuint_t kh_get_str_starts_item(kh_str_starts_t* table,
                                char* key) nogil
```
Call sites in `parsers.pyx` use `const char *word` + `const
kh_str_starts_t *<hashset>` (matching the C reality). Cython sees
the .pxd mismatch and tries to round-trip through Python, which is
illegal under `nogil`.

### What's been tried

  1. Patch khash.pxd to add `const` to both params. **Doesn't help** —
     Cython 0.29.37 and 3.0.12 both still reject. Maybe Cython's PXD
     parser doesn't propagate const equality through pointer-to-cdef-
     struct in nogil contexts.
  2. Drop `const` from parsers.pyx call sites (`word` becomes
     `char *`, hashsets become non-const). **Doesn't help** — same
     error.
  3. Add explicit `<char *>`+`<kh_str_starts_t *>` casts at call
     sites. **Doesn't help** — same error.
  4. Combinations of (1)+(2)+(3). **Doesn't help**.
  5. Strip `nogil` markers entirely from parsers.pyx (turn
     `with nogil:` → `if True:`, `cdef ... nogil:` → `cdef ... :`).
     **Compiles past 1872 but immediately hits new errors** at 1930:
     `Cannot convert 'int *' to Python object` on `&ret` in
     `kh_put_float64(table, val, &ret)` and `'kh_resize_float64' is
     not a constant, variable or function identifier` at 1934.
     Stripping nogil cascades — multiple call paths break.

### Why this is a "needs a day" problem

pandas's own CI compiles parsers.pyx fine with Cython 0.29.x. So
there's *some* environment difference between `cython parsers.pyx`
standalone and `cythonize(extensions, ...)` via the setup.py path
that pandas uses. Candidate differences:
  - cythonize() may set additional compiler directives we're not
    setting (e.g. `c_string_encoding`, `cdivision`).
  - cythonize() may pre-process .pxd files to add type aliases.
  - The maybe_cythonize() wrapper in pandas's setup.py does
    pyx_to_dep dependency tracking; might add `Cython.Compiler.Options`
    state we miss.

Path forward (~6-8 hour focused session):
  - Reproduce parsers.pyx compile inside a pip-installed pandas
    sdist (where it works) under strace/CYTHON_DEBUG to find what
    setup.py adds that the bare CLI doesn't.
  - OR: write a stub `parsers.pyx` that just no-ops the
    `_try_bool_flex_nogil` family. `pd.read_csv` would fail at
    runtime but `import pandas` succeeds and the rest of pandas
    works.
  - OR: drive pandas's setup.py end-to-end with the wasi-sdk
    cross-compiler and grab the .c files it cythonizes. Reuses
    pandas's CI-tested directive set. Highest leverage but means
    we're effectively running pandas's build, not bypassing it.

### Pragmatic next-session plan

Option B (stub parsers.pyx) gets a `import pandas` demo in 1-2 hours
instead of 1 day. CSV parsing isn't pandas's only superpower; basic
DataFrame, Series, groupby, arithmetic, IO via JSON would all still
work. Add `pd.read_csv = NotImplemented` to the stub and document
the gap.

Original "what's been tried" detail below, kept for reference.

---

## Original status (2026-05-16 late night)

Started the pandas 1.5.3 port. **Stuck at Stage 1 (Cython).** All 41
`.pyx` files except `parsers.pyx` are likely to cythonize fine with
the same `-3 --directive language_level=3,infer_types=True` recipe
we used for numpy.random. `parsers.pyx` chokes on `_try_bool_flex_nogil`
at line 1872 with:

```
parsers.pyx:1872:53: Converting to Python object not allowed without gil
            if kh_get_str_starts_item(false_hashset, word):
                                                     ^
```

The signatures involved:

  - `word` declared `cdef const char *word = NULL` (parsers.pyx:1834)
  - `false_hashset` declared `const kh_str_starts_t *false_hashset`
    (parsers.pyx:1827)
  - `kh_get_str_starts_item` declared in `khash.pxd`:
    `khuint_t kh_get_str_starts_item(kh_str_starts_t* table,
                                     char* key) nogil`

The const mismatch (call-site has const, .pxd doesn't) makes Cython
try to widen via a Python round-trip, illegal under `nogil`.

### What's been tried

  - `infer_types=True`: no change.
  - Cython 0.29.37: fails.
  - Cython 3.0.12: fails identically.
  - Patched `khash.pxd` to add `const` to both `table` and `key`
    params (`01-cython.sh` retains this patch under WISP_CONST_KEY_PATCH).
    Still fails — Cython's const propagation through pointer types
    in nogil contexts seems strict in a way that .pxd patching alone
    doesn't satisfy.

### What likely works (deferred to next session)

  - **Patch parsers.pyx**: drop the `const` qualifiers from the
    hashset params and from `word`. Use `char *` and
    `kh_str_starts_t *`. This is the brute-force lowest-touch fix;
    matches what pandas's own setup.py path effectively gets.
  - **Or build pandas via meson/setup.py end-to-end** rather than our
    manual cython→clang pipeline. Higher complexity but inherits
    pandas's CI-tested directive set.

### What's already here

  - `01-cython.sh` — full cython pipeline scaffold, runs through
    all 41 .pyx, includes the khash.pxd const-patch.

### Why pandas is the right next milestone (and worth a focused session)

Same payoff curve as numpy: getting `import pandas; df = pd.DataFrame(...)`
working in the sandbox is the obvious next "wow" demo after
`import numpy`. Pandas 1.5 leans on numpy + Cython exclusively (no
BLAS, no Fortran, no autotools) — once parsers.pyx is past, the rest
should follow the numpy.random pattern.

Estimated remaining work to `import pandas`:
  - Patch parsers.pyx const → 30 min
  - Cython all .pyx and catch new errors → 1 hr
  - Compile + archive + link via Stage-4-style script → 2 hr
  - Build pure-Python pandas tree + register ~40 inittab entries → 1 hr
  - Stub the C extensions we don't ship (none expected for pandas
    base; sas reader, ujson maybe) → 30 min
  - First `import pandas` + iterate on missing pieces → 2-3 hr

Total: ~6-8 hours focused work for a clean import. Real pandas
operations (read_csv, groupby, etc.) likely work as soon as import
does, since most of pandas's complexity is in the cython ndarray
manipulation already compiled into _libs.
