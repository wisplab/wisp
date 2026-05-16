# scripts/pandas/ — WIP

## Status as of 2026-05-16 late night

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
