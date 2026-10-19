//! AVX512 手写内核（与 AVX2 同时落地，运行时派发）。
//!
//! 参考 OpenCV5 `color_rgb.simd.hpp` 的 `RGB2Gray<uchar>`。
//! 每批处理 64 像素（解交织复用 AVX2 的 32 像素块，灰度计算用 AVX512）。

use std::arch::x86_64::*;

use super::avx2;

/// RGB u8 → 灰度 u8（AVX512，每批 64 像素）。
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn to_grayscale_u8(src: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+64 <= n 保证读写在界内；CPU 特性由调用方（isa() 派发）保证
    unsafe {
        while i + 64 <= n {
            // 两个 32 像素块解交织（复用 AVX2）
            let (r0, g0, b0) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i),
            );
            let (r1, g1, b1) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 96) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 128) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 160) as *const __m256i),
            );
            let gray0 = gray_from_rgb(r0, g0, b0);
            let gray1 = gray_from_rgb(r1, g1, b1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, gray0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 32) as *mut __m256i, gray1);
            i += 64;
        }
        // 剩余用 AVX2
        avx2::to_grayscale_u8(&src[i * 3..], &mut dst[i..], n - i);
    }
}

/// 从 32×u8 通道向量计算灰度（AVX512，`_mm512_madd_epi16`）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn gray_from_rgb(r: __m256i, g: __m256i, b: __m256i) -> __m256i {
    // 系数（RGB 输入）：cb=3735(B), cg=19235(G), cr=9798(R)
    // gray = b*cb + g*cg + r*cr = B*3735 + G*19235 + R*9798
    // _mm512_set_epi16 逆序：最后一个参数是元素 0
    // SAFETY: 调用方（avx512 主函数）已保证 AVX512F/BW/VL 可用；纯向量运算
    unsafe {
        let bg2y = _mm512_set_epi16(
            19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235,
            3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735,
            19235, 3735, 19235, 3735, 19235, 3735,
        );
        let r12y = _mm512_set_epi16(
            1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1,
            9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798,
        );
        let delta = _mm512_set1_epi16(16384);

        // 零扩展 u8 → u16（32 个）
        let r0 = _mm512_cvtepu8_epi16(r);
        let g0 = _mm512_cvtepu8_epi16(g);
        let b0 = _mm512_cvtepu8_epi16(b);

        // v_zip 交织（AVX512 单 lane，无需跨 lane 修正）
        let (bg0, bg1) = v_zip(b0, g0);
        let (rd0, rd1) = v_zip(r0, delta);

        // 点积 + 加偏置 + 右移 15
        let y0 = _mm512_srli_epi32(
            _mm512_add_epi32(_mm512_madd_epi16(bg0, bg2y), _mm512_madd_epi16(rd0, r12y)),
            15,
        );
        let y1 = _mm512_srli_epi32(
            _mm512_add_epi32(_mm512_madd_epi16(bg1, bg2y), _mm512_madd_epi16(rd1, r12y)),
            15,
        );

        // 打包 u32 → u16 → u8
        let y = pack_u32_to_u16(y0, y1);
        pack_u16_to_u8(y)
    }
}

/// `v_zip`（AVX512，u16）：跨 lane 正确交织（permutex2var，intrin_avx512.hpp:549-556）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn v_zip(a: __m512i, b: __m512i) -> (__m512i, __m512i) {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    // mask0: [a0 b0 a1 b1 ... a15 b15]（索引 0..31 交错）
    let mask0 = _mm512_set_epi16(
        47, 15, 46, 14, 45, 13, 44, 12, 43, 11, 42, 10, 41, 9, 40, 8, 39, 7, 38, 6, 37, 5, 36, 4,
        35, 3, 34, 2, 33, 1, 32, 0,
    );
    // mask1: [a16 b16 a17 b17 ... a31 b31]
    let mask1 = _mm512_set_epi16(
        63, 31, 62, 30, 61, 29, 60, 28, 59, 27, 58, 26, 57, 25, 56, 24, 55, 23, 54, 22, 53, 21, 52,
        20, 51, 19, 50, 18, 49, 17, 48, 16,
    );
    (
        _mm512_permutex2var_epi16(a, mask0, b),
        _mm512_permutex2var_epi16(a, mask1, b),
    )
}

/// `v_pack`（u32→u16）：饱和到 65535 后截断打包。
///
/// 注意：AVX512 `_mm512_packus_epi32` 按 128 位 lane 交错（非拼接），
/// OpenCV 用 `_mm512_cvtepi32_epi16`（intrin_avx512.hpp:2113-2117）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_u32_to_u16(a: __m512i, b: __m512i) -> __m512i {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    let m = _mm512_set1_epi32(65535);
    let lo = _mm512_cvtepi32_epi16(_mm512_min_epu32(a, m));
    let hi = _mm512_cvtepi32_epi16(_mm512_min_epu32(b, m));
    _mm512_inserti64x4(_mm512_castsi256_si512(lo), hi, 1)
}

/// `v_pack`（u16→u8）：饱和到 255 并打包。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_u16_to_u8(a: __m512i) -> __m256i {
    let a = _mm512_min_epu16(a, _mm512_set1_epi16(255));
    _mm512_cvtepi16_epi8(a)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avx512_adaptive_thresh_matches_scalar() {
        let n = 300; // 跨 64 边界 + AVX2 尾
        let mut src = vec![0u8; n];
        let mut mean = vec![0u8; n];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 31 % 256) as u8;
        }
        for (i, v) in mean.iter_mut().enumerate() {
            *v = ((i * 17 + 60) % 256) as u8;
        }
        for &inv in &[false, true] {
            for &idelta in &[-255i32, -10, 0, 7, 255] {
                let mut dst_scalar = vec![0u8; n];
                crate::simd_kernels::scalar::adaptive_thresh_u8(
                    &src, &mean, &mut dst_scalar, n, 200, idelta, inv,
                );
                let mut dst_simd = vec![0u8; n];
                // SAFETY: 测试机已验证支持 AVX512（本模块其余测试同前提）
                unsafe { adaptive_thresh_u8(&src, &mean, &mut dst_simd, n, 200, idelta, inv) };
                assert_eq!(dst_scalar, dst_simd, "inv={inv} idelta={idelta}");
            }
        }
    }

    #[test]
    fn avx512_matches_scalar() {
        let n = 200;
        let mut src = vec![0u8; n * 3];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 13 % 256) as u8;
        }
        let mut dst_scalar = vec![0u8; n];
        let mut dst_simd = vec![0u8; n];
        for i in 0..n {
            let r = src[i * 3] as u32;
            let g = src[i * 3 + 1] as u32;
            let b = src[i * 3 + 2] as u32;
            dst_scalar[i] = ((r * 9798 + g * 19235 + b * 3735 + 16384) >> 15) as u8;
        }
        unsafe { to_grayscale_u8(&src, &mut dst_simd, n) };
        assert_eq!(dst_scalar, dst_simd, "AVX512 mismatch");
    }

    #[test]
    fn avx512_ycrcb_to_rgb_matches_scalar() {
        let n = 100;
        let mut src = vec![0u8; n * 3];
        for i in 0..n {
            src[i * 3] = (i * 7 % 256) as u8;
            src[i * 3 + 1] = (i * 13 % 256) as u8;
            src[i * 3 + 2] = (i * 29 % 256) as u8;
        }
        let mut dst_scalar = vec![0u8; n * 3];
        let mut dst_simd = vec![0u8; n * 3];
        crate::simd_kernels::cvt_color::scalar_ycrcb_to_rgb(&src, &mut dst_scalar, n);
        unsafe { ycrcb_to_rgb(&src, &mut dst_simd, n) };
        assert_eq!(dst_scalar, dst_simd, "AVX512 ycrcb_to_rgb mismatch");
    }
}

/// u8 阈值化（AVX512，每批 64 元素，原生无符号掩码比较）。///
/// 语义与 OpenCV thresh.cpp:251-330 一致（见 avx2::threshold_u8 文档）。
/// `type_`: 0=Binary 1=BinaryInv 2=Trunc 3=ToZero 4=ToZeroInv
///
/// # Safety
/// - `src`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn threshold_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8, type_: u8) {
    let thresh_u = _mm512_set1_epi8(t as i8);
    let maxval16 = _mm512_set1_epi8(mv as i8);
    let zero = _mm512_setzero_si512();
    // 非临时存储（movntdq）需 64 字节对齐 dst；先算对齐头，中部用流式写
    // 避免写分配（RFO），提升纯流式带宽（实测 ~2-6%）。
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    let mut i = 0;
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界；CPU 特性由 isa() 派发保证
    unsafe {
        // 头部（< 64 字节，未对齐，标量）
        while i < head {
            let v = *src.get_unchecked(i);
            *dst.get_unchecked_mut(i) = match type_ {
                0 => if v > t { mv } else { 0 },
                1 => if v <= t { mv } else { 0 },
                2 => v.min(t),
                3 => if v > t { v } else { 0 },
                4 => if v <= t { v } else { 0 },
                _ => v,
            };
            i += 1;
        }
        // 中部（64 字节对齐，非临时存储），4×64 展开隐藏 load/store 延迟
        while i + 256 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let v1 = _mm512_loadu_si512(src.as_ptr().add(i + 64) as *const __m512i);
            let v2 = _mm512_loadu_si512(src.as_ptr().add(i + 128) as *const __m512i);
            let v3 = _mm512_loadu_si512(src.as_ptr().add(i + 192) as *const __m512i);
            let r0 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(maxval16, _mm512_cmpgt_epu8_mask(v0, thresh_u), zero),
                2 => _mm512_min_epu8(v0, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), v0),
                4 => _mm512_mask_mov_epi8(v0, _mm512_cmpgt_epu8_mask(v0, thresh_u), zero),
                _ => v0,
            };
            let r1 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(maxval16, _mm512_cmpgt_epu8_mask(v1, thresh_u), zero),
                2 => _mm512_min_epu8(v1, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), v1),
                4 => _mm512_mask_mov_epi8(v1, _mm512_cmpgt_epu8_mask(v1, thresh_u), zero),
                _ => v1,
            };
            let r2 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v2, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(maxval16, _mm512_cmpgt_epu8_mask(v2, thresh_u), zero),
                2 => _mm512_min_epu8(v2, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v2, thresh_u), v2),
                4 => _mm512_mask_mov_epi8(v2, _mm512_cmpgt_epu8_mask(v2, thresh_u), zero),
                _ => v2,
            };
            let r3 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v3, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(maxval16, _mm512_cmpgt_epu8_mask(v3, thresh_u), zero),
                2 => _mm512_min_epu8(v3, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v3, thresh_u), v3),
                4 => _mm512_mask_mov_epi8(v3, _mm512_cmpgt_epu8_mask(v3, thresh_u), zero),
                _ => v3,
            };
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 64) as *mut __m512i, r1);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 128) as *mut __m512i, r2);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 192) as *mut __m512i, r3);
            i += 256;
        }
        while i + 128 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let v1 = _mm512_loadu_si512(src.as_ptr().add(i + 64) as *const __m512i);
            let r0 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(
                    maxval16,
                    _mm512_cmpgt_epu8_mask(v0, thresh_u),
                    zero,
                ),
                2 => _mm512_min_epu8(v0, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), v0),
                4 => _mm512_mask_mov_epi8(v0, _mm512_cmpgt_epu8_mask(v0, thresh_u), zero),
                _ => v0,
            };
            let r1 = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(
                    maxval16,
                    _mm512_cmpgt_epu8_mask(v1, thresh_u),
                    zero,
                ),
                2 => _mm512_min_epu8(v1, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), v1),
                4 => _mm512_mask_mov_epi8(v1, _mm512_cmpgt_epu8_mask(v1, thresh_u), zero),
                _ => v1,
            };
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 64) as *mut __m512i, r1);
            i += 128;
        }
        while i + 64 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let r = match type_ {
                0 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16),
                1 => _mm512_mask_mov_epi8(
                    maxval16,
                    _mm512_cmpgt_epu8_mask(v0, thresh_u),
                    zero,
                ),
                2 => _mm512_min_epu8(v0, thresh_u),
                3 => _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), v0),
                4 => _mm512_mask_mov_epi8(v0, _mm512_cmpgt_epu8_mask(v0, thresh_u), zero),
                _ => v0,
            };
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    // 剩余用 AVX2
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度 n-i 与子内核契约一致
    unsafe {
        avx2::threshold_u8(&src[i..], &mut dst[i..], n - i, t, mv, type_);
    }
}

/// 二值阈值特化（type 0，THRESH_BINARY）：`dst = (src > t) ? mv : 0`。
///
/// 无 `match type_` 分支，热循环更紧凑。非临时存储 + 4×64 展开。
///
/// # Safety
/// `n <= src.len() == dst.len()`；CPU 须支持 AVX512F/BW/VL。
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn threshold_binary_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8) {
    let thresh_u = _mm512_set1_epi8(t as i8);
    let maxval16 = _mm512_set1_epi8(mv as i8);
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    let mut i = 0;
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界
    unsafe {
        while i < head {
            *dst.get_unchecked_mut(i) = if *src.get_unchecked(i) > t { mv } else { 0 };
            i += 1;
        }
        while i + 256 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let v1 = _mm512_loadu_si512(src.as_ptr().add(i + 64) as *const __m512i);
            let v2 = _mm512_loadu_si512(src.as_ptr().add(i + 128) as *const __m512i);
            let v3 = _mm512_loadu_si512(src.as_ptr().add(i + 192) as *const __m512i);
            let r0 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16);
            let r1 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), maxval16);
            let r2 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v2, thresh_u), maxval16);
            let r3 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v3, thresh_u), maxval16);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 64) as *mut __m512i, r1);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 128) as *mut __m512i, r2);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 192) as *mut __m512i, r3);
            i += 256;
        }
        while i + 128 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let v1 = _mm512_loadu_si512(src.as_ptr().add(i + 64) as *const __m512i);
            let r0 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16);
            let r1 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v1, thresh_u), maxval16);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 64) as *mut __m512i, r1);
            i += 128;
        }
        while i + 64 <= n {
            let v0 = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let r0 = _mm512_maskz_mov_epi8(_mm512_cmpgt_epu8_mask(v0, thresh_u), maxval16);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            i += 64;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::threshold_u8(&src[i..], &mut dst[i..], n - i, t, mv, 0);
    }
}

/// 自适应阈值化（AVX512，每批 64 像素，原生 i16 掩码比较）。
///
/// 语义见 avx2::adaptive_thresh_u8 文档。
///
/// # Safety
/// - `src`/`mean`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn adaptive_thresh_u8(
    src: &[u8],
    mean: &[u8],
    dst: &mut [u8],
    n: usize,
    imax: u8,
    idelta: i32,
    inv: bool,
) {
    // d ∈ [-255,255] + idelta ∈ [-255,255] → thr ∈ [-510,510]，i16 安全
    let thr = (-idelta) as i16;
    let imax_v = _mm512_set1_epi8(imax as i8);
    let mut i = 0;
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界；CPU 特性由 isa() 派发保证
    unsafe {
        while i + 64 <= n {
            let s = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let m = _mm512_loadu_si512(mean.as_ptr().add(i) as *const __m512i);
            // u8 → u16 差（i16，负差合法）
            let s_lo = _mm512_unpacklo_epi8(s, _mm512_setzero_si512());
            let s_hi = _mm512_unpackhi_epi8(s, _mm512_setzero_si512());
            let m_lo = _mm512_unpacklo_epi8(m, _mm512_setzero_si512());
            let m_hi = _mm512_unpackhi_epi8(m, _mm512_setzero_si512());
            let d_lo = _mm512_sub_epi16(s_lo, m_lo);
            let d_hi = _mm512_sub_epi16(s_hi, m_hi);
            // 有符号比较 d > thr → i16 掩码（0xFFFF/0x0000）。
            // AVX512 无掩码寄存器版 cmpgt_epi16，用符号位技巧：
            // d > thr ⟺ d + (32767 - thr) ≥ 0（|d - thr| ≤ 1020，无溢出），
            // 取加法结果每 i16 的符号位扩展成掩码。
            let bias = _mm512_set1_epi16(32767 - thr);
            let gt_lo = _mm512_srai_epi16(_mm512_add_epi16(d_lo, bias), 15);
            let gt_hi = _mm512_srai_epi16(_mm512_add_epi16(d_hi, bias), 15);
            // i16 掩码 → u8 掩码：packs_epi16 与 unpack 的 lane 语义互逆，
            // 恢复原始字节顺序（0xFFFF 饱和为 0xFF）
            let gt8 = _mm512_packs_epi16(gt_lo, gt_hi);
            let r = if inv {
                // BinaryInv: d <= thr → imax
                _mm512_andnot_si512(gt8, imax_v)
            } else {
                // Binary: d > thr → imax
                _mm512_and_si512(gt8, imax_v)
            };
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    // 剩余用 AVX2
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度 n-i 与子内核契约一致
    unsafe {
        avx2::adaptive_thresh_u8(&src[i..], &mean[i..], &mut dst[i..], n - i, imax, idelta, inv);
    }
}

#[cfg(test)]
mod thresh_tests {
    use super::*;

    #[test]
    fn avx512_threshold_all_types() {
        let n = 300; // 跨 64 边界 + AVX2 尾 + 标量尾
        let mut src = vec![0u8; n];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 37 % 256) as u8;
        }
        for ty in 0..5u8 {
            for (t, mv) in [(0u8, 255u8), (100, 255), (254, 200), (255, 255)] {
                let mut dst_scalar = vec![0u8; n];
                let mut dst_simd = vec![0u8; n];
                crate::simd_kernels::scalar::threshold_u8(&src, &mut dst_scalar, n, t, mv, ty);
                unsafe { threshold_u8(&src, &mut dst_simd, n, t, mv, ty) };
                assert_eq!(dst_scalar, dst_simd, "type={ty} t={t} mv={mv}");
            }
        }
    }
}

/// box_filter 行阶段：u8 → u16 滑动窗行和（AVX512，每批 64 元素，仅 cn=1）。
///
/// cn=1 时 kw=3/5 直接求和（手写 SIMD），通用 kw 滑动窗（标量）；
/// cn>1 时复用 AVX2 的自动向量化路径。
///
/// # Safety
/// - `src` 长度 ≥ `(w + kw - 1)*cn`，`dst` 长度 ≥ `w*cn`
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn box_row_sum_u8(src: &[u8], dst: &mut [u16], w: usize, kw: usize, cn: usize) {
    match (cn, kw) {
        (1, 3) => unsafe { row_sum_k3(src, dst, w) },
        (1, 5) => unsafe { row_sum_k5(src, dst, w) },
        _ => {
            // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度与子内核契约一致
            unsafe { avx2::box_row_sum_u8(src, dst, w, kw, cn) }
        }
    }
}

/// kw=3：`D[i] = S[i] + S[i+1] + S[i+2]`（每批 64 元素）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn row_sum_k3(src: &[u8], dst: &mut [u16], w: usize) {
    let mut i = 0;
    // SAFETY: i+64 <= w 保证读 [i, i+66)（src.len() >= w+2）写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= w {
            let a = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(i + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(i + 2) as *const __m512i);
            // 64 u8 → 64 u16：低/高 256 位各 cvtepu8_epi16（32 u8 → 32 u16）
            let s0 = sum3_u8_to_u16_512(a, b, c);
            let a2 = _mm512_loadu_si512(src.as_ptr().add(i + 32) as *const __m512i);
            let b2 = _mm512_loadu_si512(src.as_ptr().add(i + 33) as *const __m512i);
            let c2 = _mm512_loadu_si512(src.as_ptr().add(i + 34) as *const __m512i);
            let s1 = sum3_u8_to_u16_512(a2, b2, c2);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i + 32) as *mut __m512i, s1);
            i += 64;
        }
    }
    // 剩余用 AVX2
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度与子内核契约一致
    unsafe { avx2::box_row_sum_u8(&src[i..], &mut dst[i..], w - i, 3, 1) }
}

/// 3 个 u8 向量（各 32 字节，取低 256 位）求和 → 32 u16。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn sum3_u8_to_u16_512(a: __m512i, b: __m512i, c: __m512i) -> __m512i {
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由调用方保证
    _mm512_add_epi16(
        _mm512_add_epi16(
            _mm512_cvtepu8_epi16(_mm512_castsi512_si256(a)),
            _mm512_cvtepu8_epi16(_mm512_castsi512_si256(b)),
        ),
        _mm512_cvtepu8_epi16(_mm512_castsi512_si256(c)),
    )
}

/// kw=5：`D[i] = S[i..i+5] 之和`（每批 64 元素）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn row_sum_k5(src: &[u8], dst: &mut [u16], w: usize) {
    let mut i = 0;
    // SAFETY: i+64 <= w 保证读 [i, i+68)（src.len() >= w+4）写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= w {
            // 64 u8 → 64 u16：低/高 256 位各 cvtepu8_epi16（32 u8 → 32 u16）
            let load = |off: usize| {
                _mm512_cvtepu8_epi16(_mm512_castsi512_si256(_mm512_loadu_si512(
                    src.as_ptr().add(i + off) as *const __m512i,
                )))
            };
            let a = load(0);
            let b = load(1);
            let c = load(2);
            let d = load(3);
            let e = load(4);
            let s0 = _mm512_add_epi16(
                _mm512_add_epi16(_mm512_add_epi16(a, b), _mm512_add_epi16(c, d)),
                e,
            );
            let load2 = |off: usize| {
                _mm512_cvtepu8_epi16(_mm512_castsi512_si256(_mm512_loadu_si512(
                    src.as_ptr().add(i + 32 + off) as *const __m512i,
                )))
            };
            let a2 = load2(0);
            let b2 = load2(1);
            let c2 = load2(2);
            let d2 = load2(3);
            let e2 = load2(4);
            let s1 = _mm512_add_epi16(
                _mm512_add_epi16(_mm512_add_epi16(a2, b2), _mm512_add_epi16(c2, d2)),
                e2,
            );
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i + 32) as *mut __m512i, s1);
            i += 64;
        }
    }
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度与子内核契约一致
    unsafe { avx2::box_row_sum_u8(&src[i..], &mut dst[i..], w - i, 5, 1) }
}

