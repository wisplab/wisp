/* wisp_accel — guest-side CBLAS for the Wisp WASI Python reactor.
 *
 * numpy is compiled with -DHAVE_CBLAS so its matmul / dot / vector
 * kernels call the standard cblas_* API. This file provides those
 * symbols inside the wasm module:
 *
 *   - {s,d}gemm and {s,d}syrk (the O(n^3) hot paths) are FORWARDED to
 *     the host via the `env.wisp_dgemm` / `env.wisp_sgemm` imports,
 *     which run a native BLAS zero-copy on this instance's linear
 *     memory. If the host declines (returns nonzero — e.g. accel
 *     disabled for bit-deterministic runs), we fall back to a local
 *     scalar reference implementation, so results are always produced.
 *   - Everything else (gemv / axpy / dot, and all complex variants)
 *     is implemented locally. These are O(n^2)/O(n) or rare; the
 *     local loops autovectorize under -O3 -msimd128.
 *
 * The f2c BLAS inside numpy's lapack_lite (used by numpy.linalg) has
 * its own dgemm_/sgemm_. The reactor links with -Wl,--wrap=dgemm_
 * -Wl,--wrap=sgemm_ so those calls land on __wrap_dgemm_ /
 * __wrap_sgemm_ below, which forward to the host and fall back to the
 * f2c original (__real_dgemm_) if the host declines.
 *
 * Host import contract (see crates/wisp-runtime/src/accel.rs):
 * matrices are described by (rows, cols, row-stride, col-stride) in
 * ELEMENTS; the host validates every extent against linear-memory
 * bounds and rejects aliasing, so a hostile guest can at worst get
 * its own call refused.
 */

#include <stdint.h>
#include <stddef.h>

typedef int32_t blas_int;

enum CBLAS_ORDER { CblasRowMajor = 101, CblasColMajor = 102 };
enum CBLAS_TRANSPOSE { CblasNoTrans = 111, CblasTrans = 112, CblasConjTrans = 113 };
enum CBLAS_UPLO { CblasUpper = 121, CblasLower = 122 };

__attribute__((import_module("env"), import_name("wisp_dgemm")))
extern int32_t wisp_dgemm(
    int32_t m, int32_t n, int32_t k, double alpha,
    const double *a, int32_t rsa, int32_t csa,
    const double *b, int32_t rsb, int32_t csb,
    double beta, double *c, int32_t rsc, int32_t csc);

__attribute__((import_module("env"), import_name("wisp_sgemm")))
extern int32_t wisp_sgemm(
    int32_t m, int32_t n, int32_t k, float alpha,
    const float *a, int32_t rsa, int32_t csa,
    const float *b, int32_t rsb, int32_t csb,
    float beta, float *c, int32_t rsc, int32_t csc);

/* Map (order, trans, ld) to (row-stride, col-stride) of op(X) where
 * op(X) has shape rows x cols. For real types ConjTrans == Trans. */
static void op_strides(int order, int trans, blas_int ld,
                       blas_int *rs, blas_int *cs) {
    int t = (trans != CblasNoTrans);
    if (order == CblasRowMajor) {
        if (!t) { *rs = ld; *cs = 1; } else { *rs = 1; *cs = ld; }
    } else {
        if (!t) { *rs = 1; *cs = ld; } else { *rs = ld; *cs = 1; }
    }
}

/* BLAS vector indexing: element i of an n-vector with increment inc
 * lives at x[i*inc] for inc >= 0, x[(n-1-i)*(-inc)] for inc < 0. */
#define VIDX(i, n, inc) ((inc) >= 0 ? (ptrdiff_t)(i) * (inc) \
                                    : (ptrdiff_t)((n) - 1 - (i)) * -(ptrdiff_t)(inc))

/* ---------------- local reference gemm (fallback path) ---------------- */

#define DEF_LOCAL_GEMM(NAME, T)                                              \
static void NAME(blas_int m, blas_int n, blas_int k, T alpha,                \
                 const T *a, blas_int rsa, blas_int csa,                     \
                 const T *b, blas_int rsb, blas_int csb,                     \
                 T beta, T *c, blas_int rsc, blas_int csc) {                 \
    for (blas_int i = 0; i < m; i++) {                                       \
        for (blas_int j = 0; j < n; j++) {                                   \
            T s = 0;                                                         \
            for (blas_int l = 0; l < k; l++)                                 \
                s += a[(ptrdiff_t)i * rsa + (ptrdiff_t)l * csa] *            \
                     b[(ptrdiff_t)l * rsb + (ptrdiff_t)j * csb];             \
            T *cp = &c[(ptrdiff_t)i * rsc + (ptrdiff_t)j * csc];             \
            *cp = alpha * s + (beta == 0 ? 0 : beta * *cp);                  \
        }                                                                    \
    }                                                                        \
}

