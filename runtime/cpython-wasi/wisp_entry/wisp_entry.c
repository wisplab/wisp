/* Wisp WASI Reactor entry for CPython 3.14.
 *
 * Built with -mexec-model=reactor and exported alongside libpython3.14.a
 * to produce python-reactor.wasm. Lets a host call wisp_init() once to
 * initialize the Python runtime, then call wisp_eval(ptr, len) any number
 * of times to execute Python source — without going through _start, and
 * without re-running interpreter init each call.
 *
 * The host can also snapshot linear memory between wisp_init and the first
 * wisp_eval, then memcpy that snapshot into a fresh instance and jump
 * straight to wisp_eval. That's the per-call sandbox primitive.
 *
 * The host bridge: WASI Preview 1 has no sockets / subprocess / threads.
 * Sandboxed code that needs an outside capability (HTTP fetch, key/value
 * lookup, secret retrieval, etc.) goes through `_wisp.call_host(name,
 * payload)` which calls into a host-provided WASM import. The host
 * decides which capability names are exposed; the sandbox cannot bypass.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* WASI Reactor calls __wasm_call_ctors via _initialize automatically.
 * After that, the host can call any export. We expose:
 *
 *   wisp_init()                         -> int    initialize Python runtime
 *   wisp_eval(ptr, len)                 -> int    run Python source from linear memory
 *   wisp_alloc(size)                    -> ptr    malloc forwarded to wasm linear memory
 *   wisp_free(ptr)                      ->        free forwarded
 *
 * The alloc/free pair lets the host write source bytes into wasm memory
 * before calling wisp_eval. Both pointers are 32-bit offsets into the
 * single linear memory.
 *
 * And we IMPORT one function from the host:
 *
 *   env::host_call(name_ptr, name_len, payload_ptr, payload_len,
 *                  result_ptr, result_max) -> int  bytes written or -err
 *
 * The host implements this. From Python: `_wisp.call_host(name, payload)`.
 */

/* ------------------------------------------------------------------------
 * Host bridge import + Python binding
 * ------------------------------------------------------------------------ */

__attribute__((import_module("env"), import_name("host_call")))
extern int32_t host_call(
    const char *name_ptr,    int32_t name_len,
    const char *payload_ptr, int32_t payload_len,
    char       *result_ptr,  int32_t result_max
);

/* Lazily-allocated shared response buffer. Single-threaded reactor: one
 * call returns before the next starts. Python copies the bytes out into a
 * PyBytes object before any host code runs again. The buffer is malloc'd
 * on first use so that snapshots taken before any host_call (the common
 * case — wisp_init pre-imports _wisp but doesn't call into the host) stay
 * compact: the 1 MB doesn't enter the snapshot. */
#define WISP_RESPONSE_BUF_SIZE (1u << 20)
static char *wisp_response_buf = NULL;

static PyObject *_wisp_call_host(PyObject *self, PyObject *args) {
    const char *name; Py_ssize_t name_len;
    const char *payload; Py_ssize_t payload_len;
    if (!PyArg_ParseTuple(args, "y#y#", &name, &name_len, &payload, &payload_len)) {
        return NULL;
    }
    if (name_len > 0xffff || payload_len > (Py_ssize_t)WISP_RESPONSE_BUF_SIZE) {
        PyErr_SetString(PyExc_ValueError,
            "name or payload exceeds bridge limit");
        return NULL;
    }
    if (!wisp_response_buf) {
        wisp_response_buf = (char *)malloc(WISP_RESPONSE_BUF_SIZE);
        if (!wisp_response_buf) {
            PyErr_NoMemory();
            return NULL;
        }
    }

    int32_t n = host_call(name, (int32_t)name_len,
                          payload, (int32_t)payload_len,
                          wisp_response_buf, (int32_t)WISP_RESPONSE_BUF_SIZE);
    if (n < 0) {
        PyErr_Format(PyExc_RuntimeError,
            "wisp.host_call(%R) failed: host returned %d",
            PyBytes_FromStringAndSize(name, name_len), (int)n);
        return NULL;
    }
    if (n > (int32_t)WISP_RESPONSE_BUF_SIZE) {
        PyErr_SetString(PyExc_RuntimeError,
            "host wrote past response buffer");
        return NULL;
    }
    return PyBytes_FromStringAndSize(wisp_response_buf, n);
}

static PyMethodDef _wisp_methods[] = {
    {"call_host", _wisp_call_host, METH_VARARGS,
        "call_host(name: bytes, payload: bytes) -> bytes\n"
        "Synchronous bridge to the host runtime. The host decides which "
        "names are exposed; the sandbox cannot enumerate or bypass."},
    {NULL, NULL, 0, NULL}
};

static struct PyModuleDef _wisp_module_def = {
    PyModuleDef_HEAD_INIT,
    "_wisp",
    "Sandbox <-> host bridge for the Wisp WASI Python runtime.",
    -1,
    _wisp_methods,
    NULL, NULL, NULL, NULL
};

static PyObject *PyInit__wisp(void) {
    return PyModule_Create(&_wisp_module_def);
}

/* numpy's _multiarray_umath C extension, statically linked from
 * vendor/numpy-1.26.4/build-wasi/libnumpy.a (built via the M1 pipeline
 * in wisp/scripts/numpy/). Forward-declared here so wisp_init can
 * append it to the inittab. WASI Preview 1 has no dlopen so static
 * inittab is the only way; this mirrors how _wisp itself is wired. */
extern PyObject *PyInit__multiarray_umath(void);

/* numpy.fft._pocketfft_internal — single-file C extension compiled in
 * the same libnumpy.a archive. Registered alongside _multiarray_umath. */
extern PyObject *PyInit__pocketfft_internal(void);