/// box_filter 列阶段：u16 行和 → u8（AVX512，SHIFT=23 定点除法，每批 64 元素）。
///
/// 对齐 OpenCV `ColumnSum<ushort, uchar>` SIMD 主循环（box_filter.simd.hpp:515-546）。
///
/// # Safety
/// - `sum`/`sp`/`sm`/`dst` 长度 ≥ `w`
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn box_col_step_u16(
    sum: &mut [u16],
    sp: &[u16],
    sm: &[u16],
    dst: &mut [u8],
    w: usize,
    fx: crate::simd_kernels::box_filter::BoxFixed,
    have_scale: bool,
) {
    let ds4 = _mm512_set1_epi32(fx.ds as i32);
    let dd8 = _mm512_set1_epi16(fx.dd as i16);
    let mut i = 0;
    // SAFETY: i+64 <= w 保证所有读写 [i, i+64) 不越界；CPU 特性由 isa() 派发保证
    unsafe {
        if have_scale {
            while i + 64 <= w {
                let sm0 = _mm512_loadu_si512(sm.as_ptr().add(i) as *const __m512i);
                let sm1 = _mm512_loadu_si512(sm.as_ptr().add(i + 32) as *const __m512i);
                let s0 = _mm512_add_epi16(
                    _mm512_loadu_si512(sum.as_ptr().add(i) as *const __m512i),
                    _mm512_loadu_si512(sp.as_ptr().add(i) as *const __m512i),
                );
                let s1 = _mm512_add_epi16(
                    _mm512_loadu_si512(sum.as_ptr().add(i + 32) as *const __m512i),
                    _mm512_loadu_si512(sp.as_ptr().add(i + 32) as *const __m512i),
                );
                // (s + dd) 扩展到 u32，乘 ds，右移 23
                let (s0e0, s0e1) = expand_u16_u32(_mm512_add_epi16(s0, dd8));
                let (s1e0, s1e1) = expand_u16_u32(_mm512_add_epi16(s1, dd8));
                let r0 = _mm512_srli_epi32(_mm512_mullo_epi32(s0e0, ds4), 23);
                let r1 = _mm512_srli_epi32(_mm512_mullo_epi32(s0e1, ds4), 23);
                let r2 = _mm512_srli_epi32(_mm512_mullo_epi32(s1e0, ds4), 23);
                let r3 = _mm512_srli_epi32(_mm512_mullo_epi32(s1e1, ds4), 23);
                // pack u32 -> u16 -> u8（截断语义：OpenCV (uchar) 转换，
                // wrap 乘法结果可 > 255，必须截断而非饱和）
                let p0 = pack_u32_to_u16(r0, r1);
                let p1 = pack_u32_to_u16(r2, r3);
                let mask = _mm512_set1_epi16(0xFF);
                let lo = _mm512_cvtepi16_epi8(_mm512_and_si512(p0, mask));
                let hi = _mm512_cvtepi16_epi8(_mm512_and_si512(p1, mask));
                let out = _mm512_inserti64x4(_mm512_castsi256_si512(lo), hi, 1);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, out);
                // SUM' = s - Sm（wrap）
                _mm512_storeu_si512(
                    sum.as_mut_ptr().add(i) as *mut __m512i,
                    _mm512_sub_epi16(s0, sm0),
                );
                _mm512_storeu_si512(
                    sum.as_mut_ptr().add(i + 32) as *mut __m512i,
                    _mm512_sub_epi16(s1, sm1),
                );
                i += 64;
            }
        } else {
            let max16 = _mm512_set1_epi16(255);
            while i + 64 <= w {
                let sm0 = _mm512_loadu_si512(sm.as_ptr().add(i) as *const __m512i);
                let sm1 = _mm512_loadu_si512(sm.as_ptr().add(i + 32) as *const __m512i);
                let s0 = _mm512_add_epi16(
                    _mm512_loadu_si512(sum.as_ptr().add(i) as *const __m512i),
                    _mm512_loadu_si512(sp.as_ptr().add(i) as *const __m512i),
                );
                let s1 = _mm512_add_epi16(
                    _mm512_loadu_si512(sum.as_ptr().add(i + 32) as *const __m512i),
                    _mm512_loadu_si512(sp.as_ptr().add(i + 32) as *const __m512i),
                );
                let lo = _mm512_cvtusepi16_epi8(_mm512_min_epu16(s0, max16));
                let hi = _mm512_cvtusepi16_epi8(_mm512_min_epu16(s1, max16));
                let out = _mm512_inserti64x4(_mm512_castsi256_si512(lo), hi, 1);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, out);
                _mm512_storeu_si512(
                    sum.as_mut_ptr().add(i) as *mut __m512i,
                    _mm512_sub_epi16(s0, sm0),
                );
                _mm512_storeu_si512(
                    sum.as_mut_ptr().add(i + 32) as *mut __m512i,
                    _mm512_sub_epi16(s1, sm1),
                );
                i += 64;
            }
        }
    }
    // 剩余用 AVX2
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度与子内核契约一致
    unsafe {
        avx2::box_col_step_u16(
            &mut sum[i..],
            &sp[i..],
            &sm[i..],
            &mut dst[i..],
            w - i,
            fx,
            have_scale,
        )
    }
}

/// u16 向量零扩展为两个 u32 向量（低/高 256 位）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn expand_u16_u32(v: __m512i) -> (__m512i, __m512i) {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    let lo = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(v));
    let hi = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(v, 1));
    (lo, hi)
}

/// box_filter 行阶段（无边框源，AVX512 特权编译）：u8 → u16 行和，kw=3/5。
///
/// # Safety
/// - `src` 长度 ≥ `w*cn`，`dst` 长度 ≥ `w*cn`，`kw` ∈ {3, 5}
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn box_row_sum_u8_direct_cn(
    src: &[u8],
    dst: &mut [u16],
    w: usize,
    kw: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    if kw == 3 && cn == 3 {
        // C=3、kw=3 特化：dst[i] = src[i-3] + src[i] + src[i+3]（3 抽头，步长 1）
        let total = w * cn;
        let lo = 3;
        let hi = total.saturating_sub(3);
        let mut i = lo;
        // SAFETY: i+64 <= hi 保证读 [i-3, i+67) 写 [i, i+64) 不越界
        unsafe {
            while i + 64 <= hi {
                let a = _mm512_loadu_si512(src.as_ptr().add(i - 3) as *const __m512i);
                let b = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
                let c = _mm512_loadu_si512(src.as_ptr().add(i + 3) as *const __m512i);
                // 各 u8 向量零扩展为低/高 256 位两个 u16 向量
                let a0 = _mm512_cvtepu8_epi16(_mm512_castsi512_si256(a));
                let a1 = _mm512_cvtepu8_epi16(_mm512_extracti64x4_epi64(a, 1));
                let b0 = _mm512_cvtepu8_epi16(_mm512_castsi512_si256(b));
                let b1 = _mm512_cvtepu8_epi16(_mm512_extracti64x4_epi64(b, 1));
                let c0 = _mm512_cvtepu8_epi16(_mm512_castsi512_si256(c));
                let c1 = _mm512_cvtepu8_epi16(_mm512_extracti64x4_epi64(c, 1));
                let s0 = _mm512_add_epi16(_mm512_add_epi16(a0, b0), c0);
                let s1 = _mm512_add_epi16(_mm512_add_epi16(a1, b1), c1);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i + 32) as *mut __m512i, s1);
                i += 64;
            }
        }
        // 剩余标量
        for j in i..hi {
            dst[j] = src[j - 3] as u16 + src[j] as u16 + src[j + 3] as u16;
        }
        crate::simd_kernels::box_filter::row_sum_edges(src, dst, w, kw, cn, border);
        return;
    }
    // SAFETY: AVX512F/BW/VL 蕴含 AVX2；切片长度与子内核契约一致
    unsafe { avx2::box_row_sum_u8_direct_cn(src, dst, w, kw, cn, border) }
}

/// gaussian 水平阶段 121 核特化（AVX512）：`dst = (a + 2b + c) << 6`，32/批。
///
/// # Safety
/// - 契约同 [`avx2::hline_smooth_3n121`]
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn hline_smooth_3n121(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    let mut i = cn;
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        while i + 32 <= total - cn {
            let a = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i - cn) as *const _));
            let b = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i) as *const _));
            let c = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i + cn) as *const _));
            let sum = _mm512_add_epi16(_mm512_add_epi16(a, c), _mm512_add_epi16(b, b));
            let out = _mm512_slli_epi16(sum, 6);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 32;
        }
    }
    while i < total - cn {
        let (a, b, c) = (src[i - cn] as u16, src[i] as u16, src[i + cn] as u16);
        dst[i] = (a + 2 * b + c) << 6;
        i += 1;
    }
    crate::simd_kernels::gaussian::hline_smooth_3n121_edges(src, dst, len, cn, border);
}

/// gaussian 垂直阶段 121 核特化（AVX512）：`(r0 + 2r1 + r2 + 512) >> 10`，32/批。
///
/// # Safety
/// - 各行与 `dst` 等长
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn vline_smooth_3n121(rows: &[&[u16]; 3], dst: &mut [u8]) {
    let len = dst.len();
    let mut i = 0;
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        while i + 32 <= len {
            let r0 = _mm512_loadu_si512(rows[0].as_ptr().add(i) as *const _);
            let r1 = _mm512_loadu_si512(rows[1].as_ptr().add(i) as *const _);
            let r2 = _mm512_loadu_si512(rows[2].as_ptr().add(i) as *const _);
            // u16 → u32 扩展（cvtepu16_epi32 顺序正确）
            let r0l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r0));
            let r0h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r0, 1));
            let r1l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r1));
            let r1h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r1, 1));
            let r2l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r2));
            let r2h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r2, 1));
            let sum_l = _mm512_add_epi32(_mm512_add_epi32(r0l, r2l), _mm512_add_epi32(r1l, r1l));
            let sum_h = _mm512_add_epi32(_mm512_add_epi32(r0h, r2h), _mm512_add_epi32(r1h, r1h));
            let bias = _mm512_set1_epi32(1 << 9);
            let v_l = _mm512_srli_epi32(_mm512_add_epi32(sum_l, bias), 10);
            let v_h = _mm512_srli_epi32(_mm512_add_epi32(sum_h, bias), 10);
            // u32 → u16（cvtusepi32_epi16 顺序压缩）→ 拼接 → u8
            let w0 = _mm512_cvtusepi32_epi16(v_l);
            let w1 = _mm512_cvtusepi32_epi16(v_h);
            let w512 = _mm512_inserti64x4(_mm512_castsi256_si512(w0), w1, 1);
            let out = _mm512_cvtusepi16_epi8(w512);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 32;
        }
    }
    while i < len {
        let v = (rows[0][i] as u32 + 2 * rows[1][i] as u32 + rows[2][i] as u32 + (1 << 9)) >> 10;
        dst[i] = v.min(0xFF) as u8;
        i += 1;
    }
}

/// gaussian 水平阶段 14641 核特化（AVX512）：`(a+4b+6c+4d+e)<<4`，32/批。
///
/// # Safety
/// - 契约同 [`avx2::hline_smooth_5n14641`]
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn hline_smooth_5n14641(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    let body_end = total.saturating_sub(2 * cn);
    let mut i = 2 * cn;
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        while i + 32 <= body_end {
            let a =
                _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i - 2 * cn) as *const _));
            let b = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i - cn) as *const _));
            let c = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i) as *const _));
            let d = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i + cn) as *const _));
            let e =
                _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(i + 2 * cn) as *const _));
            let sum = _mm512_add_epi16(
                _mm512_add_epi16(a, e),
                _mm512_add_epi16(
                    _mm512_slli_epi16(_mm512_add_epi16(b, d), 2),
                    _mm512_mullo_epi16(c, _mm512_set1_epi16(6)),
                ),
            );
            let out = _mm512_slli_epi16(sum, 4);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 32;
        }
    }
    while i < body_end {
        let (a, b) = (src[i - 2 * cn] as u16, src[i - cn] as u16);
        let (c, d) = (src[i] as u16, src[i + cn] as u16);
        let e = src[i + 2 * cn] as u16;
        dst[i] = (a + 4 * b + 6 * c + 4 * d + e) << 4;
        i += 1;
    }
    crate::simd_kernels::gaussian::hline_smooth_5n14641_edges(src, dst, len, cn, border);
}

/// gaussian 垂直阶段 14641 核特化（AVX512）：32/批。
///
/// # Safety
/// - 各行与 `dst` 等长
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn vline_smooth_5n14641(rows: &[&[u16]; 5], dst: &mut [u8]) {
    let len = dst.len();
    let mut i = 0;
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        while i + 32 <= len {
            let r0 = _mm512_loadu_si512(rows[0].as_ptr().add(i) as *const _);
            let r1 = _mm512_loadu_si512(rows[1].as_ptr().add(i) as *const _);
            let r2 = _mm512_loadu_si512(rows[2].as_ptr().add(i) as *const _);
            let r3 = _mm512_loadu_si512(rows[3].as_ptr().add(i) as *const _);
            let r4 = _mm512_loadu_si512(rows[4].as_ptr().add(i) as *const _);
            let r0l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r0));
            let r0h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r0, 1));
            let r1l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r1));
            let r1h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r1, 1));
            let r2l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r2));
            let r2h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r2, 1));
            let r3l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r3));
            let r3h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r3, 1));
            let r4l = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(r4));
            let r4h = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(r4, 1));
            let six = _mm512_set1_epi32(6);
            let sum_l = _mm512_add_epi32(
                _mm512_add_epi32(r0l, r4l),
                _mm512_add_epi32(
                    _mm512_slli_epi32(_mm512_add_epi32(r1l, r3l), 2),
                    _mm512_mullo_epi32(r2l, six),
                ),
            );
            let sum_h = _mm512_add_epi32(
                _mm512_add_epi32(r0h, r4h),
                _mm512_add_epi32(
                    _mm512_slli_epi32(_mm512_add_epi32(r1h, r3h), 2),
                    _mm512_mullo_epi32(r2h, six),
                ),
            );
            let bias = _mm512_set1_epi32(1 << 11);
            let v_l = _mm512_srli_epi32(_mm512_add_epi32(sum_l, bias), 12);
            let v_h = _mm512_srli_epi32(_mm512_add_epi32(sum_h, bias), 12);
            let w0 = _mm512_cvtusepi32_epi16(v_l);
            let w1 = _mm512_cvtusepi32_epi16(v_h);
            let w512 = _mm512_inserti64x4(_mm512_castsi256_si512(w0), w1, 1);
            let out = _mm512_cvtusepi16_epi8(w512);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 32;
        }
    }
    while i < len {
        let v = (rows[2][i] as u32 * 6
            + ((rows[1][i] as u32 + rows[3][i] as u32) << 2)
            + rows[0][i] as u32
            + rows[4][i] as u32
            + (1 << 11))
            >> 12;
        dst[i] = v.min(0xFF) as u8;
        i += 1;
    }
}

/// 14641 二维融合主体区（AVX512）：x ∈ [2,w-2)，y ∈ [2,h-2)，32/批。
///
/// # Safety
/// - 契约同 [`avx2::smooth_5n14641`]
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn smooth_5n14641<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    y_lo: usize,
    y_hi: usize,
) {
    let cn = C;
    let row_w = w * cn;
    if w < 5 || h < 5 {
        return;
    }
    let x_lo = 2 * cn;
    let x_hi = row_w - 2 * cn;
    let y_start = y_lo.max(2);
    let y_end = y_hi.min(h - 2);
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        // 列归约辅助：u16 → u32（cvtepu16_epi32 顺序正确）
        let e = |v: __m512i| -> (__m512i, __m512i) {
            (
                _mm512_cvtepu16_epi32(_mm512_castsi512_si256(v)),
                _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(v, 1)),
            )
        };
        let six16 = _mm512_set1_epi16(6);
        let six32 = _mm512_set1_epi32(6);
        let bias = _mm512_set1_epi32(128);
        // 行和：b(yy) = 6s + 4(s[-cn]+s[cn]) + s[-2cn] + s[2cn]
        let bsum = |yy: usize, x: usize| -> __m512i {
            let base = yy * row_w + x;
            let s = _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(base) as *const _));
            let sn =
                _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(base - cn) as *const _));
            let sp =
                _mm512_cvtepu8_epi16(_mm256_loadu_si256(src.as_ptr().add(base + cn) as *const _));
            let snn = _mm512_cvtepu8_epi16(_mm256_loadu_si256(
                src.as_ptr().add(base - 2 * cn) as *const _
            ));
            let spp = _mm512_cvtepu8_epi16(_mm256_loadu_si256(
                src.as_ptr().add(base + 2 * cn) as *const _
            ));
            _mm512_add_epi16(
                _mm512_add_epi16(snn, spp),
                _mm512_add_epi16(
                    _mm512_mullo_epi16(s, six16),
                    _mm512_slli_epi16(_mm512_add_epi16(sn, sp), 2),
                ),
            )
        };
        // b 行滚动缓冲：brows[行%5][批]，每输出行只算 1 个新 bsum
        let nb = (x_hi - x_lo).div_ceil(32);
        let mut brows: Vec<Vec<__m512i>> = (0..5)
            .map(|_| crate::simd_kernels::alloc_uninit(nb))
            .collect();
        // 预填 5 行 b（y_start-2..y_start+3），按绝对槽位 yy%5 存
        for k in 0..5usize {
            let yy = y_start - 2 + k;
            for (bi, bv) in brows[yy % 5].iter_mut().enumerate() {
                *bv = bsum(yy, x_lo + bi * 32);
            }
        }
        for y in y_start..y_end {
            let dst_row = &mut dst[(y - y_lo) * row_w..(y - y_lo + 1) * row_w];
            // 新行 b（y+2）进槽 (y+2)%5 对应位置——滚动：槽 k = (y - 2 + k)
            // 每输出行只需算 1 个新行 b(y+2)，写入槽 (y+2)%5
            let new_slot = (y + 2) % 5;
            for (bi, bv) in brows[new_slot].iter_mut().enumerate() {
                *bv = bsum(y + 2, x_lo + bi * 32);
            }
            for (bi, _) in brows[0].iter().enumerate() {
                let x = x_lo + bi * 32;
                if x + 32 > x_hi {
                    // 尾批：标量处理
                    let mut xx = x;
                    while xx < x_hi {
                        let bs = |yy: usize| -> u32 {
                            let base = yy * row_w + xx;
                            (src[base] as u32 * 6)
                                + ((src[base - cn] as u32 + src[base + cn] as u32) << 2)
                                + src[base - 2 * cn] as u32
                                + src[base + 2 * cn] as u32
                        };
                        let b0 = bs(y - 2);
                        let b1 = bs(y - 1);
                        let b2 = bs(y);
                        let b3 = bs(y + 1);
                        let b4 = bs(y + 2);
                        let v = (b2 * 6 + ((b1 + b3) << 2) + b0 + b4 + 128) >> 8;
                        dst_row[xx] = v.min(0xFF) as u8;
                        xx += 1;
                    }
                    break;
                }
                let b0 = brows[(y - 2) % 5][bi];
                let b1 = brows[(y - 1) % 5][bi];
                let b2 = brows[y % 5][bi];
                let b3 = brows[(y + 1) % 5][bi];
                let b4 = brows[new_slot][bi];
                let (b0l, b0h) = e(b0);
                let (b1l, b1h) = e(b1);
                let (b2l, b2h) = e(b2);
                let (b3l, b3h) = e(b3);
                let (b4l, b4h) = e(b4);
                let sum_l = _mm512_add_epi32(
                    _mm512_add_epi32(b0l, b4l),
                    _mm512_add_epi32(
                        _mm512_slli_epi32(_mm512_add_epi32(b1l, b3l), 2),
                        _mm512_mullo_epi32(b2l, six32),
                    ),
                );
                let sum_h = _mm512_add_epi32(
                    _mm512_add_epi32(b0h, b4h),
                    _mm512_add_epi32(
                        _mm512_slli_epi32(_mm512_add_epi32(b1h, b3h), 2),
                        _mm512_mullo_epi32(b2h, six32),
                    ),
                );
                let v_l = _mm512_srli_epi32(_mm512_add_epi32(sum_l, bias), 8);
                let v_h = _mm512_srli_epi32(_mm512_add_epi32(sum_h, bias), 8);
                let w0 = _mm512_cvtusepi32_epi16(v_l);
                let w1 = _mm512_cvtusepi32_epi16(v_h);
                let w512 = _mm512_inserti64x4(_mm512_castsi256_si512(w0), w1, 1);
                let out = _mm512_cvtusepi16_epi8(w512);
                _mm256_storeu_si256(dst_row.as_mut_ptr().add(x) as *mut _, out);
            }
        }
    }
}

