//! 通用二维卷积 SIMD 内核（AVX2，运行时派发）。
//!
//! 参考 OpenCV5 `filter.simd.hpp` 的 `FilterVec_8u`（预计算边界行 + 逐行 SIMD 卷积）。

use crate::simd_kernels::x86;

/// 计算 u8 二维卷积（运行时派发：AVX2 → 标量）。
///
/// `src` 为 `w*h` 灰度像素，`coords` 为非零核位置（相对锚点偏移），`coeffs` 为对应系数。
/// `delta` 为累加偏移。`out` 为 `w*h` 输出。
pub fn filter2d_u8(
    src: &[u8],
    w: usize,
    h: usize,
    coords: &[(i32, i32)],
    coeffs: &[f32],
    delta: f32,
    out: &mut [u8],
) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::filter2d_u8(src, w, h, coords, coeffs, delta, out)
        },
        x86::Isa::Scalar => scalar_filter2d_u8(src, w, h, coords, coeffs, delta, out),
    }
}

/// 标量 u8 二维卷积（正确性基准）。
pub fn scalar_filter2d_u8(
    src: &[u8],
    w: usize,
    h: usize,
    coords: &[(i32, i32)],
    coeffs: &[f32],
    delta: f32,
    out: &mut [u8],
) {
    let nz = coords.len();
    for y in 0..h {
        for x in 0..w {
            let mut s0 = delta;
            for k in 0..nz {
                let (kx, ky) = coords[k];
                let sx = x as i32 + kx;
                let sy = y as i32 + ky;
                let v = if sx >= 0 && sx < w as i32 && sy >= 0 && sy < h as i32 {
                    src[sy as usize * w + sx as usize]
                } else {
                    0
                };
                s0 += coeffs[k] * v as f32;
            }
            out[y * w + x] = crate::core::image::Pixel::from_f32(s0);
        }
    }
}

/// 计算 u8 3x3 二维卷积（运行时派发：AVX2 → 标量）。
///
/// 仅用于 3x3 全非零核 + Replicate 边界 + 锚点 (1,1)。`k` 为 9 个系数（行优先，
/// 对应 `(-1,-1), (0,-1), (1,-1), (-1,0), (0,0), (1,0), (-1,1), (0,1), (1,1)`）。
pub fn filter2d_3x3_u8(
    src: &[u8],
    w: usize,
    h: usize,
    k: &[f32; 9],
    delta: f32,
    out: &mut [u8],
) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::filter2d_3x3_u8(src, w, h, k, delta, out)
        },
        x86::Isa::Scalar => scalar_filter2d_3x3_u8(src, w, h, k, delta, out),
    }
}

/// 标量 u8 3x3 二维卷积（正确性基准）。
pub fn scalar_filter2d_3x3_u8(
    src: &[u8],
    w: usize,
    h: usize,
    k: &[f32; 9],
    delta: f32,
    out: &mut [u8],
) {
    let coords = [(-1i32, -1i32), (-1, 0), (-1, 1), (0, -1), (0, 0), (0, 1), (1, -1), (1, 0), (1, 1)];
    for y in 0..h {
        for x in 0..w {
            let mut s0 = delta;
            for (idx, &(kx, ky)) in coords.iter().enumerate() {
                let sx = x as i32 + kx;
                let sy = y as i32 + ky;
                let v = if sx >= 0 && sx < w as i32 && sy >= 0 && sy < h as i32 {
                    src[sy as usize * w + sx as usize]
                } else {
                    0
                };
                s0 += k[idx] * v as f32;
            }
            out[y * w + x] = crate::core::image::Pixel::from_f32(s0);
        }
    }
}

/// 计算 u8 可分离二维滤波（运行时派发：AVX2 → 标量）。
///
/// 先对每行用 `kx` 做水平滤波（输出 f32 中间结果），再对每列用 `ky` 做垂直滤波
/// （输出 u8）。`ax`/`ay` 为核锚点，`delta` 为累加偏移。仅支持 Replicate 边界。
pub fn sep_filter2d_u8(
    src: &[u8],
    w: usize,
    h: usize,
    kx: &[f32],
    ky: &[f32],
    ax: i32,
    ay: i32,
    delta: f32,
    out: &mut [u8],
) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::avx2::sep_filter2d_u8(src, w, h, kx, ky, ax, ay, delta, out)
        },
        x86::Isa::Scalar => scalar_sep_filter2d_u8(src, w, h, kx, ky, ax, ay, delta, out),
    }
}

/// 标量 u8 可分离二维滤波（正确性基准）。
pub fn scalar_sep_filter2d_u8(
    src: &[u8],
    w: usize,
    h: usize,
    kx: &[f32],
    ky: &[f32],
    ax: i32,
    ay: i32,
    delta: f32,
    out: &mut [u8],
) {
    let (kx_len, ky_len) = (kx.len(), ky.len());
    // 水平滤波：tmp[y][x] = sum(kx[i] * src[y][x + i - ax])，输出 f32
    let mut tmp = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s0 = 0.0f32;
            for i in 0..kx_len {
                let sx = x as i32 + i as i32 - ax;
                let v = if sx >= 0 && sx < w as i32 {
                    src[y * w + sx as usize]
                } else if sx < 0 {
                    src[y * w]
                } else {
                    src[y * w + w - 1]
                };
                s0 += kx[i] * v as f32;
            }
            tmp[y * w + x] = s0;
        }
    }
    // 垂直滤波：dst[y][x] = saturate_cast<u8>(sum(ky[j] * tmp[y + j - ay][x]) + delta)
    for y in 0..h {
        for x in 0..w {
            let mut s0 = delta;
            for j in 0..ky_len {
                let sy = y as i32 + j as i32 - ay;
                let v = if sy >= 0 && sy < h as i32 {
                    tmp[sy as usize * w + x]
                } else if sy < 0 {
                    tmp[x]
                } else {
                    tmp[(h - 1) * w + x]
                };
                s0 += ky[j] * v;
            }
            out[y * w + x] = crate::core::image::Pixel::from_f32(s0);
        }
    }
}