//! 直方图手写 SIMD 内核（AVX2 + AVX512，运行时派发）。
//!
//! 参考 OpenCV5 `histogram.cpp` 的 `calcHist_8u`（查找表 + 局部累加）与
//! `equalizeHist`（直方图 → LUT → 应用）。

use crate::simd_kernels::x86;

/// 计算 8U 单通道直方图（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为灰度像素（每像素 1 字节），`n` 为像素数。`hist` 长度 ≥ `hist_size`。
/// `tab` 为 256 项查找表（像素值 → bin 偏移，越界为 `OUT_OF_RANGE`）。
pub fn calc_hist_8u(src: &[u8], n: usize, hist: &mut [i32], tab: &[usize; 256]) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe { x86::avx2::calc_hist_8u(src, n, hist, tab) },
        x86::Isa::Scalar => scalar_calc_hist_8u(src, n, hist, tab),
    }
}

/// 标量 8U 直方图（正确性基准，查找表 + 局部累加，与 OpenCV `calcHist_8u` 一致）。
pub fn scalar_calc_hist_8u(src: &[u8], n: usize, hist: &mut [i32], tab: &[usize; 256]) {
    let mut mat_h = [0i32; 256];
    let mut i = 0;
    while i + 8 <= n {
        let t0 = src[i] as usize;
        let t1 = src[i + 1] as usize;
        let t2 = src[i + 2] as usize;
        let t3 = src[i + 3] as usize;
        let t4 = src[i + 4] as usize;
        let t5 = src[i + 5] as usize;
        let t6 = src[i + 6] as usize;
        let t7 = src[i + 7] as usize;
        mat_h[t0] += 1;
        mat_h[t1] += 1;
        mat_h[t2] += 1;
        mat_h[t3] += 1;
        mat_h[t4] += 1;
        mat_h[t5] += 1;
        mat_h[t6] += 1;
        mat_h[t7] += 1;
        i += 8;
    }
    while i < n {
        mat_h[src[i] as usize] += 1;
        i += 1;
    }
    for (j, &c) in mat_h.iter().enumerate() {
        let hidx = tab[j];
        if hidx != usize::MAX {
            hist[hidx] += c;
        }
    }
}

/// 应用 LUT（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为灰度像素，`dst` 为输出，`n` 为像素数。`lut` 为 256 项映射表。
pub fn apply_lut(src: &[u8], dst: &mut [u8], n: usize, lut: &[u8; 256]) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::apply_lut(src, dst, n, lut) },
        x86::Isa::Avx2 => unsafe { x86::avx2::apply_lut(src, dst, n, lut) },
        x86::Isa::Scalar => scalar_apply_lut(src, dst, n, lut),
    }
}

/// 标量 LUT 应用（正确性基准）。
pub fn scalar_apply_lut(src: &[u8], dst: &mut [u8], n: usize, lut: &[u8; 256]) {
    for i in 0..n {
        dst[i] = lut[src[i] as usize];
    }
}

/// 直方图比较方法（与 `imgproc::histogram::HistCompMethod` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistCompMethod {
    Correl,
    ChiSqr,
    Intersect,
    Bhattacharyya,
    ChiSqrAlt,
    KlDiv,
}

/// 比较两个直方图（运行时派发：AVX512 → AVX2 → 标量）。
///
/// 位级对齐 OpenCV：累加用 `f64` 2-lane 并行（模拟 OpenCV SSE SIMD 路径的求和顺序）。
pub fn compare_hist(h1: &[f32], h2: &[f32], method: HistCompMethod) -> f64 {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::compare_hist(h1, h2, method)
        },
        x86::Isa::Scalar => scalar_compare_hist(h1, h2, method),
    }
}