/// gaussian 水平阶段通用版（AVX512F/BW/VL 特权编译）：32 元素/批。
///
/// 对齐 `hlineSmooth<u8,ufixedpoint16>` SIMD 主体：
/// `res = Σ sat_mul(m_j, expand(src[i+j·cn]))` 饱和累加。
///
/// # Safety
/// - `src` 长度 ≥ `len*cn`，`dst` 长度 ≥ `len*cn`，`m.len()` 为奇数
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn hline_smooth_u8(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    m: &[u16],
    border: crate::core::border::BorderPolicy<u8>,
) {
    let n = m.len();
    let pre = n / 2;
    let post = n - pre;
    let total = len * cn;
    // 主体：i ∈ [pre·cn, (len-post)·cn)（严格无越界，边框由 edges 特判）
    let lo = pre * cn;
    let hi = ((len - post) * cn).min(total);
    let mut i = lo;
    if i < hi {
        let vm: Vec<__m512i> = m.iter().map(|&mk| _mm512_set1_epi16(mk as i16)).collect();
        let base_off = pre * cn;
        // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
        unsafe {
            while i + 32 <= hi {
                let mut acc = sat_mul_u16(
                    _mm512_cvtepu8_epi16(_mm256_loadu_si256(
                        src.as_ptr().add(i - base_off) as *const _
                    )),
                    vm[0],
                );
                for (j, &mv) in vm.iter().enumerate().skip(1) {
                    let s = _mm512_cvtepu8_epi16(_mm256_loadu_si256(
                        src.as_ptr().add(i - base_off + j * cn) as *const _,
                    ));
                    acc = sat_add_u16(acc, sat_mul_u16(s, mv));
                }
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut _, acc);
                i += 32;
            }
        }
        while i < hi {
            let mut acc: u32 = (m[0] as u32 * src[i - base_off] as u32).min(0xFFFF);
            for (j, &mj) in m.iter().enumerate().skip(1) {
                let prod = (mj as u32 * src[i - base_off + j * cn] as u32).min(0xFFFF);
                acc = (acc + prod).min(0xFFFF);
            }
            dst[i] = acc as u16;
            i += 1;
        }
    }
    crate::simd_kernels::gaussian::hline_smooth_u8_edges(src, dst, len, cn, m, border);
}

/// gaussian 垂直阶段通用版（AVX512F/BW/VL 特权编译）：32/批。
///
/// `acc(u32) = Σ m_j·s_j`（u16×u16→u32 无饱和），
/// `dst = sat_u8((acc + 2^15) >> 16)`。
///
/// # Safety
/// - `rows.len() == m.len()`，各行与 `dst` 等长
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn vline_smooth_u8(rows: &[&[u16]], dst: &mut [u8], m: &[u16]) {
    let len = dst.len();
    let mut i = 0;
    // SAFETY: 纯向量运算，AVX512F/BW/VL 由本函数 target_feature 保证
    unsafe {
        while i + 32 <= len {
            // 两个独立 acc：低 16 元素 [i, i+16)、高 16 元素 [i+16, i+32)
            let mut acc0 = _mm512_setzero_si512();
            let mut acc1 = _mm512_setzero_si512();
            for (j, &mk) in m.iter().enumerate() {
                let mv = _mm512_set1_epi16(mk as i16);
                let s = _mm512_loadu_si512(rows[j].as_ptr().add(i) as *const _);
                // u16×u16 → u32：cvtepu16_epi32 扩展后 mullo_epi32（值域内一致）
                let s0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(s));
                let s1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(s, 1));
                let m0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(mv));
                let m1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(mv, 1));
                acc0 = _mm512_add_epi32(acc0, _mm512_mullo_epi32(s0, m0));
                acc1 = _mm512_add_epi32(acc1, _mm512_mullo_epi32(s1, m1));
            }
            // +2^15 >> 16 → u16（≤255）→ u8 饱和 pack
            let half = _mm512_set1_epi32(1 << 15);
            acc0 = _mm512_srli_epi32(_mm512_add_epi32(acc0, half), 16);
            acc1 = _mm512_srli_epi32(_mm512_add_epi32(acc1, half), 16);
            let w0 = _mm512_cvtusepi32_epi16(acc0);
            let w1 = _mm512_cvtusepi32_epi16(acc1);
            let w512 = _mm512_inserti64x4(_mm512_castsi256_si512(w0), w1, 1);
            let out = _mm512_cvtusepi16_epi8(w512);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 32;
        }
    }
    while i < len {
        let mut acc: u32 = 0;
        for (k, &mk) in m.iter().enumerate() {
            acc = acc.saturating_add((mk as u64 * rows[k][i] as u64).min(0xFFFF_FFFF) as u32);
        }
        dst[i] = (((acc as u64 + (1 << 15)) >> 16).min(0xFF)) as u8;
        i += 1;
    }
}

/// u16×u16 饱和乘（hline 专用：核系数 ≤ 256、src ≤ 255，
/// 乘积 ≤ 65280 恒不溢出 u16，mullo 即可）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn sat_mul_u16(a: __m512i, b: __m512i) -> __m512i {
    // SAFETY: 纯向量运算；值域保证无溢出
    _mm512_mullo_epi16(a, b)
}

/// u16+u16 饱和加（对齐 OpenCV `v_add<v_uint16>`）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn sat_add_u16(a: __m512i, b: __m512i) -> __m512i {
    // SAFETY: 纯向量运算
    _mm512_adds_epu16(a, b)
}

/// 3x3 中值滤波（AVX512，u8，Clamp 边界）。
///
/// 对齐 OpenCV `medianBlur_SortNet` m==3 向量路径：每批 64 字节，
/// 从 3 行各取左/中/右 3 个向量，用 `_mm512_min_epu8`/`_mm512_max_epu8`
/// 跑排序网络。首尾 `cn` 字节用标量处理（Clamp 边界）。
///
/// # Safety
/// - `src` 长度 ≥ `w*h*cn`，`dst` 同尺寸
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn median_blur_u8_3x3(src: &[u8], dst: &mut [u8], w: usize, h: usize, cn: usize) {
    let row_bytes = w * cn;
    for y in 0..h {
        let y0 = y.saturating_sub(1) * row_bytes;
        let y1 = y * row_bytes;
        let y2 = (y + 1).min(h - 1) * row_bytes;
        let dst_row = &mut dst[y * row_bytes..(y + 1) * row_bytes];
        // SAFETY: 行偏移与 dst_row 长度由循环保证在界内
        unsafe {
            median_row_u8_3x3(src, dst_row, w, cn, y0, y1, y2);
        }
    }
}

/// 3x3 排序网络（AVX512 向量版）。
#[inline]
#[allow(clippy::too_many_arguments)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn median3_vec(
    p0: __m512i,
    p1: __m512i,
    p2: __m512i,
    p3: __m512i,
    p4: __m512i,
    p5: __m512i,
    p6: __m512i,
    p7: __m512i,
    p8: __m512i,
) -> __m512i {
    // SAFETY: 纯向量 min/max 排序，无内存访问
    unsafe {
        let (p1, p2) = sort2(p1, p2);
        let (p4, p5) = sort2(p4, p5);
        let (p7, p8) = sort2(p7, p8);
        let (p0, p1) = sort2(p0, p1);
        let (p3, p4) = sort2(p3, p4);
        let (p6, p7) = sort2(p6, p7);
        let (p1, p2) = sort2(p1, p2);
        let (p4, p5) = sort2(p4, p5);
        let (p7, p8) = sort2(p7, p8);
        let (_, p3) = sort2(p0, p3);
        let (p5, _) = sort2(p5, p8);
        let (p4, p7) = sort2(p4, p7);
        let (_, p6) = sort2(p3, p6);
        let (_, p4) = sort2(p1, p4);
        let (p2, _) = sort2(p2, p5);
        let (p4, _) = sort2(p4, p7);
        let (p4, p2) = sort2(p4, p2);
        let (_, p4) = sort2(p6, p4);
        let (p4, _) = sort2(p4, p2);
        p4
    }
}

/// 向量排序：返回 (min, max)。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn sort2(a: __m512i, b: __m512i) -> (__m512i, __m512i) {
    (_mm512_min_epu8(a, b), _mm512_max_epu8(a, b))
}

/// 3x3 中值滤波单行（AVX512，u8，Clamp 边界）。
///
/// 处理第 `y` 行，`y0`/`y1`/`y2` 为三行源偏移（已 Clamp），`dst_row` 为输出行。
///
/// # Safety
/// - `src` 长度 ≥ `y2 + w*cn`，`dst_row` 长度 ≥ `w*cn`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn median_row_u8_3x3(
    src: &[u8],
    dst_row: &mut [u8],
    w: usize,
    cn: usize,
    y0: usize,
    y1: usize,
    y2: usize,
) {
    let row_bytes = w * cn;
    // 首尾 cn 字节标量（Clamp 边界）
    for j in 0..cn {
        let j0 = if j >= cn { j - cn } else { j };
        let j2 = if j + cn < row_bytes { j + cn } else { j };
        let p0 = src[y0 + j0];
        let p1 = src[y0 + j];
        let p2 = src[y0 + j2];
        let p3 = src[y1 + j0];
        let p4 = src[y1 + j];
        let p5 = src[y1 + j2];
        let p6 = src[y2 + j0];
        let p7 = src[y2 + j];
        let p8 = src[y2 + j2];
        dst_row[j] = crate::simd_kernels::median::median3(p0, p1, p2, p3, p4, p5, p6, p7, p8);
    }
    // 中间字节 SIMD：每批 64 字节。
    // 尾部回退对齐 OpenCV（median_blur.simd.hpp:697-705）：批越界时把 j 回退到
    // limit-64 重算一批（同输入同输出，幂等），使向量路径恰好覆盖到 row_bytes-cn，
    // 标量尾仅剩 cn 字节。
    let limit = row_bytes - cn;
    let mut j = cn;
    while j < limit {
        if limit - j < 64 {
            if j == cn {
                break; // 行太窄，一批都放不下，剩余全走标量
            }
            j = limit - 64; // 回退；此时 limit > cn+64 ≥ 65，不会下溢
        }
        // SAFETY: j+64 <= limit = row_bytes-cn 保证 j-cn >= 0 且 j+64+cn <= row_bytes，读/写均在界内
        unsafe {
            let p0 = _mm512_loadu_si512(src.as_ptr().add(y0 + j - cn) as *const _);
            let p1 = _mm512_loadu_si512(src.as_ptr().add(y0 + j) as *const _);
            let p2 = _mm512_loadu_si512(src.as_ptr().add(y0 + j + cn) as *const _);
            let p3 = _mm512_loadu_si512(src.as_ptr().add(y1 + j - cn) as *const _);
            let p4 = _mm512_loadu_si512(src.as_ptr().add(y1 + j) as *const _);
            let p5 = _mm512_loadu_si512(src.as_ptr().add(y1 + j + cn) as *const _);
            let p6 = _mm512_loadu_si512(src.as_ptr().add(y2 + j - cn) as *const _);
            let p7 = _mm512_loadu_si512(src.as_ptr().add(y2 + j) as *const _);
            let p8 = _mm512_loadu_si512(src.as_ptr().add(y2 + j + cn) as *const _);
            let med = median3_vec(p0, p1, p2, p3, p4, p5, p6, p7, p8);
            _mm512_storeu_si512(dst_row.as_mut_ptr().add(j) as *mut _, med);
        }
        j += 64;
    }
    // 尾部标量
    for j in j..row_bytes {
        let j0 = if j >= cn { j - cn } else { j };
        let j2 = if j + cn < row_bytes { j + cn } else { j };
        let p0 = src[y0 + j0];
        let p1 = src[y0 + j];
        let p2 = src[y0 + j2];
        let p3 = src[y1 + j0];
        let p4 = src[y1 + j];
        let p5 = src[y1 + j2];
        let p6 = src[y2 + j0];
        let p7 = src[y2 + j];
        let p8 = src[y2 + j2];
        dst_row[j] = crate::simd_kernels::median::median3(p0, p1, p2, p3, p4, p5, p6, p7, p8);
    }
}

// ==================== bilateral_filter（8u） ====================

/// 8u 双边滤波单行（AVX512，cn=1：每批 64 像素 = 2×32 f32）。
///
/// 对齐 OpenCV `BilateralFilter_8u_Invoker` cn==1 SIMD 数据流
/// （AVX512 下 nlanes=16，nlanes_4=64）。
///
/// # Safety
/// - `padded` 长度 ≥ stride·(h+2r)，`dst_row` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bilateral_row_u8_cn1(
    padded: &[u8],
    dst_row: &mut [u8],
    w: usize,
    y: usize,
    params: &crate::simd_kernels::bilateral::BilateralParams,
) {
    let r = params.radius;
    let stride = w + 2 * r;
    let base = (y + r) * stride + r;
    let mut j = 0;
    // SAFETY: j+32 <= w 保证所有 load/gather/store 在界内（每批 32 像素 = 2×16 f32）
    unsafe {
        while j + 32 <= w {
            let sptr = padded.as_ptr().add(base + j);
            let v_sptr8 = _mm256_loadu_si256(sptr as *const _);
            let mut wsum0 = _mm512_setzero_ps();
            let mut wsum1 = _mm512_setzero_ps();
            let mut sum0 = _mm512_setzero_ps();
            let mut sum1 = _mm512_setzero_ps();
            for k in 0..params.maxk {
                let kweight = _mm512_set1_ps(params.space_weight[k]);
                let ksptr = sptr.add(params.space_ofs[k]);
                let v_val8 = _mm256_loadu_si256(ksptr as *const _);
                let v_diff8 = absdiff_epu8_256(v_val8, v_sptr8);
                // 32 u8 → 32 u16 → 2×16 u32
                let d16 = _mm512_cvtepu8_epi16(v_diff8);
                let diff0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(d16));
                let diff1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(d16, 1));
                let v16 = _mm512_cvtepu8_epi16(v_val8);
                let val0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(v16));
                let val1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(v16, 1));
                let cw = params.color_weight.as_ptr();
                let w0 = _mm512_mul_ps(kweight, _mm512_i32gather_ps(diff0, cw, 4));
                let w1 = _mm512_mul_ps(kweight, _mm512_i32gather_ps(diff1, cw, 4));
                wsum0 = _mm512_add_ps(wsum0, w0);
                sum0 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(val0), w0, sum0);
                wsum1 = _mm512_add_ps(wsum1, w1);
                sum1 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(val1), w1, sum1);
            }
            let o0 = _mm512_cvtps_epu32(_mm512_div_ps(sum0, wsum0));
            let o1 = _mm512_cvtps_epu32(_mm512_div_ps(sum1, wsum1));
            // 2×16 u32 → 32 u16（一个 __m512i）→ 32 u8（__m256i）
            let o16 = pack_u32_u16_512(o0, o1);
            let out = _mm512_cvtepi16_epi8(o16);
            _mm256_storeu_si256(dst_row.as_mut_ptr().add(j) as *mut _, out);
            j += 32;
        }
    }
    crate::simd_kernels::bilateral::bilateral_row_u8_scalar_tail(
        padded,
        dst_row,
        w,
        1,
        y,
        params,
        j,
    );
}

/// 8u 双边滤波单行（AVX512，cn=3：每批 32 像素 = 2×16 f32）。
///
/// # Safety
/// - `padded` 长度 ≥ stride·(h+2r)，`dst_row` 长度 ≥ w·3
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bilateral_row_u8_cn3(
    padded: &[u8],
    dst_row: &mut [u8],
    w: usize,
    y: usize,
    params: &crate::simd_kernels::bilateral::BilateralParams,
) {
    let r = params.radius;
    let stride = (w + 2 * r) * 3;
    let base = (y + r) * stride + r * 3;
    let mut j = 0;
    // SAFETY: j+32 <= w 保证所有 load/gather/store 在界内
    unsafe {
        while j + 32 <= w {
            let sptr = padded.as_ptr().add(base + j * 3);
            let (rb, rg, rr) = avx2::deinterleave3(
                _mm256_loadu_si256(sptr as *const _),
                _mm256_loadu_si256(sptr.add(32) as *const _),
                _mm256_loadu_si256(sptr.add(64) as *const _),
            );
            let mut wsum0 = _mm512_setzero_ps();
            let mut wsum1 = _mm512_setzero_ps();
            let mut sum_b0 = _mm512_setzero_ps();
            let mut sum_b1 = _mm512_setzero_ps();
            let mut sum_g0 = _mm512_setzero_ps();
            let mut sum_g1 = _mm512_setzero_ps();
            let mut sum_r0 = _mm512_setzero_ps();
            let mut sum_r1 = _mm512_setzero_ps();
            for k in 0..params.maxk {
                let kweight = _mm512_set1_ps(params.space_weight[k]);
                let ksptr = sptr.add(params.space_ofs[k]);
                let (kb, kg, kr) = avx2::deinterleave3(
                    _mm256_loadu_si256(ksptr as *const _),
                    _mm256_loadu_si256(ksptr.add(32) as *const _),
                    _mm256_loadu_si256(ksptr.add(64) as *const _),
                );
                // 三通道 absdiff 求和（u16 域 ≤ 765）：32 u8 → 32 u16 → 2×16 u32
                let bd = absdiff_epu8_256(kb, rb);
                let gd = absdiff_epu8_256(kg, rg);
                let rd = absdiff_epu8_256(kr, rr);
                let b16 = _mm512_cvtepu8_epi16(bd);
                let g16 = _mm512_cvtepu8_epi16(gd);
                let r16 = _mm512_cvtepu8_epi16(rd);
                let s = _mm512_add_epi16(_mm512_add_epi16(b16, g16), r16);
                let val0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(s));
                let val1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(s, 1));
                let cw = params.color_weight.as_ptr();
                let w0 = _mm512_mul_ps(kweight, _mm512_i32gather_ps(val0, cw, 4));
                let w1 = _mm512_mul_ps(kweight, _mm512_i32gather_ps(val1, cw, 4));
                wsum0 = _mm512_add_ps(wsum0, w0);
                wsum1 = _mm512_add_ps(wsum1, w1);
                let kb16 = _mm512_cvtepu8_epi16(kb);
                let kg16 = _mm512_cvtepu8_epi16(kg);
                let kr16 = _mm512_cvtepu8_epi16(kr);
                let b0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(kb16));
                let b1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(kb16, 1));
                let g0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(kg16));
                let g1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(kg16, 1));
                let r0 = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(kr16));
                let r1 = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(kr16, 1));
                sum_b0 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(b0), w0, sum_b0);
                sum_b1 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(b1), w1, sum_b1);
                sum_g0 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(g0), w0, sum_g0);
                sum_g1 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(g1), w1, sum_g1);
                sum_r0 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(r0), w0, sum_r0);
                sum_r1 = _mm512_fmadd_ps(_mm512_cvtepi32_ps(r1), w1, sum_r1);
            }
            let one = _mm512_set1_ps(1.0);
            let iw0 = _mm512_div_ps(one, wsum0);
            let iw1 = _mm512_div_ps(one, wsum1);
            // 2×16 u32 → 32 u16 → 32 u8（__m256i），复用 AVX2 交织
            let ob = _mm512_cvtepi16_epi8(pack_u32_u16_512(
                _mm512_cvtps_epu32(_mm512_mul_ps(iw0, sum_b0)),
                _mm512_cvtps_epu32(_mm512_mul_ps(iw1, sum_b1)),
            ));
            let og = _mm512_cvtepi16_epi8(pack_u32_u16_512(
                _mm512_cvtps_epu32(_mm512_mul_ps(iw0, sum_g0)),
                _mm512_cvtps_epu32(_mm512_mul_ps(iw1, sum_g1)),
            ));
            let orr = _mm512_cvtepi16_epi8(pack_u32_u16_512(
                _mm512_cvtps_epu32(_mm512_mul_ps(iw0, sum_r0)),
                _mm512_cvtps_epu32(_mm512_mul_ps(iw1, sum_r1)),
            ));
            avx2::interleave3_store(dst_row.as_mut_ptr().add(j * 3), ob, og, orr);
            j += 32;
        }
    }
    crate::simd_kernels::bilateral::bilateral_row_u8_scalar_tail(
        padded,
        dst_row,
        w,
        3,
        y,
        params,
        j,
    );
}

/// u8 无符号绝对差（AVX512，256 位输入）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn absdiff_epu8_256(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    _mm256_sub_epi8(_mm256_max_epu8(a, b), _mm256_min_epu8(a, b))
}

/// u32 → u16 饱和打包（AVX512 `v_pack`：两个 16×u32 → 32×u16）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_u32_u16_512(a: __m512i, b: __m512i) -> __m512i {
    // SAFETY: 纯向量运算，AVX512 由调用方保证；值域 ≤ 255 无需饱和
    let m = _mm512_set1_epi32(65535);
    let a16 = _mm512_cvtepi32_epi16(_mm512_min_epu32(a, m));
    let b16 = _mm512_cvtepi32_epi16(_mm512_min_epu32(b, m));
    _mm512_inserti64x4(_mm512_castsi256_si512(a16), b16, 1)
}

// ==================== 形态学（morphology） ====================

/// 形态学行阶段：u8 水平滑动窗 min/max（AVX512，每批 64 元素）。
///
/// `src` 为带左右边框的行（长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`。
/// `erode=true` 为 min，`erode=false` 为 max。cn=1 走 SIMD，cn>1 走通道感知标量。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn morph_row_u8(src: &[u8], dst: &mut [u8], w: usize, kw: usize, cn: usize, erode: bool) {
    if cn == 1 {
        // SAFETY: 调用方保证切片长度；AVX512 由 target_feature 保证
        unsafe { morph_row_cn1(src, dst, w, kw, erode) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_cn(src, dst, w, kw, cn, erode)
    }
}

/// cn=1 行 min/max：每批 64 元素，加载 kw 个偏移向量取 min/max。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn morph_row_cn1(src: &[u8], dst: &mut [u8], w: usize, kw: usize, erode: bool) {
    let mut i = 0;
    // SAFETY: i+64 <= w 保证读 [i, i+64+kw-1)（src.len() >= w+kw-1）写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= w {
            let mut acc = _mm512_loadu_si512(src.as_ptr().add(i) as *const __m512i);
            for k in 1..kw {
                let v = _mm512_loadu_si512(src.as_ptr().add(i + k) as *const __m512i);
                acc = if erode {
                    _mm512_min_epu8(acc, v)
                } else {
                    _mm512_max_epu8(acc, v)
                };
            }
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, acc);
            i += 64;
        }
    }
    for j in i..w {
        let mut acc = src[j];
        for k in 1..kw {
            let v = src[j + k];
            acc = if erode { acc.min(v) } else { acc.max(v) };
        }
        dst[j] = acc;
    }
}

