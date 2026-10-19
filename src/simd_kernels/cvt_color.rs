//! cvtColor 手写 SIMD 内核（AVX2 + AVX512，运行时派发）。
//!
//! 参考 OpenCV5 `color_yuv.simd.hpp` 的 `RGB2YCrCb_i<uchar>` / `YCrCb2RGB_i<uchar>`。
//! 整数定点（YUV_SHIFT=14），用 `_mm256_madd_epi16` 点积。

use crate::simd_kernels::x86;

/// RGB u8 → HSV u8（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为 RGB 交错（每像素 3 字节），`dst` 为 HSV 交错（每像素 3 字节），`n` 为像素数。
pub fn rgb_to_hsv(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::rgb_to_hsv(src, dst, n, hrange) },
        x86::Isa::Avx2 => unsafe { x86::avx2::rgb_to_hsv(src, dst, n, hrange) },
        x86::Isa::Scalar => scalar_rgb_to_hsv(src, dst, n, hrange),
    }
}

/// 标量 RGB2HSV（正确性基准）。
pub fn scalar_rgb_to_hsv(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    let sdiv = crate::imgproc::cvt_color::hsv_sdiv_table();
    let hdiv = crate::imgproc::cvt_color::hsv_hdiv_table(hrange);
    let descale = 1 << 11;
    for i in 0..n {
        let r = src[i * 3] as i32;
        let g = src[i * 3 + 1] as i32;
        let b = src[i * 3 + 2] as i32;
        let v = r.max(g).max(b);
        let vmin = r.min(g).min(b);
        let diff = (v - vmin) as u8;
        let vr = if v == r { -1 } else { 0 };
        let vg = if v == g { -1 } else { 0 };
        let s = (diff as i32 * sdiv[v as usize] + descale) >> 12;
        let h = (vr & (g - b))
            + (!vr & ((vg & (b - r + 2 * diff as i32)) + ((!vg) & (r - g + 4 * diff as i32))));
        let h = (h * hdiv[diff as usize] + descale) >> 12;
        let h = if h < 0 { h + hrange } else { h };
        dst[i * 3] = h as u8;
        dst[i * 3 + 1] = s as u8;
        dst[i * 3 + 2] = v as u8;
    }
}

/// HSV u8 → RGB u8（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为 HSV 交错（每像素 3 字节），`dst` 为 RGB 交错（每像素 3 字节），`n` 为像素数。
pub fn hsv_to_rgb(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe { x86::avx2::hsv_to_rgb(src, dst, n, hrange) },
        x86::Isa::Scalar => scalar_hsv_to_rgb(src, dst, n, hrange),
    }
}

/// 标量 HSV2RGB（正确性基准，RNE + 无 FMA）。
pub fn scalar_hsv_to_rgb(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    let hscale = 6.0f32 / hrange as f32;
    let sector_data: [[usize; 3]; 6] = [
        [1, 3, 0],
        [1, 0, 2],
        [3, 0, 1],
        [0, 2, 1],
        [0, 1, 3],
        [2, 1, 0],
    ];
    for i in 0..n {
        let h = src[i * 3] as f32;
        let s = src[i * 3 + 1] as f32 * (1.0f32 / 255.0f32);
        let v = src[i * 3 + 2] as f32 * (1.0f32 / 255.0f32);
        let (b, g, r) = if s == 0.0 {
            (v, v, v)
        } else {
            let mut h = h * hscale;
            let mut sector = h.floor() as i32;
            h -= sector as f32;
            sector %= 6;
            if sector < 0 {
                sector += 6;
            }
            let tab = [
                v,
                v * (1.0 - s),
                v * (1.0 - s * h),
                v * (1.0 - s * (1.0 - h)),
            ];
            let sd = sector_data[sector as usize];
            (tab[sd[0]], tab[sd[1]], tab[sd[2]])
        };
        dst[i * 3] = (r * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
        dst[i * 3 + 1] = (g * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
        dst[i * 3 + 2] = (b * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
    }
}

/// RGB u8 → YCrCb u8（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为 RGB 交错（每像素 3 字节），`dst` 为 YCrCb 交错（每像素 3 字节），`n` 为像素数。
pub fn rgb_to_ycrcb(src: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::rgb_to_ycrcb(src, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::rgb_to_ycrcb(src, dst, n) },
        x86::Isa::Scalar => scalar_rgb_to_ycrcb(src, dst, n),
    }
}

/// YCrCb u8 → RGB u8（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为 YCrCb 交错（每像素 3 字节），`dst` 为 RGB 交错（每像素 3 字节），`n` 为像素数。
pub fn ycrcb_to_rgb(src: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::ycrcb_to_rgb(src, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::ycrcb_to_rgb(src, dst, n) },
        x86::Isa::Scalar => scalar_ycrcb_to_rgb(src, dst, n),
    }
}

/// 标量 RGB2YCrCb（正确性基准）。
pub fn scalar_rgb_to_ycrcb(src: &[u8], dst: &mut [u8], n: usize) {
    const YUV_SHIFT: i32 = 14;
    const R2Y: i32 = 4899;
    const G2Y: i32 = 9617;
    const B2Y: i32 = 1868;
    const YCRI: i32 = 11682;
    const YCBI: i32 = 9241;
    let delta = 128 << YUV_SHIFT;
    let descale = 1 << (YUV_SHIFT - 1);
    for i in 0..n {
        let r = src[i * 3] as i32;
        let g = src[i * 3 + 1] as i32;
        let b = src[i * 3 + 2] as i32;
        let y = (r * R2Y + g * G2Y + b * B2Y + descale) >> YUV_SHIFT;
        let cr = ((r - y) * YCRI + delta + descale) >> YUV_SHIFT;
        let cb = ((b - y) * YCBI + delta + descale) >> YUV_SHIFT;
        dst[i * 3] = y.clamp(0, 255) as u8;
        dst[i * 3 + 1] = cr.clamp(0, 255) as u8;
        dst[i * 3 + 2] = cb.clamp(0, 255) as u8;
    }
}

/// 标量 YCrCb2RGB（正确性基准）。
pub fn scalar_ycrcb_to_rgb(src: &[u8], dst: &mut [u8], n: usize) {
    const YUV_SHIFT: i32 = 14;
    const CR2RI: i32 = 22987;
    const CR2GI: i32 = -11698;
    const CB2GI: i32 = -5636;
    const CB2BI: i32 = 29049;
    let descale = 1 << (YUV_SHIFT - 1);
    for i in 0..n {
        let y = src[i * 3] as i32;
        let cr = src[i * 3 + 1] as i32;
        let cb = src[i * 3 + 2] as i32;
        let b = y + (((cb - 128) * CB2BI + descale) >> YUV_SHIFT);
        let g = y + ((((cb - 128) * CB2GI + (cr - 128) * CR2GI) + descale) >> YUV_SHIFT);
        let r = y + (((cr - 128) * CR2RI + descale) >> YUV_SHIFT);
        dst[i * 3] = r.clamp(0, 255) as u8;
        dst[i * 3 + 1] = g.clamp(0, 255) as u8;
        dst[i * 3 + 2] = b.clamp(0, 255) as u8;
    }
}