DEF_LOCAL_GEMM(local_dgemm, double)
DEF_LOCAL_GEMM(local_sgemm, float)

/* ---------------- cblas real gemm / syrk (host-forwarded) ------------- */

void cblas_dgemm(int order, int transa, int transb,
                 blas_int m, blas_int n, blas_int k, double alpha,
                 const double *a, blas_int lda,
                 const double *b, blas_int ldb,
                 double beta, double *c, blas_int ldc) {
    blas_int rsa, csa, rsb, csb, rsc, csc;
    op_strides(order, transa, lda, &rsa, &csa);
    op_strides(order, transb, ldb, &rsb, &csb);
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);
    if (wisp_dgemm(m, n, k, alpha, a, rsa, csa, b, rsb, csb,
                   beta, c, rsc, csc) != 0)
        local_dgemm(m, n, k, alpha, a, rsa, csa, b, rsb, csb, beta, c, rsc, csc);
}

void cblas_sgemm(int order, int transa, int transb,
                 blas_int m, blas_int n, blas_int k, float alpha,
                 const float *a, blas_int lda,
                 const float *b, blas_int ldb,
                 float beta, float *c, blas_int ldc) {
    blas_int rsa, csa, rsb, csb, rsc, csc;
    op_strides(order, transa, lda, &rsa, &csa);
    op_strides(order, transb, ldb, &rsb, &csb);
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);
    if (wisp_sgemm(m, n, k, alpha, a, rsa, csa, b, rsb, csb,
                   beta, c, rsc, csc) != 0)
        local_sgemm(m, n, k, alpha, a, rsa, csa, b, rsb, csb, beta, c, rsc, csc);
}

/* syrk: C := alpha*op(A)*op(A)^T + beta*C, C is n x n. We compute the
 * FULL symmetric product (a superset of the Uplo-triangle contract —
 * both triangles receive correct values, so callers that mirror one
 * triangle afterward are unaffected). op(A)^T is the same buffer with
 * swapped strides. */
void cblas_dsyrk(int order, int uplo, int trans, blas_int n, blas_int k,
                 double alpha, const double *a, blas_int lda,
                 double beta, double *c, blas_int ldc) {
    (void)uplo;
    blas_int rsa, csa, rsc, csc;
    op_strides(order, trans, lda, &rsa, &csa);
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);
    /* op(A) is n x k; its transpose is the same buffer with swapped
     * strides. */
    if (wisp_dgemm(n, n, k, alpha, a, rsa, csa, a, csa, rsa,
                   beta, c, rsc, csc) != 0)
        local_dgemm(n, n, k, alpha, a, rsa, csa, a, csa, rsa, beta, c, rsc, csc);
}

void cblas_ssyrk(int order, int uplo, int trans, blas_int n, blas_int k,
                 float alpha, const float *a, blas_int lda,
                 float beta, float *c, blas_int ldc) {
    (void)uplo;
    blas_int rsa, csa, rsc, csc;
    op_strides(order, trans, lda, &rsa, &csa);
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);
    if (wisp_sgemm(n, n, k, alpha, a, rsa, csa, a, csa, rsa,
                   beta, c, rsc, csc) != 0)
        local_sgemm(n, n, k, alpha, a, rsa, csa, a, csa, rsa, beta, c, rsc, csc);
}

/* ---------------- real gemv / axpy / dot (local) ---------------------- */

#define DEF_GEMV(NAME, T)                                                    \
void NAME(int order, int trans, blas_int m, blas_int n, T alpha,             \
          const T *a, blas_int lda, const T *x, blas_int incx,               \
          T beta, T *y, blas_int incy) {                                     \
    /* op(A) is rows x cols; y has rows elements, x has cols. */             \
    blas_int rs, cs;                                                         \
    op_strides(order, trans, lda, &rs, &cs);                                 \
    blas_int rows = (trans == CblasNoTrans) ? m : n;                         \
    blas_int cols = (trans == CblasNoTrans) ? n : m;                         \
    for (blas_int i = 0; i < rows; i++) {                                    \
        T s = 0;                                                             \
        for (blas_int j = 0; j < cols; j++)                                  \
            s += a[(ptrdiff_t)i * rs + (ptrdiff_t)j * cs] *                  \
                 x[VIDX(j, cols, incx)];                                     \
        T *yp = &y[VIDX(i, rows, incy)];                                     \
        *yp = alpha * s + (beta == 0 ? 0 : beta * *yp);                      \
    }                                                                        \
}

