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
 * straight to wisp_eval. That's the per-call sandbox primitive we want
 * to demonstrate in Spike A2.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* WASI Reactor calls __wasm_call_ctors via _initialize automatically.
 * After that, the host can call any export. We expose four:
 *
 *   wisp_init()                         -> int    initialize Python runtime
 *   wisp_eval(ptr, len)                 -> int    run Python source from linear memory
 *   wisp_alloc(size)                    -> ptr    malloc forwarded to wasm linear memory
 *   wisp_free(ptr)                      ->        free forwarded
 *
 * The alloc/free pair lets the host write source bytes into wasm memory
 * before calling wisp_eval. Both pointers are 32-bit offsets into the
 * single linear memory.
 */

__attribute__((export_name("wisp_init")))
int32_t wisp_init(void) {
    if (Py_IsInitialized()) {
        return 0;
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
     * time. Subsequent `import X` in wisp_eval becomes a dict lookup. */
    int rc = PyRun_SimpleString(
        "import sys, os, io, re, json, math, time, datetime, "
        "collections, itertools, functools, hashlib, base64, struct, "
        "urllib.parse, sqlite3\n"
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