/// 标量直方图比较（正确性基准，2-lane f64 模拟 OpenCV SSE SIMD 路径）。
pub fn scalar_compare_hist(h1: &[f32], h2: &[f32], method: HistCompMethod) -> f64 {
    let len = h1.len();
    let dbl_eps = f64::EPSILON;
    let flt_eps = f32::EPSILON;

    match method {
        HistCompMethod::ChiSqr | HistCompMethod::ChiSqrAlt => {
            let mut lanes = [0.0f64; 2];
            let mut j = 0;
            while j + 4 <= len {
                for k in 0..4 {
                    let idx = j + k;
                    let a = h1[idx] as f64 - h2[idx] as f64;
                    let b = match method {
                        HistCompMethod::ChiSqr => h1[idx] as f64,
                        _ => h1[idx] as f64 + h2[idx] as f64,
                    };
                    if b.abs() > dbl_eps {
                        lanes[k % 2] += a * a * (1.0 / b);
                    }
                }
                j += 4;
            }
            let mut result = lanes[0] + lanes[1];
            for idx in j..len {
                let a = h1[idx] as f64 - h2[idx] as f64;
                let b = match method {
                    HistCompMethod::ChiSqr => h1[idx] as f64,
                    _ => h1[idx] as f64 + h2[idx] as f64,
                };
                if b.abs() > dbl_eps {
                    result += a * a * (1.0 / b);
                }
            }
            if method == HistCompMethod::ChiSqrAlt {
                result *= 2.0;
            }
            result
        }
        HistCompMethod::Correl => {
            let mut s1 = [0.0f64; 2];
            let mut s2 = [0.0f64; 2];
            let mut s11 = [0.0f64; 2];
            let mut s12 = [0.0f64; 2];
            let mut s22 = [0.0f64; 2];
            let mut j = 0;
            while j + 4 <= len {
                for k in 0..4 {
                    let idx = j + k;
                    let a = h1[idx] as f64;
                    let b = h2[idx] as f64;
                    let l = k % 2;
                    s12[l] += a * b;
                    s1[l] += a;
                    s11[l] += a * a;
                    s2[l] += b;
                    s22[l] += b * b;
                }
                j += 4;
            }
            let mut s1 = s1[0] + s1[1];
            let mut s2 = s2[0] + s2[1];
            let mut s11 = s11[0] + s11[1];
            let mut s12 = s12[0] + s12[1];
            let mut s22 = s22[0] + s22[1];
            for idx in j..len {
                let a = h1[idx] as f64;
                let b = h2[idx] as f64;
                s12 += a * b;
                s1 += a;
                s11 += a * a;
                s2 += b;
                s22 += b * b;
            }
            let total = len as f64;
            let scale = 1.0 / total;
            let num = s12 - s1 * s2 * scale;
            let denom2 = (s11 - s1 * s1 * scale) * (s22 - s2 * s2 * scale);
            if denom2.abs() > dbl_eps {
                num / denom2.sqrt()
            } else {
                1.0
            }
        }
        HistCompMethod::Intersect => {
            let mut lanes = [0.0f64; 2];
            let mut j = 0;
            while j + 4 <= len {
                for k in 0..4 {
                    let idx = j + k;
                    lanes[k % 2] += h1[idx].min(h2[idx]) as f64;
                }
                j += 4;
            }
            let mut result = lanes[0] + lanes[1];
            for idx in j..len {
                result += h1[idx].min(h2[idx]) as f64;
            }
            result
        }
        HistCompMethod::Bhattacharyya => {
            let mut s1 = [0.0f64; 2];
            let mut s2 = [0.0f64; 2];
            let mut result = [0.0f64; 2];
            let mut j = 0;
            while j + 4 <= len {
                for k in 0..4 {
                    let idx = j + k;
                    let a = h1[idx] as f64;
                    let b = h2[idx] as f64;
                    let l = k % 2;
                    result[l] += (a * b).sqrt();
                    s1[l] += a;
                    s2[l] += b;
                }
                j += 4;
            }
            let mut s1 = s1[0] + s1[1];
            let mut s2 = s2[0] + s2[1];
            let mut result = result[0] + result[1];
            for idx in j..len {
                let a = h1[idx] as f64;
                let b = h2[idx] as f64;
                result += (a * b).sqrt();
                s1 += a;
                s2 += b;
            }
            s1 *= s2;
            s1 = if s1.abs() > flt_eps as f64 {
                1.0 / s1.sqrt()
            } else {
                1.0
            };
            (1.0 - result * s1).max(0.0).sqrt()
        }
        HistCompMethod::KlDiv => {
            let mut result = 0.0f64;
            for j in 0..len {
                let p = h1[j] as f64;
                let mut q = h2[j] as f64;
                if p.abs() <= dbl_eps {
                    continue;
                }
                if q.abs() <= dbl_eps {
                    q = 1e-10;
                }
                result += p * (p / q).ln();
            }
            result
        }
    }
}