DEF_GEMV(cblas_dgemv, double)
DEF_GEMV(cblas_sgemv, float)

#define DEF_AXPY(NAME, T)                                                    \
void NAME(blas_int n, T alpha, const T *x, blas_int incx,                    \
          T *y, blas_int incy) {                                             \
    for (blas_int i = 0; i < n; i++)                                         \
        y[VIDX(i, n, incy)] += alpha * x[VIDX(i, n, incx)];                  \
}

DEF_AXPY(cblas_daxpy, double)
DEF_AXPY(cblas_saxpy, float)

double cblas_ddot(blas_int n, const double *x, blas_int incx,
                  const double *y, blas_int incy) {
    double s = 0;
    for (blas_int i = 0; i < n; i++)
        s += x[VIDX(i, n, incx)] * y[VIDX(i, n, incy)];
    return s;
}

float cblas_sdot(blas_int n, const float *x, blas_int incx,
                 const float *y, blas_int incy) {
    float s = 0;
    for (blas_int i = 0; i < n; i++)
        s += x[VIDX(i, n, incx)] * y[VIDX(i, n, incy)];
    return s;
}

/* ---------------- complex variants (local reference) ------------------ */
/* Interleaved re/im pairs behind void*. Manual re/im arithmetic — no
 * libc complex helpers needed. ConjTrans conjugates op(A) elements. */

