//! wisp-accel — native BLAS host functions for the guest's cblas shim.
//!
//! The reactor's wisp_accel.c forwards {s,d}gemm (and syrk expressed as
//! gemm) to `env.wisp_dgemm` / `env.wisp_sgemm`. The host runs the
//! multiply with the `matrixmultiply` crate (pure Rust, cache-blocked,
//! SIMD) directly on the instance's linear memory — zero copy.
//!
//! Trust boundary: every pointer/stride/dimension here is
//! guest-controlled. Before touching memory we validate that each
//! matrix's full strided extent lies inside linear memory, that
//! pointers are element-aligned, and that the output does not alias an
//! input. On any violation we return a nonzero code and the guest
//! falls back to its local in-wasm implementation — a hostile guest
//! can at worst get its own call refused.
//!
//! `WISP_ACCEL=0` disables the fast path (every call returns -1): the
//! guest then computes everything in pure wasm, which is slower but
//! bit-deterministic across hosts (native BLAS rounding differs
//! between platforms).

use wasmtime::{Caller, Extern, Linker};
use wasmtime_wasi::preview1::WasiP1Ctx;

/// Byte range [start, end) of an rows×cols matrix with element strides
/// (rs, cs), or None if any part falls outside `mem_len` / is
/// misaligned / has non-positive dims or strides.
fn mat_range(
    ptr: i32,
    rows: i64,
    cols: i64,
    rs: i64,
    cs: i64,
    elem: usize,
    mem_len: usize,
) -> Option<(usize, usize)> {
    if ptr < 0 || rows <= 0 || cols <= 0 || rs < 1 || cs < 1 {
        return None;
    }
    let start = ptr as usize;
    if start % elem != 0 {
        return None;
    }
    let max_el = (rows - 1)
        .checked_mul(rs)?
        .checked_add((cols - 1).checked_mul(cs)?)?;
    let bytes = usize::try_from(max_el)
        .ok()?
        .checked_add(1)?
        .checked_mul(elem)?;
    let end = start.checked_add(bytes)?;
    if end > mem_len {
        return None;
    }
    Some((start, end))
}

fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// On macOS, route stride patterns that CBLAS can express to
/// Accelerate (AMX-backed — several times faster than any portable
/// gemm). Anything CBLAS can't express falls through to
/// matrixmultiply. The strides here were already bounds-validated.
#[cfg(target_os = "macos")]
mod accelerate {
    #[link(name = "Accelerate", kind = "framework")]
    extern "C" {
        pub fn cblas_dgemm(
            order: i32,
            ta: i32,
            tb: i32,
            m: i32,
            n: i32,
            k: i32,
            alpha: f64,
            a: *const f64,
            lda: i32,
            b: *const f64,
            ldb: i32,
            beta: f64,
            c: *mut f64,
            ldc: i32,
        );
        pub fn cblas_sgemm(
            order: i32,
            ta: i32,
            tb: i32,
            m: i32,
            n: i32,
            k: i32,
            alpha: f32,
            a: *const f32,
            lda: i32,
            b: *const f32,
            ldb: i32,
            beta: f32,
            c: *mut f32,
            ldc: i32,
        );
    }

    /// (trans, ld) for an op-matrix rows×cols with strides (rs, cs)
    /// under `order`, or None if not CBLAS-expressible.
    pub fn flags(order: i32, rows: isize, cols: isize, rs: isize, cs: isize) -> Option<(i32, i32)> {
        let (no_trans_unit, no_trans_ld_min) = if order == 101 { (cs, cols) } else { (rs, rows) };
        let (trans_unit, trans_ld_min) = if order == 101 { (rs, rows) } else { (cs, cols) };
        let no_trans_ld = if order == 101 { rs } else { cs };
        let trans_ld = if order == 101 { cs } else { rs };
        if no_trans_unit == 1 && no_trans_ld >= no_trans_ld_min.max(1) {
            Some((111, no_trans_ld as i32))
        } else if trans_unit == 1 && trans_ld >= trans_ld_min.max(1) {
            Some((112, trans_ld as i32))
        } else {
            None
        }
    }
}

