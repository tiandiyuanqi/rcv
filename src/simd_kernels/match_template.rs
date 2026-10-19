//! 模板匹配 SIMD 内核（AVX2，运行时派发）。
//!
//! 参考 OpenCV5 `templmatch.cpp` 的 `crossCorr`（互相关计算）。

use crate::simd_kernels::x86;

/// 计算 u8 互相关（CCORR）：`corr[y][x] = sum(T * I)`，输出 f32。
///
/// `src` 为 `w*h` 灰度像素，`templ` 为 `tw*th` 模板。`corr` 为 `(w-tw+1)*(h-th+1)` 输出。
pub fn ccorr_u8(
    src: &[u8],
    w: usize,
    h: usize,
    templ: &[u8],
    tw: usize,
    th: usize,
    corr: &mut [f32],
) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::ccorr_u8(src, w, h, templ, tw, th, corr)
        },
        x86::Isa::Scalar => scalar_ccorr_u8(src, w, h, templ, tw, th, corr),
    }
}

/// 标量 u8 互相关（正确性基准）。
pub fn scalar_ccorr_u8(
    src: &[u8],
    w: usize,
    h: usize,
    templ: &[u8],
    tw: usize,
    th: usize,
    corr: &mut [f32],
) {
    let (rw, rh) = (w - tw + 1, h - th + 1);
    for y in 0..rh {
        for x in 0..rw {
            let mut s = 0.0f32;
            for ty in 0..th {
                for tx in 0..tw {
                    s += templ[ty * tw + tx] as f32 * src[(y + ty) * w + (x + tx)] as f32;
                }
            }
            corr[y * rw + x] = s;
        }
    }
}
/// 归一化互相关结果（AVX2 加速，f64 积分图查询向量化）。
///
/// 对齐 OpenCV `templmatch.cpp` 的归一化逻辑：
/// - num_type 0（CCORR）：num = corr
/// - num_type 1（CCOEFF）：num = corr - wnd_sum * templ_mean
/// - num_type 2（SQDIFF）：num = wnd_sum2 - 2*corr + templ_sum2
/// - is_normed：num /= sqrt(wnd_sum2 - wnd_mean2) * templ_norm（带 clamp）
///
/// `sum`/`sqsum` 为 `(w+1)*(h+1)` 的 f64 积分图，`sw = w+1`。
pub fn normalize_corr_simd(
    corr: &[f32],
    sum: &[i32],
    sqsum: &[f64],
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    num_type: i32,
    is_normed: bool,
    is_sqdiff_normed: bool,
    inv_area: f64,
    templ_mean: f64,
    templ_norm: f64,
    templ_sum2: f64,
    out: &mut [f32],
) {
    let (rw, rh) = (w - tw + 1, h - th + 1);
    debug_assert_eq!(out.len(), rw * rh);
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::normalize_corr_avx2(
                corr, sum, sqsum, w, h, tw, th, rw, rh,
                num_type, is_normed, is_sqdiff_normed,
                inv_area, templ_mean, templ_norm, templ_sum2, out,
            )
        },
        x86::Isa::Scalar => scalar_normalize_corr(
            corr, sum, sqsum, w, h, tw, th, num_type, is_normed, is_sqdiff_normed,
            inv_area, templ_mean, templ_norm, templ_sum2, out,
        ),
    }
}

/// 标量归一化（正确性基准，对齐 OpenCV 逻辑）。
#[allow(clippy::too_many_arguments)]
pub fn scalar_normalize_corr(
    corr: &[f32],
    sum: &[i32],
    sqsum: &[f64],
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    num_type: i32,
    is_normed: bool,
    is_sqdiff_normed: bool,
    inv_area: f64,
    templ_mean: f64,
    templ_norm: f64,
    templ_sum2: f64,
    out: &mut [f32],
) {
    let (rw, rh) = (w - tw + 1, h - th + 1);
    let sw = w + 1;
    for y in 0..rh {
        for x in 0..rw {
            let mut num = corr[y * rw + x] as f64;
            let mut wnd_mean2 = 0.0f64;
            let mut wnd_sum2 = 0.0f64;
            let p0 = sum[y * sw + x] as f64;
            let p1 = sum[y * sw + x + tw] as f64;
            let p2 = sum[(y + th) * sw + x] as f64;
            let p3 = sum[(y + th) * sw + x + tw] as f64;
            let wnd_sum = p0 - p1 - p2 + p3;
            if num_type == 1 {
                wnd_mean2 = wnd_sum * wnd_sum * inv_area;
                num -= wnd_sum * templ_mean;
            }
            if is_normed || num_type == 2 {
                let q0 = sqsum[y * sw + x];
                let q1 = sqsum[y * sw + x + tw];
                let q2 = sqsum[(y + th) * sw + x];
                let q3 = sqsum[(y + th) * sw + x + tw];
                wnd_sum2 = q0 - q1 - q2 + q3;
                if num_type == 2 {
                    num = wnd_sum2 - 2.0 * num + templ_sum2;
                    num = num.max(0.0);
                }
            }
            if is_normed {
                let diff2 = (wnd_sum2 - wnd_mean2).max(0.0);
                let t = if diff2 <= 0.5f64.min(10.0 * f32::EPSILON as f64 * wnd_sum2) {
                    0.0
                } else {
                    diff2.sqrt() * templ_norm
                };
                if num.abs() < t {
                    num /= t;
                } else if num.abs() < t * 1.125 {
                    num = if num > 0.0 { 1.0 } else { -1.0 };
                } else {
                    num = if !is_sqdiff_normed { 0.0 } else { 1.0 };
                }
            }
            out[y * rw + x] = num as f32;
        }
    }
}