#define DEF_CPLX(PRE, T)                                                     \
static void PRE##_mac(T ar, T ai, T br, T bi, T *sr, T *si, int conj_a) {    \
    if (conj_a) ai = -ai;                                                    \
    *sr += ar * br - ai * bi;                                                \
    *si += ar * bi + ai * br;                                                \
}                                                                            \
void cblas_##PRE##gemm(int order, int transa, int transb,                    \
        blas_int m, blas_int n, blas_int k, const void *alpha_p,             \
        const void *a_p, blas_int lda, const void *b_p, blas_int ldb,        \
        const void *beta_p, void *c_p, blas_int ldc) {                       \
    const T *alpha = (const T *)alpha_p, *beta = (const T *)beta_p;          \
    const T *a = (const T *)a_p, *b = (const T *)b_p;                        \
    T *c = (T *)c_p;                                                         \
    blas_int rsa, csa, rsb, csb, rsc, csc;                                   \
    op_strides(order, transa, lda, &rsa, &csa);                              \
    op_strides(order, transb, ldb, &rsb, &csb);                              \
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);                        \
    int ca = (transa == CblasConjTrans), cb = (transb == CblasConjTrans);    \
    for (blas_int i = 0; i < m; i++)                                         \
        for (blas_int j = 0; j < n; j++) {                                   \
            T sr = 0, si = 0;                                                \
            for (blas_int l = 0; l < k; l++) {                               \
                ptrdiff_t ia = 2 * ((ptrdiff_t)i * rsa + (ptrdiff_t)l * csa);\
                ptrdiff_t ib = 2 * ((ptrdiff_t)l * rsb + (ptrdiff_t)j * csb);\
                T br = b[ib], bi = cb ? -b[ib + 1] : b[ib + 1];              \
                PRE##_mac(a[ia], a[ia + 1], br, bi, &sr, &si, ca);           \
            }                                                                \
            ptrdiff_t ic = 2 * ((ptrdiff_t)i * rsc + (ptrdiff_t)j * csc);    \
            T cr = (beta[0] == 0 && beta[1] == 0)                            \
                     ? 0 : beta[0] * c[ic] - beta[1] * c[ic + 1];            \
            T ci = (beta[0] == 0 && beta[1] == 0)                            \
                     ? 0 : beta[0] * c[ic + 1] + beta[1] * c[ic];            \
            c[ic]     = cr + alpha[0] * sr - alpha[1] * si;                  \
            c[ic + 1] = ci + alpha[0] * si + alpha[1] * sr;                  \
        }                                                                    \
}                                                                            \
void cblas_##PRE##syrk(int order, int uplo, int trans, blas_int n,           \
        blas_int k, const void *alpha_p, const void *a_p, blas_int lda,      \
        const void *beta_p, void *c_p, blas_int ldc) {                       \
    (void)uplo;                                                              \
    const T *alpha = (const T *)alpha_p, *beta = (const T *)beta_p;          \
    const T *a = (const T *)a_p;                                             \
    T *c = (T *)c_p;                                                         \
    blas_int rsa, csa, rsc, csc;                                             \
    op_strides(order, trans, lda, &rsa, &csa);                               \
    op_strides(order, CblasNoTrans, ldc, &rsc, &csc);                        \
    for (blas_int i = 0; i < n; i++)                                         \
        for (blas_int j = 0; j < n; j++) {                                   \
            T sr = 0, si = 0;                                                \
            for (blas_int l = 0; l < k; l++) {                               \
                ptrdiff_t ia = 2 * ((ptrdiff_t)i * rsa + (ptrdiff_t)l * csa);\
                ptrdiff_t ib = 2 * ((ptrdiff_t)j * rsa + (ptrdiff_t)l * csa);\
                PRE##_mac(a[ia], a[ia + 1], a[ib], a[ib + 1], &sr, &si, 0);  \
            }                                                                \
            ptrdiff_t ic = 2 * ((ptrdiff_t)i * rsc + (ptrdiff_t)j * csc);    \
            T cr = (beta[0] == 0 && beta[1] == 0)                            \
                     ? 0 : beta[0] * c[ic] - beta[1] * c[ic + 1];            \
            T ci = (beta[0] == 0 && beta[1] == 0)                            \
                     ? 0 : beta[0] * c[ic + 1] + beta[1] * c[ic];            \
            c[ic]     = cr + alpha[0] * sr - alpha[1] * si;                  \
            c[ic + 1] = ci + alpha[0] * si + alpha[1] * sr;                  \
        }                                                                    \
}                                                                            \
void cblas_##PRE##gemv(int order, int trans, blas_int m, blas_int n,         \
        const void *alpha_p, const void *a_p, blas_int lda,                  \
        const void *x_p, blas_int incx, const void *beta_p,                  \
        void *y_p, blas_int incy) {                                          \
    const T *alpha = (const T *)alpha_p, *beta = (const T *)beta_p;          \
    const T *a = (const T *)a_p, *x = (const T *)x_p;                        \
    T *y = (T *)y_p;                                                         \
    blas_int rs, cs;                                                         \
    op_strides(order, trans, lda, &rs, &cs);                                 \
    blas_int rows = (trans == CblasNoTrans) ? m : n;                         \
    blas_int cols = (trans == CblasNoTrans) ? n : m;                         \
    int ca = (trans == CblasConjTrans);                                      \
    for (blas_int i = 0; i < rows; i++) {                                    \
        T sr = 0, si = 0;                                                    \
        for (blas_int j = 0; j < cols; j++) {                                \
            ptrdiff_t ia = 2 * ((ptrdiff_t)i * rs + (ptrdiff_t)j * cs);      \
            ptrdiff_t ix = 2 * VIDX(j, cols, incx);                          \
            PRE##_mac(a[ia], a[ia + 1], x[ix], x[ix + 1], &sr, &si, ca);     \
        }                                                                    \
        ptrdiff_t iy = 2 * VIDX(i, rows, incy);                              \
        T yr = (beta[0] == 0 && beta[1] == 0)                                \
                 ? 0 : beta[0] * y[iy] - beta[1] * y[iy + 1];                \
        T yi = (beta[0] == 0 && beta[1] == 0)                                \
                 ? 0 : beta[0] * y[iy + 1] + beta[1] * y[iy];                \
        y[iy]     = yr + alpha[0] * sr - alpha[1] * si;                      \
        y[iy + 1] = yi + alpha[0] * si + alpha[1] * sr;                      \
    }                                                                        \
}                                                                            \
void cblas_##PRE##axpy(blas_int n, const void *alpha_p,                      \
        const void *x_p, blas_int incx, void *y_p, blas_int incy) {          \
    const T *alpha = (const T *)alpha_p;                                     \
    const T *x = (const T *)x_p;                                             \
    T *y = (T *)y_p;                                                         \
    for (blas_int i = 0; i < n; i++) {                                       \
        ptrdiff_t ix = 2 * VIDX(i, n, incx), iy = 2 * VIDX(i, n, incy);      \
        y[iy]     += alpha[0] * x[ix] - alpha[1] * x[ix + 1];                \
        y[iy + 1] += alpha[0] * x[ix + 1] + alpha[1] * x[ix];                \
    }                                                                        \
}                                                                            \
void cblas_##PRE##dotu_sub(blas_int n, const void *x_p, blas_int incx,       \
        const void *y_p, blas_int incy, void *ret_p) {                       \
    const T *x = (const T *)x_p, *y = (const T *)y_p;                        \
    T *ret = (T *)ret_p;                                                     \
    T sr = 0, si = 0;                                                        \
    for (blas_int i = 0; i < n; i++) {                                       \
        ptrdiff_t ix = 2 * VIDX(i, n, incx), iy = 2 * VIDX(i, n, incy);      \
        PRE##_mac(x[ix], x[ix + 1], y[iy], y[iy + 1], &sr, &si, 0);          \
    }                                                                        \
    ret[0] = sr; ret[1] = si;                                                \
}                                                                            \
/* dotc: conjugate(x) . y — used by np.vdot. */                              \
void cblas_##PRE##dotc_sub(blas_int n, const void *x_p, blas_int incx,       \
        const void *y_p, blas_int incy, void *ret_p) {                       \
    const T *x = (const T *)x_p, *y = (const T *)y_p;                        \
    T *ret = (T *)ret_p;                                                     \
    T sr = 0, si = 0;                                                        \
    for (blas_int i = 0; i < n; i++) {                                       \
        ptrdiff_t ix = 2 * VIDX(i, n, incx), iy = 2 * VIDX(i, n, incy);      \
        PRE##_mac(x[ix], x[ix + 1], y[iy], y[iy + 1], &sr, &si, 1);          \
    }                                                                        \
    ret[0] = sr; ret[1] = si;                                                \
}