/* numpy.linalg._umath_linalg — C++ wrapper + 9 f2c-translated reference
 * BLAS+LAPACK .c files (numpy ships these as the no-system-BLAS
 * fallback, exactly our case on wasm). */
extern PyObject *PyInit__umath_linalg(void);

/* numpy.random — 9 cythonized C extensions covering bit generators
 * (mt19937, philox, pcg64, sfc64), the modern Generator API,
 * legacy mtrand, and supporting modules. */
extern PyObject *PyInit__common(void);
extern PyObject *PyInit_bit_generator(void);
extern PyObject *PyInit__bounded_integers(void);
extern PyObject *PyInit__generator(void);
extern PyObject *PyInit__mt19937(void);
extern PyObject *PyInit__pcg64(void);
extern PyObject *PyInit__philox(void);
extern PyObject *PyInit__sfc64(void);
extern PyObject *PyInit_mtrand(void);

/* ------------------------------------------------------------------------
 * Reactor exports
 * ------------------------------------------------------------------------ */

__attribute__((export_name("wisp_init")))
int32_t wisp_init(void) {
    if (Py_IsInitialized()) {
        return 0;
    }

    /* Register the _wisp builtin BEFORE Py_InitializeFromConfig so the
     * subsequent `import _wisp` resolves it. */
    if (PyImport_AppendInittab("_wisp", PyInit__wisp) != 0) {
        return -30;
    }

    /* Same for numpy's _multiarray_umath. The symbol comes from
     * libnumpy.a which build.sh static-links into this wasm.
     * numpy 1.26 imports it as `numpy.core._multiarray_umath`, so
     * register the full dotted name. */
    if (PyImport_AppendInittab("numpy.core._multiarray_umath",
                               PyInit__multiarray_umath) != 0) {
        return -31;
    }
    if (PyImport_AppendInittab("numpy.fft._pocketfft_internal",
                               PyInit__pocketfft_internal) != 0) {
        return -32;
    }
    if (PyImport_AppendInittab("numpy.linalg._umath_linalg",
                               PyInit__umath_linalg) != 0) {
        return -33;
    }

    /* numpy.random — register all 9 C extensions under their full
     * dotted names. Order doesn't matter for inittab. */
    struct { const char *name; PyObject *(*init)(void); } random_mods[] = {
        {"numpy.random._common",            PyInit__common},
        {"numpy.random.bit_generator",      PyInit_bit_generator},
        {"numpy.random._bounded_integers",  PyInit__bounded_integers},
        {"numpy.random._generator",         PyInit__generator},
        {"numpy.random._mt19937",           PyInit__mt19937},
        {"numpy.random._pcg64",             PyInit__pcg64},
        {"numpy.random._philox",            PyInit__philox},
        {"numpy.random._sfc64",             PyInit__sfc64},
        {"numpy.random.mtrand",             PyInit_mtrand},
    };
    for (size_t i = 0; i < sizeof(random_mods)/sizeof(random_mods[0]); i++) {
        if (PyImport_AppendInittab(random_mods[i].name, random_mods[i].init) != 0) {
            return -40 - (int)i;
        }
    }

    /* Use the modern PyConfig embedding API. Default Py_InitializeEx skips
     * the path-detection logic that Py_BytesMain runs, so it can't find the
     * stdlib without explicit configuration. We use isolated config and
     * set program_name + (optionally read PYTHONPATH from env). */
    PyConfig config;
    PyConfig_InitIsolatedConfig(&config);
    config.install_signal_handlers = 0;
    config.parse_argv = 0;
    config.use_environment = 1;          /* read PYTHONPATH, PYTHONHOME */
    config.user_site_directory = 0;
    config.site_import = 1;

    /* Without a program_name, getpath.py can't compute prefix. Pick a path
     * that lives under our preopen so the relative resolution works. */
    PyStatus status = PyConfig_SetBytesString(
        &config, &config.program_name, "/cross-build/wasm32-wasip1/python.wasm");
    if (PyStatus_Exception(status)) {
        PyConfig_Clear(&config);
        return -11;
    }

    status = Py_InitializeFromConfig(&config);
    PyConfig_Clear(&config);
    if (PyStatus_Exception(status)) {
        return -10;
    }
    if (!Py_IsInitialized()) {
        return -1;
    }

    /* Pre-import commonly used modules so they're in sys.modules at snapshot
     * time. Subsequent `import X` in wisp_eval becomes a dict lookup.
     * _wisp goes in too so Python code can use the bridge without a fresh
     * import on every call. */
    int rc = PyRun_SimpleString(
        "import sys, os, io, re, json, math, time, datetime, "
        "collections, itertools, functools, hashlib, base64, struct, "
        "urllib.parse, sqlite3, _hashlib, _wisp\n"
    );
    return rc == 0 ? 0 : -20;
}

__attribute__((export_name("wisp_eval")))
int32_t wisp_eval(int32_t code_ptr, int32_t code_len) {
    if (!Py_IsInitialized()) {
        return -2;
    }
    char *code = (char *)(uintptr_t)code_ptr;

    /* PyRun_SimpleString needs a NUL-terminated buffer. Make a copy with
     * an explicit NUL since the host's slice may not be terminated. */
    char *buf = (char *)malloc((size_t)code_len + 1);
    if (!buf) {
        return -3;
    }
    memcpy(buf, code, (size_t)code_len);
    buf[code_len] = '\0';

    int rc = PyRun_SimpleString(buf);
    free(buf);
    return rc;
}

__attribute__((export_name("wisp_alloc")))
int32_t wisp_alloc(int32_t size) {
    void *p = malloc((size_t)size);
    return (int32_t)(uintptr_t)p;
}

__attribute__((export_name("wisp_free")))
void wisp_free(int32_t ptr) {
    free((void *)(uintptr_t)ptr);
}