/// 形态学列阶段：u8 垂直 min/max（AVX512，每批 64 元素）。
///
/// `rows` 为 kh 行 u8（每行长度 ≥ w，stride 相同），`dst` 长度 ≥ w。
///
/// # Safety
/// - `rows` 每行长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn morph_col_u8(rows: &[&[u8]], dst: &mut [u8], w: usize, erode: bool) {
    let mut i = 0;
    // SAFETY: i+64 <= w 保证读 [i, i+64) 写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= w {
            let mut acc = _mm512_loadu_si512(rows[0].as_ptr().add(i) as *const __m512i);
            for row in rows.iter().skip(1) {
                let v = _mm512_loadu_si512(row.as_ptr().add(i) as *const __m512i);
                acc = if erode {
                    _mm512_min_epu8(acc, v)
                } else {
                    _mm512_max_epu8(acc, v)
                };
            }
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, acc);
            i += 64;
        }
    }
    for j in i..w {
        let mut acc = rows[0][j];
        for row in rows.iter().skip(1) {
            let v = row[j];
            acc = if erode { acc.min(v) } else { acc.max(v) };
        }
        dst[j] = acc;
    }
}

/// 形态学行阶段（无边框源）：u8 水平滑动窗 min/max（AVX512，每批 64 元素）。
///
/// `src` 为原始行（长度 ≥ `w*cn`，无左右边框），`dst` 长度 ≥ `w*cn`。
/// 首尾 `kw-1` 像素的越界采样由 `border` 策略给出。cn=1 走 SIMD，cn>1 走标量。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn morph_row_u8_direct(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    kw: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    if cn == 1 {
        // SAFETY: 调用方保证切片长度；AVX512 由 target_feature 保证
        unsafe { morph_row_direct_cn1(src, dst, w, kw, erode, border) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_direct_cn(src, dst, w, kw, cn, erode, border)
    }
}

/// cn=1 直接边框行 min/max：主体 SIMD，首尾特判。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn morph_row_direct_cn1(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    kw: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let ax = kw / 2;
    let lo = ax;
    let hi = w.saturating_sub(kw - 1 - ax);
    let mut i = lo;
    // SAFETY: i+64 <= hi 保证读 [i-ax, i-ax+64+kw-1)（src.len() >= w）写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= hi {
            let base = i - ax;
            let mut acc = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            for k in 1..kw {
                let v = _mm512_loadu_si512(src.as_ptr().add(base + k) as *const __m512i);
                acc = if erode {
                    _mm512_min_epu8(acc, v)
                } else {
                    _mm512_max_epu8(acc, v)
                };
            }
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, acc);
            i += 64;
        }
    }
    for x in i..hi {
        let mut acc = src[x - ax];
        for k in 1..kw {
            let v = src[x - ax + k];
            acc = if erode { acc.min(v) } else { acc.max(v) };
        }
        dst[x] = acc;
    }
    // 首尾特判
    crate::simd_kernels::morphology::row_morph_edges(src, dst, w, kw, 1, erode, border, ax);
}

/// 形态学梯度：`dst = d - e`（u8 饱和减，AVX512，每批 64 元素）。
///
/// # Safety
/// - `d`/`e`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn gradient_u8(d: &[u8], e: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: i+64 <= n 保证读 [i, i+64) 写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= n {
            let dv = _mm512_loadu_si512(d.as_ptr().add(i) as *const __m512i);
            let ev = _mm512_loadu_si512(e.as_ptr().add(i) as *const __m512i);
            let r = _mm512_subs_epu8(dv, ev);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    for j in i..n {
        dst[j] = d[j].saturating_sub(e[j]);
    }
}

/// 形态学行阶段（无边框源）kw=3 特化：u8 水平滑动窗 min/max（AVX512，每批 64 元素）。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn morph_row_u8_direct_k3(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    if cn == 1 {
        // SAFETY: 调用方保证切片长度；AVX512 由 target_feature 保证
        unsafe { morph_row_direct_k3_cn1(src, dst, w, erode, border) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_direct_k3_cn(src, dst, w, cn, erode, border)
    }
}

/// cn=1 kw=3 直接边框行 min/max：主体 SIMD（3 个偏移向量），首尾特判。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn morph_row_direct_k3_cn1(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let lo = 1;
    let hi = w.saturating_sub(1);
    let mut i = lo;
    // SAFETY: i+64 <= hi 保证读 [i-1, i-1+64+2)（src.len() >= w）写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= hi {
            let base = i - 1;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let acc = if erode {
                _mm512_min_epu8(_mm512_min_epu8(a, b), c)
            } else {
                _mm512_max_epu8(_mm512_max_epu8(a, b), c)
            };
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, acc);
            i += 64;
        }
        // 尾部：未对齐重叠 SIMD（min/max 幂等，重算无害），覆盖 [i, hi)。
        // store 在 [base+1, base+65)，需 base+65 <= hi；读 [base, base+66)，需 base+66 <= w。
        // 取 base = w-66 = hi-65：store 覆盖 [hi-64, hi)，读上界恰为 w。
        if i < hi && w >= 67 {
            let base = w - 66;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let acc = if erode {
                _mm512_min_epu8(_mm512_min_epu8(a, b), c)
            } else {
                _mm512_max_epu8(_mm512_max_epu8(a, b), c)
            };
            _mm512_storeu_si512(dst.as_mut_ptr().add(base + 1) as *mut __m512i, acc);
        } else {
            for x in i..hi {
                let mut acc = src[x - 1];
                let v1 = src[x];
                let v2 = src[x + 1];
                acc = if erode { acc.min(v1).min(v2) } else { acc.max(v1).max(v2) };
                dst[x] = acc;
            }
        }
    }
    crate::simd_kernels::morphology::row_morph_edges(src, dst, w, 3, 1, erode, border, 1);
}


/// kw=3 行阶段融合：一次读 3 偏移向量，同时产出 min（erode）与 max（dilate）。
///
/// 供 morph_gradient 融合路径使用（src 只读一遍）。
///
/// # Safety
/// - `src` 长度 ≥ w，`dst_min`/`dst_max` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn morph_row_minmax_k3(
    src: &[u8],
    dst_min: &mut [u8],
    dst_max: &mut [u8],
    w: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    // SAFETY: 调用方保证切片长度；AVX512 由 target_feature 保证
    unsafe { morph_row_minmax_k3_cn1(src, dst_min, dst_max, w, border) }
}

/// cn=1 kw=3 融合行 min/max：主体 SIMD，尾部未对齐重叠，首尾特判。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn morph_row_minmax_k3_cn1(
    src: &[u8],
    dst_min: &mut [u8],
    dst_max: &mut [u8],
    w: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let lo = 1;
    let hi = w.saturating_sub(1);
    let mut i = lo;
    // SAFETY: i+64 <= hi 保证读 [i-1, i-1+64+2) 写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= hi {
            let base = i - 1;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let mn = _mm512_min_epu8(_mm512_min_epu8(a, b), c);
            let mx = _mm512_max_epu8(_mm512_max_epu8(a, b), c);
            _mm512_storeu_si512(dst_min.as_mut_ptr().add(i) as *mut __m512i, mn);
            _mm512_storeu_si512(dst_max.as_mut_ptr().add(i) as *mut __m512i, mx);
            i += 64;
        }
        // 尾部：未对齐重叠 SIMD。store 在 [base+1, base+65)，读 [base, base+66)。
        // 取 base = w-66：store 覆盖 [w-65, w-1)，读上界恰为 w。
        if i < hi && w >= 67 {
            let base = w - 66;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let mn = _mm512_min_epu8(_mm512_min_epu8(a, b), c);
            let mx = _mm512_max_epu8(_mm512_max_epu8(a, b), c);
            _mm512_storeu_si512(dst_min.as_mut_ptr().add(base + 1) as *mut __m512i, mn);
            _mm512_storeu_si512(dst_max.as_mut_ptr().add(base + 1) as *mut __m512i, mx);
        } else {
            for x in i..hi {
                let v0 = src[x - 1];
                let v1 = src[x];
                let v2 = src[x + 1];
                dst_min[x] = v0.min(v1).min(v2);
                dst_max[x] = v0.max(v1).max(v2);
            }
        }
    }
    crate::simd_kernels::morphology::row_morph_edges(src, dst_min, w, 3, 1, true, border, 1);
    crate::simd_kernels::morphology::row_morph_edges(src, dst_max, w, 3, 1, false, border, 1);
}

// ==================== 梯度（gradient） ====================

/// 梯度行阶段（u8 → i32，对称核 ksize=3，cn=1，AVX512，每批 64 元素）。
///
/// `D[i] = k0*S[i] + k1*(S[i-1] + S[i+1])`。
///
/// # Safety
/// - `src` 长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn deriv_row_symm_k3_cn1(
    src: &[u8],
    dst: &mut [i32],
    w: usize,
    k0: i32,
    k1: i32,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let sample = |x: i64| -> i32 {
        match border.interpolate(x as isize, w) {
            Some(sx) => src[sx] as i32,
            None => match border {
                crate::core::border::BorderPolicy::Constant(v) => v as i32,
                _ => unreachable!("non-constant border always interpolates"),
            },
        }
    };
    // [1,2,1] 专用加法路径（对齐 OpenCV SymmRowSmallVec_8u32s：2c + l + r，无乘法）
    if k0 == 2 && k1 == 1 {
        let mut i = 1usize;
        let hi = w.saturating_sub(1);
        // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
        unsafe {
            while i + 32 <= hi {
                let base = i - 1;
                let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
                let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
                let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
                let (s0, s1) = deriv_symm_121_core_512(a, b, c);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i + 16) as *mut __m512i, s1);
                i += 32;
            }
        }
        // SAFETY: 纯标量循环（target_feature 下编译），索引在 [1, w-1) 窗口全在界
        for x in i..hi {
            dst[x] = 2 * src[x] as i32 + src[x - 1] as i32 + src[x + 1] as i32;
        }
        if w > 0 {
            let r0 = if w > 1 { src[1] as i32 } else { sample(1) };
            dst[0] = 2 * src[0] as i32 + sample(-1) + r0;
            if w > 1 {
                dst[w - 1] = 2 * src[w - 1] as i32 + src[w - 2] as i32 + sample(w as i64);
            }
        }
        return;
    }
    let vk0 = _mm512_set1_epi32(k0);
    let vk1 = _mm512_set1_epi32(k1);
    let mut i = 1usize;
    let hi = w.saturating_sub(1);
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let (s0, s1) = deriv_symm_core_512(a, b, c, vk0, vk1);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i + 16) as *mut __m512i, s1);
            i += 32;
        }
    }
    // 尾部：未对齐重叠 SIMD
    unsafe {
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let b = _mm512_loadu_si512(src.as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let (s0, s1) = deriv_symm_core_512(a, b, c, vk0, vk1);
            _mm512_storeu_si512(dst.as_mut_ptr().add(base + 1) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(base + 17) as *mut __m512i, s1);
        } else {
            for x in i..hi {
                let l = src[x - 1] as i32;
                let r = src[x + 1] as i32;
                dst[x] = k0 * src[x] as i32 + k1 * (l + r);
            }
        }
    }
    if w > 0 {
        let r0 = if w > 1 { src[1] as i32 } else { sample(1) };
        dst[0] = k0 * src[0] as i32 + k1 * (sample(-1) + r0);
        if w > 1 {
            dst[w - 1] = k0 * src[w - 1] as i32 + k1 * (src[w - 2] as i32 + sample(w as i64));
        }
    }
}

/// 对称 [1,2,1] 核核心（AVX512）：`2b + a + c`（32 u8 → 32 i32，纯加法）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn deriv_symm_121_core_512(a: __m512i, b: __m512i, c: __m512i) -> (__m512i, __m512i) {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    // 128 位半区 u8 → cvtepu8_epi32 → 16×i32，i32 域加法
    let acc = |a: __m128i, b: __m128i, c: __m128i| -> __m512i {
        let a32 = _mm512_cvtepu8_epi32(a);
        let b32 = _mm512_cvtepu8_epi32(b);
        let c32 = _mm512_cvtepu8_epi32(c);
        _mm512_add_epi32(_mm512_add_epi32(b32, b32), _mm512_add_epi32(a32, c32))
    };
    let lo = acc(
        _mm512_castsi512_si128(a),
        _mm512_castsi512_si128(b),
        _mm512_castsi512_si128(c),
    );
    let hi = acc(
        _mm512_extracti32x4_epi32(a, 1),
        _mm512_extracti32x4_epi32(b, 1),
        _mm512_extracti32x4_epi32(c, 1),
    );
    (lo, hi)
}

/// 对称核核心（AVX512）：`k0*b + k1*(a + c)`（32 u8 → 32 i32，返回低/高各 16）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn deriv_symm_core_512(
    a: __m512i,
    b: __m512i,
    c: __m512i,
    vk0: __m512i,
    vk1: __m512i,
) -> (__m512i, __m512i) {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    // 128 位半区 u8 直接 cvtepu8_epi32 → 16×i32，无 i16 中间态
    let acc = |a: __m128i, b: __m128i, c: __m128i| -> __m512i {
        let a32 = _mm512_cvtepu8_epi32(a);
        let b32 = _mm512_cvtepu8_epi32(b);
        let c32 = _mm512_cvtepu8_epi32(c);
        _mm512_add_epi32(
            _mm512_mullo_epi32(b32, vk0),
            _mm512_mullo_epi32(_mm512_add_epi32(a32, c32), vk1),
        )
    };
    let lo = acc(
        _mm512_castsi512_si128(a),
        _mm512_castsi512_si128(b),
        _mm512_castsi512_si128(c),
    );
    let hi = acc(
        _mm512_extracti32x4_epi32(a, 1),
        _mm512_extracti32x4_epi32(b, 1),
        _mm512_extracti32x4_epi32(c, 1),
    );
    (lo, hi)
}

/// 梯度行阶段（u8 → i32，反对称核 ksize=3，cn=1，AVX512，每批 32 元素）。
///
/// # Safety
/// - `src` 长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn deriv_row_asymm_k3_cn1(
    src: &[u8],
    dst: &mut [i32],
    w: usize,
    k1: i32,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let sample = |x: i64| -> i32 {
        match border.interpolate(x as isize, w) {
            Some(sx) => src[sx] as i32,
            None => match border {
                crate::core::border::BorderPolicy::Constant(v) => v as i32,
                _ => unreachable!("non-constant border always interpolates"),
            },
        }
    };
    let vk1 = _mm512_set1_epi32(k1);
    let mut i = 1usize;
    let hi = w.saturating_sub(1);
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let (s0, s1) = deriv_asymm_core_512(a, c, vk1);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(i + 16) as *mut __m512i, s1);
            i += 32;
        }
    }
    unsafe {
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let c = _mm512_loadu_si512(src.as_ptr().add(base + 2) as *const __m512i);
            let (s0, s1) = deriv_asymm_core_512(a, c, vk1);
            _mm512_storeu_si512(dst.as_mut_ptr().add(base + 1) as *mut __m512i, s0);
            _mm512_storeu_si512(dst.as_mut_ptr().add(base + 17) as *mut __m512i, s1);
        } else {
            for x in i..hi {
                dst[x] = k1 * (src[x + 1] as i32 - src[x - 1] as i32);
            }
        }
    }
    if w > 0 {
        let r0 = if w > 1 { src[1] as i32 } else { sample(1) };
        dst[0] = k1 * (r0 - sample(-1));
        if w > 1 {
            dst[w - 1] = k1 * (sample(w as i64) - src[w - 2] as i32);
        }
    }
}

/// 反对称核核心（AVX512）：`k1*(c - a)`（32 u8 → 32 i32，返回低/高各 16）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn deriv_asymm_core_512(a: __m512i, c: __m512i, vk1: __m512i) -> (__m512i, __m512i) {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    // 128 位半区 u8 直接 cvtepu8_epi32 → 16×i32，i32 域相减再乘
    let acc = |a: __m128i, c: __m128i| -> __m512i {
        let a32 = _mm512_cvtepu8_epi32(a);
        let c32 = _mm512_cvtepu8_epi32(c);
        _mm512_mullo_epi32(_mm512_sub_epi32(c32, a32), vk1)
    };
    let lo = acc(_mm512_castsi512_si128(a), _mm512_castsi512_si128(c));
    let hi = acc(_mm512_extracti32x4_epi32(a, 1), _mm512_extracti32x4_epi32(c, 1));
    (lo, hi)
}

