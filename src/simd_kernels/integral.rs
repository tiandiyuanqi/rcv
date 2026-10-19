//! 积分图 SIMD 内核（AVX2，运行时派发）。
//!
//! 参考 OpenCV5 `sumpixels.simd.hpp` 的 `Integral_SIMD<uchar, int, double>`。

use crate::simd_kernels::x86;

/// 计算 u8→i32 积分图（运行时派发：AVX2 → 标量）。
///
/// `src` 为 `w*h` 灰度像素，`sum` 为 `(w+1)*(h+1)` 输出（首行首列 0）。
pub fn integral_u8_i32(src: &[u8], w: usize, h: usize, sum: &mut [i32]) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe { x86::avx2::integral_u8_i32(src, w, h, sum) },
        x86::Isa::Scalar => scalar_integral_u8_i32(src, w, h, sum),
    }
}

/// 标量 u8→i32 积分图（正确性基准）。
pub fn scalar_integral_u8_i32(src: &[u8], w: usize, h: usize, sum: &mut [i32]) {
    let sumstep = w + 1;
    for y in 0..h {
        let mut s = 0i32;
        let src_row = &src[y * w..(y + 1) * w];
        let cur = (y + 1) * sumstep;
        let prev = y * sumstep;
        for x in 0..w {
            s += src_row[x] as i32;
            sum[cur + x + 1] = sum[prev + x + 1] + s;
        }
    }
}

/// 计算 u8→i32+f64 积分图（sum + sqsum，运行时派发：AVX2 → 标量）。
///
/// `src` 为 `w*h` 灰度像素，`sum` 为 `(w+1)*(h+1)` i32 输出，`sqsum` 为
/// `(w+1)*(h+1)` f64 输出（首行首列 0）。
pub fn integral_u8_i32_sqsum(src: &[u8], w: usize, h: usize, sum: &mut [i32], sqsum: &mut [f64]) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::integral_u8_i32_sqsum(src, w, h, sum, sqsum)
        },
        x86::Isa::Scalar => scalar_integral_u8_i32_sqsum(src, w, h, sum, sqsum),
    }
}

/// 标量 u8→i32+f64 积分图（正确性基准）。
pub fn scalar_integral_u8_i32_sqsum(src: &[u8], w: usize, h: usize, sum: &mut [i32], sqsum: &mut [f64]) {
    let sumstep = w + 1;
    for y in 0..h {
        let mut s = 0i32;
        let mut sq = 0.0f64;
        let src_row = &src[y * w..(y + 1) * w];
        let cur = (y + 1) * sumstep;
        let prev = y * sumstep;
        for x in 0..w {
            let it = src_row[x] as i32;
            s += it;
            sq += (it as f64) * (it as f64);
            sum[cur + x + 1] = sum[prev + x + 1] + s;
            sqsum[cur + x + 1] = sqsum[prev + x + 1] + sq;
        }
    }
}