macro_rules! gemm_exec {
    ($fn_name:ident, $ty:ty, $mm:path, $acc:path) => {
        /// Run one validated gemm on raw pointers into guest memory.
        /// Argument order matches matrixmultiply: (m, k, n).
        unsafe fn $fn_name(
            m: usize,
            k: usize,
            n: usize,
            alpha: $ty,
            a: *const $ty,
            rsa: isize,
            csa: isize,
            b: *const $ty,
            rsb: isize,
            csb: isize,
            beta: $ty,
            c: *mut $ty,
            rsc: isize,
            csc: isize,
        ) {
            #[cfg(target_os = "macos")]
            {
                let (mi, ni, ki) = (m as isize, n as isize, k as isize);
                let order = if csc == 1 && rsc >= ni.max(1) {
                    Some((101, rsc as i32))
                } else if rsc == 1 && csc >= mi.max(1) {
                    Some((102, csc as i32))
                } else {
                    None
                };
                if let Some((order, ldc)) = order {
                    if let (Some((ta, lda)), Some((tb, ldb))) = (
                        accelerate::flags(order, mi, ki, rsa, csa),
                        accelerate::flags(order, ki, ni, rsb, csb),
                    ) {
                        $acc(
                            order, ta, tb, m as i32, n as i32, k as i32, alpha, a, lda, b, ldb,
                            beta, c, ldc,
                        );
                        return;
                    }
                }
            }
            $mm(m, k, n, alpha, a, rsa, csa, b, rsb, csb, beta, c, rsc, csc);
        }
    };
}

gemm_exec!(
    dgemm_exec,
    f64,
    matrixmultiply::dgemm,
    accelerate::cblas_dgemm
);
gemm_exec!(
    sgemm_exec,
    f32,
    matrixmultiply::sgemm,
    accelerate::cblas_sgemm
);

macro_rules! gemm_host_fn {
    ($name:literal, $ty:ty, $elem:expr, $exec:path) => {
        move |mut caller: Caller<'_, WasiP1Ctx>,
              m: i32,
              n: i32,
              k: i32,
              alpha: $ty,
              a: i32,
              rsa: i32,
              csa: i32,
              b: i32,
              rsb: i32,
              csb: i32,
              beta: $ty,
              c: i32,
              rsc: i32,
              csc: i32|
              -> i32 {
            let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                return -100;
            };
            let data = mem.data_mut(&mut caller);
            let len = data.len();
            let (m64, n64, k64) = (m as i64, n as i64, k as i64);
            let Some(ar) = mat_range(a, m64, k64, rsa as i64, csa as i64, $elem, len) else {
                return -2;
            };
            let Some(br) = mat_range(b, k64, n64, rsb as i64, csb as i64, $elem, len) else {
                return -2;
            };
            let Some(cr) = mat_range(c, m64, n64, rsc as i64, csc as i64, $elem, len) else {
                return -2;
            };
            if overlaps(cr, ar) || overlaps(cr, br) {
                return -3;
            }
            let base = data.as_mut_ptr();
            // Safety: ranges validated in-bounds against this
            // instance's linear memory, output disjoint from inputs,
            // pointers element-aligned. The instance is parked on this
            // thread for the duration of the call (single-threaded
            // guest), so no concurrent access to the memory.
            unsafe {
                $exec(
                    m as usize,
                    k as usize,
                    n as usize,
                    alpha,
                    base.add(ar.0) as *const $ty,
                    rsa as isize,
                    csa as isize,
                    base.add(br.0) as *const $ty,
                    rsb as isize,
                    csb as isize,
                    beta,
                    base.add(cr.0) as *mut $ty,
                    rsc as isize,
                    csc as isize,
                );
            }
            0
        }
    };
}

/// Register wisp_dgemm / wisp_sgemm on a linker. With `enabled ==
/// false` both report -1 and the guest computes locally
/// (determinism mode).
pub(crate) fn add_to_linker(linker: &mut Linker<WasiP1Ctx>, enabled: bool) -> anyhow::Result<()> {
    if enabled {
        linker.func_wrap(
            "env",
            "wisp_dgemm",
            gemm_host_fn!("wisp_dgemm", f64, 8, dgemm_exec),
        )?;
        linker.func_wrap(
            "env",
            "wisp_sgemm",
            gemm_host_fn!("wisp_sgemm", f32, 4, sgemm_exec),
        )?;
    } else {
        linker.func_wrap(
            "env",
            "wisp_dgemm",
            |_: Caller<'_, WasiP1Ctx>,
             _: i32,
             _: i32,
             _: i32,
             _: f64,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: f64,
             _: i32,
             _: i32,
             _: i32|
             -> i32 { -1 },
        )?;
        linker.func_wrap(
            "env",
            "wisp_sgemm",
            |_: Caller<'_, WasiP1Ctx>,
             _: i32,
             _: i32,
             _: i32,
             _: f32,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: i32,
             _: f32,
             _: i32,
             _: i32,
             _: i32|
             -> i32 { -1 },
        )?;
    }
    Ok(())
}

/// WISP_ACCEL env toggle — default on; "0"/"false"/"off" disables.
pub(crate) fn enabled_from_env() -> bool {
    !matches!(
        std::env::var("WISP_ACCEL").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}