/// 梯度列阶段（i32 → i16，对称核 ksize=3，AVX512，每批 32 元素）。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn deriv_col_symm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f0: i32, f1: i32, delta: i32) {
    // [1,2,1] 专用加法路径（对齐 OpenCV SymmColumnSmallVec_32s16s：S0+S2+2*S1，无乘法）
    if f0 == 2 && f1 == 1 && delta == 0 {
        let mut i = 0usize;
        // SAFETY: i+32 <= w 保证读 [i, i+32)×3 行写 [i, i+32) 不越界
        unsafe {
            while i + 32 <= w {
                let acc = |off: usize| -> __m512i {
                    let s0 = _mm512_loadu_si512(rows[0].as_ptr().add(i + off) as *const __m512i);
                    let s1 = _mm512_loadu_si512(rows[1].as_ptr().add(i + off) as *const __m512i);
                    let s2 = _mm512_loadu_si512(rows[2].as_ptr().add(i + off) as *const __m512i);
                    _mm512_add_epi32(_mm512_add_epi32(s0, s2), _mm512_add_epi32(s1, s1))
                };
                // i32 → i16 饱和打包（cvtsepi32_epi16 顺序拼接，无 lane 交错问题）
                let p0 = _mm512_cvtsepi32_epi16(acc(0));
                let p1 = _mm512_cvtsepi32_epi16(acc(16));
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, p0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, p1);
                i += 32;
            }
        }
        for x in i..w {
            let s = rows[0][x] + rows[2][x] + 2 * rows[1][x];
            dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        return;
    }
    let vf0 = _mm512_set1_epi32(f0);
    let vf1 = _mm512_set1_epi32(f1);
    let vd = _mm512_set1_epi32(delta);
    let mut i = 0usize;
    // SAFETY: i+32 <= w 保证读 [i, i+32)×3 行写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let acc = |off: usize| -> __m512i {
                let s0 = _mm512_loadu_si512(rows[0].as_ptr().add(i + off) as *const __m512i);
                let s1 = _mm512_loadu_si512(rows[1].as_ptr().add(i + off) as *const __m512i);
                let s2 = _mm512_loadu_si512(rows[2].as_ptr().add(i + off) as *const __m512i);
                _mm512_add_epi32(
                    _mm512_add_epi32(_mm512_mullo_epi32(s1, vf0), vd),
                    _mm512_mullo_epi32(_mm512_add_epi32(s0, s2), vf1),
                )
            };
            // i32 → i16 饱和打包（cvtsepi32_epi16 顺序拼接，无 lane 交错问题）
            let p0 = _mm512_cvtsepi32_epi16(acc(0));
            let p1 = _mm512_cvtsepi32_epi16(acc(16));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, p0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, p1);
            i += 32;
        }
    }
    for x in i..w {
        let s = f0 * rows[1][x] + f1 * (rows[0][x] + rows[2][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// 梯度列阶段（i32 → i16，反对称核 ksize=3，AVX512，每批 32 元素）。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn deriv_col_asymm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f1: i32, delta: i32) {
    let vf1 = _mm512_set1_epi32(f1);
    let vd = _mm512_set1_epi32(delta);
    let mut i = 0usize;
    // SAFETY: i+32 <= w 保证读 [i, i+32)×3 行写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let acc = |off: usize| -> __m512i {
                let s0 = _mm512_loadu_si512(rows[0].as_ptr().add(i + off) as *const __m512i);
                let s2 = _mm512_loadu_si512(rows[2].as_ptr().add(i + off) as *const __m512i);
                _mm512_add_epi32(_mm512_mullo_epi32(_mm512_sub_epi32(s2, s0), vf1), vd)
            };
            let p0 = _mm512_cvtsepi32_epi16(acc(0));
            let p1 = _mm512_cvtsepi32_epi16(acc(16));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, p0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, p1);
            i += 32;
        }
    }
    for x in i..w {
        let s = f1 * (rows[2][x] - rows[0][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// Laplacian 3x3（u8 → i16，AVX512，每批 32 元素）。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn laplacian3_rows_u8(
    rows: [&[u8]; 3],
    dst: &mut [i16],
    w: usize,
    ksize1: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    // 主体 SIMD 处理 x ∈ [1, w-1)（窗口 ±1 全在界），首尾像素标量特判
    let mut i = 1usize;
    let hi = w.saturating_sub(1);
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let u = _mm512_loadu_si512(rows[0].as_ptr().add(base + 1) as *const __m512i);
            let d = _mm512_loadu_si512(rows[2].as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(rows[1].as_ptr().add(base + 1) as *const __m512i);
            if ksize1 {
                // 十字核：u + d + l + r - 4c
                let l = _mm512_loadu_si512(rows[1].as_ptr().add(base) as *const __m512i);
                let r = _mm512_loadu_si512(rows[1].as_ptr().add(base + 2) as *const __m512i);
                let s = laplacian_cross_core_512(u, d, l, r, c);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s);
            } else {
                // 对角核：2*(r0[x±1] + r2[x±1]) - 8c
                let u_l = _mm512_loadu_si512(rows[0].as_ptr().add(base) as *const __m512i);
                let u_r = _mm512_loadu_si512(rows[0].as_ptr().add(base + 2) as *const __m512i);
                let d_l = _mm512_loadu_si512(rows[2].as_ptr().add(base) as *const __m512i);
                let d_r = _mm512_loadu_si512(rows[2].as_ptr().add(base + 2) as *const __m512i);
                let s = laplacian_diag_core_512(u_l, u_r, d_l, d_r, c);
                _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, s);
            }
            i += 32;
        }
        // 尾部：未对齐重叠 SIMD（重算无害）
        if i < hi && w >= 35 {
            let base = w - 34;
            let u = _mm512_loadu_si512(rows[0].as_ptr().add(base + 1) as *const __m512i);
            let d = _mm512_loadu_si512(rows[2].as_ptr().add(base + 1) as *const __m512i);
            let c = _mm512_loadu_si512(rows[1].as_ptr().add(base + 1) as *const __m512i);
            if ksize1 {
                let l = _mm512_loadu_si512(rows[1].as_ptr().add(base) as *const __m512i);
                let r = _mm512_loadu_si512(rows[1].as_ptr().add(base + 2) as *const __m512i);
                let s = laplacian_cross_core_512(u, d, l, r, c);
                _mm512_storeu_si512(dst.as_mut_ptr().add(base + 1) as *mut __m512i, s);
            } else {
                let u_l = _mm512_loadu_si512(rows[0].as_ptr().add(base) as *const __m512i);
                let u_r = _mm512_loadu_si512(rows[0].as_ptr().add(base + 2) as *const __m512i);
                let d_l = _mm512_loadu_si512(rows[2].as_ptr().add(base) as *const __m512i);
                let d_r = _mm512_loadu_si512(rows[2].as_ptr().add(base + 2) as *const __m512i);
                let s = laplacian_diag_core_512(u_l, u_r, d_l, d_r, c);
                _mm512_storeu_si512(dst.as_mut_ptr().add(base + 1) as *mut __m512i, s);
            }
        } else {
            // SAFETY: 纯标量循环（target_feature 下编译），索引在 [1, w-1) 窗口全在界
            for x in i..hi {
                let s = if ksize1 {
                    rows[0][x] as i32 + rows[2][x] as i32 + rows[1][x - 1] as i32
                        + rows[1][x + 1] as i32
                        - 4 * rows[1][x] as i32
                } else {
                    2 * (rows[0][x - 1] as i32 + rows[0][x + 1] as i32 + rows[2][x - 1] as i32
                        + rows[2][x + 1] as i32)
                        - 8 * rows[1][x] as i32
                };
                dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
    }
    // 首尾像素标量特判（行内按 border 采样）
    if w > 0 {
        let sample = |ri: usize, x: i64| -> i32 {
            match border.interpolate(x as isize, w) {
                Some(sx) => rows[ri][sx] as i32,
                None => match border {
                    crate::core::border::BorderPolicy::Constant(v) => v as i32,
                    _ => unreachable!("non-constant border always interpolates"),
                },
            }
        };
        // x=0：左邻 = sample(-1)，右邻 = w>1 ? rows[ri][1] : sample(w)
        let s = if ksize1 {
            let r = if w > 1 { rows[1][1] as i32 } else { sample(1, w as i64) };
            rows[0][0] as i32 + rows[2][0] as i32 + sample(1, -1) + r - 4 * rows[1][0] as i32
        } else {
            let r0 = if w > 1 { rows[0][1] as i32 } else { sample(0, w as i64) };
            let r2 = if w > 1 { rows[2][1] as i32 } else { sample(2, w as i64) };
            2 * (sample(0, -1) + r0 + sample(2, -1) + r2) - 8 * rows[1][0] as i32
        };
        dst[0] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        if w > 1 {
            let x = w - 1;
            // x=w-1：右邻 = sample(w)，左邻 = rows[ri][x-1]
            let s = if ksize1 {
                rows[0][x] as i32 + rows[2][x] as i32 + rows[1][x - 1] as i32
                    + sample(1, w as i64)
                    - 4 * rows[1][x] as i32
            } else {
                2 * (rows[0][x - 1] as i32 + sample(0, w as i64) + rows[2][x - 1] as i32
                    + sample(2, w as i64))
                    - 8 * rows[1][x] as i32
            };
            dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
}

/// Laplacian 十字核核心（AVX512）：`u + d + l + r - 4c`（32 u8 → 32 i16）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn laplacian_cross_core_512(
    u: __m512i,
    d: __m512i,
    l: __m512i,
    r: __m512i,
    c: __m512i,
) -> __m512i {
    // SAFETY: 纯向量运算，AVX512 由调用方保证（unsafe fn 体即 unsafe 上下文）
    // 32 u8 → 32 i16（低 256 位 cvtepu8_epi16）
    let cvt = |v: __m512i| _mm512_cvtepu8_epi16(_mm512_castsi512_si256(v));
    let u16 = cvt(u);
    let d16 = cvt(d);
    let l16 = cvt(l);
    let r16 = cvt(r);
    let c16 = cvt(c);
    let m4 = _mm512_set1_epi16(-4);
    _mm512_add_epi16(
        _mm512_add_epi16(_mm512_add_epi16(u16, d16), _mm512_add_epi16(l16, r16)),
        _mm512_mullo_epi16(c16, m4),
    )
}

/// Laplacian 对角核核心（AVX512）：`2*(u_l + u_r + d_l + d_r) - 8c`（32 u8 → 32 i16）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn laplacian_diag_core_512(
    u_l: __m512i,
    u_r: __m512i,
    d_l: __m512i,
    d_r: __m512i,
    c: __m512i,
) -> __m512i {
    // SAFETY: 纯向量运算，AVX512 由调用方保证（unsafe fn 体即 unsafe 上下文）
    let cvt = |v: __m512i| _mm512_cvtepu8_epi16(_mm512_castsi512_si256(v));
    let ul = cvt(u_l);
    let ur = cvt(u_r);
    let dl = cvt(d_l);
    let dr = cvt(d_r);
    let c16 = cvt(c);
    // 和 ≤ 4*255 = 1020，×2 = 2040；-8c ≥ -2040：i16 域安全
    let m2 = _mm512_set1_epi16(2);
    let m8 = _mm512_set1_epi16(-8);
    _mm512_add_epi16(
        _mm512_mullo_epi16(
            _mm512_add_epi16(_mm512_add_epi16(ul, ur), _mm512_add_epi16(dl, dr)),
            m2,
        ),
        _mm512_mullo_epi16(c16, m8),
    )
}

/// magnitude（f32，AVX512，每批 16 元素）。
///
/// # Safety
/// - `x`/`y`/`dst` 长度 ≥ n
/// - CPU 需支持 AVX512F
#[inline(never)]
#[target_feature(enable = "avx512f")]
pub unsafe fn magnitude32f(x: &[f32], y: &[f32], dst: &mut [f32], n: usize) {
    let mut i = 0usize;
    // 非临时存储（movntps）需 64 字节对齐 dst；避免写分配（RFO），
    // 提升 3 流（2 读 + 1 写）纯流式带宽。
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = (((64 - (dst_ptr & 63)) & 63) / 4).min(n);
    // SAFETY: i+64 <= n 保证读 [i, i+64)×2 写 [i, i+64) 不越界
    unsafe {
        // 头部（< 16 元素，未对齐，标量）
        while i < head {
            *dst.get_unchecked_mut(i) = (x[i] * x[i] + y[i] * y[i]).sqrt();
            i += 1;
        }
        // 中部（64 字节对齐，非临时存储）
        let mut px = x.as_ptr().add(i);
        let mut py = y.as_ptr().add(i);
        let mut pd = dst.as_mut_ptr().add(i);
        while i + 64 <= n {
            let vx0 = _mm512_loadu_ps(px);
            let vy0 = _mm512_loadu_ps(py);
            let vx1 = _mm512_loadu_ps(px.add(16));
            let vy1 = _mm512_loadu_ps(py.add(16));
            let vx2 = _mm512_loadu_ps(px.add(32));
            let vy2 = _mm512_loadu_ps(py.add(32));
            let vx3 = _mm512_loadu_ps(px.add(48));
            let vy3 = _mm512_loadu_ps(py.add(48));
            let m0 = _mm512_sqrt_ps(_mm512_fmadd_ps(vx0, vx0, _mm512_mul_ps(vy0, vy0)));
            let m1 = _mm512_sqrt_ps(_mm512_fmadd_ps(vx1, vx1, _mm512_mul_ps(vy1, vy1)));
            let m2 = _mm512_sqrt_ps(_mm512_fmadd_ps(vx2, vx2, _mm512_mul_ps(vy2, vy2)));
            let m3 = _mm512_sqrt_ps(_mm512_fmadd_ps(vx3, vx3, _mm512_mul_ps(vy3, vy3)));
            _mm512_stream_ps(pd, m0);
            _mm512_stream_ps(pd.add(16), m1);
            _mm512_stream_ps(pd.add(32), m2);
            _mm512_stream_ps(pd.add(48), m3);
            px = px.add(64);
            py = py.add(64);
            pd = pd.add(64);
            i += 64;
        }
        while i + 16 <= n {
            let vx = _mm512_loadu_ps(x.as_ptr().add(i));
            let vy = _mm512_loadu_ps(y.as_ptr().add(i));
            let m = _mm512_sqrt_ps(_mm512_fmadd_ps(vx, vx, _mm512_mul_ps(vy, vy)));
            _mm512_storeu_ps(dst.as_mut_ptr().add(i), m);
            i += 16;
        }
    }
    for j in i..n {
        dst[j] = (x[j] * x[j] + y[j] * y[j]).sqrt();
    }
}

/// phase（f32，AVX512，每批 16 元素）。
///
/// # Safety
/// - `y`/`x`/`dst` 长度 ≥ n
/// - CPU 需支持 AVX512F
#[inline(never)]
#[target_feature(enable = "avx512f")]
pub unsafe fn fast_atan2_32f(y: &[f32], x: &[f32], dst: &mut [f32], n: usize, angle_in_degrees: bool) {
    let scale = if angle_in_degrees {
        1.0f32
    } else {
        std::f32::consts::PI / 180.0
    };
    let vs = _mm512_set1_ps(scale);
    let mut i = 0usize;
    // SAFETY: i+16 <= n 保证读 [i, i+16)×2 写 [i, i+16) 不越界
    unsafe {
        while i + 16 <= n {
            let vy = _mm512_loadu_ps(y.as_ptr().add(i));
            let vx = _mm512_loadu_ps(x.as_ptr().add(i));
            let a = atan_vec_512(vy, vx);
            _mm512_storeu_ps(dst.as_mut_ptr().add(i), _mm512_mul_ps(a, vs));
            i += 16;
        }
    }
    for j in i..n {
        dst[j] = crate::simd_kernels::gradient::atan_f32_scalar(y[j], x[j]) * scale;
    }
}

/// cart_to_polar（f32，AVX512，每批 16 元素）。
///
/// # Safety
/// - 4 个切片长度 ≥ n
/// - CPU 需支持 AVX512F
#[inline(never)]
#[target_feature(enable = "avx512f")]
pub unsafe fn cart_to_polar32f(
    x: &[f32],
    y: &[f32],
    mag: &mut [f32],
    angle: &mut [f32],
    n: usize,
    angle_in_degrees: bool,
) {
    let scale = if angle_in_degrees {
        1.0f32
    } else {
        std::f32::consts::PI / 180.0
    };
    let vs = _mm512_set1_ps(scale);
    let mut i = 0usize;
    // SAFETY: i+32 <= n 保证读 [i, i+32)×2 写 [i, i+32)×2 不越界
    unsafe {
        // 2×16 展开（对齐 OpenCV 2×VECSZ），隐藏 _mm512_div_ps 高延迟
        while i + 32 <= n {
            let x0 = _mm512_loadu_ps(x.as_ptr().add(i));
            let y0 = _mm512_loadu_ps(y.as_ptr().add(i));
            let x1 = _mm512_loadu_ps(x.as_ptr().add(i + 16));
            let y1 = _mm512_loadu_ps(y.as_ptr().add(i + 16));
            let m0 = _mm512_sqrt_ps(_mm512_fmadd_ps(x0, x0, _mm512_mul_ps(y0, y0)));
            let m1 = _mm512_sqrt_ps(_mm512_fmadd_ps(x1, x1, _mm512_mul_ps(y1, y1)));
            let a0 = _mm512_mul_ps(atan_vec_512(y0, x0), vs);
            let a1 = _mm512_mul_ps(atan_vec_512(y1, x1), vs);
            _mm512_storeu_ps(mag.as_mut_ptr().add(i), m0);
            _mm512_storeu_ps(mag.as_mut_ptr().add(i + 16), m1);
            _mm512_storeu_ps(angle.as_mut_ptr().add(i), a0);
            _mm512_storeu_ps(angle.as_mut_ptr().add(i + 16), a1);
            i += 32;
        }
        while i + 16 <= n {
            let vx = _mm512_loadu_ps(x.as_ptr().add(i));
            let vy = _mm512_loadu_ps(y.as_ptr().add(i));
            let m = _mm512_sqrt_ps(_mm512_fmadd_ps(vx, vx, _mm512_mul_ps(vy, vy)));
            let a = _mm512_mul_ps(atan_vec_512(vy, vx), vs);
            _mm512_storeu_ps(mag.as_mut_ptr().add(i), m);
            _mm512_storeu_ps(angle.as_mut_ptr().add(i), a);
            i += 16;
        }
    }
    for j in i..n {
        mag[j] = (x[j] * x[j] + y[j] * y[j]).sqrt();
        angle[j] = crate::simd_kernels::gradient::atan_f32_scalar(y[j], x[j]) * scale;
    }
}

/// atan 向量核心（AVX512，度数 [0, 360)）。
#[inline]
#[target_feature(enable = "avx512f")]
unsafe fn atan_vec_512(y: __m512, x: __m512) -> __m512 {
    // SAFETY: 纯向量运算，AVX512 由调用方保证
    // 系数来自 OpenCV v_atan_f32（f64 精度常量，乘 180/π 转度数）
    #[allow(clippy::excessive_precision)]
    const P1: f32 = 0.9997878412794807 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P3: f32 = -0.3258083974640975 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P5: f32 = 0.1555786518463281 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P7: f32 = -0.04432655554792128 * (180.0 / std::f32::consts::PI);
    let eps = _mm512_set1_ps(f64::EPSILON as f32);
    let z = _mm512_setzero_ps();
    let p7 = _mm512_set1_ps(P7);
    let p5 = _mm512_set1_ps(P5);
    let p3 = _mm512_set1_ps(P3);
    let p1 = _mm512_set1_ps(P1);
    let v90 = _mm512_set1_ps(90.0);
    let v180 = _mm512_set1_ps(180.0);
    let v360 = _mm512_set1_ps(360.0);

    let ax = _mm512_abs_ps(x);
    let ay = _mm512_abs_ps(y);
    let c = _mm512_div_ps(
        _mm512_min_ps(ax, ay),
        _mm512_add_ps(_mm512_max_ps(ax, ay), eps),
    );
    let cc = _mm512_mul_ps(c, c);
    let mut a = _mm512_mul_ps(
        _mm512_fmadd_ps(
            _mm512_fmadd_ps(_mm512_fmadd_ps(cc, p7, p5), cc, p3),
            cc,
            p1,
        ),
        c,
    );
    let mask = _mm512_cmp_ps_mask(ax, ay, _CMP_GE_OQ);
    a = _mm512_mask_sub_ps(a, !mask, v90, a);
    let mask = _mm512_cmp_ps_mask(x, z, _CMP_LT_OQ);
    a = _mm512_mask_sub_ps(a, mask, v180, a);
    let mask = _mm512_cmp_ps_mask(y, z, _CMP_LT_OQ);
    _mm512_mask_sub_ps(a, mask, v360, a)
}

// ==================== Canny（edges.rs 对应内核） ====================

/// Canny 幅度行（i16 dx/dy → i32 mag，AVX512，每批 16 元素）。
///
/// L1: `|dx| + |dy|`；L2: `dx² + dy²`。
///
/// # Safety
/// - `dx`/`dy`/`mag` 长度 ≥ n
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn canny_mag_row(dx: &[i16], dy: &[i16], mag: &mut [i32], n: usize, l2: bool) {
    let mut i = 0usize;
    // SAFETY: i+16 <= n 保证读 [i, i+16)×2 写 [i, i+16) 不越界
    unsafe {
        while i + 16 <= n {
            let vx = _mm256_loadu_si256(dx.as_ptr().add(i) as *const __m256i);
            let vy = _mm256_loadu_si256(dy.as_ptr().add(i) as *const __m256i);
            let m = if l2 {
                let x = _mm512_cvtepi16_epi32(vx);
                let y = _mm512_cvtepi16_epi32(vy);
                _mm512_add_epi32(_mm512_mullo_epi32(x, x), _mm512_mullo_epi32(y, y))
            } else {
                let x = _mm512_cvtepi16_epi32(_mm256_abs_epi16(vx));
                let y = _mm512_cvtepi16_epi32(_mm256_abs_epi16(vy));
                _mm512_add_epi32(x, y)
            };
            _mm512_storeu_si512(mag.as_mut_ptr().add(i) as *mut __m512i, m);
            i += 16;
        }
    }
    crate::simd_kernels::edges::mag_row(&dx[i..], &dy[i..], &mut mag[i..], n - i, l2);
}

/// Canny NMS 批预筛（AVX512）：`mag_a[0..32] > low` 的 32 位掩码。
///
/// bit j 对应 mag_a[j]。调用方保证长度 ≥ 32。
///
/// # Safety
/// - `mag_a` 长度 ≥ 32
/// - CPU 需支持 AVX512F
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn canny_low_mask32(mag_a: &[i32], low: i32) -> u32 {
    // SAFETY: 调用方保证长度 ≥ 32
    unsafe {
        let vlow = _mm512_set1_epi32(low);
        let m0 = _mm512_loadu_si512(mag_a.as_ptr() as *const __m512i);
        let m1 = _mm512_loadu_si512(mag_a.as_ptr().add(16) as *const __m512i);
        let c0 = _mm512_cmpgt_epi32_mask(m0, vlow);
        let c1 = _mm512_cmpgt_epi32_mask(m1, vlow);
        (c0 as u32) | ((c1 as u32) << 16)
    }
}

/// Canny NMS 批预筛（AVX512）：`mag_a[0..64] > low` 的 64 位掩码。
///
/// bit j 对应 mag_a[j]。调用方保证长度 ≥ 64。
///
/// # Safety
/// - `mag_a` 长度 ≥ 64
/// - CPU 需支持 AVX512F
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn canny_low_mask64(mag_a: &[i32], low: i32) -> u64 {
    // SAFETY: 调用方保证长度 ≥ 64
    unsafe {
        let vlow = _mm512_set1_epi32(low);
        let m0 = _mm512_loadu_si512(mag_a.as_ptr() as *const __m512i);
        let m1 = _mm512_loadu_si512(mag_a.as_ptr().add(32) as *const __m512i);
        let c0 = _mm512_cmpgt_epi32_mask(m0, vlow);
        let c1 = _mm512_cmpgt_epi32_mask(m1, vlow);
        (c0 as u64) | ((c1 as u64) << 32)
    }
}

/// Canny finalPass 单行（AVX512，每批 64 元素）：`dst = (map==2) ? 255 : 0`。
///
/// # Safety
/// - `pmap` 长度 ≥ w+2（含哨兵），`dst` 长度 ≥ w
/// - CPU 需支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn canny_final_pass_row(pmap: &[u8], dst: &mut [u8], w: usize) {
    let mut i = 0usize;
    // SAFETY: i+64 <= w 保证读 [i+1, i+65) 写 [i, i+64) 不越界
    unsafe {
        while i + 64 <= w {
            let m = _mm512_loadu_si512(pmap.as_ptr().add(i + 1) as *const __m512i);
            let eq = _mm512_cmpeq_epi8_mask(m, _mm512_set1_epi8(2));
            // 掩码 bit j → 字节 0xFF/0x00
            let out = _mm512_mask_mov_epi8(_mm512_setzero_si512(), eq, _mm512_set1_epi8(-1));
            _mm512_storeu_si512(dst.as_mut_ptr().add(i) as *mut __m512i, out);
            i += 64;
        }
        while i + 16 <= w {
            let m = _mm_loadu_si128(pmap.as_ptr().add(i + 1) as *const __m128i);
            let eq = _mm_cmpeq_epi8(m, _mm_set1_epi8(2));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, eq);
            i += 16;
        }
    }
    crate::simd_kernels::edges::final_pass_row(&pmap[i..], &mut dst[i..], w - i);
}

/// Canny NMS 批预写 1（AVX512，每批 64 元素）：`pmap[0..64] = 1`。
///
/// # Safety
/// - `pmap` 长度 ≥ 64
/// - CPU 需支持 AVX512F/BW/VL
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn canny_fill_one64(pmap: &mut [u8]) {
    // SAFETY: 调用方保证长度 ≥ 64
    unsafe {
        let one = _mm512_set1_epi8(1);
        _mm512_storeu_si512(pmap.as_mut_ptr() as *mut __m512i, one);
    }
}