DEF_CPLX(c, float)
DEF_CPLX(z, double)

/* ---------------- f2c BLAS wrap (lapack_lite hot path) ---------------- */
/* Column-major, Fortran calling convention (everything by pointer).
 * Linked with -Wl,--wrap=dgemm_ -Wl,--wrap=sgemm_ so lapack_lite's
 * internal dgemm calls land here; fall back to the f2c original
 * (__real_*) when the host declines. */

extern int __real_dgemm_(char *transa, char *transb, blas_int *m,
                         blas_int *n, blas_int *k, double *alpha, double *a,
                         blas_int *lda, double *b, blas_int *ldb,
                         double *beta, double *c, blas_int *ldc);
extern int __real_sgemm_(char *transa, char *transb, blas_int *m,
                         blas_int *n, blas_int *k, float *alpha, float *a,
                         blas_int *lda, float *b, blas_int *ldb,
                         float *beta, float *c, blas_int *ldc);

static int f2c_notrans(char t) { return t == 'N' || t == 'n'; }

int __wrap_dgemm_(char *transa, char *transb, blas_int *m, blas_int *n,
                  blas_int *k, double *alpha, double *a, blas_int *lda,
                  double *b, blas_int *ldb, double *beta, double *c,
                  blas_int *ldc) {
    blas_int rsa, csa, rsb, csb;
    if (f2c_notrans(*transa)) { rsa = 1; csa = *lda; } else { rsa = *lda; csa = 1; }
    if (f2c_notrans(*transb)) { rsb = 1; csb = *ldb; } else { rsb = *ldb; csb = 1; }
    if (wisp_dgemm(*m, *n, *k, *alpha, a, rsa, csa, b, rsb, csb,
                   *beta, c, 1, *ldc) == 0)
        return 0;
    return __real_dgemm_(transa, transb, m, n, k, alpha, a, lda, b, ldb,
                         beta, c, ldc);
}

int __wrap_sgemm_(char *transa, char *transb, blas_int *m, blas_int *n,
                  blas_int *k, float *alpha, float *a, blas_int *lda,
                  float *b, blas_int *ldb, float *beta, float *c,
                  blas_int *ldc) {
    blas_int rsa, csa, rsb, csb;
    if (f2c_notrans(*transa)) { rsa = 1; csa = *lda; } else { rsa = *lda; csa = 1; }
    if (f2c_notrans(*transb)) { rsb = 1; csb = *ldb; } else { rsb = *ldb; csb = 1; }
    if (wisp_sgemm(*m, *n, *k, *alpha, a, rsa, csa, b, rsb, csb,
                   *beta, c, 1, *ldc) == 0)
        return 0;
    return __real_sgemm_(transa, transb, m, n, k, alpha, a, lda, b, ldb,
                         beta, c, ldc);
}