/// Canny NMS + 双阈值整行（AVX512，全向量化：每批 16 像素，无逐候选标量）。
///
/// 对齐 canny.cpp:536-638 语义：TG22=13573 定点角度，水平/垂直分支前向
/// `>` 后向 `>=`，对角分支两侧 `>`（s = (xs^ys)<0 ? -1 : 1）。
/// `is_edge` 按角度三分支选择；`pmap = is_edge ? (m>high ? 2 : 0) : 1`；
/// `is_edge & m>high` 的列号（1..w+1）经 compressstore 直接入栈。
///
/// `mag_*`/`pmap` 长度 w+2（哨兵），有效区 [1, w]；`dx`/`dy` 长度 w。
/// 栈元素为 pmap 相对偏移（列 1..w+1）。
///
/// # Safety
/// - 上述长度契约由调用方保证
/// - CPU 需支持 AVX512F/BW/VL
#[allow(clippy::too_many_arguments)]
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn canny_nms_row(
    mag_p: &[i32],
    mag_a: &[i32],
    mag_n: &[i32],
    dx: &[i16],
    dy: &[i16],
    pmap: &mut [u8],
    w: usize,
    low: i32,
    high: i32,
    stack: &mut Vec<usize>,
) {
    const TG22: i32 = 13573;
    // SAFETY: 本函数所有指针访问的边界由函数文档契约保证（主循环
    // j+16 <= w 且 j+32 <= w+2 时 mag_p/a/n 读 [j, j+32)（行长 w+2）、
    // dx/dy 读 [j, j+16)、pmap 写 [j+1, j+17) 均在界内）
    unsafe {
        let vlow = _mm512_set1_epi32(low);
        let vhigh = _mm512_set1_epi32(high);
        let vtg22 = _mm512_set1_epi32(TG22);
        // 首尾哨兵（对齐 OpenCV _pmap[-1] = _pmap[w] = 1）
        pmap[0] = 1;
        pmap[w + 1] = 1;
        let mut j = 0usize;
        // 主循环：每批 16px，mag 三行各 load 2×512 位（[j, j+16) 与
        // [j+16, j+32)），alignr 提取 [j+1, j+17)/[j+2, j+18) 列偏移视图，
        // mag loads 9 → 6/批
        while j + 16 <= w && j + 32 <= w + 2 {
            // mag_a：A=[j, j+16)，B=[j+16, j+32)；m=[j+1, j+17)，mr=[j+2, j+18)
            let a_lo = _mm512_loadu_si512(mag_a.as_ptr().add(j) as *const __m512i);
            let a_hi = _mm512_loadu_si512(mag_a.as_ptr().add(j + 16) as *const __m512i);
            let m = _mm512_alignr_epi32(a_hi, a_lo, 1);
            let mr = _mm512_alignr_epi32(a_hi, a_lo, 2);
            // mag_p / mag_n 同构
            let p_lo = _mm512_loadu_si512(mag_p.as_ptr().add(j) as *const __m512i);
            let p_hi = _mm512_loadu_si512(mag_p.as_ptr().add(j + 16) as *const __m512i);
            let mp = _mm512_alignr_epi32(p_hi, p_lo, 1);
            let mp_k2 = _mm512_alignr_epi32(p_hi, p_lo, 2);
            let n_lo = _mm512_loadu_si512(mag_n.as_ptr().add(j) as *const __m512i);
            let n_hi = _mm512_loadu_si512(mag_n.as_ptr().add(j + 16) as *const __m512i);
            let mn = _mm512_alignr_epi32(n_hi, n_lo, 1);
            let mn_k2 = _mm512_alignr_epi32(n_hi, n_lo, 2);
            // x = |dx|, y = |dy| << 15（i16 → i32 零扩展 + abs）
            let dxdy = _mm256_loadu_si256(dx.as_ptr().add(j) as *const __m256i);
            let dydy = _mm256_loadu_si256(dy.as_ptr().add(j) as *const __m256i);
            let xs = _mm512_cvtepi16_epi32(dxdy);
            let ys = _mm512_cvtepi16_epi32(dydy);
            let vx = _mm512_abs_epi32(xs);
            let vy = _mm512_slli_epi32(_mm512_abs_epi32(ys), 15);
            let tg22x = _mm512_mullo_epi32(vx, vtg22);
            let tg67x = _mm512_add_epi32(tg22x, _mm512_slli_epi32(vx, 16));
            // 水平分支（y < tg22x）：前向 m > mag_a[k]（左邻 = a_lo），后向 m >= mr
            let k_h: __mmask16 =
                _mm512_cmpgt_epi32_mask(m, a_lo) & _mm512_cmpge_epi32_mask(m, mr);
            // 垂直分支（y > tg67x）：前向 m > mp，后向 m >= mn
            let k_v: __mmask16 =
                _mm512_cmpgt_epi32_mask(m, mp) & _mm512_cmpge_epi32_mask(m, mn);
            // 对角分支：s = (xs^ys)<0 ? -1 : 1；前向 m > mag_p[k+1-s]，后向 m > mag_n[k+1+s]
            // 列偏移 k+1±s：s=+1 → p[k], n[k+2]；s=-1 → p[k+2], n[k]
            // mask_blend 按 neg 掩码选对角邻居：
            //   mp_d = neg ? mag_p[k+2] : mag_p[k]（k 视图 = alignr(...,3)）
            //   mn_d = neg ? mag_n[k]   : mag_n[k+2]
            let neg = _mm512_cmplt_epi32_mask(_mm512_xor_si512(xs, ys), _mm512_setzero_si512());
            // mag_p[k] 视图 = [j, j+16) = p_lo 本身
            let mp_d = _mm512_mask_blend_epi32(neg, p_lo, mp_k2);
            let mn_d = _mm512_mask_blend_epi32(neg, mn_k2, n_lo);
            let k_d: __mmask16 =
                _mm512_cmpgt_epi32_mask(m, mp_d) & _mm512_cmpgt_epi32_mask(m, mn_d);
            // 按角度选择分支：horiz = y < tg22x；vert = !horiz & (y > tg67x)；diag = 其余
            let horiz: __mmask16 = _mm512_cmplt_epi32_mask(vy, tg22x);
            let vert: __mmask16 = !horiz & _mm512_cmpgt_epi32_mask(vy, tg67x);
            // k_e = horiz ? k_h : (vert ? k_v : k_d)（掩码位选择）
            let k_e: __mmask16 = (horiz & k_h) | (vert & k_v) | (!(horiz | vert) & k_d);
            // low 预筛 + 双阈值
            let k_low: __mmask16 = _mm512_cmpgt_epi32_mask(m, vlow);
            let k_edge: __mmask16 = k_low & k_e;
            let k_strong: __mmask16 = k_edge & _mm512_cmpgt_epi32_mask(m, vhigh);
            // pmap = !k_edge → 1；k_edge & !k_strong → 0；k_strong → 2
            let v1 = _mm512_set1_epi32(1);
            let v2 = _mm512_set1_epi32(2);
            let v0 = _mm512_setzero_si512();
            let val = _mm512_mask_mov_epi32(
                _mm512_mask_mov_epi32(v1, k_edge, v0),
                k_strong,
                v2,
            );
            // i32 → u8 截断打包（值域 0/1/2，低 8 位即结果）
            let bytes = _mm512_cvtusepi32_epi8(val);
            _mm_storeu_si128(pmap.as_mut_ptr().add(j + 1) as *mut __m128i, bytes);
            // 栈收集：k_strong 的 pmap 列偏移（k+1，k = j..j+16 → 偏移 j+1..j+17）
            if k_strong != 0 {
                let cols = _mm512_add_epi32(
                    _mm512_set1_epi32((j + 1) as i32),
                    _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15),
                );
                let cnt = k_strong.count_ones() as usize;
                // SAFETY: compressstore 写 i32（4 字节）到临时缓冲，随后逐元素
                // 转 usize 追加到栈（避免 resize 清零 + 破坏 usize 布局）
                let mut tmp = [0i32; 16];
                _mm512_mask_compressstoreu_epi32(tmp.as_mut_ptr(), k_strong, cols);
                stack.extend(tmp.iter().take(cnt).map(|&t| t as usize));
            }
            j += 16;
        }
        // 尾部标量（逐像素，含 low 预筛）
        for k in j..w {
            if mag_a[k + 1] > low {
                crate::simd_kernels::edges::nms_one(
                    mag_p.as_ptr(), mag_a.as_ptr(), mag_n.as_ptr(), dx.as_ptr(), dy.as_ptr(),
                    pmap.as_mut_ptr(), k, high, stack,
                );
            } else {
                pmap[k + 1] = 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 轮廓查找：行扫描跳转（对齐 contours_link.cpp，AVX512 掩码寄存器 + tzcnt）
// ---------------------------------------------------------------------------

/// 行内找第一个非零 u8（从 `j` 到 `row.len()`）。
///
/// # Safety
/// `j <= row.len()`。
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn find_start_contour_point(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    let zero = _mm512_setzero_si512();
    // SAFETY: k + 64 <= width 保证 load [k, k+64) 在界内
    while k + 64 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm512_loadu_si512(row.as_ptr().add(k) as *const __m512i);
        // 非零 = !cmpeq(v, 0)（不能用 cmpgt：u8 ≥ 128 的有符号值为负）
        let mask = !_mm512_cmpeq_epi8_mask(v, zero);
        if mask != 0 {
            return k + mask.trailing_zeros() as usize;
        }
        k += 64;
        }
    }
    while k < width && row[k] == 0 {
        k += 1;
    }
    k
}

/// 行内找第一个零 u8（从 `j` 到 `row.len()`）。
///
/// # Safety
/// `j <= row.len()`。
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn find_end_contour_point(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    let zero = _mm512_setzero_si512();
    // SAFETY: k + 64 <= width 保证 load 在界内
    while k + 64 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm512_loadu_si512(row.as_ptr().add(k) as *const __m512i);
        // 零像素 = cmpeq(v, 0)
        let mask = _mm512_cmpeq_epi8_mask(v, zero);
        if mask != 0 {
            return k + mask.trailing_zeros() as usize;
        }
        k += 64;
        }
    }
    while k < width && row[k] != 0 {
        k += 1;
    }
    k
}

/// 单趟扫描一行，返回所有 run 边界 `[start, end)`（start/end 为行内下标）。
///
/// 对齐 OpenCV TRU 的 findStart/findEnd 交替扫描，但用单趟 AVX512 块扫描
/// 消除逐 run 的分支预测抖动（bench 图 35K runs 时扫描占 0.65ms/1.12ms）。
///
/// # Safety
/// `row` 长度 ≥ 1；`starts`/`ends` 复用缓冲（调用方 clear）。
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn scan_row_runs(row: &[u8], starts: &mut Vec<u32>, ends: &mut Vec<u32>) {
    starts.clear();
    ends.clear();
    let width = row.len();
    let zero = _mm512_setzero_si512();
    let mut k = 1usize;
    let mut in_run = false;
    let mut run_start = 0u32;
    // SAFETY: k + 64 <= width 保证 load [k, k+64) 在界内
    while k + 64 <= width {
        // SAFETY: 循环条件保证 load 在界内
        unsafe {
        let v = _mm512_loadu_si512(row.as_ptr().add(k) as *const __m512i);
        let mask = !_mm512_cmpeq_epi8_mask(v, zero);
        let mut m = mask;
        // 上一块末尾的 run 在本块开头结束
        if in_run && (m & 1) == 0 {
            starts.push(run_start);
            ends.push(k as u32);
            in_run = false;
        }
        // 块内已消费的位偏移（m 被逐步右移，tz/to 相对当前 m）
        let mut bit = 0usize;
        while m != 0 {
            let tz = m.trailing_zeros() as usize;
            let p = k + bit + tz;
            let shifted = m >> tz;
            let to = shifted.trailing_ones() as usize;
            let end = p + to;
            if !in_run {
                run_start = p as u32;
            }
            if bit + tz + to == 64 {
                // run 延伸到块末尾
                in_run = true;
                m = 0;
            } else {
                starts.push(run_start);
                ends.push(end as u32);
                in_run = false;
                m = shifted >> to;
                bit += tz + to;
            }
        }
        }
        k += 64;
    }
    // 尾部（< 64 字节）
    while k < width {
        if row[k] != 0 {
            if !in_run {
                run_start = k as u32;
                in_run = true;
            }
        } else if in_run {
            starts.push(run_start);
            ends.push(k as u32);
            in_run = false;
        }
        k += 1;
    }
    if in_run {
        starts.push(run_start);
        ends.push(width as u32);
    }
}

/// Suzuki findNextX：行内找第一个与 `prev` 不同的 i8（从 `j` 到 `width`）。
///
/// 调用方已做首元素检查。
/// # Safety
/// `j <= width <= row.len()`。
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn find_next_diff(row: &[i8], j: usize, width: usize, prev: i8, p: &mut i8) -> usize {
    let mut k = j;
    let vprev = _mm512_set1_epi8(prev);
    // SAFETY: k + 64 <= width 保证 load [k, k+64) 在界内
    while k + 64 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm512_loadu_si512(row.as_ptr().add(k) as *const __m512i);
        let ne = _mm512_cmpeq_epi8_mask(v, vprev);
        let mask = !ne;
        if mask != 0 {
            let r = k + mask.trailing_zeros() as usize;
            *p = row[r];
            return r;
        }
        k += 64;
        }
    }
    while k < width && row[k] == prev {
        k += 1;
    }
    if k < width {
        *p = row[k];
    }
    k
}

// ---- 按位逻辑（bitwise.rs，对齐 arithm.simd.hpp and8u/or8u/xor8u/not8u）----

/// 按位与：64 字节/迭代。
///
/// # Safety
/// `n <= a.len() == b.len() == dst.len()`；CPU 须支持 AVX512F/BW/VL（由 isa() 派发保证）。
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bitwise_and_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // 非临时存储（movntdq）需 64 字节对齐 dst；避免写分配（RFO），
    // 提升 3 流（2 读 + 1 写）纯流式带宽。
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界
    unsafe {
        // 头部（< 64 字节，未对齐，标量）
        while i < head {
            *dst.get_unchecked_mut(i) = *a.get_unchecked(i) & *b.get_unchecked(i);
            i += 1;
        }
        // 中部（64 字节对齐，非临时存储）
        while i + 64 <= n {
            let va = _mm512_loadu_si512(a.as_ptr().add(i) as *const __m512i);
            let vb = _mm512_loadu_si512(b.as_ptr().add(i) as *const __m512i);
            let r = _mm512_and_si512(va, vb);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_and_bytes(&a[i..], &b[i..], &mut dst[i..], n - i);
    }
}

/// 按位或。
///
/// # Safety
/// 同 [`bitwise_and_bytes`]。
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bitwise_or_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界
    unsafe {
        while i < head {
            *dst.get_unchecked_mut(i) = *a.get_unchecked(i) | *b.get_unchecked(i);
            i += 1;
        }
        while i + 64 <= n {
            let va = _mm512_loadu_si512(a.as_ptr().add(i) as *const __m512i);
            let vb = _mm512_loadu_si512(b.as_ptr().add(i) as *const __m512i);
            let r = _mm512_or_si512(va, vb);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_or_bytes(&a[i..], &b[i..], &mut dst[i..], n - i);
    }
}

/// 按位异或。
///
/// # Safety
/// 同 [`bitwise_and_bytes`]。
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bitwise_xor_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界
    unsafe {
        while i < head {
            *dst.get_unchecked_mut(i) = *a.get_unchecked(i) ^ *b.get_unchecked(i);
            i += 1;
        }
        while i + 64 <= n {
            let va = _mm512_loadu_si512(a.as_ptr().add(i) as *const __m512i);
            let vb = _mm512_loadu_si512(b.as_ptr().add(i) as *const __m512i);
            let r = _mm512_xor_si512(va, vb);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_xor_bytes(&a[i..], &b[i..], &mut dst[i..], n - i);
    }
}

/// 按位取反。
///
/// 用 `vpternlogd $0xFF`（imm=0xFF 时结果恒 ~a，与 b/c 无关）融合
/// load+not 为单条指令（内存操作数形式），2 指令/64B。
///
/// # Safety
/// `n <= a.len() == dst.len()`；CPU 须支持 AVX512F/BW/VL。
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn bitwise_not_bytes(a: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // 非临时存储（movntdq）需 64 字节对齐 dst；避免写分配（RFO）。
    // 输出为每次新分配（冷），流式写绕过 RFO 提升 2 流带宽。
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((64 - (dst_ptr & 63)) & 63).min(n);
    let ones = _mm512_set1_epi8(-1);
    // SAFETY: 循环条件 i+64 <= n 保证读 [i, i+64) 不越界
    unsafe {
        // 头部（< 64 字节，未对齐，标量）
        while i < head {
            *dst.get_unchecked_mut(i) = !*a.get_unchecked(i);
            i += 1;
        }
        // 中部（64 字节对齐，非临时存储），2×64 展开隐藏 load/store 延迟
        while i + 128 <= n {
            let va0 = _mm512_loadu_si512(a.as_ptr().add(i) as *const __m512i);
            let va1 = _mm512_loadu_si512(a.as_ptr().add(i + 64) as *const __m512i);
            let r0 = _mm512_xor_si512(va0, ones);
            let r1 = _mm512_xor_si512(va1, ones);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r0);
            _mm512_stream_si512(dst.as_mut_ptr().add(i + 64) as *mut __m512i, r1);
            i += 128;
        }
        while i + 64 <= n {
            let va = _mm512_loadu_si512(a.as_ptr().add(i) as *const __m512i);
            let r = _mm512_xor_si512(va, ones);
            _mm512_stream_si512(dst.as_mut_ptr().add(i) as *mut __m512i, r);
            i += 64;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_not_bytes(&a[i..], &mut dst[i..], n - i);
    }
}

/// 3×3 距离变换前向扫描（AVX512，每批 16 像素）。
///
/// 向量化 top 行 min 计算（`min(top[j-1]+diag, top[j]+hv, top[j+1]+diag)`），
/// 标量修复 `left` 依赖（对齐 OpenCV `distanceTransform_3x3` 的 SIMD 路径）。
///
/// `temp` 为含 border 哨兵的 u32 缓冲，`step` 为行步长（含 border），
/// `row` 为当前行起始索引（含 border），`s` 为当前行源像素。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + step) + border + w + 1`
/// - `s` 长度 ≥ `w`
/// - CPU 需支持 AVX512F/BW/VL
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn dist_forward_3x3(
    temp: &mut [u32],
    s: &[u8],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    dist_max: u32,
) {
    let v_hv = _mm512_set1_epi32(hv_dist as i32);
    let v_diag = _mm512_set1_epi32(diag_dist as i32);
    let v_max = _mm512_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (k+1)*hv（前向，从左到右）
    let v_ramp1 = _mm512_set_epi32(
        (16 * hv_dist) as i32,
        (15 * hv_dist) as i32,
        (14 * hv_dist) as i32,
        (13 * hv_dist) as i32,
        (12 * hv_dist) as i32,
        (11 * hv_dist) as i32,
        (10 * hv_dist) as i32,
        (9 * hv_dist) as i32,
        (8 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (1 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv（避免加法回绕产生假胜出）
    let v_c1 = _mm512_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm512_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm512_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_c8 = _mm512_set1_epi32((dist_max - 8 * hv_dist) as i32);
    let v_s1 = _mm512_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm512_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm512_set1_epi32((4 * hv_dist) as i32);
    let v_s8 = _mm512_set1_epi32((8 * hv_dist) as i32);
    // 前缀扫描有效掩码：step s 有效 lane k >= s
    let m1 = !0u16 << 1;
    let m2 = !0u16 << 2;
    let m4 = !0u16 << 4;
    let m8 = !0u16 << 8;

    let nb = w / 16;
    let tail = w % 16;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部前缀扫描（块独立，向量化 top 行 min + 零掩码）
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let top = temp.as_ptr().add(row - step + border + j - 1);
            let tl = _mm512_loadu_si512(top as *const __m512i);
            let t = _mm512_loadu_si512(top.add(1) as *const __m512i);
            let tr = _mm512_loadu_si512(top.add(2) as *const __m512i);
            let m = _mm512_min_epu32(
                _mm512_min_epu32(
                    _mm512_add_epi32(tl, v_diag),
                    _mm512_add_epi32(t, v_hv),
                ),
                _mm512_add_epi32(tr, v_diag),
            );
            let m = _mm512_min_epu32(m, _mm512_set1_epi32(dist_max as i32));
            // 零掩码：s[j]==0 的 lane 置 0（_mm_test_epi8_mask 置位非零字节，取反）
            let sv = _mm_loadu_si128(s.as_ptr().add(j) as *const __m128i);
            let zm = _mm_test_epi8_mask(sv, sv);
            let mut a = _mm512_mask_mov_epi32(m, !zm, _mm512_setzero_si512());
            // 前缀扫描：s=1,2,4,8
            let src = _mm512_alignr_epi32(a, a, 15);
            let src = _mm512_min_epu32(src, v_c1);
            let t = _mm512_add_epi32(src, v_s1);
            a = _mm512_mask_min_epu32(a, m1, a, t);
            let src = _mm512_alignr_epi32(a, a, 14);
            let src = _mm512_min_epu32(src, v_c2);
            let t = _mm512_add_epi32(src, v_s2);
            a = _mm512_mask_min_epu32(a, m2, a, t);
            let src = _mm512_alignr_epi32(a, a, 12);
            let src = _mm512_min_epu32(src, v_c4);
            let t = _mm512_add_epi32(src, v_s4);
            a = _mm512_mask_min_epu32(a, m4, a, t);
            let src = _mm512_alignr_epi32(a, a, 8);
            let src = _mm512_min_epu32(src, v_c8);
            let t = _mm512_add_epi32(src, v_s8);
            a = _mm512_mask_min_epu32(a, m8, a, t);
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, a);
        }
    }

    // 标量尾部（最左 [0, tail) 像素，含 left 依赖）
    let mut left = temp[row + border - 1];
    for j in 0..tail {
        let idx = row + border + j;
        if s[j] == 0 {
            temp[idx] = 0;
            left = 0;
        } else {
            let t0 = temp[idx - step - 1].wrapping_add(diag_dist);
            let t1 = temp[idx - step].wrapping_add(hv_dist);
            let t2 = temp[idx - step + 1].wrapping_add(diag_dist);
            let t3 = left.wrapping_add(hv_dist);
            let mut m = t0.min(t1);
            m = m.min(t2);
            m = m.min(t3);
            temp[idx] = if m > dist_max { dist_max } else { m };
            left = temp[idx];
        }
    }

    // Phase 2+3: 串行 stitch + 折叠（块从左到右，lane15 输出即下一块 incoming）
    let mut l = left;
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let local = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let vl = _mm512_set1_epi32(l as i32);
            let t = _mm512_add_epi32(vl, v_ramp1);
            let wrap = _mm512_cmplt_epu32_mask(t, vl);
            let t = _mm512_mask_mov_epi32(t, wrap, v_max);
            let v = _mm512_min_epu32(local, t);
            l = _mm_extract_epi32(_mm512_extracti32x4_epi32(v, 3), 3) as u32;
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, v);
        }
    }
}

/// 5×5 距离变换前向扫描（AVX512，每批 16 像素）。
///
/// 向量化 top1/top2 行 min 计算，标量修复 `left1`/`left2` 依赖
/// （对齐 OpenCV `distanceTransform_5x5` 的 SIMD 路径）。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + 2*step) + border + w + 2`
/// - `s` 长度 ≥ `w`
/// - CPU 需支持 AVX512F/BW/VL
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn dist_forward_5x5(
    temp: &mut [u32],
    s: &[u8],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    long_dist: u32,
    dist_max: u32,
) {
    let v_hv = _mm512_set1_epi32(hv_dist as i32);
    let v_diag = _mm512_set1_epi32(diag_dist as i32);
    let v_long = _mm512_set1_epi32(long_dist as i32);
    let v_max = _mm512_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (k+1)*hv（前向，从左到右）
    let v_ramp1 = _mm512_set_epi32(
        (16 * hv_dist) as i32,
        (15 * hv_dist) as i32,
        (14 * hv_dist) as i32,
        (13 * hv_dist) as i32,
        (12 * hv_dist) as i32,
        (11 * hv_dist) as i32,
        (10 * hv_dist) as i32,
        (9 * hv_dist) as i32,
        (8 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (1 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv（避免加法回绕产生假胜出）
    let v_c1 = _mm512_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm512_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm512_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_c8 = _mm512_set1_epi32((dist_max - 8 * hv_dist) as i32);
    let v_s1 = _mm512_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm512_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm512_set1_epi32((4 * hv_dist) as i32);
    let v_s8 = _mm512_set1_epi32((8 * hv_dist) as i32);
    // 前缀扫描有效掩码：step s 有效 lane k >= s
    let m1 = !0u16 << 1;
    let m2 = !0u16 << 2;
    let m4 = !0u16 << 4;
    let m8 = !0u16 << 8;

    let nb = w / 16;
    let tail = w % 16;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部前缀扫描（块独立，向量化 top1/top2 行 min + 零掩码）
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let top1 = temp.as_ptr().add(row - step + border + j - 2);
            let top2 = temp.as_ptr().add(row - 2 * step + border + j - 1);
            let t2m1 = _mm512_loadu_si512(top2 as *const __m512i);
            let t2p1 = _mm512_loadu_si512(top2.add(2) as *const __m512i);
            let t1m2 = _mm512_loadu_si512(top1 as *const __m512i);
            let t1m1 = _mm512_loadu_si512(top1.add(1) as *const __m512i);
            let t1 = _mm512_loadu_si512(top1.add(2) as *const __m512i);
            let t1p1 = _mm512_loadu_si512(top1.add(3) as *const __m512i);
            let t1p2 = _mm512_loadu_si512(top1.add(4) as *const __m512i);
            let m = _mm512_min_epu32(
                _mm512_min_epu32(
                    _mm512_min_epu32(
                        _mm512_add_epi32(t2m1, v_long),
                        _mm512_add_epi32(t2p1, v_long),
                    ),
                    _mm512_min_epu32(
                        _mm512_add_epi32(t1m2, v_long),
                        _mm512_add_epi32(t1m1, v_diag),
                    ),
                ),
                _mm512_min_epu32(
                    _mm512_min_epu32(
                        _mm512_add_epi32(t1, v_hv),
                        _mm512_add_epi32(t1p1, v_diag),
                    ),
                    _mm512_add_epi32(t1p2, v_long),
                ),
            );
            let m = _mm512_min_epu32(m, _mm512_set1_epi32(dist_max as i32));
            // 零掩码：s[j]==0 的 lane 置 0（_mm_test_epi8_mask 置位非零字节，取反）
            let sv = _mm_loadu_si128(s.as_ptr().add(j) as *const __m128i);
            let zm = _mm_test_epi8_mask(sv, sv);
            let mut a = _mm512_mask_mov_epi32(m, !zm, _mm512_setzero_si512());
            // 前缀扫描：s=1,2,4,8
            let src = _mm512_alignr_epi32(a, a, 15);
            let src = _mm512_min_epu32(src, v_c1);
            let t = _mm512_add_epi32(src, v_s1);
            a = _mm512_mask_min_epu32(a, m1, a, t);
            let src = _mm512_alignr_epi32(a, a, 14);
            let src = _mm512_min_epu32(src, v_c2);
            let t = _mm512_add_epi32(src, v_s2);
            a = _mm512_mask_min_epu32(a, m2, a, t);
            let src = _mm512_alignr_epi32(a, a, 12);
            let src = _mm512_min_epu32(src, v_c4);
            let t = _mm512_add_epi32(src, v_s4);
            a = _mm512_mask_min_epu32(a, m4, a, t);
            let src = _mm512_alignr_epi32(a, a, 8);
            let src = _mm512_min_epu32(src, v_c8);
            let t = _mm512_add_epi32(src, v_s8);
            a = _mm512_mask_min_epu32(a, m8, a, t);
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, a);
        }
    }

    // 标量尾部（最左 [0, tail) 像素，含 left 依赖）
    let mut left = temp[row + border - 1];
    for j in 0..tail {
        let idx = row + border + j;
        if s[j] == 0 {
            temp[idx] = 0;
            left = 0;
        } else {
            let mut t0 = temp[idx - 2 * step - 1].wrapping_add(long_dist);
            let mut t = temp[idx - 2 * step + 1].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx - step - 2].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx - step - 1].wrapping_add(diag_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx - step].wrapping_add(hv_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx - step + 1].wrapping_add(diag_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx - step + 2].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = left.wrapping_add(hv_dist);
            if t < t0 {
                t0 = t;
            }
            temp[idx] = if t0 > dist_max { dist_max } else { t0 };
            left = temp[idx];
        }
    }

    // Phase 2+3: 串行 stitch + 折叠（块从左到右，lane15 输出即下一块 incoming）
    let mut l = left;
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let local = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let vl = _mm512_set1_epi32(l as i32);
            let t = _mm512_add_epi32(vl, v_ramp1);
            let wrap = _mm512_cmplt_epu32_mask(t, vl);
            let t = _mm512_mask_mov_epi32(t, wrap, v_max);
            let v = _mm512_min_epu32(local, t);
            l = _mm_extract_epi32(_mm512_extracti32x4_epi32(v, 3), 3) as u32;
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, v);
        }
    }
}

/// 5×5 距离变换后向扫描（AVX512，每批 16 像素）。
///
/// 向量化 bot1/bot2 行 min 计算，标量修复 `right` 依赖（对齐 OpenCV）。
/// 定点 → 浮点转换向量化。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + 2*step) + border + w + 2`
/// - `drow` 长度 ≥ `w`
/// - CPU 需支持 AVX512F/BW/VL
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn dist_backward_5x5(
    temp: &mut [u32],
    drow: &mut [f32],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    long_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    let v_hv = _mm512_set1_epi32(hv_dist as i32);
    let v_diag = _mm512_set1_epi32(diag_dist as i32);
    let v_long = _mm512_set1_epi32(long_dist as i32);
    let v_scale = _mm512_set1_ps(scale);
    let v_max = _mm512_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (16-k)*hv（后向，从右到左）
    let v_ramp2 = _mm512_set_epi32(
        (1 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (8 * hv_dist) as i32,
        (9 * hv_dist) as i32,
        (10 * hv_dist) as i32,
        (11 * hv_dist) as i32,
        (12 * hv_dist) as i32,
        (13 * hv_dist) as i32,
        (14 * hv_dist) as i32,
        (15 * hv_dist) as i32,
        (16 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv（避免加法回绕产生假胜出）
    let v_c1 = _mm512_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm512_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm512_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_c8 = _mm512_set1_epi32((dist_max - 8 * hv_dist) as i32);
    let v_s1 = _mm512_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm512_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm512_set1_epi32((4 * hv_dist) as i32);
    let v_s8 = _mm512_set1_epi32((8 * hv_dist) as i32);
    // 后缀扫描有效掩码：step s 有效 lane k < 16-s
    let m1 = (1u16 << 15) - 1;
    let m2 = (1u16 << 14) - 1;
    let m4 = (1u16 << 12) - 1;
    let m8 = (1u16 << 8) - 1;

    let nb = w / 16;
    let tail = w % 16;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部后缀扫描（块独立，向量化 bot1/bot2 行 min + t0）
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let bot1 = temp.as_ptr().add(row + step + border + j - 2);
            let bot2 = temp.as_ptr().add(row + 2 * step + border + j - 1);
            let b2m1 = _mm512_loadu_si512(bot2 as *const __m512i);
            let b2p1 = _mm512_loadu_si512(bot2.add(2) as *const __m512i);
            let b1m2 = _mm512_loadu_si512(bot1 as *const __m512i);
            let b1m1 = _mm512_loadu_si512(bot1.add(1) as *const __m512i);
            let b1 = _mm512_loadu_si512(bot1.add(2) as *const __m512i);
            let b1p1 = _mm512_loadu_si512(bot1.add(3) as *const __m512i);
            let b1p2 = _mm512_loadu_si512(bot1.add(4) as *const __m512i);
            let m = _mm512_min_epu32(
                _mm512_min_epu32(
                    _mm512_min_epu32(
                        _mm512_add_epi32(b2m1, v_long),
                        _mm512_add_epi32(b2p1, v_long),
                    ),
                    _mm512_min_epu32(
                        _mm512_add_epi32(b1m2, v_long),
                        _mm512_add_epi32(b1m1, v_diag),
                    ),
                ),
                _mm512_min_epu32(
                    _mm512_min_epu32(
                        _mm512_add_epi32(b1, v_hv),
                        _mm512_add_epi32(b1p1, v_diag),
                    ),
                    _mm512_add_epi32(b1p2, v_long),
                ),
            );
            let t0 = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let mut a = _mm512_min_epu32(m, t0);
            // 后缀扫描：s=1,2,4,8
            let src = _mm512_alignr_epi32(a, a, 1);
            let src = _mm512_min_epu32(src, v_c1);
            let t = _mm512_add_epi32(src, v_s1);
            a = _mm512_mask_min_epu32(a, m1, a, t);
            let src = _mm512_alignr_epi32(a, a, 2);
            let src = _mm512_min_epu32(src, v_c2);
            let t = _mm512_add_epi32(src, v_s2);
            a = _mm512_mask_min_epu32(a, m2, a, t);
            let src = _mm512_alignr_epi32(a, a, 4);
            let src = _mm512_min_epu32(src, v_c4);
            let t = _mm512_add_epi32(src, v_s4);
            a = _mm512_mask_min_epu32(a, m4, a, t);
            let src = _mm512_alignr_epi32(a, a, 8);
            let src = _mm512_min_epu32(src, v_c8);
            let t = _mm512_add_epi32(src, v_s8);
            a = _mm512_mask_min_epu32(a, m8, a, t);
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, a);
        }
    }

    // Phase 2+3: 串行 stitch + 折叠 + 定点→浮点（块从右到左，lane0 输出即下一块 incoming）
    let mut r = temp[row + border + w];
    unsafe {
        for b in (0..nb).rev() {
            let j = base + 16 * b;
            let local = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let vr = _mm512_set1_epi32(r as i32);
            let t = _mm512_add_epi32(vr, v_ramp2);
            let wrap = _mm512_cmplt_epu32_mask(t, vr);
            let t = _mm512_mask_mov_epi32(t, wrap, v_max);
            let v = _mm512_min_epu32(local, t);
            r = _mm512_cvtsi512_si32(v) as u32;
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, v);
            let f = _mm512_mul_ps(_mm512_cvtepi32_ps(v), v_scale);
            _mm512_storeu_ps(drow.as_mut_ptr().add(j), f);
        }
    }

    // 标量尾部（最左 [0, tail) 像素，含 right 依赖）
    let mut right = temp[row + border + tail];
    for j in (0..tail).rev() {
        let idx = row + border + j;
        let mut t0 = temp[idx];
        if t0 > hv_dist {
            let mut t = temp[idx + 2 * step + 1].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + 2 * step - 1].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + step + 2].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + step + 1].wrapping_add(diag_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + step].wrapping_add(hv_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + step - 1].wrapping_add(diag_dist);
            if t < t0 {
                t0 = t;
            }
            t = temp[idx + step - 2].wrapping_add(long_dist);
            if t < t0 {
                t0 = t;
            }
            t = right.wrapping_add(hv_dist);
            if t < t0 {
                t0 = t;
            }
            temp[idx] = t0;
        }
        right = temp[idx];
    }
    // 尾部定点 → 浮点
    for j in 0..tail {
        drow[j] = temp[row + border + j] as f32 * scale;
    }
}

/// 3×3 距离变换后向扫描（AVX512，每批 16 像素）。
///
/// 向量化 bottom 行 min 计算，标量修复 `right` 依赖（对齐 OpenCV）。
/// 定点 → 浮点转换向量化。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + step) + border + w + 1`
/// - `drow` 长度 ≥ `w`
/// - CPU 需支持 AVX512F/BW/VL
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn dist_backward_3x3(
    temp: &mut [u32],
    drow: &mut [f32],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    let v_hv = _mm512_set1_epi32(hv_dist as i32);
    let v_diag = _mm512_set1_epi32(diag_dist as i32);
    let v_scale = _mm512_set1_ps(scale);
    let v_max = _mm512_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (16-k)*hv（后向，从右到左）
    let v_ramp2 = _mm512_set_epi32(
        (1 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (8 * hv_dist) as i32,
        (9 * hv_dist) as i32,
        (10 * hv_dist) as i32,
        (11 * hv_dist) as i32,
        (12 * hv_dist) as i32,
        (13 * hv_dist) as i32,
        (14 * hv_dist) as i32,
        (15 * hv_dist) as i32,
        (16 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv（避免加法回绕产生假胜出）
    let v_c1 = _mm512_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm512_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm512_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_c8 = _mm512_set1_epi32((dist_max - 8 * hv_dist) as i32);
    let v_s1 = _mm512_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm512_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm512_set1_epi32((4 * hv_dist) as i32);
    let v_s8 = _mm512_set1_epi32((8 * hv_dist) as i32);
    // 后缀扫描有效掩码：step s 有效 lane k < 16-s
    let m1 = (1u16 << 15) - 1;
    let m2 = (1u16 << 14) - 1;
    let m4 = (1u16 << 12) - 1;
    let m8 = (1u16 << 8) - 1;

    let nb = w / 16;
    let tail = w % 16;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部后缀扫描（块独立，向量化 bottom 行 min + t0）
    unsafe {
        for b in 0..nb {
            let j = base + 16 * b;
            let bottom = temp.as_ptr().add(row + step + border + j - 1);
            let bl = _mm512_loadu_si512(bottom as *const __m512i);
            let b = _mm512_loadu_si512(bottom.add(1) as *const __m512i);
            let br = _mm512_loadu_si512(bottom.add(2) as *const __m512i);
            let m = _mm512_min_epu32(
                _mm512_min_epu32(
                    _mm512_add_epi32(bl, v_diag),
                    _mm512_add_epi32(b, v_hv),
                ),
                _mm512_add_epi32(br, v_diag),
            );
            let t0 = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let mut a = _mm512_min_epu32(m, t0);
            // 后缀扫描：s=1,2,4,8
            let src = _mm512_alignr_epi32(a, a, 1);
            let src = _mm512_min_epu32(src, v_c1);
            let t = _mm512_add_epi32(src, v_s1);
            a = _mm512_mask_min_epu32(a, m1, a, t);
            let src = _mm512_alignr_epi32(a, a, 2);
            let src = _mm512_min_epu32(src, v_c2);
            let t = _mm512_add_epi32(src, v_s2);
            a = _mm512_mask_min_epu32(a, m2, a, t);
            let src = _mm512_alignr_epi32(a, a, 4);
            let src = _mm512_min_epu32(src, v_c4);
            let t = _mm512_add_epi32(src, v_s4);
            a = _mm512_mask_min_epu32(a, m4, a, t);
            let src = _mm512_alignr_epi32(a, a, 8);
            let src = _mm512_min_epu32(src, v_c8);
            let t = _mm512_add_epi32(src, v_s8);
            a = _mm512_mask_min_epu32(a, m8, a, t);
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, a);
        }
    }

    // Phase 2+3: 串行 stitch + 折叠 + 定点→浮点（块从右到左，lane0 输出即下一块 incoming）
    let mut r = temp[row + border + w];
    unsafe {
        for b in (0..nb).rev() {
            let j = base + 16 * b;
            let local = _mm512_loadu_si512(temp.as_ptr().add(row + border + j) as *const __m512i);
            let vr = _mm512_set1_epi32(r as i32);
            let t = _mm512_add_epi32(vr, v_ramp2);
            let wrap = _mm512_cmplt_epu32_mask(t, vr);
            let t = _mm512_mask_mov_epi32(t, wrap, v_max);
            let v = _mm512_min_epu32(local, t);
            r = _mm512_cvtsi512_si32(v) as u32;
            _mm512_storeu_si512(temp.as_mut_ptr().add(row + border + j) as *mut __m512i, v);
            let f = _mm512_mul_ps(_mm512_cvtepi32_ps(v), v_scale);
            _mm512_storeu_ps(drow.as_mut_ptr().add(j), f);
        }
    }

    // 标量尾部（最左 [0, tail) 像素，含 right 依赖）
    let mut right = temp[row + border + tail];
    for j in (0..tail).rev() {
        let idx = row + border + j;
        let t0 = temp[idx];
        if t0 > hv_dist {
            let b1 = temp[idx + step + 1].wrapping_add(diag_dist);
            let b2 = temp[idx + step].wrapping_add(hv_dist);
            let b3 = temp[idx + step - 1].wrapping_add(diag_dist);
            let r = right.wrapping_add(hv_dist);
            let mut t = b1.min(b2);
            t = t.min(b3);
            t = t.min(r);
            if t < t0 {
                temp[idx] = t;
            }
        }
        right = temp[idx];
    }
    // 尾部定点 → 浮点
    for j in 0..tail {
        drow[j] = temp[row + border + j] as f32 * scale;
    }
}

// ==================== cvtColor（RGB2YCrCb / YCrCb2RGB） ====================

/// RGB u8 → YCrCb u8（AVX512，每批 64 像素）。
///
/// 对齐 OpenCV `RGB2YCrCb_i<uchar>` SIMD 路径。公式（RGB 输入，blueIdx=0）：
///   y  = (R*4899 + G*9617 + B*1868 + 8192) >> 14
///   cr = ((R - y)*11682 + 2105344) >> 14
///   cb = ((B - y)*9241  + 2105344) >> 14
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn rgb_to_ycrcb(src: &[u8], dst: &mut [u8], n: usize) {
    // SAFETY: 循环条件 i+64 <= n 保证读写界内；AVX512 由调用方保证
    unsafe {
        let mut i = 0;
        while i + 64 <= n {
            // 两个 32 像素块解交织（复用 AVX2）
            let (r0, g0, b0) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i),
            );
            let (r1, g1, b1) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 96) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 128) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 160) as *const __m256i),
            );
            let (y0, cr0, cb0) = rgb2ycrcb_64(b0, g0, r0);
            let (y1, cr1, cb1) = rgb2ycrcb_64(b1, g1, r1);
            // 交织存储 YCrCb（两个 32 像素块）
            let o0 = avx2::interleave3(y0, cr0, cb0);
            let o1 = avx2::interleave3(y1, cr1, cb1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, o0.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, o0.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, o0.2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 96) as *mut __m256i, o1.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 128) as *mut __m256i, o1.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 160) as *mut __m256i, o1.2);
            i += 64;
        }
        // 剩余用 AVX2
        avx2::rgb_to_ycrcb(&src[i * 3..], &mut dst[i * 3..], n - i);
    }
}

/// 32×u8 通道 → YCrCb（AVX512，`_mm512_madd_epi16` 点积）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn rgb2ycrcb_64(b: __m256i, g: __m256i, r: __m256i) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        // 零扩展 u8 → u16（32 个）
        let b0 = _mm512_cvtepu8_epi16(b);
        let g0 = _mm512_cvtepu8_epi16(g);
        let r0 = _mm512_cvtepu8_epi16(r);

        // 交织 B/G 和 R/descale
        let (bg00, bg01) = v_zip(b0, g0);
        let (rd00, rd01) = v_zip(r0, DESCALE);

        // y = (B*1868 + G*9617 + R*4899 + 8192) >> 14
        let y00 = _mm512_srli_epi32(
            _mm512_add_epi32(_mm512_madd_epi16(bg00, YC_BG2Y), _mm512_madd_epi16(rd00, YC_R12Y)),
            14,
        );
        let y01 = _mm512_srli_epi32(
            _mm512_add_epi32(_mm512_madd_epi16(bg01, YC_BG2Y), _mm512_madd_epi16(rd01, YC_R12Y)),
            14,
        );
        let y0 = pack_u32_to_u16(y00, y01);

        // (r - y) 和 (b - y)，i16
        let sr0 = _mm512_sub_epi16(r0, y0);
        let sb0 = _mm512_sub_epi16(b0, y0);

        // cr = ((r-y)*11682 + 2105344) >> 14
        let (rd00, rd01) = v_zip(sr0, DESCALE);
        let cr00 = _mm512_srli_epi32(_mm512_madd_epi16(rd00, YC_C3H), 14);
        let cr01 = _mm512_srli_epi32(_mm512_madd_epi16(rd01, YC_C3H), 14);
        let cr0 = pack_u32_to_u16(cr00, cr01);

        // cb = ((b-y)*9241 + 2105344) >> 14
        let (bd00, bd01) = v_zip(sb0, DESCALE);
        let cb00 = _mm512_srli_epi32(_mm512_madd_epi16(bd00, YC_C4H), 14);
        let cb01 = _mm512_srli_epi32(_mm512_madd_epi16(bd01, YC_C4H), 14);
        let cb0 = pack_u32_to_u16(cb00, cb01);

        // 饱和打包 u16 → u8
        let y = pack_u16_to_u8(y0);
        let cr = pack_u16_to_u8(cr0);
        let cb = pack_u16_to_u8(cb0);
        (y, cr, cb)
    }
}

/// YCrCb u8 → RGB u8（AVX512，每批 64 像素）。
///
/// 对齐 OpenCV `YCrCb2RGB_i<uchar>` SIMD 路径。公式（YCrCb 输入，blueIdx=0）：
///   b = Y + ((Cb-128)*29049 + 8192) >> 14
///   g = Y + ((Cb-128)*(-5636) + (Cr-128)*(-11698) + 8192) >> 14
///   r = Y + ((Cr-128)*22987 + 8192) >> 14
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn ycrcb_to_rgb(src: &[u8], dst: &mut [u8], n: usize) {
    // SAFETY: 循环条件 i+64 <= n 保证读写界内；AVX512 由调用方保证
    unsafe {
        let mut i = 0;
        while i + 64 <= n {
            let (y0, cr0, cb0) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i),
            );
            let (y1, cr1, cb1) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 96) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 128) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 160) as *const __m256i),
            );
            let (r0, g0, b0) = ycrcb2rgb_64(y0, cr0, cb0);
            let (r1, g1, b1) = ycrcb2rgb_64(y1, cr1, cb1);
            let o0 = avx2::interleave3(r0, g0, b0);
            let o1 = avx2::interleave3(r1, g1, b1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, o0.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, o0.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, o0.2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 96) as *mut __m256i, o1.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 128) as *mut __m256i, o1.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 160) as *mut __m256i, o1.2);
            i += 64;
        }
        // 剩余用 AVX2
        avx2::ycrcb_to_rgb(&src[i * 3..], &mut dst[i * 3..], n - i);
    }
}

/// 32×u8 YCrCb 通道 → RGB（AVX512）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn ycrcb2rgb_64(y: __m256i, cr: __m256i, cb: __m256i) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        // cr/cb 减 128（有符号）
        let cr = _mm256_sub_epi8(cr, DELTA8);
        let cb = _mm256_sub_epi8(cb, DELTA8);
        // 符号扩展 i8 → i16（32 个）
        let cr0 = _mm512_cvtepi8_epi16(cr);
        let cb0 = _mm512_cvtepi8_epi16(cb);

        // b = (Cb-128)*29049
        let (b00, b01) = v_mul_expand(cb0, YC_VC3);
        // g = (Cb-128)*(-5636) + (Cr-128)*(-11698)
        let (t00, t01) = v_mul_expand(cb0, YC_VC2);
        let (g00, g01) = v_mul_expand(cr0, YC_VC1);
        let g00 = _mm512_add_epi32(g00, t00);
        let g01 = _mm512_add_epi32(g01, t01);
        // r = (Cr-128)*22987
        let (r00, r01) = v_mul_expand(cr0, YC_VC0);

        // + descale >> 14
        let b00 = _mm512_srai_epi32(_mm512_add_epi32(b00, DESCALE32), 14);
        let b01 = _mm512_srai_epi32(_mm512_add_epi32(b01, DESCALE32), 14);
        let g00 = _mm512_srai_epi32(_mm512_add_epi32(g00, DESCALE32), 14);
        let g01 = _mm512_srai_epi32(_mm512_add_epi32(g01, DESCALE32), 14);
        let r00 = _mm512_srai_epi32(_mm512_add_epi32(r00, DESCALE32), 14);
        let r01 = _mm512_srai_epi32(_mm512_add_epi32(r01, DESCALE32), 14);

        // 打包 i32 → i16
        let b0 = pack_i32_to_i16(b00, b01);
        let g0 = pack_i32_to_i16(g00, g01);
        let r0 = pack_i32_to_i16(r00, r01);

        // + Y（i16）
        let y0 = _mm512_cvtepu8_epi16(y);
        let b0 = _mm512_add_epi16(b0, y0);
        let g0 = _mm512_add_epi16(g0, y0);
        let r0 = _mm512_add_epi16(r0, y0);

        // 饱和打包 i16 → u8
        let b = pack_i16_to_u8(b0);
        let g = pack_i16_to_u8(g0);
        let r = pack_i16_to_u8(r0);
        (r, g, b)
    }
}

/// `v_mul_expand`（i16 × i16 → i32 对，AVX512）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn v_mul_expand(a: __m512i, b: __m512i) -> (__m512i, __m512i) {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        let lo = _mm512_mullo_epi16(a, b);
        let hi = _mm512_mulhi_epi16(a, b);
        v_zip(lo, hi)
    }
}

/// i32 → i16 饱和打包（`v_pack`，AVX512）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_i32_to_i16(a: __m512i, b: __m512i) -> __m512i {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        let lo = _mm512_cvtepi32_epi16(a);
        let hi = _mm512_cvtepi32_epi16(b);
        _mm512_inserti64x4(_mm512_castsi256_si512(lo), hi, 1)
    }
}

/// i16 → u8 饱和打包（`v_pack_u`，AVX512）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_i16_to_u8(a: __m512i) -> __m256i {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    // _mm512_cvtepi16_epi8 是截断，需先饱和到 [0,255]
    unsafe {
        let a = _mm512_max_epi16(a, _mm512_setzero_si512());
        let a = _mm512_min_epi16(a, _mm512_set1_epi16(255));
        _mm512_cvtepi16_epi8(a)
    }
}

// ---- cvtColor 常量向量 ----
static DESCALE: __m512i = unsafe { std::mem::transmute::<[i16; 32], __m512i>([8192; 32]) };
static DESCALE32: __m512i = unsafe { std::mem::transmute::<[i32; 16], __m512i>([8192; 16]) };
static DELTA8: __m256i = unsafe { std::mem::transmute::<[i8; 32], __m256i>([-128; 32]) };
// bg2y = [B2Y, G2Y] 交织 = [1868, 9617]
static YC_BG2Y: __m512i = unsafe {
    std::mem::transmute::<[i16; 32], __m512i>([
        1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868,
        9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617,
        1868, 9617,
    ])
};
// r12y = [R2Y, 1] 交织 = [4899, 1]
static YC_R12Y: __m512i = unsafe {
    std::mem::transmute::<[i16; 32], __m512i>([
        4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1,
        4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1,
    ])
};
// c3h = [YCRI, h21] = [11682, 257]
static YC_C3H: __m512i = unsafe {
    std::mem::transmute::<[i16; 32], __m512i>([
        11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682,
        257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257,
        11682, 257,
    ])
};
// c4h = [YCBI, h21] = [9241, 257]
static YC_C4H: __m512i = unsafe {
    std::mem::transmute::<[i16; 32], __m512i>([
        9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257,
        9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257,
    ])
};
// YCrCb2RGB 系数
static YC_VC0: __m512i = unsafe { std::mem::transmute::<[i16; 32], __m512i>([22987; 32]) };
static YC_VC1: __m512i = unsafe { std::mem::transmute::<[i16; 32], __m512i>([-11698; 32]) };
static YC_VC2: __m512i = unsafe { std::mem::transmute::<[i16; 32], __m512i>([-5636; 32]) };
static YC_VC3: __m512i = unsafe { std::mem::transmute::<[i16; 32], __m512i>([29049; 32]) };

// ==================== cvtColor（RGB2HSV） ====================

/// RGB u8 → HSV u8（AVX512，每批 64 像素）。
///
/// 对齐 OpenCV `RGB2HSV_b` SIMD 路径（color_hsv.simd.hpp:88-232）。
/// 整数定点（HSV_SHIFT=12），用 gather 查找表 sdiv/hdiv。
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn rgb_to_hsv(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    // SAFETY: 循环条件 i+64 <= n 保证读写界内；AVX512 由调用方保证
    unsafe {
        let sdiv = crate::imgproc::cvt_color::hsv_sdiv_table();
        let hdiv = crate::imgproc::cvt_color::hsv_hdiv_table(hrange);
        let mut i = 0;
        while i + 64 <= n {
            // 两个 32 像素块解交织（复用 AVX2）
            let (r0, g0, b0) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i),
            );
            let (r1, g1, b1) = avx2::deinterleave3(
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 96) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 128) as *const __m256i),
                _mm256_loadu_si256(src.as_ptr().add(i * 3 + 160) as *const __m256i),
            );
            let (h0, s0, v0) = rgb2hsv_32(r0, g0, b0, &sdiv, &hdiv, hrange);
            let (h1, s1, v1) = rgb2hsv_32(r1, g1, b1, &sdiv, &hdiv, hrange);
            let o0 = avx2::interleave3(h0, s0, v0);
            let o1 = avx2::interleave3(h1, s1, v1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, o0.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, o0.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, o0.2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 96) as *mut __m256i, o1.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 128) as *mut __m256i, o1.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 160) as *mut __m256i, o1.2);
            i += 64;
        }
        // 剩余用 AVX2
        avx2::rgb_to_hsv(&src[i * 3..], &mut dst[i * 3..], n - i, hrange);
    }
}

/// 32×u8 RGB 通道 → HSV（AVX512，gather 查找表 + 整数定点）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn rgb2hsv_32(
    r: __m256i,
    g: __m256i,
    b: __m256i,
    sdiv: &[i32; 256],
    hdiv: &[i32; 256],
    hrange: i32,
) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        // v = max(r, g, b), vmin = min(r, g, b)
        let v = _mm256_max_epu8(r, _mm256_max_epu8(g, b));
        let vmin = _mm256_min_epu8(r, _mm256_min_epu8(g, b));
        let diff = _mm256_sub_epi8(v, vmin);

        // vr = (v == r) ? 0xFF : 0, vg = (v == g) ? 0xFF : 0
        let vr = _mm256_cmpeq_epi8(v, r);
        let vg = _mm256_cmpeq_epi8(v, g);

        // 展开 v 到 i32（分低/高 16 像素）
        let v_lo = _mm512_cvtepu8_epi32(_mm256_castsi256_si128(v));
        let v_hi = _mm512_cvtepu8_epi32(_mm256_extracti128_si256(v, 1));
        // gather sdiv[v]
        let sdiv_lo = _mm512_i32gather_epi32(v_lo, sdiv.as_ptr(), 4);
        let sdiv_hi = _mm512_i32gather_epi32(v_hi, sdiv.as_ptr(), 4);

        // 展开 diff 到 i32
        let diff_lo = _mm512_cvtepu8_epi32(_mm256_castsi256_si128(diff));
        let diff_hi = _mm512_cvtepu8_epi32(_mm256_extracti128_si256(diff, 1));
        // gather hdiv[diff]
        let hdiv_lo = _mm512_i32gather_epi32(diff_lo, hdiv.as_ptr(), 4);
        let hdiv_hi = _mm512_i32gather_epi32(diff_hi, hdiv.as_ptr(), 4);

        // s = (diff * sdiv[v] + 2048) >> 12
        let descale = _mm512_set1_epi32(2048);
        let sq_lo = _mm512_srai_epi32(_mm512_add_epi32(_mm512_mullo_epi32(diff_lo, sdiv_lo), descale), 12);
        let sq_hi = _mm512_srai_epi32(_mm512_add_epi32(_mm512_mullo_epi32(diff_hi, sdiv_hi), descale), 12);
        let s = pack_i32_to_u8(sq_lo, sq_hi);

        // 展开 r, g, b, diff 到 i16（32 个）
        let r0 = _mm512_cvtepu8_epi16(r);
        let g0 = _mm512_cvtepu8_epi16(g);
        let b0 = _mm512_cvtepu8_epi16(b);
        let diff0 = _mm512_cvtepu8_epi16(diff);

        // vr/vg 符号扩展 i8 → i16（32 个）
        let vr0 = _mm512_cvtepi8_epi16(vr);
        let vg0 = _mm512_cvtepi8_epi16(vg);

        // h = (vr & (g - b)) + (~vr & ((vg & (b - r + 2*diff)) + ((~vg) & (r - g + 4*diff))))
        let hd = h_before_div(r0, g0, b0, diff0, vr0, vg0);

        // 展开 hd 到 i32（分低/高 16 个）
        let hq_lo = _mm512_cvtepi16_epi32(_mm512_castsi512_si256(hd));
        let hq_hi = _mm512_cvtepi16_epi32(_mm512_extracti64x4_epi64(hd, 1));

        // h = (h * hdiv[diff] + 2048) >> 12
        let hq_lo = _mm512_srai_epi32(_mm512_add_epi32(_mm512_mullo_epi32(hq_lo, hdiv_lo), descale), 12);
        let hq_hi = _mm512_srai_epi32(_mm512_add_epi32(_mm512_mullo_epi32(hq_hi, hdiv_hi), descale), 12);

        // h = h < 0 ? h + hrange : h
        let vhr = _mm512_set1_epi32(hrange);
        let hq_lo = _mm512_add_epi32(hq_lo, _mm512_maskz_mov_epi32(_mm512_cmpgt_epi32_mask(_mm512_setzero_si512(), hq_lo), vhr));
        let hq_hi = _mm512_add_epi32(hq_hi, _mm512_maskz_mov_epi32(_mm512_cmpgt_epi32_mask(_mm512_setzero_si512(), hq_hi), vhr));

        let h = pack_i32_to_u8(hq_lo, hq_hi);

        (h, s, v)
    }
}

/// h 除法前的计算（i16，AVX512）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn h_before_div(
    r: __m512i,
    g: __m512i,
    b: __m512i,
    diff: __m512i,
    vr: __m512i,
    vg: __m512i,
) -> __m512i {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        let gb = _mm512_sub_epi16(g, b);
        let br = _mm512_add_epi16(_mm512_sub_epi16(b, r), _mm512_slli_epi16(diff, 1));
        let rg = _mm512_add_epi16(_mm512_sub_epi16(r, g), _mm512_slli_epi16(diff, 2));
        let not_vr = _mm512_xor_si512(vr, _mm512_set1_epi16(-1));
        let not_vg = _mm512_xor_si512(vg, _mm512_set1_epi16(-1));
        let inner = _mm512_add_epi16(
            _mm512_and_si512(vg, br),
            _mm512_and_si512(not_vg, rg),
        );
        _mm512_add_epi16(
            _mm512_and_si512(vr, gb),
            _mm512_and_si512(not_vr, inner),
        )
    }
}

/// i32 → u8 饱和打包（AVX512，`v_pack_u`）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_i32_to_u8(a: __m512i, b: __m512i) -> __m256i {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        let a = _mm512_max_epi32(a, _mm512_setzero_si512());
        let a = _mm512_min_epi32(a, _mm512_set1_epi32(255));
        let b = _mm512_max_epi32(b, _mm512_setzero_si512());
        let b = _mm512_min_epi32(b, _mm512_set1_epi32(255));
        let lo = _mm512_cvtepi32_epi8(a);
        let hi = _mm512_cvtepi32_epi8(b);
        _mm256_inserti128_si256(_mm256_castsi128_si256(lo), hi, 1)
    }
}

/// 4×i32 → u8 饱和打包（AVX512，`v_pack_u`）。
#[inline]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
unsafe fn pack_i32_to_u8_4(a: __m512i, b: __m512i, c: __m512i, d: __m512i) -> __m256i {
    // SAFETY: 调用方保证 AVX512；纯向量运算
    unsafe {
        let a = _mm512_max_epi32(a, _mm512_setzero_si512());
        let a = _mm512_min_epi32(a, _mm512_set1_epi32(255));
        let b = _mm512_max_epi32(b, _mm512_setzero_si512());
        let b = _mm512_min_epi32(b, _mm512_set1_epi32(255));
        let c = _mm512_max_epi32(c, _mm512_setzero_si512());
        let c = _mm512_min_epi32(c, _mm512_set1_epi32(255));
        let d = _mm512_max_epi32(d, _mm512_setzero_si512());
        let d = _mm512_min_epi32(d, _mm512_set1_epi32(255));
        let lo = _mm512_cvtepi32_epi8(a);
        let hi = _mm512_cvtepi32_epi8(b);
        let lo2 = _mm512_cvtepi32_epi8(c);
        let hi2 = _mm512_cvtepi32_epi8(d);
        let lo = _mm256_inserti128_si256(_mm256_castsi128_si256(lo), hi, 1);
        let hi = _mm256_inserti128_si256(_mm256_castsi128_si256(lo2), hi2, 1);
        _mm256_inserti128_si256(lo, _mm256_castsi256_si128(hi), 1)
    }
}

// ==================== 直方图（histogram） ====================

/// 8U 直方图（AVX512，查找表 + 局部累加，与 OpenCV `calcHist_8u` 一致）。
///
/// `src` 为灰度像素，`n` 为像素数，`hist` 长度 ≥ `hist_size`，`tab` 为 256 项查找表。
///
/// # Safety
/// - `src` 长度 ≥ `n`，`hist` 长度 ≥ `hist_size`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn calc_hist_8u(src: &[u8], n: usize, hist: &mut [i32], tab: &[usize; 256]) {
    // SAFETY: 循环条件 i+8 <= n 保证读写界内；AVX512 由调用方保证
    unsafe {
        let mut mat_h = [0i32; 256];
        let mut i = 0;
        while i + 8 <= n {
            let t0 = *src.get_unchecked(i) as usize;
            let t1 = *src.get_unchecked(i + 1) as usize;
            let t2 = *src.get_unchecked(i + 2) as usize;
            let t3 = *src.get_unchecked(i + 3) as usize;
            let t4 = *src.get_unchecked(i + 4) as usize;
            let t5 = *src.get_unchecked(i + 5) as usize;
            let t6 = *src.get_unchecked(i + 6) as usize;
            let t7 = *src.get_unchecked(i + 7) as usize;
            *mat_h.get_unchecked_mut(t0) += 1;
            *mat_h.get_unchecked_mut(t1) += 1;
            *mat_h.get_unchecked_mut(t2) += 1;
            *mat_h.get_unchecked_mut(t3) += 1;
            *mat_h.get_unchecked_mut(t4) += 1;
            *mat_h.get_unchecked_mut(t5) += 1;
            *mat_h.get_unchecked_mut(t6) += 1;
            *mat_h.get_unchecked_mut(t7) += 1;
            i += 8;
        }
        while i < n {
            *mat_h.get_unchecked_mut(*src.get_unchecked(i) as usize) += 1;
            i += 1;
        }
        for (j, &c) in mat_h.iter().enumerate() {
            let hidx = *tab.get_unchecked(j);
            if hidx != usize::MAX {
                *hist.get_unchecked_mut(hidx) += c;
            }
        }
    }
}

/// 应用 LUT（AVX512，gather 查找表）。
///
/// `src` 为灰度像素，`dst` 为输出，`n` 为像素数，`lut` 为 256 项映射表。
///
/// # Safety
/// - `src` 长度 ≥ `n`，`dst` 长度 ≥ `n`
/// - 调用方需保证 CPU 支持 AVX512F/BW/VL
#[inline(never)]
#[target_feature(enable = "avx512f", enable = "avx512bw", enable = "avx512vl")]
pub unsafe fn apply_lut(src: &[u8], dst: &mut [u8], n: usize, lut: &[u8; 256]) {
    // SAFETY: 循环条件 i+64 <= n 保证读写界内；AVX512 由调用方保证
    unsafe {
        // 把 u8 LUT 展开为 i32（每项 4 字节），供 gather 使用
        let mut lut_i32 = [0i32; 256];
        for (i, &v) in lut.iter().enumerate() {
            lut_i32[i] = v as i32;
        }
        let lut_ptr = lut_i32.as_ptr();
        let mut i = 0;
        while i + 64 <= n {
            // 加载 64 个源像素（两个 32 字节块）
            let v0 = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let v1 = _mm256_loadu_si256(src.as_ptr().add(i + 32) as *const __m256i);
            // 展开到 i32（每块分低/高 16 个）
            let lo0 = _mm512_cvtepu8_epi32(_mm256_castsi256_si128(v0));
            let hi0 = _mm512_cvtepu8_epi32(_mm256_extracti128_si256(v0, 1));
            let lo1 = _mm512_cvtepu8_epi32(_mm256_castsi256_si128(v1));
            let hi1 = _mm512_cvtepu8_epi32(_mm256_extracti128_si256(v1, 1));
            // gather 4 组（scale=4，i32 元素）
            let g0 = _mm512_i32gather_epi32(lo0, lut_ptr, 4);
            let g1 = _mm512_i32gather_epi32(hi0, lut_ptr, 4);
            let g2 = _mm512_i32gather_epi32(lo1, lut_ptr, 4);
            let g3 = _mm512_i32gather_epi32(hi1, lut_ptr, 4);
            // 4×16 i32 → 64 u8
            let g0 = _mm512_max_epi32(g0, _mm512_setzero_si512());
            let g0 = _mm512_min_epi32(g0, _mm512_set1_epi32(255));
            let g1 = _mm512_max_epi32(g1, _mm512_setzero_si512());
            let g1 = _mm512_min_epi32(g1, _mm512_set1_epi32(255));
            let g2 = _mm512_max_epi32(g2, _mm512_setzero_si512());
            let g2 = _mm512_min_epi32(g2, _mm512_set1_epi32(255));
            let g3 = _mm512_max_epi32(g3, _mm512_setzero_si512());
            let g3 = _mm512_min_epi32(g3, _mm512_set1_epi32(255));
            let o0 = _mm512_cvtepi32_epi8(g0);
            let o1 = _mm512_cvtepi32_epi8(g1);
            let o2 = _mm512_cvtepi32_epi8(g2);
            let o3 = _mm512_cvtepi32_epi8(g3);
            let o0 = _mm256_inserti128_si256(_mm256_castsi128_si256(o0), o1, 1);
            let o1 = _mm256_inserti128_si256(_mm256_castsi128_si256(o2), o3, 1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, o0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 32) as *mut __m256i, o1);
            i += 64;
        }
        while i < n {
            *dst.get_unchecked_mut(i) = *lut.get_unchecked(*src.get_unchecked(i) as usize);
            i += 1;
        }
    }
}
