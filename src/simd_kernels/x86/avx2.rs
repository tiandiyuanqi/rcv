//! AVX2 手写内核（主力路径）。
//!
//! 参考 OpenCV5 `color_rgb.simd.hpp` 的 `RGB2Gray<uchar>`（660-748 行）。
//! 每批处理 32 像素，用 `_mm256_madd_epi16` 做整数点积。
//!
//! 性能要点：所有常量向量提升为 `static`（.rodata，RIP 相对寻址），
//! 避免循环内从栈重新物化（LLVM 寄存器压力导致的溢出）。

use std::arch::x86_64::*;

// ---- 常量向量（.rodata，编译期物化）----
// SAFETY: transmute 从 const 字节数组构造 __m256i，位模式确定，无运行时初始化。
static M0: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1,
        0, 0, -1, 0, 0,
    ])
};
static M1: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0,
        0, -1, 0, 0, -1,
    ])
};
static SH_B: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        0, 3, 6, 9, 12, 15, 2, 5, 8, 11, 14, 1, 4, 7, 10, 13, 0, 3, 6, 9, 12, 15, 2, 5, 8, 11, 14,
        1, 4, 7, 10, 13,
    ])
};
static SH_G: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        1, 4, 7, 10, 13, 0, 3, 6, 9, 12, 15, 2, 5, 8, 11, 14, 1, 4, 7, 10, 13, 0, 3, 6, 9, 12, 15,
        2, 5, 8, 11, 14,
    ])
};
static SH_R: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        2, 5, 8, 11, 14, 1, 4, 7, 10, 13, 0, 3, 6, 9, 12, 15, 2, 5, 8, 11, 14, 1, 4, 7, 10, 13, 0,
        3, 6, 9, 12, 15,
    ])
};
// 系数（RGB 输入，blueIdx=2，不 swap）：cb=3735(B), cg=19235(G), cr=9798(R)
// gray = b*cb + g*cg + r*cr = B*3735 + G*19235 + R*9798
static BG2Y: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235, 3735, 19235,
        3735, 19235,
    ])
};
static R12Y: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1, 9798, 1,
    ])
};
static DELTA: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([16384; 16]) };
static MAX_U32: __m256i = unsafe { std::mem::transmute::<[i32; 8], __m256i>([65535; 8]) };
static MAX_U16: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([255; 16]) };
// 无符号比较 bias（intrin_avx.hpp:916：u8 转 i8 比较 xor 0x80）
static BIAS: __m256i = unsafe { std::mem::transmute::<[i8; 32], __m256i>([-128; 32]) };

/// RGB u8 → 灰度 u8（AVX2，每批 32 像素）。
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn to_grayscale_u8(src: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证 src 读 [i*3, i*3+96) 不越界（src.len() >= n*3），dst 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let a = _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i);
            let (r, g, bl) = deinterleave3(a, b, c);
            let gray = gray_from_rgb(r, g, bl);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, gray);
            i += 32;
        }
    }
    // 标量尾循环
    for j in i..n {
        let r = src[j * 3] as u32;
        let g = src[j * 3 + 1] as u32;
        let b = src[j * 3 + 2] as u32;
        dst[j] = ((r * 9798 + g * 19235 + b * 3735 + 16384) >> 15) as u8;
    }
}

/// 解交织 3 通道（OpenCV `v_load_deinterleave`，intrin_avx.hpp:2421）。
///
/// 输入 `a,b,c` 为 96 字节 RGB 交错（32 像素），输出 3 个 32×u8 通道向量。
#[inline]
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn deinterleave3(
    a: __m256i,
    b: __m256i,
    c: __m256i,
) -> (__m256i, __m256i, __m256i) {
    let s02_low = _mm256_permute2x128_si256(a, c, 0x20); // 0 + 2*16
    let s02_high = _mm256_permute2x128_si256(a, c, 0x31); // 1 + 3*16

    let b0 = _mm256_blendv_epi8(_mm256_blendv_epi8(s02_low, s02_high, M0), b, M1);
    let g0 = _mm256_blendv_epi8(_mm256_blendv_epi8(s02_high, s02_low, M1), b, M0);
    let r0 = _mm256_blendv_epi8(_mm256_blendv_epi8(b, s02_low, M0), s02_high, M1);

    let b0 = _mm256_shuffle_epi8(b0, SH_B);
    let g0 = _mm256_shuffle_epi8(g0, SH_G);
    let r0 = _mm256_shuffle_epi8(r0, SH_R);
    (b0, g0, r0)
}

/// 从 RGB 通道向量计算灰度（OpenCV `RGB2Gray<uchar>` 主循环）。
///
/// 输入 `r,g,b` 为 32×u8 通道向量，输出 32×u8 灰度。
/// 公式：`gray = (R·9798 + G·19235 + B·3735 + 16384) >> 15`。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn gray_from_rgb(r: __m256i, g: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 调用方（avx2 主函数）已保证 AVX2 可用；所有操作为纯向量运算
    unsafe {
        // 零扩展 u8 → u16（低/高 128 位）
        let r0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(r));
        let r1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(r, 1));
        let g0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(g));
        let g1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(g, 1));
        let b0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(b));
        let b1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(b, 1));

        // v_zip 交织 B/G 和 R/delta
        let (bg00, bg01) = v_zip(b0, g0);
        let (bg10, bg11) = v_zip(b1, g1);
        let (rd00, rd01) = v_zip(r0, DELTA);
        let (rd10, rd11) = v_zip(r1, DELTA);

        // 点积 + 加偏置 + 右移 15
        let y00 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg00, BG2Y), _mm256_madd_epi16(rd00, R12Y)),
            15,
        );
        let y01 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg01, BG2Y), _mm256_madd_epi16(rd01, R12Y)),
            15,
        );
        let y10 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg10, BG2Y), _mm256_madd_epi16(rd10, R12Y)),
            15,
        );
        let y11 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg11, BG2Y), _mm256_madd_epi16(rd11, R12Y)),
            15,
        );

        // 打包 u32 → u16 → u8
        let y0 = pack_u32_to_u16(y00, y01);
        let y1 = pack_u32_to_u16(y10, y11);
        pack_u16_to_u8(y0, y1)
    }
}

/// `v_zip`（intrin_avx.hpp:517）：跨 128 位 lane 修正后交织。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn v_zip(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
    let v0 = _mm256_permute4x64_epi64(a, 0xD8); // _MM_SHUFFLE(3,1,2,0)
    let v1 = _mm256_permute4x64_epi64(b, 0xD8);
    let lo = _mm256_unpacklo_epi16(v0, v1);
    let hi = _mm256_unpackhi_epi16(v0, v1);
    (lo, hi)
}

/// `v_pack`（u32→u16）：饱和到 65535 后打包，修正 lane 顺序。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_u32_to_u16(a: __m256i, b: __m256i) -> __m256i {
    let a = _mm256_min_epu32(a, MAX_U32);
    let b = _mm256_min_epu32(b, MAX_U32);
    let packed = _mm256_packus_epi32(a, b);
    _mm256_permute4x64_epi64(packed, 0xD8)
}

/// u16 → u8 截断打包（`as u8` 语义，非饱和；box_col_step 专用：
/// OpenCV `(uchar)((s + dd) * ds >> 23)` 是截断）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn trunc_pack_u16_to_u8(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let mask = _mm256_set1_epi16(0xFF);
    let packed = _mm256_packus_epi16(_mm256_and_si256(a, mask), _mm256_and_si256(b, mask));
    _mm256_permute4x64_epi64(packed, 0xD8)
}

/// `v_pack`（u16→u8）：饱和到 255 并打包，修正 lane 顺序。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_u16_to_u8(a: __m256i, b: __m256i) -> __m256i {
    let a = _mm256_min_epu16(a, MAX_U16);
    let b = _mm256_min_epu16(b, MAX_U16);
    let packed = _mm256_packus_epi16(a, b);
    _mm256_permute4x64_epi64(packed, 0xD8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avx2_matches_scalar() {
        let n = 100;
        let mut src = vec![0u8; n * 3];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 7 % 256) as u8;
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
        assert_eq!(dst_scalar, dst_simd, "AVX2 mismatch");
    }

    #[test]
    fn avx2_rgb_to_ycrcb_matches_scalar() {
        let n = 100;
        let mut src = vec![0u8; n * 3];
        for i in 0..n {
            src[i * 3] = (i * 7 % 256) as u8;
            src[i * 3 + 1] = (i * 13 % 256) as u8;
            src[i * 3 + 2] = (i * 29 % 256) as u8;
        }
        let mut dst_scalar = vec![0u8; n * 3];
        let mut dst_simd = vec![0u8; n * 3];
        crate::simd_kernels::cvt_color::scalar_rgb_to_ycrcb(&src, &mut dst_scalar, n);
        unsafe { rgb_to_ycrcb(&src, &mut dst_simd, n) };
        assert_eq!(dst_scalar, dst_simd, "AVX2 rgb_to_ycrcb mismatch");
    }

    #[test]
    fn avx2_ycrcb_to_rgb_matches_scalar() {
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
        assert_eq!(dst_scalar, dst_simd, "AVX2 ycrcb_to_rgb mismatch");
    }
}

/// u8 阈值化（AVX2，每批 32 元素）。
///
/// 参考 OpenCV thresh.cpp:251-330 的 SIMD 主循环：
/// - Binary:    `v > t ? mv : 0`   → `v_lt(thresh_u, v) & maxval`（无符号）
/// - BinaryInv: `v <= t ? mv : 0`  → `v_le(v, thresh_u) & maxval`
/// - Trunc:     `min(v, t)`        → `v_min(v, thresh_u)`
/// - ToZero:    `v > t ? v : 0`    → `v_lt(thresh_u, v) & v`
/// - ToZeroInv: `v <= t ? v : 0`   → `v_le(v, thresh_u) & v`
///
/// 无符号比较用 bias 技巧（intrin_avx.hpp:914-920）：`xor 0x80` 后有符号 `cmpgt`。
/// `type_`: 0=Binary 1=BinaryInv 2=Trunc 3=ToZero 4=ToZeroInv
///
/// # Safety
/// - `src`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn threshold_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8, type_: u8) {
    let thresh_u = _mm256_set1_epi8(t as i8);
    let maxval16 = _mm256_set1_epi8(mv as i8);
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let v0 = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let r = match type_ {
                // Binary: v > t → cmpgt(v^bias, t^bias) & mv
                0 => _mm256_and_si256(ugt_epi8(v0, thresh_u), maxval16),
                // BinaryInv: v <= t → !(v > t) & mv
                1 => {
                    let gt = ugt_epi8(v0, thresh_u);
                    _mm256_andnot_si256(gt, maxval16)
                }
                // Trunc: min(v, t)
                2 => _mm256_min_epu8(v0, thresh_u),
                // ToZero: v > t ? v : 0
                3 => _mm256_and_si256(ugt_epi8(v0, thresh_u), v0),
                // ToZeroInv: v <= t ? v : 0
                4 => {
                    let gt = ugt_epi8(v0, thresh_u);
                    _mm256_andnot_si256(gt, v0)
                }
                _ => v0,
            };
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
    }
    // 尾循环：查找表
    if i < n {
        crate::simd_kernels::scalar::threshold_u8(&src[i..], &mut dst[i..], n - i, t, mv, type_);
    }
}

/// 二值阈值快路径（AVX2，每批 32 字节，streaming store）。
///
/// `dst = src > t ? mv : 0`。与 avx512::threshold_binary_u8 同构，
/// 用 `_mm256_stream_si256` 非临时写（大缓冲时避免污染 cache）。
///
/// # Safety
/// `n <= src.len() == dst.len()`；CPU 须支持 AVX2（由 isa() 派发保证）。
#[target_feature(enable = "avx2")]
pub unsafe fn threshold_binary_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8) {
    let thresh_u = _mm256_set1_epi8(t as i8);
    let maxval16 = _mm256_set1_epi8(mv as i8);
    let dst_ptr = dst.as_mut_ptr() as usize;
    let head = ((32 - (dst_ptr & 31)) & 31).min(n);
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i < head {
            *dst.get_unchecked_mut(i) = if *src.get_unchecked(i) > t { mv } else { 0 };
            i += 1;
        }
        while i + 128 <= n {
            let v0 = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let v1 = _mm256_loadu_si256(src.as_ptr().add(i + 32) as *const __m256i);
            let v2 = _mm256_loadu_si256(src.as_ptr().add(i + 64) as *const __m256i);
            let v3 = _mm256_loadu_si256(src.as_ptr().add(i + 96) as *const __m256i);
            let r0 = _mm256_and_si256(ugt_epi8(v0, thresh_u), maxval16);
            let r1 = _mm256_and_si256(ugt_epi8(v1, thresh_u), maxval16);
            let r2 = _mm256_and_si256(ugt_epi8(v2, thresh_u), maxval16);
            let r3 = _mm256_and_si256(ugt_epi8(v3, thresh_u), maxval16);
            _mm256_stream_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r0);
            _mm256_stream_si256(dst.as_mut_ptr().add(i + 32) as *mut __m256i, r1);
            _mm256_stream_si256(dst.as_mut_ptr().add(i + 64) as *mut __m256i, r2);
            _mm256_stream_si256(dst.as_mut_ptr().add(i + 96) as *mut __m256i, r3);
            i += 128;
        }
        while i + 64 <= n {
            let v0 = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let v1 = _mm256_loadu_si256(src.as_ptr().add(i + 32) as *const __m256i);
            let r0 = _mm256_and_si256(ugt_epi8(v0, thresh_u), maxval16);
            let r1 = _mm256_and_si256(ugt_epi8(v1, thresh_u), maxval16);
            _mm256_stream_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r0);
            _mm256_stream_si256(dst.as_mut_ptr().add(i + 32) as *mut __m256i, r1);
            i += 64;
        }
        while i + 32 <= n {
            let v0 = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let r0 = _mm256_and_si256(ugt_epi8(v0, thresh_u), maxval16);
            _mm256_stream_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r0);
            i += 32;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::threshold_u8(&src[i..], &mut dst[i..], n - i, t, mv, 0);
    }
}

/// 无符号 u8 比较 `a > b`（bias 技巧，intrin_avx.hpp:914-920）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn ugt_epi8(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    _mm256_cmpgt_epi8(_mm256_xor_si256(a, BIAS), _mm256_xor_si256(b, BIAS))
}

/// 自适应阈值化（AVX2，每批 32 像素）。
///
/// `d = src - mean`（i16，∈ [-255,255]）；`d > -idelta` 用 bias 技巧
/// （`d ^ 0x8000` 后有符号比较）实现无符号语义下的有符号比较。
///
/// # Safety
/// - `src`/`mean`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    let thr_v = _mm256_set1_epi16(thr);
    let imax_v = _mm256_set1_epi8(imax as i8);
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let s = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let m = _mm256_loadu_si256(mean.as_ptr().add(i) as *const __m256i);
            // u8 → u16 差：sub_sat 无符号饱和会截断负差，必须走 i16
            let s_lo = _mm256_unpacklo_epi8(s, _mm256_setzero_si256());
            let s_hi = _mm256_unpackhi_epi8(s, _mm256_setzero_si256());
            let m_lo = _mm256_unpacklo_epi8(m, _mm256_setzero_si256());
            let m_hi = _mm256_unpackhi_epi8(m, _mm256_setzero_si256());
            let d_lo = _mm256_sub_epi16(s_lo, m_lo);
            let d_hi = _mm256_sub_epi16(s_hi, m_hi);
            // 有符号比较 d > thr：直接 cmpgt_epi16（d、thr 均为有符号 i16）
            let gt_lo = _mm256_cmpgt_epi16(d_lo, thr_v);
            let gt_hi = _mm256_cmpgt_epi16(d_hi, thr_v);
            // i16 掩码 → u8 掩码（每字节复制符号扩展）
            let gt8 = _mm256_packs_epi16(gt_lo, gt_hi); // 0xFF/0x00
            let r = if inv {
                // BinaryInv: d <= thr → imax
                _mm256_andnot_si256(gt8, imax_v)
            } else {
                // Binary: d > thr → imax
                _mm256_and_si256(gt8, imax_v)
            };
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::adaptive_thresh_u8(
            &src[i..],
            &mean[i..],
            &mut dst[i..],
            n - i,
            imax,
            idelta,
            inv,
        );
    }
}

/// box_filter 行阶段：u8 → u16 滑动窗行和（AVX2）。
///
/// 对齐 OpenCV `RowSum<uchar, ushort>`（box_filter.simd.hpp:68-176）。
/// - cn=1：kw=3/5 直接求和（手写 SIMD），通用 kw 滑动窗（标量）
/// - cn>1：直接求和/滑动窗标量（`target_feature(avx2)` 下编译，
///   LLVM 对 strided 直接求和自动向量化，等价 gcc 对 OpenCV 标量的处理）
///
/// # Safety
/// - `src` 长度 ≥ `(w + kw - 1)*cn`，`dst` 长度 ≥ `w*cn`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn box_row_sum_u8(src: &[u8], dst: &mut [u16], w: usize, kw: usize, cn: usize) {
    match (cn, kw) {
        (1, 3) => unsafe { row_sum_k3(src, dst, w) },
        (1, 5) => unsafe { row_sum_k5(src, dst, w) },
        _ => {
            // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
            unsafe { row_sum_auto(src, dst, w, kw, cn) }
        }
    }
}

/// 通用行和（标量源码，AVX2 特权编译，LLVM 自动向量化）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn row_sum_auto(src: &[u8], dst: &mut [u16], w: usize, kw: usize, cn: usize) {
    let total = w * cn;
    if kw == 3 {
        // 直接求和（OpenCV ksize==3 特化）：D[i] = S[i] + S[i+cn] + S[i+2cn]
        for i in 0..total {
            dst[i] = src[i] as u16 + src[i + cn] as u16 + src[i + cn * 2] as u16;
        }
    } else if kw == 5 {
        for i in 0..total {
            dst[i] = src[i] as u16
                + src[i + cn] as u16
                + src[i + cn * 2] as u16
                + src[i + cn * 3] as u16
                + src[i + cn * 4] as u16;
        }
    } else {
        crate::simd_kernels::box_filter::box_row_sum_u8_cn(src, dst, w, kw, cn);
    }
}

/// kw=3：`D[i] = S[i] + S[i+1] + S[i+2]`（每批 32 元素）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn row_sum_k3(src: &[u8], dst: &mut [u16], w: usize) {
    let mut i = 0;
    // SAFETY: i+32 <= w 保证读 [i, i+34)（src.len() >= w+2）写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let a = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i + 2) as *const __m256i);
            let (s0, s1) = sum3_u8_to_u16(a, b, c);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s1);
            i += 32;
        }
    }
    for j in i..w {
        dst[j] = src[j] as u16 + src[j + 1] as u16 + src[j + 2] as u16;
    }
}

/// kw=5：`D[i] = S[i..i+5] 之和`（每批 32 元素）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn row_sum_k5(src: &[u8], dst: &mut [u16], w: usize) {
    let mut i = 0;
    // SAFETY: i+32 <= w 保证读 [i, i+36)（src.len() >= w+4）写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let a = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i + 2) as *const __m256i);
            let d = _mm256_loadu_si256(src.as_ptr().add(i + 3) as *const __m256i);
            let e = _mm256_loadu_si256(src.as_ptr().add(i + 4) as *const __m256i);
            let (s0, s1) = sum3_u8_to_u16(a, b, c);
            let (t0, t1) = sum3_u8_to_u16(d, e, _mm256_setzero_si256());
            _mm256_storeu_si256(
                dst.as_mut_ptr().add(i) as *mut __m256i,
                _mm256_add_epi16(s0, t0),
            );
            _mm256_storeu_si256(
                dst.as_mut_ptr().add(i + 16) as *mut __m256i,
                _mm256_add_epi16(s1, t1),
            );
            i += 32;
        }
    }
    for j in i..w {
        dst[j] = src[j] as u16
            + src[j + 1] as u16
            + src[j + 2] as u16
            + src[j + 3] as u16
            + src[j + 4] as u16;
    }
}

/// 3 个 u8 向量求和 → u16（零扩展后加，无饱和风险：3*255 < 65535）。
///
/// 返回 (低 16 元素, 高 16 元素) 两个向量（32 u8 输入 → 32 u16 输出）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn sum3_u8_to_u16(a: __m256i, b: __m256i, c: __m256i) -> (__m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let a0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(a));
    let a1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(a, 1));
    let b0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(b));
    let b1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(b, 1));
    let c0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(c));
    let c1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(c, 1));
    (
        _mm256_add_epi16(_mm256_add_epi16(a0, b0), c0),
        _mm256_add_epi16(_mm256_add_epi16(a1, b1), c1),
    )
}

/// box_filter 列阶段：u16 行和 → u8（AVX2，SHIFT=23 定点除法，每批 32 元素）。
///
/// 对齐 OpenCV `ColumnSum<ushort, uchar>` SIMD 主循环（box_filter.simd.hpp:515-546）：
/// `s = SUM + Sp`（wrap），`D = (s + dd) * ds >> 23`，`SUM' = s - Sm`（wrap）。
/// `have_scale=false` 时 `D = min(s, 255)`。
///
/// # Safety
/// - `sum`/`sp`/`sm`/`dst` 长度 ≥ `w`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn box_col_step_u16(
    sum: &mut [u16],
    sp: &[u16],
    sm: &[u16],
    dst: &mut [u8],
    w: usize,
    fx: crate::simd_kernels::box_filter::BoxFixed,
    have_scale: bool,
) {
    let ds4 = _mm256_set1_epi32(fx.ds as i32);
    let dd8 = _mm256_set1_epi16(fx.dd as i16);
    let mut i = 0;
    // SAFETY: i+32 <= w 保证所有读写 [i, i+32) 不越界
    unsafe {
        if have_scale {
            while i + 32 <= w {
                let sm0 = _mm256_loadu_si256(sm.as_ptr().add(i) as *const __m256i);
                let sm1 = _mm256_loadu_si256(sm.as_ptr().add(i + 16) as *const __m256i);
                let s0 = _mm256_add_epi16(
                    _mm256_loadu_si256(sum.as_ptr().add(i) as *const __m256i),
                    _mm256_loadu_si256(sp.as_ptr().add(i) as *const __m256i),
                );
                let s1 = _mm256_add_epi16(
                    _mm256_loadu_si256(sum.as_ptr().add(i + 16) as *const __m256i),
                    _mm256_loadu_si256(sp.as_ptr().add(i + 16) as *const __m256i),
                );
                // (s + dd) 扩展到 u32，乘 ds，右移 23
                let (s0e0, s0e1) = expand_u16_u32(_mm256_add_epi16(s0, dd8));
                let (s1e0, s1e1) = expand_u16_u32(_mm256_add_epi16(s1, dd8));
                let r0 = _mm256_srli_epi32(_mm256_mullo_epi32(s0e0, ds4), 23);
                let r1 = _mm256_srli_epi32(_mm256_mullo_epi32(s0e1, ds4), 23);
                let r2 = _mm256_srli_epi32(_mm256_mullo_epi32(s1e0, ds4), 23);
                let r3 = _mm256_srli_epi32(_mm256_mullo_epi32(s1e1, ds4), 23);
                // pack u32 -> u16 -> u8（截断语义：OpenCV (uchar) 转换，
                // wrap 乘法结果可 > 255，必须截断而非饱和）
                let p0 = pack_u32_to_u16(r0, r1);
                let p1 = pack_u32_to_u16(r2, r3);
                let out = trunc_pack_u16_to_u8(p0, p1);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, out);
                // SUM' = s - Sm（wrap）
                _mm256_storeu_si256(
                    sum.as_mut_ptr().add(i) as *mut __m256i,
                    _mm256_sub_epi16(s0, sm0),
                );
                _mm256_storeu_si256(
                    sum.as_mut_ptr().add(i + 16) as *mut __m256i,
                    _mm256_sub_epi16(s1, sm1),
                );
                i += 32;
            }
        } else {
            let max16 = _mm256_set1_epi16(255);
            while i + 32 <= w {
                let sm0 = _mm256_loadu_si256(sm.as_ptr().add(i) as *const __m256i);
                let sm1 = _mm256_loadu_si256(sm.as_ptr().add(i + 16) as *const __m256i);
                let s0 = _mm256_add_epi16(
                    _mm256_loadu_si256(sum.as_ptr().add(i) as *const __m256i),
                    _mm256_loadu_si256(sp.as_ptr().add(i) as *const __m256i),
                );
                let s1 = _mm256_add_epi16(
                    _mm256_loadu_si256(sum.as_ptr().add(i + 16) as *const __m256i),
                    _mm256_loadu_si256(sp.as_ptr().add(i + 16) as *const __m256i),
                );
                let out = pack_u16_to_u8(_mm256_min_epu16(s0, max16), _mm256_min_epu16(s1, max16));
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, out);
                _mm256_storeu_si256(
                    sum.as_mut_ptr().add(i) as *mut __m256i,
                    _mm256_sub_epi16(s0, sm0),
                );
                _mm256_storeu_si256(
                    sum.as_mut_ptr().add(i + 16) as *mut __m256i,
                    _mm256_sub_epi16(s1, sm1),
                );
                i += 32;
            }
        }
    }
    // 尾循环
    while i < w {
        let s0 = sum[i].wrapping_add(sp[i]);
        dst[i] = if have_scale {
            (((s0 as u32 + fx.dd as u32) * fx.ds) >> 23) as u8
        } else {
            s0.min(255) as u8
        };
        sum[i] = s0.wrapping_sub(sm[i]);
        i += 1;
    }
}

/// u16 向量零扩展为两个 u32 向量（低/高 128 位）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn expand_u16_u32(v: __m256i) -> (__m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(v));
    let hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(v, 1));
    (lo, hi)
}

#[cfg(test)]
mod thresh_tests {
    use super::*;

    #[test]
    fn avx2_adaptive_thresh_matches_scalar() {
        let n = 300; // 跨 32 边界 + 尾循环
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
                // SAFETY: 测试机已验证支持 AVX2（本模块其余测试同前提）
                unsafe { adaptive_thresh_u8(&src, &mean, &mut dst_simd, n, 200, idelta, inv) };
                assert_eq!(dst_scalar, dst_simd, "inv={inv} idelta={idelta}");
            }
        }
    }

    #[test]
    fn avx2_threshold_all_types() {
        let n = 300; // 跨 32 边界 + 尾循环
        let mut src = vec![0u8; n];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 31 % 256) as u8;
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

/// box_filter 行阶段（无边框源，AVX2 特权编译）：u8 → u16 行和，kw=3/5。
///
/// 主体直接求和（LLVM 自动向量化），首尾 `kw-1` 像素按 border 逐点特判。
///
/// # Safety
/// - `src` 长度 ≥ `w*cn`，`dst` 长度 ≥ `w*cn`，`kw` ∈ {3, 5}
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn box_row_sum_u8_direct_cn(
    src: &[u8],
    dst: &mut [u16],
    w: usize,
    kw: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = w * cn;
    let lo = (kw - 1) * cn;
    let hi = total.saturating_sub((kw - 1) * cn);
    if kw == 3 {
        for i in lo..hi {
            dst[i] = src[i - cn] as u16 + src[i] as u16 + src[i + cn] as u16;
        }
    } else {
        for i in lo..hi {
            dst[i] = src[i - 2 * cn] as u16
                + src[i - cn] as u16
                + src[i] as u16
                + src[i + cn] as u16
                + src[i + 2 * cn] as u16;
        }
    }
    // 首尾特判（标量，像素数固定 kw-1）
    // SAFETY: 纯标量循环，切片长度由调用方契约保证
    crate::simd_kernels::box_filter::row_sum_edges(src, dst, w, kw, cn, border)
}

/// u16×u16 饱和乘（对齐 OpenCV `v_mul<v_uint16>`：mullo+mulhi → u32 → packs_epu32）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn sat_mul_u16(a: __m256i, b: __m256i) -> __m256i {
    let pl = _mm256_mullo_epi16(a, b);
    let ph = _mm256_mulhi_epu16(a, b);
    let p0 = _mm256_unpacklo_epi16(pl, ph);
    let p1 = _mm256_unpackhi_epi16(pl, ph);
    _mm256_packus_epi32(p0, p1)
}

/// u16+u16 饱和加（对齐 OpenCV `v_add<v_uint16>`）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn sat_add_u16(a: __m256i, b: __m256i) -> __m256i {
    _mm256_adds_epu16(a, b)
}

/// gaussian 水平阶段（AVX2 特权编译）：u8 → u16 定点（SHIFT=8）。
///
/// 主体（无越界区）SIMD：`res = Σ sat_mul(m_j, expand(src[i+j·cn]))` 饱和累加；
/// 首尾 `n/2` 像素逐点特判（对齐 `hlineSmooth<u8,ufixedpoint16>` 边框语义）。
///
/// # Safety
/// - `src` 长度 ≥ `len*cn`，`dst` 长度 ≥ `len*cn`，`m.len()` 为奇数
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    // 主体：i ∈ [pre·cn, (len-post)·cn)（严格无越界；OpenCV 放宽到
    // (len-post+1)·cn 但最后 pre 个像素被其右边框循环覆盖，rcv 由
    // edges 特判精确计算，结果一致）
    let lo = pre * cn;
    let hi = ((len - post) * cn).min(total);
    let mut i = lo;
    if i < hi {
        // 窗口中心偏移：dst[i] = Σ_j m[j]·src[i + (j-pre)·cn]
        let base_off = pre * cn;
        while i + 16 <= hi {
            // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证；
            // load 基址 i - base_off + j·cn ≥ 0 且 ≤ hi - 1 + (n-1)·cn < total
            unsafe {
                // 16 u8 → 16 u16（cvtepu8_epi16 输入 __m128i）
                // 核系数循环内 set1（LLVM CSE 后每行仅 1 次，避免堆分配）
                let mut acc = sat_mul_u16(
                    _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(i - base_off) as *const _
                    )),
                    _mm256_set1_epi16(m[0] as i16),
                );
                for (j, &mj) in m.iter().enumerate().skip(1) {
                    let s = _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(i - base_off + j * cn) as *const _,
                    ));
                    acc = sat_add_u16(acc, sat_mul_u16(s, _mm256_set1_epi16(mj as i16)));
                }
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, acc);
            }
            i += 16;
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
    // 首尾特判（复用标量基准的逐点逻辑）
    crate::simd_kernels::gaussian::hline_smooth_u8_edges(src, dst, len, cn, m, border);
}

/// gaussian 垂直阶段（AVX2 特权编译）：kylen 行 u16 定点 → u8。
///
/// `acc(u32) = Σ m_j·s_j`（u16×u16→u32 无饱和），`dst = sat_u8((acc + 2^15) >> 16)`。
///
/// # Safety
/// - `rows.len() == m.len()`，各行与 `dst` 等长
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn vline_smooth_u8(rows: &[&[u16]], dst: &mut [u8], m: &[u16]) {
    let len = dst.len();
    let mut i = 0;
    while i + 16 <= len {
        // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
        unsafe {
            // 两个独立 acc：低 8 元素 [i, i+8)、高 8 元素 [i+8, i+16)
            let mut acc0 = _mm256_setzero_si256();
            let mut acc1 = _mm256_setzero_si256();
            for (j, &mj) in m.iter().enumerate() {
                let mv = _mm256_set1_epi16(mj as i16);
                let s = _mm256_loadu_si256(rows[j].as_ptr().add(i) as *const _);
                // u16×u16 → u32：unpacklo/hi 有 128-lane 陷阱，
                // 改用 cvtepu16_epi32 扩展后 mullo_epi32（值域内一致）
                let (s0, s1) = expand_u16_u32(s);
                let (m0, m1) = expand_u16_u32(mv);
                acc0 = _mm256_add_epi32(acc0, _mm256_mullo_epi32(s0, m0));
                acc1 = _mm256_add_epi32(acc1, _mm256_mullo_epi32(s1, m1));
            }
            // +2^15 >> 16 → u16（≤255）
            let half = _mm256_set1_epi32(1 << 15);
            acc0 = _mm256_srli_epi32(_mm256_add_epi32(acc0, half), 16);
            acc1 = _mm256_srli_epi32(_mm256_add_epi32(acc1, half), 16);
            // u32→u16 饱和压缩（纯 AVX2，避免 AVX512VL intrinsic 在
            // target_feature("avx2") 函数内被 LTO outline 成函数调用）：
            // 值域 ≤ 255 无需饱和，packus_epi32（有符号饱和）+ lane 修复
            let p = _mm256_packus_epi32(acc0, acc1); // [L0: a0..a3,b0..b3 | L1: a4..a7,b4..b7]
            let p = _mm256_permute4x64_epi64(p, 0xD8); // [a0..a7 | b0..b7]
            let b = _mm_packus_epi16(_mm256_castsi256_si128(p), _mm256_extracti128_si256(p, 1));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut _, b);
        }
        i += 16;
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

/// gaussian 水平阶段 121 核特化（AVX2）：`dst = (a + 2b + c) << 6`，16/批。
///
/// # Safety
/// - `src` 长度 ≥ `len*cn`，`dst` 长度 ≥ `len*cn`，`len >= 2`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn hline_smooth_3n121(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    // 主体 i ∈ [cn, total-cn)
    let mut i = cn;
    // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
    unsafe {
        while i + 16 <= total - cn {
            let a = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i - cn) as *const _));
            let b = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i) as *const _));
            let c = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i + cn) as *const _));
            // (a + 2b + c) << 6：u16 域 ≤ 1020<<6 = 65280 不溢出
            let sum = _mm256_add_epi16(_mm256_add_epi16(a, c), _mm256_add_epi16(b, b));
            let out = _mm256_slli_epi16(sum, 6);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 16;
        }
    }
    while i < total - cn {
        let (a, b, c) = (src[i - cn] as u16, src[i] as u16, src[i + cn] as u16);
        dst[i] = (a + 2 * b + c) << 6;
        i += 1;
    }
    // 首尾特判（标量）
    crate::simd_kernels::gaussian::hline_smooth_3n121_edges(src, dst, len, cn, border);
}

/// gaussian 垂直阶段 121 核特化（AVX2）：`(r0 + 2r1 + r2 + 512) >> 10`，16/批。
///
/// # Safety
/// - 各行与 `dst` 等长
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn vline_smooth_3n121(rows: &[&[u16]; 3], dst: &mut [u8]) {
    let len = dst.len();
    let mut i = 0;
    // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
    unsafe {
        while i + 16 <= len {
            let r0 = _mm256_loadu_si256(rows[0].as_ptr().add(i) as *const _);
            let r1 = _mm256_loadu_si256(rows[1].as_ptr().add(i) as *const _);
            let r2 = _mm256_loadu_si256(rows[2].as_ptr().add(i) as *const _);
            // u16 → u32 扩展（lane 陷阱：用 cvtepu16_epi32）
            let (r0l, r0h) = expand_u16_u32(r0);
            let (r1l, r1h) = expand_u16_u32(r1);
            let (r2l, r2h) = expand_u16_u32(r2);
            let sum_l = _mm256_add_epi32(_mm256_add_epi32(r0l, r2l), _mm256_add_epi32(r1l, r1l));
            let sum_h = _mm256_add_epi32(_mm256_add_epi32(r0h, r2h), _mm256_add_epi32(r1h, r1h));
            let bias = _mm256_set1_epi32(1 << 9);
            let v_l = _mm256_srli_epi32(_mm256_add_epi32(sum_l, bias), 10);
            let v_h = _mm256_srli_epi32(_mm256_add_epi32(sum_h, bias), 10);
            // u32 → u16 → u8（纯 AVX2：packus_epi32 + lane 修复，
            // 避免 AVX512VL intrinsic 被 LTO outline 成函数调用）
            let p = _mm256_packus_epi32(v_l, v_h); // [L0: a0..a3,b0..b3 | L1: a4..a7,b4..b7]
            let p = _mm256_permute4x64_epi64(p, 0xD8); // [a0..a7 | b0..b7]
            let b = _mm_packus_epi16(_mm256_castsi256_si128(p), _mm256_extracti128_si256(p, 1));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut _, b);
            i += 16;
        }
    }
    while i < len {
        let v = (rows[0][i] as u32 + 2 * rows[1][i] as u32 + rows[2][i] as u32 + (1 << 9)) >> 10;
        dst[i] = v.min(0xFF) as u8;
        i += 1;
    }
}

/// gaussian 水平阶段 14641 核特化（AVX2）：`(a+4b+6c+4d+e)<<4`，16/批。
///
/// # Safety
/// - `src` 长度 ≥ `len*cn`，`dst` 长度 ≥ `len*cn`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
    unsafe {
        while i + 16 <= body_end {
            let a = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i - 2 * cn) as *const _));
            let b = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i - cn) as *const _));
            let c = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i) as *const _));
            let d = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i + cn) as *const _));
            let e = _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(i + 2 * cn) as *const _));
            // a + e + 4(b+d) + 6c（u16 域 ≤ 4080，<<4 ≤ 65280 不溢出）
            let sum = _mm256_add_epi16(
                _mm256_add_epi16(a, e),
                _mm256_add_epi16(
                    _mm256_slli_epi16(_mm256_add_epi16(b, d), 2),
                    _mm256_mullo_epi16(c, _mm256_set1_epi16(6)),
                ),
            );
            let out = _mm256_slli_epi16(sum, 4);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, out);
            i += 16;
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

/// gaussian 垂直阶段 14641 核特化（AVX2）：`(6r2 + 4(r1+r3) + r0 + r4 + 2^11) >> 12`。
///
/// # Safety
/// - 各行与 `dst` 等长
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn vline_smooth_5n14641(rows: &[&[u16]; 5], dst: &mut [u8]) {
    let len = dst.len();
    let mut i = 0;
    // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
    unsafe {
        while i + 16 <= len {
            let r0 = _mm256_loadu_si256(rows[0].as_ptr().add(i) as *const _);
            let r1 = _mm256_loadu_si256(rows[1].as_ptr().add(i) as *const _);
            let r2 = _mm256_loadu_si256(rows[2].as_ptr().add(i) as *const _);
            let r3 = _mm256_loadu_si256(rows[3].as_ptr().add(i) as *const _);
            let r4 = _mm256_loadu_si256(rows[4].as_ptr().add(i) as *const _);
            let (r0l, r0h) = expand_u16_u32(r0);
            let (r1l, r1h) = expand_u16_u32(r1);
            let (r2l, r2h) = expand_u16_u32(r2);
            let (r3l, r3h) = expand_u16_u32(r3);
            let (r4l, r4h) = expand_u16_u32(r4);
            let sum_l = _mm256_add_epi32(
                _mm256_add_epi32(r0l, r4l),
                _mm256_add_epi32(
                    _mm256_slli_epi32(_mm256_add_epi32(r1l, r3l), 2),
                    _mm256_add_epi32(r2l, _mm256_slli_epi32(r2l, 2)),
                ),
            );
            let sum_h = _mm256_add_epi32(
                _mm256_add_epi32(r0h, r4h),
                _mm256_add_epi32(
                    _mm256_slli_epi32(_mm256_add_epi32(r1h, r3h), 2),
                    _mm256_add_epi32(r2h, _mm256_slli_epi32(r2h, 2)),
                ),
            );
            let bias = _mm256_set1_epi32(1 << 11);
            let v_l = _mm256_srli_epi32(_mm256_add_epi32(sum_l, bias), 12);
            let v_h = _mm256_srli_epi32(_mm256_add_epi32(sum_h, bias), 12);
            let p = _mm256_packus_epi32(v_l, v_h);
            let p = _mm256_permute4x64_epi64(p, 0xD8);
            let b = _mm_packus_epi16(_mm256_castsi256_si128(p), _mm256_extracti128_si256(p, 1));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut _, b);
            i += 16;
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

/// 14641 二维融合主体区（AVX2）：x ∈ [2,w-2)，y ∈ [2,h-2)，16/批。
///
/// 对齐 `smooth5N14641Impl<u8,ufixedpoint16>`：行和 b = 6s+4(s±cn)+s±2cn
/// （u16 域），列归约 dst = (6b2+4(b1+b3)+b0+b4+128)>>8。
///
/// # Safety
/// - `src`/`dst` 长度 ≥ w*cn*h，w ≥ 5 且 h ≥ 5
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    // SAFETY: 纯向量运算，AVX2 由本函数 target_feature 保证
    unsafe {
        for y in y_start..y_end {
            let dst_row = &mut dst[(y - y_lo) * row_w..(y - y_lo + 1) * row_w];
            let mut x = x_lo;
            while x + 16 <= x_hi {
                // 行和 b(yy) = 6s + 4(s[-cn]+s[cn]) + s[-2cn] + s[2cn]
                let bsum = |yy: usize| -> __m256i {
                    let base = yy * row_w + x;
                    let s =
                        _mm256_cvtepu8_epi16(_mm_loadu_si128(src.as_ptr().add(base) as *const _));
                    let sn = _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(base - cn) as *const _
                    ));
                    let sp = _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(base + cn) as *const _
                    ));
                    let snn = _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(base - 2 * cn) as *const _,
                    ));
                    let spp = _mm256_cvtepu8_epi16(_mm_loadu_si128(
                        src.as_ptr().add(base + 2 * cn) as *const _,
                    ));
                    let six = _mm256_set1_epi16(6);
                    _mm256_add_epi16(
                        _mm256_add_epi16(snn, spp),
                        _mm256_add_epi16(
                            _mm256_mullo_epi16(s, six),
                            _mm256_slli_epi16(_mm256_add_epi16(sn, sp), 2),
                        ),
                    )
                };
                let b0 = bsum(y - 2);
                let b1 = bsum(y - 1);
                let b2 = bsum(y);
                let b3 = bsum(y + 1);
                let b4 = bsum(y + 2);
                // 列归约：u16 → u32 扩展（lane 陷阱：cvtepu16_epi32）
                let (b0l, b0h) = expand_u16_u32(b0);
                let (b1l, b1h) = expand_u16_u32(b1);
                let (b2l, b2h) = expand_u16_u32(b2);
                let (b3l, b3h) = expand_u16_u32(b3);
                let (b4l, b4h) = expand_u16_u32(b4);
                let six32 = _mm256_set1_epi32(6);
                let sum_l = _mm256_add_epi32(
                    _mm256_add_epi32(b0l, b4l),
                    _mm256_add_epi32(
                        _mm256_slli_epi32(_mm256_add_epi32(b1l, b3l), 2),
                        _mm256_mullo_epi32(b2l, six32),
                    ),
                );
                let sum_h = _mm256_add_epi32(
                    _mm256_add_epi32(b0h, b4h),
                    _mm256_add_epi32(
                        _mm256_slli_epi32(_mm256_add_epi32(b1h, b3h), 2),
                        _mm256_mullo_epi32(b2h, six32),
                    ),
                );
                let bias = _mm256_set1_epi32(128);
                let v_l = _mm256_srli_epi32(_mm256_add_epi32(sum_l, bias), 8);
                let v_h = _mm256_srli_epi32(_mm256_add_epi32(sum_h, bias), 8);
                let p = _mm256_packus_epi32(v_l, v_h);
                let p = _mm256_permute4x64_epi64(p, 0xD8);
                let out =
                    _mm_packus_epi16(_mm256_castsi256_si128(p), _mm256_extracti128_si256(p, 1));
                _mm_storeu_si128(dst_row.as_mut_ptr().add(x) as *mut _, out);
                x += 16;
            }
            while x < x_hi {
                let bsum = |yy: usize| -> u32 {
                    let base = yy * row_w + x;
                    (src[base] as u32 * 6)
                        + ((src[base - cn] as u32 + src[base + cn] as u32) << 2)
                        + src[base - 2 * cn] as u32
                        + src[base + 2 * cn] as u32
                };
                let b0 = bsum(y - 2);
                let b1 = bsum(y - 1);
                let b2 = bsum(y);
                let b3 = bsum(y + 1);
                let b4 = bsum(y + 2);
                let v = (b2 * 6 + ((b1 + b3) << 2) + b0 + b4 + 128) >> 8;
                dst_row[x] = v.min(0xFF) as u8;
                x += 1;
            }
        }
    }
}

/// 3x3 中值滤波（AVX2，u8，Clamp 边界）。
///
/// 对齐 OpenCV `medianBlur_SortNet` m==3 向量路径：每批 32 字节，
/// 从 3 行各取左/中/右 3 个向量，用 `_mm256_min_epu8`/`_mm256_max_epu8`
/// 跑排序网络。首尾 `cn` 字节用标量处理（Clamp 边界）。
///
/// # Safety
/// - `src` 长度 ≥ `w*h*cn`，`dst` 同尺寸
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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

/// 3x3 排序网络（AVX2 向量版，OpenCV `medianBlur_Sort` m==3 向量路径）。
#[inline]
#[allow(clippy::too_many_arguments)]
#[target_feature(enable = "avx2")]
unsafe fn median3_vec(
    p0: __m256i,
    p1: __m256i,
    p2: __m256i,
    p3: __m256i,
    p4: __m256i,
    p5: __m256i,
    p6: __m256i,
    p7: __m256i,
    p8: __m256i,
) -> __m256i {
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
#[target_feature(enable = "avx2")]
unsafe fn sort2(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
    (_mm256_min_epu8(a, b), _mm256_max_epu8(a, b))
}

/// 3x3 中值滤波单行（AVX2，u8，Clamp 边界）。
///
/// 处理第 `y` 行，`y0`/`y1`/`y2` 为三行源偏移（已 Clamp），`dst_row` 为输出行。
/// 行宽按字节计（`w*cn`），窗口为字节偏移 `j-cn, j, j+cn`（对齐 OpenCV
/// `medianBlur_SortNet` 的 `size.width *= cn` 字节化处理）。
///
/// # Safety
/// - `src` 长度 ≥ `y2 + w*cn`，`dst_row` 长度 ≥ `w*cn`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    // 中间字节 SIMD：每批 32 字节。
    // 尾部回退对齐 OpenCV（median_blur.simd.hpp:697-705）：批越界时把 j 回退到
    // limit-32 重算一批（同输入同输出，幂等），使向量路径恰好覆盖到 row_bytes-cn，
    // 标量尾仅剩 cn 字节。
    let limit = row_bytes - cn;
    let mut j = cn;
    while j < limit {
        if limit - j < 32 {
            if j == cn {
                break; // 行太窄，一批都放不下，剩余全走标量
            }
            j = limit - 32; // 回退；此时 limit > cn+32，不会下溢
        }
        // SAFETY: j+32 <= limit = row_bytes-cn 保证 j-cn >= 0 且 j+32+cn <= row_bytes，读/写均在界内
        unsafe {
            let p0 = _mm256_loadu_si256(src.as_ptr().add(y0 + j - cn) as *const __m256i);
            let p1 = _mm256_loadu_si256(src.as_ptr().add(y0 + j) as *const __m256i);
            let p2 = _mm256_loadu_si256(src.as_ptr().add(y0 + j + cn) as *const __m256i);
            let p3 = _mm256_loadu_si256(src.as_ptr().add(y1 + j - cn) as *const __m256i);
            let p4 = _mm256_loadu_si256(src.as_ptr().add(y1 + j) as *const __m256i);
            let p5 = _mm256_loadu_si256(src.as_ptr().add(y1 + j + cn) as *const __m256i);
            let p6 = _mm256_loadu_si256(src.as_ptr().add(y2 + j - cn) as *const __m256i);
            let p7 = _mm256_loadu_si256(src.as_ptr().add(y2 + j) as *const __m256i);
            let p8 = _mm256_loadu_si256(src.as_ptr().add(y2 + j + cn) as *const __m256i);
            let med = median3_vec(p0, p1, p2, p3, p4, p5, p6, p7, p8);
            _mm256_storeu_si256(dst_row.as_mut_ptr().add(j) as *mut __m256i, med);
        }
        j += 32;
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

/// 8u 双边滤波单行（AVX2，cn=1：每批 32 像素 = 4×8 f32）。
///
/// 对齐 OpenCV `BilateralFilter_8u_Invoker` cn==1 SIMD 主循环：
/// absdiff → 零扩展 u8→u32 → gather color_weight → FMA 累加 →
/// `v_round(v_div)` + `v_pack_u_store`。
///
/// # Safety
/// - `padded` 长度 ≥ stride·(h+2r)，`dst_row` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    // SAFETY: j+32 <= w 保证所有 load/gather/store 在 padded/dst_row 界内
    unsafe {
        while j + 32 <= w {
            let sptr = padded.as_ptr().add(base + j);
            let v_sptr8 = _mm256_loadu_si256(sptr as *const __m256i);
            let mut wsum0 = _mm256_setzero_ps();
            let mut wsum1 = _mm256_setzero_ps();
            let mut wsum2 = _mm256_setzero_ps();
            let mut wsum3 = _mm256_setzero_ps();
            let mut sum0 = _mm256_setzero_ps();
            let mut sum1 = _mm256_setzero_ps();
            let mut sum2 = _mm256_setzero_ps();
            let mut sum3 = _mm256_setzero_ps();
            for k in 0..params.maxk {
                let kweight = _mm256_set1_ps(params.space_weight[k]);
                let ksptr = sptr.add(params.space_ofs[k]);
                let v_val8 = _mm256_loadu_si256(ksptr as *const __m256i);
                let v_diff8 = absdiff_epu8(v_val8, v_sptr8);
                // u8 → 4×u32（v_expand 两级）
                let (d0, d1) = expand_u8_u16(v_diff8);
                let (diff0, diff1) = expand_u16_u32(d0);
                let (diff2, diff3) = expand_u16_u32(d1);
                let (v0, v1) = expand_u8_u16(v_val8);
                let (val0, val1) = expand_u16_u32(v0);
                let (val2, val3) = expand_u16_u32(v1);
                let cw = params.color_weight.as_ptr();
                let w0 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, diff0, 4),
                );
                let w1 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, diff1, 4),
                );
                let w2 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, diff2, 4),
                );
                let w3 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, diff3, 4),
                );
                wsum0 = _mm256_add_ps(wsum0, w0);
                sum0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(val0), w0, sum0);
                wsum1 = _mm256_add_ps(wsum1, w1);
                sum1 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(val1), w1, sum1);
                wsum2 = _mm256_add_ps(wsum2, w2);
                sum2 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(val2), w2, sum2);
                wsum3 = _mm256_add_ps(wsum3, w3);
                sum3 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(val3), w3, sum3);
            }
            // round(div) → pack u32→u16→u8（v_pack_u_store 语义）
            let o0 = cvtps_u32(_mm256_div_ps(sum0, wsum0));
            let o1 = cvtps_u32(_mm256_div_ps(sum1, wsum1));
            let o2 = cvtps_u32(_mm256_div_ps(sum2, wsum2));
            let o3 = cvtps_u32(_mm256_div_ps(sum3, wsum3));
            let lo = pack_u32_u16(o0, o1);
            let hi = pack_u32_u16(o2, o3);
            let out = pack_u16_to_u8(lo, hi);
            _mm256_storeu_si256(dst_row.as_mut_ptr().add(j) as *mut __m256i, out);
            j += 32;
        }
    }
    // 尾部标量
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

/// 8u 双边滤波单行（AVX2，cn=3：每批 32 像素）。
///
/// 对齐 OpenCV cn==3 SIMD 主循环：解交织 → 三通道 absdiff 求和 →
/// gather → FMA 三通道独立累加 → `1/wsum` 乘法归一。
///
/// # Safety
/// - `padded` 长度 ≥ stride·(h+2r)，`dst_row` 长度 ≥ w·3
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
            let (rb, rg, rr) = deinterleave3(
                _mm256_loadu_si256(sptr as *const __m256i),
                _mm256_loadu_si256(sptr.add(32) as *const __m256i),
                _mm256_loadu_si256(sptr.add(64) as *const __m256i),
            );
            let mut wsum0 = _mm256_setzero_ps();
            let mut wsum1 = _mm256_setzero_ps();
            let mut wsum2 = _mm256_setzero_ps();
            let mut wsum3 = _mm256_setzero_ps();
            let mut sum_b0 = _mm256_setzero_ps();
            let mut sum_b1 = _mm256_setzero_ps();
            let mut sum_b2 = _mm256_setzero_ps();
            let mut sum_b3 = _mm256_setzero_ps();
            let mut sum_g0 = _mm256_setzero_ps();
            let mut sum_g1 = _mm256_setzero_ps();
            let mut sum_g2 = _mm256_setzero_ps();
            let mut sum_g3 = _mm256_setzero_ps();
            let mut sum_r0 = _mm256_setzero_ps();
            let mut sum_r1 = _mm256_setzero_ps();
            let mut sum_r2 = _mm256_setzero_ps();
            let mut sum_r3 = _mm256_setzero_ps();
            for k in 0..params.maxk {
                let kweight = _mm256_set1_ps(params.space_weight[k]);
                let ksptr = sptr.add(params.space_ofs[k]);
                let (kb, kg, kr) = deinterleave3(
                    _mm256_loadu_si256(ksptr as *const __m256i),
                    _mm256_loadu_si256(ksptr.add(32) as *const __m256i),
                    _mm256_loadu_si256(ksptr.add(64) as *const __m256i),
                );
                // 三通道 absdiff 求和（u16 域 ≤ 765）
                let (b_l, b_h) = expand_u8_u16(absdiff_epu8(kb, rb));
                let (g_l, g_h) = expand_u8_u16(absdiff_epu8(kg, rg));
                let (r_l, r_h) = expand_u8_u16(absdiff_epu8(kr, rr));
                let s_l = _mm256_add_epi16(_mm256_add_epi16(b_l, g_l), r_l);
                let s_h = _mm256_add_epi16(_mm256_add_epi16(b_h, g_h), r_h);
                let (val0, val1) = expand_u16_u32(s_l);
                let (val2, val3) = expand_u16_u32(s_h);
                let cw = params.color_weight.as_ptr();
                let w0 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, val0, 4),
                );
                let w1 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, val1, 4),
                );
                let w2 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, val2, 4),
                );
                let w3 = _mm256_mul_ps(
                    kweight,
                    _mm256_i32gather_ps(cw, val3, 4),
                );
                wsum0 = _mm256_add_ps(wsum0, w0);
                wsum1 = _mm256_add_ps(wsum1, w1);
                wsum2 = _mm256_add_ps(wsum2, w2);
                wsum3 = _mm256_add_ps(wsum3, w3);
                // 三通道值零扩展后 FMA
                let (kb_l, kb_h) = expand_u8_u16(kb);
                let (kg_l, kg_h) = expand_u8_u16(kg);
                let (kr_l, kr_h) = expand_u8_u16(kr);
                let (b0, b1) = expand_u16_u32(kb_l);
                let (b2, b3) = expand_u16_u32(kb_h);
                let (g0, g1) = expand_u16_u32(kg_l);
                let (g2, g3) = expand_u16_u32(kg_h);
                let (r0, r1) = expand_u16_u32(kr_l);
                let (r2, r3) = expand_u16_u32(kr_h);
                sum_b0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(b0), w0, sum_b0);
                sum_b1 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(b1), w1, sum_b1);
                sum_b2 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(b2), w2, sum_b2);
                sum_b3 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(b3), w3, sum_b3);
                sum_g0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(g0), w0, sum_g0);
                sum_g1 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(g1), w1, sum_g1);
                sum_g2 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(g2), w2, sum_g2);
                sum_g3 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(g3), w3, sum_g3);
                sum_r0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(r0), w0, sum_r0);
                sum_r1 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(r1), w1, sum_r1);
                sum_r2 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(r2), w2, sum_r2);
                sum_r3 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(r3), w3, sum_r3);
            }
            // 1/wsum → mul 归一（对齐 OpenCV v_div(v_one, wsum)）
            let one = _mm256_set1_ps(1.0);
            let iw0 = _mm256_div_ps(one, wsum0);
            let iw1 = _mm256_div_ps(one, wsum1);
            let iw2 = _mm256_div_ps(one, wsum2);
            let iw3 = _mm256_div_ps(one, wsum3);
            let ob0 = cvtps_u32(_mm256_mul_ps(iw0, sum_b0));
            let ob1 = cvtps_u32(_mm256_mul_ps(iw1, sum_b1));
            let ob2 = cvtps_u32(_mm256_mul_ps(iw2, sum_b2));
            let ob3 = cvtps_u32(_mm256_mul_ps(iw3, sum_b3));
            let og0 = cvtps_u32(_mm256_mul_ps(iw0, sum_g0));
            let og1 = cvtps_u32(_mm256_mul_ps(iw1, sum_g1));
            let og2 = cvtps_u32(_mm256_mul_ps(iw2, sum_g2));
            let og3 = cvtps_u32(_mm256_mul_ps(iw3, sum_g3));
            let or0 = cvtps_u32(_mm256_mul_ps(iw0, sum_r0));
            let or1 = cvtps_u32(_mm256_mul_ps(iw1, sum_r1));
            let or2 = cvtps_u32(_mm256_mul_ps(iw2, sum_r2));
            let or3 = cvtps_u32(_mm256_mul_ps(iw3, sum_r3));
            // 4×8 u32 → 2×16 u16 → 32 u8（OpenCV v_pack_u(v_pack, v_pack)）
            let b = pack_u16_to_u8(pack_u32_u16(ob0, ob1), pack_u32_u16(ob2, ob3));
            let g = pack_u16_to_u8(pack_u32_u16(og0, og1), pack_u32_u16(og2, og3));
            let rr_out = pack_u16_to_u8(pack_u32_u16(or0, or1), pack_u32_u16(or2, or3));
            // 交织 3 通道写出（v_store_interleave）
            interleave3_store(
                dst_row.as_mut_ptr().add(j * 3),
                b,
                g,
                rr_out,
            );
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

/// u8 向量零扩展为两个 u16 向量（低/高 128 位）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn expand_u8_u16(v: __m256i) -> (__m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    (
        _mm256_cvtepu8_epi16(_mm256_castsi256_si128(v)),
        _mm256_cvtepu8_epi16(_mm256_extracti128_si256(v, 1)),
    )
}

/// u8 无符号绝对差（OpenCV `v_absdiff<v_uint8>`：`max - min`）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn absdiff_epu8(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    _mm256_sub_epi8(
        _mm256_max_epu8(a, b),
        _mm256_min_epu8(a, b),
    )
}

/// u32 → u16 饱和打包 + lane 修复（值域 ≤ 255 无需饱和，packus 即可）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_u32_u16(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let p = _mm256_packus_epi32(a, b);
    _mm256_permute4x64_epi64(p, 0xD8)
}

/// f32 → u32（round-half-even，值域 [0, 255]）。
///
/// `_mm256_cvtps_epu32` 是 AVX512VL 指令，AVX2-only CPU 不可用；
/// 值域非负时 `_mm256_cvtps_epi32`（i32，round-half-even）位表示相同，
/// 后续 `packus_epi32` 按 u32 解释。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn cvtps_u32(v: __m256) -> __m256i {
    // SAFETY: 纯向量运算，AVX2 由调用方保证；调用方保证值域 [0, 255]
    _mm256_cvtps_epi32(v)
}

/// 3 通道 u8 交织存储（OpenCV `v_store_interleave`，intrin_avx.hpp:2752，
/// 32 像素 → 96 字节）。
///
/// 输入 `b/g/r` 为 32×u8 通道向量（BGR 序，与 [`deinterleave3`] 对应）。
#[inline]
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn interleave3_store(dst: *mut u8, b: __m256i, g: __m256i, r: __m256i) {
    // SAFETY: 调用方保证 dst 可写 96 字节；纯向量运算
    const SH_B: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8,
            3, 14, 9, 4, 15, 10, 5,
        ])
    };
    const SH_G: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13,
            8, 3, 14, 9, 4, 15, 10,
        ])
    };
    const SH_R: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2,
            13, 8, 3, 14, 9, 4, 15,
        ])
    };
    const M0: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0,
            0, -1, 0, 0, -1, 0, 0,
        ])
    };
    const M1: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1,
            0, 0, -1, 0, 0, -1, 0,
        ])
    };
    let b0 = _mm256_shuffle_epi8(b, SH_B);
    let g0 = _mm256_shuffle_epi8(g, SH_G);
    let r0 = _mm256_shuffle_epi8(r, SH_R);
    let p0 = _mm256_blendv_epi8(_mm256_blendv_epi8(b0, g0, M0), r0, M1);
    let p1 = _mm256_blendv_epi8(_mm256_blendv_epi8(g0, r0, M0), b0, M1);
    let p2 = _mm256_blendv_epi8(_mm256_blendv_epi8(r0, b0, M0), g0, M1);
    let bgr0 = _mm256_permute2x128_si256(p0, p1, 0x20);
    let bgr1 = _mm256_permute2x128_si256(p2, p0, 0x30);
    let bgr2 = _mm256_permute2x128_si256(p1, p2, 0x31);
    // SAFETY: 调用方保证 dst 可写 96 字节
    unsafe {
        _mm256_storeu_si256(dst as *mut __m256i, bgr0);
        _mm256_storeu_si256(dst.add(32) as *mut __m256i, bgr1);
        _mm256_storeu_si256(dst.add(64) as *mut __m256i, bgr2);
    }
}

// ==================== 形态学（morphology） ====================

/// 形态学行阶段：u8 水平滑动窗 min/max（AVX2，每批 32 元素）。
///
/// `src` 为带左右边框的行（长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`。
/// `erode=true` 为 min，`erode=false` 为 max。cn=1 走 SIMD，cn>1 走通道感知标量。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn morph_row_u8(src: &[u8], dst: &mut [u8], w: usize, kw: usize, cn: usize, erode: bool) {
    if cn == 1 {
        // SAFETY: 调用方保证切片长度；AVX2 由 target_feature 保证
        unsafe { morph_row_cn1(src, dst, w, kw, erode) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_cn(src, dst, w, kw, cn, erode)
    }
}

/// cn=1 行 min/max：每批 32 元素，加载 kw 个偏移向量取 min/max。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn morph_row_cn1(src: &[u8], dst: &mut [u8], w: usize, kw: usize, erode: bool) {
    let mut i = 0;
    // SAFETY: i+32 <= w 保证读 [i, i+32+kw-1)（src.len() >= w+kw-1）写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let mut acc = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            for k in 1..kw {
                let v = _mm256_loadu_si256(src.as_ptr().add(i + k) as *const __m256i);
                acc = if erode {
                    _mm256_min_epu8(acc, v)
                } else {
                    _mm256_max_epu8(acc, v)
                };
            }
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, acc);
            i += 32;
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

/// 形态学列阶段：u8 垂直 min/max（AVX2，每批 32 元素）。
///
/// `rows` 为 kh 行 u8（每行长度 ≥ w，stride 相同），`dst` 长度 ≥ w。
///
/// # Safety
/// - `rows` 每行长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn morph_col_u8(rows: &[&[u8]], dst: &mut [u8], w: usize, erode: bool) {
    let mut i = 0;
    // SAFETY: i+32 <= w 保证读 [i, i+32) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let mut acc = _mm256_loadu_si256(rows[0].as_ptr().add(i) as *const __m256i);
            for row in rows.iter().skip(1) {
                let v = _mm256_loadu_si256(row.as_ptr().add(i) as *const __m256i);
                acc = if erode {
                    _mm256_min_epu8(acc, v)
                } else {
                    _mm256_max_epu8(acc, v)
                };
            }
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, acc);
            i += 32;
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

/// 形态学行阶段（无边框源）：u8 水平滑动窗 min/max（AVX2，每批 32 元素）。
///
/// `src` 为原始行（长度 ≥ `w*cn`，无左右边框），`dst` 长度 ≥ `w*cn`。
/// 首尾 `kw-1` 像素的越界采样由 `border` 策略给出。cn=1 走 SIMD，cn>1 走标量。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
        // SAFETY: 调用方保证切片长度；AVX2 由 target_feature 保证
        unsafe { morph_row_direct_cn1(src, dst, w, kw, erode, border) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_direct_cn(src, dst, w, kw, cn, erode, border)
    }
}

/// cn=1 直接边框行 min/max：主体 SIMD，首尾特判。
#[inline]
#[target_feature(enable = "avx2")]
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
    // SAFETY: i+32 <= hi 保证读 [i-ax, i-ax+32+kw-1)（src.len() >= w）写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - ax;
            let mut acc = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            for k in 1..kw {
                let v = _mm256_loadu_si256(src.as_ptr().add(base + k) as *const __m256i);
                acc = if erode {
                    _mm256_min_epu8(acc, v)
                } else {
                    _mm256_max_epu8(acc, v)
                };
            }
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, acc);
            i += 32;
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

/// 形态学梯度：`dst = d - e`（u8 饱和减，AVX2，每批 32 元素）。
///
/// # Safety
/// - `d`/`e`/`dst` 长度 ≥ `n`
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn gradient_u8(d: &[u8], e: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: i+32 <= n 保证读 [i, i+32) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let dv = _mm256_loadu_si256(d.as_ptr().add(i) as *const __m256i);
            let ev = _mm256_loadu_si256(e.as_ptr().add(i) as *const __m256i);
            let r = _mm256_subs_epu8(dv, ev);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
    }
    for j in i..n {
        dst[j] = d[j].saturating_sub(e[j]);
    }
}

/// 形态学行阶段（无边框源）kw=3 特化：u8 水平滑动窗 min/max（AVX2，每批 32 元素）。
///
/// # Safety
/// - `src`/`dst` 长度契约由调用方保证
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn morph_row_u8_direct_k3(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    if cn == 1 {
        // SAFETY: 调用方保证切片长度；AVX2 由 target_feature 保证
        unsafe { morph_row_direct_k3_cn1(src, dst, w, erode, border) }
    } else {
        // SAFETY: 纯标量循环（target_feature 下编译），切片长度由调用方契约保证
        crate::simd_kernels::morphology::row_morph_u8_direct_k3_cn(src, dst, w, cn, erode, border)
    }
}

/// cn=1 kw=3 直接边框行 min/max：主体 SIMD（3 个偏移向量），首尾特判。
#[inline]
#[target_feature(enable = "avx2")]
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
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2)（src.len() >= w）写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let acc = if erode {
                _mm256_min_epu8(_mm256_min_epu8(a, b), c)
            } else {
                _mm256_max_epu8(_mm256_max_epu8(a, b), c)
            };
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, acc);
            i += 32;
        }
        // 尾部：未对齐重叠 SIMD（min/max 幂等，重算无害），覆盖 [i, hi)。
        // store 在 [base+1, base+33)，需 base+33 <= hi；读 [base, base+34)，需 base+34 <= w。
        // 取 base = w-34 = hi-33：store 覆盖 [hi-32, hi)，读上界恰为 w。
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let acc = if erode {
                _mm256_min_epu8(_mm256_min_epu8(a, b), c)
            } else {
                _mm256_max_epu8(_mm256_max_epu8(a, b), c)
            };
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 1) as *mut __m256i, acc);
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
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn morph_row_minmax_k3(
    src: &[u8],
    dst_min: &mut [u8],
    dst_max: &mut [u8],
    w: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    // SAFETY: 调用方保证切片长度；AVX2 由 target_feature 保证
    unsafe { morph_row_minmax_k3_cn1(src, dst_min, dst_max, w, border) }
}

/// cn=1 kw=3 融合行 min/max：主体 SIMD，尾部未对齐重叠，首尾特判。
#[inline]
#[target_feature(enable = "avx2")]
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
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let mn = _mm256_min_epu8(_mm256_min_epu8(a, b), c);
            let mx = _mm256_max_epu8(_mm256_max_epu8(a, b), c);
            _mm256_storeu_si256(dst_min.as_mut_ptr().add(i) as *mut __m256i, mn);
            _mm256_storeu_si256(dst_max.as_mut_ptr().add(i) as *mut __m256i, mx);
            i += 32;
        }
        // 尾部：未对齐重叠 SIMD。store 在 [base+1, base+33)，读 [base, base+34)。
        // 取 base = w-34：store 覆盖 [w-33, w-1)，读上界恰为 w。
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let mn = _mm256_min_epu8(_mm256_min_epu8(a, b), c);
            let mx = _mm256_max_epu8(_mm256_max_epu8(a, b), c);
            _mm256_storeu_si256(dst_min.as_mut_ptr().add(base + 1) as *mut __m256i, mn);
            _mm256_storeu_si256(dst_max.as_mut_ptr().add(base + 1) as *mut __m256i, mx);
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

/// 梯度行阶段（u8 → i32，对称核 ksize=3，cn=1，AVX2，每批 32 元素）。
///
/// `D[i] = k0*S[i] + k1*(S[i-1] + S[i+1])`，对齐 `SymmRowSmallVec_8u32s`。
/// 主体 3 偏移向量 + madd（k0/k1 ≤ 16 位），首尾 1 像素标量特判。
///
/// # Safety
/// - `src` 长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
                let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
                let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
                let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
                let (s0, s1, s2, s3) = deriv_symm_121_core(a, b, c);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 8) as *mut __m256i, s1);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s2);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 24) as *mut __m256i, s3);
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
    let vk0 = _mm256_set1_epi16(k0 as i16);
    let vk1 = _mm256_set1_epi16(k1 as i16);
    let mut i = 1usize;
    let hi = w.saturating_sub(1);
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let (s0, s1, s2, s3) = deriv_symm_core(a, b, c, vk0, vk1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 8) as *mut __m256i, s1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 24) as *mut __m256i, s3);
            i += 32;
        }
    }
    // 尾部：未对齐重叠 SIMD（与主体同式，重算无害）
    unsafe {
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let (s0, s1, s2, s3) = deriv_symm_core(a, b, c, vk0, vk1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 1) as *mut __m256i, s0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 9) as *mut __m256i, s1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 17) as *mut __m256i, s2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 25) as *mut __m256i, s3);
        } else {
            for x in i..hi {
                let l = src[x - 1] as i32;
                let r = src[x + 1] as i32;
                dst[x] = k0 * src[x] as i32 + k1 * (l + r);
            }
        }
    }
    // 首尾像素标量特判（窗口越界）
    if w > 0 {
        let r0 = if w > 1 { src[1] as i32 } else { sample(1) };
        dst[0] = k0 * src[0] as i32 + k1 * (sample(-1) + r0);
        if w > 1 {
            dst[w - 1] = k0 * src[w - 1] as i32 + k1 * (src[w - 2] as i32 + sample(w as i64));
        }
    }
}

/// 对称核核心：`k0*b + k1*(a + c)`（32 u8 → 32 i32，madd 点积）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn deriv_symm_core(
    a: __m256i,
    b: __m256i,
    c: __m256i,
    vk0: __m256i,
    vk1: __m256i,
) -> (__m256i, __m256i, __m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    // 32 u8 → 2×16 i16（零扩展）
    let a0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(a));
    let a1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(a, 1));
    let b0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(b));
    let b1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(b, 1));
    let c0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(c));
    let c1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(c, 1));
    // k0*b + k1*(a+c)：madd 需要相邻对 [k1,k0] 交织，改用 mullo + add（i16 域不溢出：
    // |k0|,|k1| ≤ 32767 且 u8 ≤ 255 → |k0*b| ≤ 8.4e6 超 i16！
    // 改为 i32 域：先 widen 到 i32 再 mul。
    let widen = |lo: __m256i, hi: __m256i| -> (__m256i, __m256i, __m256i, __m256i) {
        let l0 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(lo));
        let l1 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(lo, 1));
        let h0 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(hi));
        let h1 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(hi, 1));
        (l0, l1, h0, h1)
    };
    let (a0w, a1w, a2w, a3w) = widen(a0, a1);
    let (b0w, b1w, b2w, b3w) = widen(b0, b1);
    let (c0w, c1w, c2w, c3w) = widen(c0, c1);
    let vk0_32 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(vk0));
    let vk1_32 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(vk1));
    let acc = |a: __m256i, b: __m256i, c: __m256i| -> __m256i {
        _mm256_add_epi32(
            _mm256_mullo_epi32(b, vk0_32),
            _mm256_mullo_epi32(_mm256_add_epi32(a, c), vk1_32),
        )
    };
    // widen(a0, a1) 的 l0/l1/h0/h1 = 元素 0-7/8-15/16-23/24-31，各 8 个 i32
    (
        acc(a0w, b0w, c0w),
        acc(a1w, b1w, c1w),
        acc(a2w, b2w, c2w),
        acc(a3w, b3w, c3w),
    )
}

/// 对称 [1,2,1] 核核心（AVX2）：`2b + a + c`（32 u8 → 32 i32，纯加法）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn deriv_symm_121_core(a: __m256i, b: __m256i, c: __m256i) -> (__m256i, __m256i, __m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    // u8 → i16 wrap 加（和 ≤ 4*255 = 1020 < 32767 安全）→ i32 widen
    let cvt = |v: __m256i| -> (__m256i, __m256i) {
        (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(v)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256(v, 1)),
        )
    };
    let (a0, a1) = cvt(a);
    let (b0, b1) = cvt(b);
    let (c0, c1) = cvt(c);
    let s0 = _mm256_add_epi16(_mm256_add_epi16(b0, b0), _mm256_add_epi16(a0, c0));
    let s1 = _mm256_add_epi16(_mm256_add_epi16(b1, b1), _mm256_add_epi16(a1, c1));
    let w32 = |v: __m256i| -> (__m256i, __m256i) {
        (
            _mm256_cvtepi16_epi32(_mm256_castsi256_si128(v)),
            _mm256_cvtepi16_epi32(_mm256_extracti128_si256(v, 1)),
        )
    };
    let (r0, r1) = w32(s0);
    let (r2, r3) = w32(s1);
    (r0, r1, r2, r3)
}

/// 梯度行阶段（u8 → i32，反对称核 ksize=3，cn=1，AVX2，每批 32 元素）。
///
/// `D[i] = k1*(S[i+1] - S[i-1])`。主体 2 偏移向量相减后 widen-mul。
///
/// # Safety
/// - `src` 长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    let vk1 = _mm256_set1_epi32(k1);
    let mut i = 1usize;
    let hi = w.saturating_sub(1);
    // SAFETY: i+32 <= hi 保证读 [i-1, i-1+32+2) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= hi {
            let base = i - 1;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let (s0, s1, s2, s3) = deriv_asymm_core(a, c, vk1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 8) as *mut __m256i, s1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i + 24) as *mut __m256i, s3);
            i += 32;
        }
    }
    unsafe {
        if i < hi && w >= 35 {
            let base = w - 34;
            let a = _mm256_loadu_si256(src.as_ptr().add(base) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(base + 2) as *const __m256i);
            let (s0, s1, s2, s3) = deriv_asymm_core(a, c, vk1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 1) as *mut __m256i, s0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 9) as *mut __m256i, s1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 17) as *mut __m256i, s2);
            _mm256_storeu_si256(dst.as_mut_ptr().add(base + 25) as *mut __m256i, s3);
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

/// 反对称核核心：`k1*(c - a)`（32 u8 → 32 i32）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn deriv_asymm_core(a: __m256i, c: __m256i, vk1: __m256i) -> (__m256i, __m256i, __m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    // u8 差：先 i16 域相减（-255..255 安全），再 widen i32 乘
    let a0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(a));
    let a1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(a, 1));
    let c0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(c));
    let c1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(c, 1));
    let d0 = _mm256_sub_epi16(c0, a0);
    let d1 = _mm256_sub_epi16(c1, a1);
    let d0w0 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(d0));
    let d0w1 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(d0, 1));
    let d1w0 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(d1));
    let d1w1 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(d1, 1));
    // d0w0/d0w1/d1w0/d1w1 = 元素 0-7/8-15/16-23/24-31，各 8 个 i32
    (
        _mm256_mullo_epi32(d0w0, vk1),
        _mm256_mullo_epi32(d0w1, vk1),
        _mm256_mullo_epi32(d1w0, vk1),
        _mm256_mullo_epi32(d1w1, vk1),
    )
}

/// 梯度列阶段（i32 → i16，对称核 ksize=3，AVX2，每批 32 元素）。
///
/// `D = f0*S1 + f1*(S0 + S2) + delta`，饱和 i16（`v_pack` + min/max 钳位）。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn deriv_col_symm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f0: i32, f1: i32, delta: i32) {
    // [1,2,1] 专用加法路径（对齐 OpenCV SymmColumnSmallVec_32s16s：S0+S2+2*S1，无乘法）
    if f0 == 2 && f1 == 1 && delta == 0 {
        let mut i = 0usize;
        // SAFETY: i+16 <= w 保证读 [i, i+16)×3 行写 [i, i+16) 不越界
        unsafe {
            while i + 16 <= w {
                let acc = |off: usize| -> __m256i {
                    let s0 = _mm256_loadu_si256(rows[0].as_ptr().add(i + off) as *const __m256i);
                    let s1 = _mm256_loadu_si256(rows[1].as_ptr().add(i + off) as *const __m256i);
                    let s2 = _mm256_loadu_si256(rows[2].as_ptr().add(i + off) as *const __m256i);
                    _mm256_add_epi32(_mm256_add_epi32(s0, s2), _mm256_add_epi32(s1, s1))
                };
                let a0 = acc(0);
                let a1 = acc(8);
                let lo = _mm256_castsi256_si128(a0);
                let hi = _mm256_extracti128_si256(a0, 1);
                let packed0 = _mm_packs_epi32(lo, hi); // 饱和 i32→i16
                let lo = _mm256_castsi256_si128(a1);
                let hi = _mm256_extracti128_si256(a1, 1);
                let packed1 = _mm_packs_epi32(lo, hi);
                _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, packed0);
                _mm_storeu_si128(dst.as_mut_ptr().add(i + 8) as *mut __m128i, packed1);
                i += 16;
            }
        }
        for x in i..w {
            let s = rows[0][x] + rows[2][x] + 2 * rows[1][x];
            dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        return;
    }
    let vf0 = _mm256_set1_epi32(f0);
    let vf1 = _mm256_set1_epi32(f1);
    let vd = _mm256_set1_epi32(delta);
    let mut i = 0usize;
    // SAFETY: i+16 <= w 保证读 [i, i+16)×3 行写 [i, i+16) 不越界
    unsafe {
        while i + 16 <= w {
            let acc = |off: usize| -> __m256i {
                let s0 = _mm256_loadu_si256(rows[0].as_ptr().add(i + off) as *const __m256i);
                let s1 = _mm256_loadu_si256(rows[1].as_ptr().add(i + off) as *const __m256i);
                let s2 = _mm256_loadu_si256(rows[2].as_ptr().add(i + off) as *const __m256i);
                _mm256_add_epi32(
                    _mm256_add_epi32(_mm256_mullo_epi32(s1, vf0), vd),
                    _mm256_mullo_epi32(_mm256_add_epi32(s0, s2), vf1),
                )
            };
            let a0 = acc(0);
            let a1 = acc(8);
            let lo = _mm256_castsi256_si128(a0);
            let hi = _mm256_extracti128_si256(a0, 1);
            let packed0 = _mm_packs_epi32(lo, hi); // 元素 0-7
            let lo = _mm256_castsi256_si128(a1);
            let hi = _mm256_extracti128_si256(a1, 1);
            let packed1 = _mm_packs_epi32(lo, hi); // 元素 8-15
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, packed0);
            _mm_storeu_si128(dst.as_mut_ptr().add(i + 8) as *mut __m128i, packed1);
            i += 16;
        }
    }
    for x in i..w {
        let s = f0 * rows[1][x] + f1 * (rows[0][x] + rows[2][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// 梯度列阶段（i32 → i16，反对称核 ksize=3，AVX2，每批 16 元素）。
///
/// `D = f1*(S2 - S0) + delta`，饱和 i16。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn deriv_col_asymm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f1: i32, delta: i32) {
    let vf1 = _mm256_set1_epi32(f1);
    let vd = _mm256_set1_epi32(delta);
    let mut i = 0usize;
    // SAFETY: i+16 <= w 保证读 [i, i+16)×3 行写 [i, i+16) 不越界
    unsafe {
        while i + 16 <= w {
            let acc = |off: usize| -> __m256i {
                let s0 = _mm256_loadu_si256(rows[0].as_ptr().add(i + off) as *const __m256i);
                let s2 = _mm256_loadu_si256(rows[2].as_ptr().add(i + off) as *const __m256i);
                _mm256_add_epi32(_mm256_mullo_epi32(_mm256_sub_epi32(s2, s0), vf1), vd)
            };
            let a0 = acc(0);
            let a1 = acc(8);
            let lo = _mm256_castsi256_si128(a0);
            let hi = _mm256_extracti128_si256(a0, 1);
            let packed0 = _mm_packs_epi32(lo, hi);
            let lo = _mm256_castsi256_si128(a1);
            let hi = _mm256_extracti128_si256(a1, 1);
            let packed1 = _mm_packs_epi32(lo, hi);
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, packed0);
            _mm_storeu_si128(dst.as_mut_ptr().add(i + 8) as *mut __m128i, packed1);
            i += 16;
        }
    }
    for x in i..w {
        let s = f1 * (rows[2][x] - rows[0][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// Laplacian 3x3（u8 → i16，AVX2，每批 32 元素）。
///
/// 5-tap：ksize1 核 [0,1,0;1,-4,1;0,1,0]，否则 [2,0,2;0,-8,0;2,0,2]。
/// 行内左右邻域 clamp 采样（x=0 取 x=1，x=w-1 取 x=w-2，w=1 取自身）。
///
/// # Safety
/// - 3 行切片长度 ≥ w，`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
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
            let u = _mm256_loadu_si256(rows[0].as_ptr().add(base + 1) as *const __m256i);
            let d = _mm256_loadu_si256(rows[2].as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(rows[1].as_ptr().add(base + 1) as *const __m256i);
            if ksize1 {
                // 十字核：u + d + l + r - 4c
                let l = _mm256_loadu_si256(rows[1].as_ptr().add(base) as *const __m256i);
                let r = _mm256_loadu_si256(rows[1].as_ptr().add(base + 2) as *const __m256i);
                let (s0, s1) = laplacian_cross_core(u, d, l, r, c);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s1);
            } else {
                // 对角核：2*(r0[x±1] + r2[x±1]) - 8c
                let u_l = _mm256_loadu_si256(rows[0].as_ptr().add(base) as *const __m256i);
                let u_r = _mm256_loadu_si256(rows[0].as_ptr().add(base + 2) as *const __m256i);
                let d_l = _mm256_loadu_si256(rows[2].as_ptr().add(base) as *const __m256i);
                let d_r = _mm256_loadu_si256(rows[2].as_ptr().add(base + 2) as *const __m256i);
                let (s0, s1) = laplacian_diag_core(u_l, u_r, d_l, d_r, c);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, s0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 16) as *mut __m256i, s1);
            }
            i += 32;
        }
        // 尾部：未对齐重叠 SIMD（重算无害）
        if i < hi && w >= 35 {
            let base = w - 34;
            let u = _mm256_loadu_si256(rows[0].as_ptr().add(base + 1) as *const __m256i);
            let d = _mm256_loadu_si256(rows[2].as_ptr().add(base + 1) as *const __m256i);
            let c = _mm256_loadu_si256(rows[1].as_ptr().add(base + 1) as *const __m256i);
            if ksize1 {
                let l = _mm256_loadu_si256(rows[1].as_ptr().add(base) as *const __m256i);
                let r = _mm256_loadu_si256(rows[1].as_ptr().add(base + 2) as *const __m256i);
                let (s0, s1) = laplacian_cross_core(u, d, l, r, c);
                _mm256_storeu_si256(dst.as_mut_ptr().add(base + 1) as *mut __m256i, s0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(base + 17) as *mut __m256i, s1);
            } else {
                let u_l = _mm256_loadu_si256(rows[0].as_ptr().add(base) as *const __m256i);
                let u_r = _mm256_loadu_si256(rows[0].as_ptr().add(base + 2) as *const __m256i);
                let d_l = _mm256_loadu_si256(rows[2].as_ptr().add(base) as *const __m256i);
                let d_r = _mm256_loadu_si256(rows[2].as_ptr().add(base + 2) as *const __m256i);
                let (s0, s1) = laplacian_diag_core(u_l, u_r, d_l, d_r, c);
                _mm256_storeu_si256(dst.as_mut_ptr().add(base + 1) as *mut __m256i, s0);
                _mm256_storeu_si256(dst.as_mut_ptr().add(base + 17) as *mut __m256i, s1);
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

/// Laplacian 十字核核心：`u + d + l + r - 4c`（32 u8 → 32 i16）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn laplacian_cross_core(
    u: __m256i,
    d: __m256i,
    l: __m256i,
    r: __m256i,
    c: __m256i,
) -> (__m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let cvt = |v: __m256i| -> (__m256i, __m256i) {
        (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(v)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256(v, 1)),
        )
    };
    let (u0, u1) = cvt(u);
    let (d0, d1) = cvt(d);
    let (l0, l1) = cvt(l);
    let (r0, r1) = cvt(r);
    let (c0, c1) = cvt(c);
    let m4 = _mm256_set1_epi16(-4);
    let sum0 = _mm256_add_epi16(
        _mm256_add_epi16(_mm256_add_epi16(u0, d0), _mm256_add_epi16(l0, r0)),
        _mm256_mullo_epi16(c0, m4),
    );
    let sum1 = _mm256_add_epi16(
        _mm256_add_epi16(_mm256_add_epi16(u1, d1), _mm256_add_epi16(l1, r1)),
        _mm256_mullo_epi16(c1, m4),
    );
    (sum0, sum1)
}

/// Laplacian 对角核核心：`2*(u_l + u_r + d_l + d_r) - 8c`（32 u8 → 32 i16）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn laplacian_diag_core(
    u_l: __m256i,
    u_r: __m256i,
    d_l: __m256i,
    d_r: __m256i,
    c: __m256i,
) -> (__m256i, __m256i) {
    // SAFETY: 纯向量运算，AVX2 由调用方保证
    let cvt = |v: __m256i| -> (__m256i, __m256i) {
        (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(v)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256(v, 1)),
        )
    };
    let (ul0, ul1) = cvt(u_l);
    let (ur0, ur1) = cvt(u_r);
    let (dl0, dl1) = cvt(d_l);
    let (dr0, dr1) = cvt(d_r);
    let (c0, c1) = cvt(c);
    // 和 ≤ 4*255 = 1020，×2 = 2040；-8c ≥ -2040：i16 域安全
    let m2 = _mm256_set1_epi16(2);
    let m8 = _mm256_set1_epi16(-8);
    let sum0 = _mm256_add_epi16(
        _mm256_mullo_epi16(
            _mm256_add_epi16(_mm256_add_epi16(ul0, ur0), _mm256_add_epi16(dl0, dr0)),
            m2,
        ),
        _mm256_mullo_epi16(c0, m8),
    );
    let sum1 = _mm256_add_epi16(
        _mm256_mullo_epi16(
            _mm256_add_epi16(_mm256_add_epi16(ul1, ur1), _mm256_add_epi16(dl1, dr1)),
            m2,
        ),
        _mm256_mullo_epi16(c1, m8),
    );
    (sum0, sum1)
}

/// magnitude（f32，AVX2，每批 16 元素）：`dst = sqrt(fma(x,x,y*y))`。
///
/// # Safety
/// - `x`/`y`/`dst` 长度 ≥ n
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn magnitude32f(x: &[f32], y: &[f32], dst: &mut [f32], n: usize) {
    let mut i = 0usize;
    // SAFETY: i+8 <= n 保证读 [i, i+8)×2 写 [i, i+8) 不越界
    unsafe {
        while i + 8 <= n {
            let vx = _mm256_loadu_ps(x.as_ptr().add(i));
            let vy = _mm256_loadu_ps(y.as_ptr().add(i));
            let m = _mm256_sqrt_ps(_mm256_fmadd_ps(vx, vx, _mm256_mul_ps(vy, vy)));
            _mm256_storeu_ps(dst.as_mut_ptr().add(i), m);
            i += 8;
        }
    }
    for j in i..n {
        dst[j] = (x[j] * x[j] + y[j] * y[j]).sqrt();
    }
}

/// phase（f32，AVX2，每批 8 元素）：atan 多项式（对齐 `v_atan_f32`）。
///
/// # Safety
/// - `y`/`x`/`dst` 长度 ≥ n
/// - CPU 需支持 AVX2+FMA
#[inline(never)]
#[target_feature(enable = "avx2", enable = "fma")]
pub unsafe fn fast_atan2_32f(y: &[f32], x: &[f32], dst: &mut [f32], n: usize, angle_in_degrees: bool) {
    let scale = if angle_in_degrees {
        1.0f32
    } else {
        std::f32::consts::PI / 180.0
    };
    let vs = _mm256_set1_ps(scale);
    let mut i = 0usize;
    // SAFETY: i+8 <= n 保证读 [i, i+8)×2 写 [i, i+8) 不越界
    unsafe {
        while i + 8 <= n {
            let vy = _mm256_loadu_ps(y.as_ptr().add(i));
            let vx = _mm256_loadu_ps(x.as_ptr().add(i));
            let a = atan_vec(vy, vx);
            _mm256_storeu_ps(dst.as_mut_ptr().add(i), _mm256_mul_ps(a, vs));
            i += 8;
        }
    }
    for j in i..n {
        dst[j] = crate::simd_kernels::gradient::atan_f32_scalar(y[j], x[j]) * scale;
    }
}

/// cart_to_polar（f32，AVX2，每批 8 元素）：一次扫描同时产出 mag 与 angle。
///
/// # Safety
/// - 4 个切片长度 ≥ n
/// - CPU 需支持 AVX2+FMA
#[inline(never)]
#[target_feature(enable = "avx2", enable = "fma")]
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
    let vs = _mm256_set1_ps(scale);
    let mut i = 0usize;
    // SAFETY: i+8 <= n 保证读 [i, i+8)×2 写 [i, i+8)×2 不越界
    unsafe {
        while i + 8 <= n {
            let vx = _mm256_loadu_ps(x.as_ptr().add(i));
            let vy = _mm256_loadu_ps(y.as_ptr().add(i));
            let m = _mm256_sqrt_ps(_mm256_fmadd_ps(vx, vx, _mm256_mul_ps(vy, vy)));
            let a = _mm256_mul_ps(atan_vec(vy, vx), vs);
            _mm256_storeu_ps(mag.as_mut_ptr().add(i), m);
            _mm256_storeu_ps(angle.as_mut_ptr().add(i), a);
            i += 8;
        }
    }
    for j in i..n {
        mag[j] = (x[j] * x[j] + y[j] * y[j]).sqrt();
        angle[j] = crate::simd_kernels::gradient::atan_f32_scalar(y[j], x[j]) * scale;
    }
}

/// atan 向量核心（对齐 `v_atan_f32`，度数 [0, 360)）。
#[inline]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn atan_vec(y: __m256, x: __m256) -> __m256 {
    // SAFETY: 纯向量运算，AVX2+FMA 由调用方保证
    // 系数来自 OpenCV v_atan_f32（f64 精度常量，乘 180/π 转度数）
    #[allow(clippy::excessive_precision)]
    const P1: f32 = 0.9997878412794807 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P3: f32 = -0.3258083974640975 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P5: f32 = 0.1555786518463281 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P7: f32 = -0.04432655554792128 * (180.0 / std::f32::consts::PI);
    let eps = _mm256_set1_ps(f64::EPSILON as f32);
    let z = _mm256_setzero_ps();
    let p7 = _mm256_set1_ps(P7);
    let p5 = _mm256_set1_ps(P5);
    let p3 = _mm256_set1_ps(P3);
    let p1 = _mm256_set1_ps(P1);
    let v90 = _mm256_set1_ps(90.0);
    let v180 = _mm256_set1_ps(180.0);
    let v360 = _mm256_set1_ps(360.0);

    let ax = _mm256_andnot_ps(_mm256_set1_ps(-0.0), x);
    let ay = _mm256_andnot_ps(_mm256_set1_ps(-0.0), y);
    let c = _mm256_div_ps(
        _mm256_min_ps(ax, ay),
        _mm256_add_ps(_mm256_max_ps(ax, ay), eps),
    );
    let cc = _mm256_mul_ps(c, c);
    let mut a = _mm256_mul_ps(
        _mm256_fmadd_ps(
            _mm256_fmadd_ps(_mm256_fmadd_ps(cc, p7, p5), cc, p3),
            cc,
            p1,
        ),
        c,
    );
    // ax >= ay ? a : 90 - a
    let mask = _mm256_cmp_ps(ax, ay, _CMP_GE_OQ);
    a = _mm256_blendv_ps(_mm256_sub_ps(v90, a), a, mask);
    // x < 0 ? 180 - a : a
    let mask = _mm256_cmp_ps(x, z, _CMP_LT_OQ);
    a = _mm256_blendv_ps(a, _mm256_sub_ps(v180, a), mask);
    // y < 0 ? 360 - a : a
    let mask = _mm256_cmp_ps(y, z, _CMP_LT_OQ);
    _mm256_blendv_ps(a, _mm256_sub_ps(v360, a), mask)
}

// ==================== Canny（edges.rs 对应内核） ====================

/// Canny 幅度行（i16 dx/dy → i32 mag，AVX2，每批 16 元素）。
///
/// L1: `|dx| + |dy|`；L2: `dx² + dy²`。
///
/// # Safety
/// - `dx`/`dy`/`mag` 长度 ≥ n
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn canny_mag_row(dx: &[i16], dy: &[i16], mag: &mut [i32], n: usize, l2: bool) {
    let mut i = 0usize;
    // SAFETY: i+16 <= n 保证读 [i, i+16)×2 写 [i, i+16) 不越界
    unsafe {
        while i + 16 <= n {
            let vx = _mm256_loadu_si256(dx.as_ptr().add(i) as *const __m256i);
            let vy = _mm256_loadu_si256(dy.as_ptr().add(i) as *const __m256i);
            if l2 {
                // widen i16 → i32（每 __m256i 8 个 i32，低/高 128 位各 8 个 i16）
                let xl = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(vx));
                let xh = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(vx, 1));
                let yl = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(vy));
                let yh = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(vy, 1));
                let m0 = _mm256_add_epi32(_mm256_mullo_epi32(xl, xl), _mm256_mullo_epi32(yl, yl));
                let m1 = _mm256_add_epi32(_mm256_mullo_epi32(xh, xh), _mm256_mullo_epi32(yh, yh));
                _mm256_storeu_si256(mag.as_mut_ptr().add(i) as *mut __m256i, m0);
                _mm256_storeu_si256(mag.as_mut_ptr().add(i + 8) as *mut __m256i, m1);
            } else {
                // |dx|+|dy| ≤ 65534 超 i16 范围 → widen 到 i32 后加
                let ax = _mm256_abs_epi16(vx);
                let ay = _mm256_abs_epi16(vy);
                let xl = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(ax));
                let xh = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(ax, 1));
                let yl = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(ay));
                let yh = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(ay, 1));
                let m0 = _mm256_add_epi32(xl, yl);
                let m1 = _mm256_add_epi32(xh, yh);
                _mm256_storeu_si256(mag.as_mut_ptr().add(i) as *mut __m256i, m0);
                _mm256_storeu_si256(mag.as_mut_ptr().add(i + 8) as *mut __m256i, m1);
            }
            i += 16;
        }
    }
    crate::simd_kernels::edges::mag_row(&dx[i..], &dy[i..], &mut mag[i..], n - i, l2);
}

/// Canny NMS 批预筛（AVX2）：`mag_a[0..32] > low` 的 32 位掩码。
///
/// bit j 对应 mag_a[j]。不足 32 元素时高位补 0（调用方保证 n ≥ 32 才走本内核）。
///
/// # Safety
/// - `mag_a` 长度 ≥ 32
/// - CPU 需支持 AVX2
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn canny_low_mask32(mag_a: &[i32], low: i32) -> u32 {
    // SAFETY: 调用方保证长度 ≥ 32；__m256i 装 8 个 i32，4 次 load 覆盖 32
    unsafe {
        let vlow = _mm256_set1_epi32(low);
        let mut mask = 0u32;
        for k in 0..4 {
            let m = _mm256_loadu_si256(mag_a.as_ptr().add(k * 8) as *const __m256i);
            let c = _mm256_cmpgt_epi32(m, vlow);
            mask |= (_mm256_movemask_ps(_mm256_castsi256_ps(c)) as u32) << (k * 8);
        }
        mask
    }
}

/// Canny finalPass 单行（AVX2，每批 32 元素）：`dst = (map==2) ? 255 : 0`。
///
/// # Safety
/// - `pmap` 长度 ≥ w+2（含哨兵），`dst` 长度 ≥ w
/// - CPU 需支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn canny_final_pass_row(pmap: &[u8], dst: &mut [u8], w: usize) {
    let mut i = 0usize;
    let v2 = _mm256_set1_epi8(2);
    let vzero = _mm256_setzero_si256();
    // SAFETY: i+32 <= w 保证读 [i+1, i+33) 写 [i, i+32) 不越界
    unsafe {
        while i + 32 <= w {
            let m = _mm256_loadu_si256(pmap.as_ptr().add(i + 1) as *const __m256i);
            let eq = _mm256_cmpeq_epi8(m, v2);
            // eq 全 1 字节 → 255
            let out = _mm256_or_si256(_mm256_and_si256(eq, _mm256_set1_epi8(-1)), vzero);
            let _ = out;
            // cmpeq 结果本身就是 0xFF/0x00，直接 store
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, eq);
            i += 32;
        }
        while i + 16 <= w {
            let m = _mm_loadu_si128(pmap.as_ptr().add(i + 1) as *const __m128i);
            let eq = _mm_cmpeq_epi8(m, _mm256_castsi256_si128(v2));
            _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, eq);
            i += 16;
        }
    }
    crate::simd_kernels::edges::final_pass_row(&pmap[i..], &mut dst[i..], w - i);
}

/// Canny NMS 批预写 1（AVX2，每批 32 元素）：`pmap[0..32] = 1`。
///
/// # Safety
/// - `pmap` 长度 ≥ 32
/// - CPU 需支持 AVX2
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn canny_fill_one32(pmap: &mut [u8]) {
    // SAFETY: 调用方保证长度 ≥ 32
    unsafe {
        let one = _mm256_set1_epi8(1);
        _mm256_storeu_si256(pmap.as_mut_ptr() as *mut __m256i, one);
    }
}

/// Canny NMS + 双阈值整行（AVX2：32 元素批预筛 + 候选标量 + 尾部）。
///
/// `mag_*`/`pmap` 长度 w+2（哨兵），有效区 [1, w]；`dx`/`dy` 长度 w。
/// 栈元素为 pmap 相对偏移（列 1..w+1）。
///
/// # Safety
/// - 上述长度契约由调用方保证
/// - CPU 需支持 AVX2
#[allow(clippy::too_many_arguments)]
#[inline(never)]
#[target_feature(enable = "avx2")]
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
    let vlow = _mm256_set1_epi32(low);
    let one = _mm256_set1_epi8(1);
    // 首尾哨兵（对齐 OpenCV _pmap[-1] = _pmap[w] = 1）
    pmap[0] = 1;
    pmap[w + 1] = 1;
    let mut j = 0usize;
    // SAFETY: j+32 <= w 保证 mag_a 读 [j+1, j+33)、pmap 写 [j+1, j+33) 不越界
    unsafe {
        while j + 32 <= w {
            _mm256_storeu_si256(pmap.as_mut_ptr().add(j + 1) as *mut __m256i, one);
            // __m256i = 8 i32，32 元素批需 4 次 load（显式展开）
            let m0 = _mm256_loadu_si256(mag_a.as_ptr().add(j + 1) as *const __m256i);
            let m1 = _mm256_loadu_si256(mag_a.as_ptr().add(j + 9) as *const __m256i);
            let m2 = _mm256_loadu_si256(mag_a.as_ptr().add(j + 17) as *const __m256i);
            let m3 = _mm256_loadu_si256(mag_a.as_ptr().add(j + 25) as *const __m256i);
            let c0 = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpgt_epi32(m0, vlow))) as u32;
            let c1 = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpgt_epi32(m1, vlow))) as u32;
            let c2 = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpgt_epi32(m2, vlow))) as u32;
            let c3 = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpgt_epi32(m3, vlow))) as u32;
            let mask = c0 | (c1 << 8) | (c2 << 16) | (c3 << 24);
            let mut m = mask as u64;
            while m != 0 {
                let l = m.trailing_zeros() as usize;
                m &= m.wrapping_sub(1);
                // SAFETY: j+l < w（掩码位范围），各缓冲长度契约由函数文档保证
                crate::simd_kernels::edges::nms_one(
                    mag_p.as_ptr(), mag_a.as_ptr(), mag_n.as_ptr(), dx.as_ptr(), dy.as_ptr(),
                    pmap.as_mut_ptr(), j + l, high, stack,
                );
            }
            j += 32;
        }
    }
    // 尾部标量（逐像素，含 low 预筛）
    for k in j..w {
        if mag_a[k + 1] > low {
            // SAFETY: k < w，各缓冲长度契约由函数文档保证
            unsafe {
                crate::simd_kernels::edges::nms_one(
                    mag_p.as_ptr(), mag_a.as_ptr(), mag_n.as_ptr(), dx.as_ptr(), dy.as_ptr(),
                    pmap.as_mut_ptr(), k, high, stack,
                );
            }
        } else {
            pmap[k + 1] = 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 轮廓查找：行扫描跳转（对齐 contours_link.cpp findStart/EndContourPoint
// 的 v_uint8 + scan_forward，AVX2 用 movemask + tzcnt）
// ---------------------------------------------------------------------------

/// 行内找第一个非零 u8（从 `j` 到 `row.len()`）。
///
/// # Safety
/// `j <= row.len()`。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn find_start_contour_point(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    let zero = _mm256_setzero_si256();
    // SAFETY: k + 32 <= width 保证 load [k, k+32) 在界内
    while k + 32 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm256_loadu_si256(row.as_ptr().add(k) as *const __m256i);
        // 非零 = !cmpeq(v, 0)（不能用 cmpgt：u8 ≥ 128 的有符号值为负）
        let mask = (!_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, zero))) as u32;
        if mask != 0 {
            return k + mask.trailing_zeros() as usize;
        }
        k += 32;
        }
    }
    // 尾部标量
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
#[target_feature(enable = "avx2")]
pub unsafe fn find_end_contour_point(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    let zero = _mm256_setzero_si256();
    // SAFETY: k + 32 <= width 保证 load 在界内
    while k + 32 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm256_loadu_si256(row.as_ptr().add(k) as *const __m256i);
        // 零像素 = cmpeq(v, 0)
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, zero)) as u32;
        if mask != 0 {
            return k + mask.trailing_zeros() as usize;
        }
        k += 32;
        }
    }
    while k < width && row[k] != 0 {
        k += 1;
    }
    k
}

/// Suzuki findNextX：行内找第一个与 `prev` 不同的 i8（从 `j` 到 `width`）。
///
/// 调用方已做首元素检查（`row[j] == prev` 时才进入）。
/// # Safety
/// `j <= width <= row.len()`。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn find_next_diff(row: &[i8], j: usize, width: usize, prev: i8, p: &mut i8) -> usize {
    let mut k = j;
    let vprev = _mm256_set1_epi8(prev);
    // SAFETY: k + 32 <= width 保证 load [k, k+32) 在界内
    while k + 32 <= width {
        // SAFETY: 循环条件保证 load [k, k+N) 在界内
        unsafe {
        let v = _mm256_loadu_si256(row.as_ptr().add(k) as *const __m256i);
        let ne = _mm256_cmpeq_epi8(v, vprev);
        // 不同：ne 位为 0 → 取反后位为 1
        let mask = (!_mm256_movemask_epi8(ne)) as u32;
        if mask != 0 {
            let r = k + mask.trailing_zeros() as usize;
            *p = row[r];
            return r;
        }
        k += 32;
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

/// 单趟扫描一行，返回所有 run 边界（AVX2，每批 32 字节）。
///
/// 与 avx512::scan_row_runs 同构：`_mm256_movemask_epi8` 得 32 位非零掩码，
/// 用 tz/to 位操作提取 run 边界。
///
/// # Safety
/// `row` 长度 ≥ 1；CPU 须支持 AVX2（由 isa() 派发保证）。
#[target_feature(enable = "avx2")]
pub unsafe fn scan_row_runs(row: &[u8], starts: &mut Vec<u32>, ends: &mut Vec<u32>) {
    starts.clear();
    ends.clear();
    let width = row.len();
    let zero = _mm256_setzero_si256();
    let mut k = 1usize;
    let mut in_run = false;
    let mut run_start = 0u32;
    // SAFETY: k + 32 <= width 保证 load [k, k+32) 在界内
    while k + 32 <= width {
        // SAFETY: 循环条件保证 load 在界内
        unsafe {
        let v = _mm256_loadu_si256(row.as_ptr().add(k) as *const __m256i);
        let mask = (!_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, zero))) as u32;
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
            if bit + tz + to == 32 {
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
        k += 32;
    }
    // 尾部（< 32 字节）
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

// ---- 按位逻辑（bitwise.rs，对齐 arithm.simd.hpp and8u/or8u/xor8u/not8u）----

/// 按位与：32 字节/迭代。
///
/// # Safety
/// `n <= a.len() == b.len() == dst.len()`；CPU 须支持 AVX2（由 isa() 派发保证）。
#[target_feature(enable = "avx2")]
pub unsafe fn bitwise_and_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let r = _mm256_and_si256(va, vb);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
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
#[target_feature(enable = "avx2")]
pub unsafe fn bitwise_or_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let r = _mm256_or_si256(va, vb);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
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
#[target_feature(enable = "avx2")]
pub unsafe fn bitwise_xor_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let r = _mm256_xor_si256(va, vb);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_xor_bytes(&a[i..], &b[i..], &mut dst[i..], n - i);
    }
}

/// 按位取反。
///
/// # Safety
/// `n <= a.len() == dst.len()`；CPU 须支持 AVX2。
#[target_feature(enable = "avx2")]
pub unsafe fn bitwise_not_bytes(a: &[u8], dst: &mut [u8], n: usize) {
    let mut i = 0;
    // SAFETY: 循环条件 i+32 <= n 保证读 [i, i+32) 不越界
    unsafe {
        while i + 32 <= n {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let r = _mm256_xor_si256(va, _mm256_set1_epi8(-1));
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
            i += 32;
        }
    }
    if i < n {
        crate::simd_kernels::scalar::bitwise_not_bytes(&a[i..], &mut dst[i..], n - i);
    }
}

// ---- 距离变换（AVX2，分块并行 min-plus 扫描）----
//
// 与 avx512 版本同构：chamfer 水平依赖是 min-plus 前缀/后缀扫描，用 Brent-Kung
// 分块扫描打破串行依赖链。每批 8 像素（256-bit）。
// - Phase 1：每 8-px 块独立做局部前缀/后缀扫描（3 步 log 扫描，permutevar 移位 +
//   clamp 加法 + min/blend），块间无依赖。
// - Phase 2+3 融合：串行 stitch 只跨块，块的 lane0/lane7 输出即下一块 incoming。
//
// 关键正确性细节（与 avx512 一致）：
// - 用无符号 min（`_mm256_min_epu32`），距离值可 > 2^31。
// - 扫描加法 `a[k+s]+s*hv` 可能回绕 → 每步先 clamp 源 `min(a[k+s], dist_max - s*hv)`。
// - 折叠 `bcast(incoming)+ramp*hv` 回绕用符号翻转技巧检测（`t^0x80000000 < vr^0x80000000`）。
// - 前向零掩码：`_mm256_cvtepu8_epi32` 零扩展 + `_mm256_cmpeq_epi32` + blendv。
// - 5×5 水平依赖可降为一阶（`2*hv <= long` 恒成立，二阶项永不胜出）。

/// 3×3 距离变换前向扫描（AVX2，分块并行前缀扫描）。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + step) + border + w + 1`
/// - `s` 长度 ≥ `w`
/// - CPU 需支持 AVX2
#[target_feature(enable = "avx2")]
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
    let v_hv = _mm256_set1_epi32(hv_dist as i32);
    let v_diag = _mm256_set1_epi32(diag_dist as i32);
    let v_max = _mm256_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (k+1)*hv（前向，从左到右）
    let v_ramp1 = _mm256_setr_epi32(
        (1 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (8 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv
    let v_c1 = _mm256_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm256_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm256_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_s1 = _mm256_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm256_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm256_set1_epi32((4 * hv_dist) as i32);
    // 前缀扫描移位索引：idx_s[k] = k-s（k<s 为垃圾，被掩码排除）
    let idx1 = _mm256_setr_epi32(0, 0, 1, 2, 3, 4, 5, 6);
    let idx2 = _mm256_setr_epi32(0, 0, 0, 1, 2, 3, 4, 5);
    let idx4 = _mm256_setr_epi32(0, 0, 0, 0, 0, 1, 2, 3);
    // 前缀扫描有效掩码：step s 有效 lane k >= s
    let m1 = _mm256_setr_epi32(0, -1, -1, -1, -1, -1, -1, -1);
    let m2 = _mm256_setr_epi32(0, 0, -1, -1, -1, -1, -1, -1);
    let m4 = _mm256_setr_epi32(0, 0, 0, 0, -1, -1, -1, -1);

    let nb = w / 8;
    let tail = w % 8;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部前缀扫描（块独立，向量化 top 行 min + 零掩码）
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let top = temp.as_ptr().add(row - step + border + j - 1);
            let tl = _mm256_loadu_si256(top as *const __m256i);
            let t = _mm256_loadu_si256(top.add(1) as *const __m256i);
            let tr = _mm256_loadu_si256(top.add(2) as *const __m256i);
            let m = _mm256_min_epu32(
                _mm256_min_epu32(
                    _mm256_add_epi32(tl, v_diag),
                    _mm256_add_epi32(t, v_hv),
                ),
                _mm256_add_epi32(tr, v_diag),
            );
            let m = _mm256_min_epu32(m, _mm256_set1_epi32(dist_max as i32));
            // 零掩码：s[j]==0 的 lane 置 0
            let sv = _mm_loadl_epi64(s.as_ptr().add(j) as *const __m128i);
            let sv32 = _mm256_cvtepu8_epi32(sv);
            let is_zero = _mm256_cmpeq_epi32(sv32, _mm256_setzero_si256());
            let mut a = _mm256_blendv_epi8(m, _mm256_setzero_si256(), is_zero);
            // 前缀扫描：s=1,2,4
            let src = _mm256_permutevar8x32_epi32(a, idx1);
            let src = _mm256_min_epu32(src, v_c1);
            let t = _mm256_add_epi32(src, v_s1);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m1);
            let src = _mm256_permutevar8x32_epi32(a, idx2);
            let src = _mm256_min_epu32(src, v_c2);
            let t = _mm256_add_epi32(src, v_s2);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m2);
            let src = _mm256_permutevar8x32_epi32(a, idx4);
            let src = _mm256_min_epu32(src, v_c4);
            let t = _mm256_add_epi32(src, v_s4);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m4);
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, a);
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

    // Phase 2+3: 串行 stitch + 折叠（块从左到右，lane7 输出即下一块 incoming）
    let mut l = left;
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let local = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let vl = _mm256_set1_epi32(l as i32);
            let t = _mm256_add_epi32(vl, v_ramp1);
            // 回绕检测：t < vl（无符号）⟺ (t^0x80000000) < (vl^0x80000000)（有符号）
            let tf = _mm256_xor_si256(t, _mm256_set1_epi32(i32::MIN));
            let vlf = _mm256_xor_si256(vl, _mm256_set1_epi32(i32::MIN));
            let wrap = _mm256_cmpgt_epi32(vlf, tf);
            let t = _mm256_blendv_epi8(t, v_max, wrap);
            let v = _mm256_min_epu32(local, t);
            l = _mm256_extract_epi32(v, 7) as u32;
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, v);
        }
    }
}

/// 3×3 距离变换后向扫描（AVX2，分块并行后缀扫描 + 定点→浮点）。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + step) + border + w + 1`
/// - `drow` 长度 ≥ `w`
/// - CPU 需支持 AVX2
#[target_feature(enable = "avx2")]
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
    let v_hv = _mm256_set1_epi32(hv_dist as i32);
    let v_diag = _mm256_set1_epi32(diag_dist as i32);
    let v_scale = _mm256_set1_ps(scale);
    let v_max = _mm256_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (8-k)*hv（后向，从右到左）
    let v_ramp2 = _mm256_setr_epi32(
        (8 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (1 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv
    let v_c1 = _mm256_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm256_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm256_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_s1 = _mm256_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm256_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm256_set1_epi32((4 * hv_dist) as i32);
    // 后缀扫描移位索引：idx_s[k] = k+s（k+s>=8 为垃圾，被掩码排除）
    let idx1 = _mm256_setr_epi32(1, 2, 3, 4, 5, 6, 7, 0);
    let idx2 = _mm256_setr_epi32(2, 3, 4, 5, 6, 7, 0, 1);
    let idx4 = _mm256_setr_epi32(4, 5, 6, 7, 0, 1, 2, 3);
    // 后缀扫描有效掩码：step s 有效 lane k < 8-s
    let m1 = _mm256_setr_epi32(-1, -1, -1, -1, -1, -1, -1, 0);
    let m2 = _mm256_setr_epi32(-1, -1, -1, -1, -1, -1, 0, 0);
    let m4 = _mm256_setr_epi32(-1, -1, -1, -1, 0, 0, 0, 0);

    let nb = w / 8;
    let tail = w % 8;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部后缀扫描（块独立，向量化 bottom 行 min + t0）
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let bottom = temp.as_ptr().add(row + step + border + j - 1);
            let bl = _mm256_loadu_si256(bottom as *const __m256i);
            let b = _mm256_loadu_si256(bottom.add(1) as *const __m256i);
            let br = _mm256_loadu_si256(bottom.add(2) as *const __m256i);
            let m = _mm256_min_epu32(
                _mm256_min_epu32(
                    _mm256_add_epi32(bl, v_diag),
                    _mm256_add_epi32(b, v_hv),
                ),
                _mm256_add_epi32(br, v_diag),
            );
            let t0 = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let mut a = _mm256_min_epu32(m, t0);
            // 后缀扫描：s=1,2,4
            let src = _mm256_permutevar8x32_epi32(a, idx1);
            let src = _mm256_min_epu32(src, v_c1);
            let t = _mm256_add_epi32(src, v_s1);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m1);
            let src = _mm256_permutevar8x32_epi32(a, idx2);
            let src = _mm256_min_epu32(src, v_c2);
            let t = _mm256_add_epi32(src, v_s2);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m2);
            let src = _mm256_permutevar8x32_epi32(a, idx4);
            let src = _mm256_min_epu32(src, v_c4);
            let t = _mm256_add_epi32(src, v_s4);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m4);
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, a);
        }
    }

    // Phase 2+3: 串行 stitch + 折叠 + 定点→浮点（块从右到左，lane0 输出即下一块 incoming）
    let mut r = temp[row + border + w];
    unsafe {
        for b in (0..nb).rev() {
            let j = base + 8 * b;
            let local = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let vr = _mm256_set1_epi32(r as i32);
            let t = _mm256_add_epi32(vr, v_ramp2);
            // 回绕检测：t < vr（无符号）
            let tf = _mm256_xor_si256(t, _mm256_set1_epi32(i32::MIN));
            let vrf = _mm256_xor_si256(vr, _mm256_set1_epi32(i32::MIN));
            let wrap = _mm256_cmpgt_epi32(vrf, tf);
            let t = _mm256_blendv_epi8(t, v_max, wrap);
            let v = _mm256_min_epu32(local, t);
            r = _mm256_extract_epi32(v, 0) as u32;
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, v);
            let f = _mm256_mul_ps(_mm256_cvtepi32_ps(v), v_scale);
            _mm256_storeu_ps(drow.as_mut_ptr().add(j), f);
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

/// 5×5 距离变换前向扫描（AVX2，分块并行前缀扫描）。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + 2*step) + border + w + 2`
/// - `s` 长度 ≥ `w`
/// - CPU 需支持 AVX2
#[target_feature(enable = "avx2")]
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
    let v_hv = _mm256_set1_epi32(hv_dist as i32);
    let v_diag = _mm256_set1_epi32(diag_dist as i32);
    let v_long = _mm256_set1_epi32(long_dist as i32);
    let v_max = _mm256_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (k+1)*hv（前向，从左到右）
    let v_ramp1 = _mm256_setr_epi32(
        (1 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (8 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv
    let v_c1 = _mm256_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm256_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm256_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_s1 = _mm256_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm256_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm256_set1_epi32((4 * hv_dist) as i32);
    // 前缀扫描移位索引
    let idx1 = _mm256_setr_epi32(0, 0, 1, 2, 3, 4, 5, 6);
    let idx2 = _mm256_setr_epi32(0, 0, 0, 1, 2, 3, 4, 5);
    let idx4 = _mm256_setr_epi32(0, 0, 0, 0, 0, 1, 2, 3);
    // 前缀扫描有效掩码
    let m1 = _mm256_setr_epi32(0, -1, -1, -1, -1, -1, -1, -1);
    let m2 = _mm256_setr_epi32(0, 0, -1, -1, -1, -1, -1, -1);
    let m4 = _mm256_setr_epi32(0, 0, 0, 0, -1, -1, -1, -1);

    let nb = w / 8;
    let tail = w % 8;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部前缀扫描（块独立，向量化 top1/top2 行 min + 零掩码）
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let top1 = temp.as_ptr().add(row - step + border + j - 2);
            let top2 = temp.as_ptr().add(row - 2 * step + border + j - 1);
            let t2m1 = _mm256_loadu_si256(top2 as *const __m256i);
            let t2p1 = _mm256_loadu_si256(top2.add(2) as *const __m256i);
            let t1m2 = _mm256_loadu_si256(top1 as *const __m256i);
            let t1m1 = _mm256_loadu_si256(top1.add(1) as *const __m256i);
            let t1 = _mm256_loadu_si256(top1.add(2) as *const __m256i);
            let t1p1 = _mm256_loadu_si256(top1.add(3) as *const __m256i);
            let t1p2 = _mm256_loadu_si256(top1.add(4) as *const __m256i);
            let m = _mm256_min_epu32(
                _mm256_min_epu32(
                    _mm256_min_epu32(
                        _mm256_add_epi32(t2m1, v_long),
                        _mm256_add_epi32(t2p1, v_long),
                    ),
                    _mm256_min_epu32(
                        _mm256_add_epi32(t1m2, v_long),
                        _mm256_add_epi32(t1m1, v_diag),
                    ),
                ),
                _mm256_min_epu32(
                    _mm256_min_epu32(
                        _mm256_add_epi32(t1, v_hv),
                        _mm256_add_epi32(t1p1, v_diag),
                    ),
                    _mm256_add_epi32(t1p2, v_long),
                ),
            );
            let m = _mm256_min_epu32(m, _mm256_set1_epi32(dist_max as i32));
            // 零掩码：s[j]==0 的 lane 置 0
            let sv = _mm_loadl_epi64(s.as_ptr().add(j) as *const __m128i);
            let sv32 = _mm256_cvtepu8_epi32(sv);
            let is_zero = _mm256_cmpeq_epi32(sv32, _mm256_setzero_si256());
            let mut a = _mm256_blendv_epi8(m, _mm256_setzero_si256(), is_zero);
            // 前缀扫描：s=1,2,4
            let src = _mm256_permutevar8x32_epi32(a, idx1);
            let src = _mm256_min_epu32(src, v_c1);
            let t = _mm256_add_epi32(src, v_s1);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m1);
            let src = _mm256_permutevar8x32_epi32(a, idx2);
            let src = _mm256_min_epu32(src, v_c2);
            let t = _mm256_add_epi32(src, v_s2);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m2);
            let src = _mm256_permutevar8x32_epi32(a, idx4);
            let src = _mm256_min_epu32(src, v_c4);
            let t = _mm256_add_epi32(src, v_s4);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m4);
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, a);
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

    // Phase 2+3: 串行 stitch + 折叠（块从左到右，lane7 输出即下一块 incoming）
    let mut l = left;
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let local = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let vl = _mm256_set1_epi32(l as i32);
            let t = _mm256_add_epi32(vl, v_ramp1);
            // 回绕检测：t < vl（无符号）
            let tf = _mm256_xor_si256(t, _mm256_set1_epi32(i32::MIN));
            let vlf = _mm256_xor_si256(vl, _mm256_set1_epi32(i32::MIN));
            let wrap = _mm256_cmpgt_epi32(vlf, tf);
            let t = _mm256_blendv_epi8(t, v_max, wrap);
            let v = _mm256_min_epu32(local, t);
            l = _mm256_extract_epi32(v, 7) as u32;
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, v);
        }
    }
}

/// 5×5 距离变换后向扫描（AVX2，分块并行后缀扫描 + 定点→浮点）。
///
/// # Safety
/// - `temp` 长度 ≥ `(row + 2*step) + border + w + 2`
/// - `drow` 长度 ≥ `w`
/// - CPU 需支持 AVX2
#[target_feature(enable = "avx2")]
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
    let v_hv = _mm256_set1_epi32(hv_dist as i32);
    let v_diag = _mm256_set1_epi32(diag_dist as i32);
    let v_long = _mm256_set1_epi32(long_dist as i32);
    let v_scale = _mm256_set1_ps(scale);
    let v_max = _mm256_set1_epi32(u32::MAX as i32);
    // 折叠 ramp：lane k 贡献 (8-k)*hv（后向，从右到左）
    let v_ramp2 = _mm256_setr_epi32(
        (8 * hv_dist) as i32,
        (7 * hv_dist) as i32,
        (6 * hv_dist) as i32,
        (5 * hv_dist) as i32,
        (4 * hv_dist) as i32,
        (3 * hv_dist) as i32,
        (2 * hv_dist) as i32,
        (1 * hv_dist) as i32,
    );
    // 扫描 clamp 常量：dist_max - s*hv
    let v_c1 = _mm256_set1_epi32((dist_max - 1 * hv_dist) as i32);
    let v_c2 = _mm256_set1_epi32((dist_max - 2 * hv_dist) as i32);
    let v_c4 = _mm256_set1_epi32((dist_max - 4 * hv_dist) as i32);
    let v_s1 = _mm256_set1_epi32((1 * hv_dist) as i32);
    let v_s2 = _mm256_set1_epi32((2 * hv_dist) as i32);
    let v_s4 = _mm256_set1_epi32((4 * hv_dist) as i32);
    // 后缀扫描移位索引
    let idx1 = _mm256_setr_epi32(1, 2, 3, 4, 5, 6, 7, 0);
    let idx2 = _mm256_setr_epi32(2, 3, 4, 5, 6, 7, 0, 1);
    let idx4 = _mm256_setr_epi32(4, 5, 6, 7, 0, 1, 2, 3);
    // 后缀扫描有效掩码
    let m1 = _mm256_setr_epi32(-1, -1, -1, -1, -1, -1, -1, 0);
    let m2 = _mm256_setr_epi32(-1, -1, -1, -1, -1, -1, 0, 0);
    let m4 = _mm256_setr_epi32(-1, -1, -1, -1, 0, 0, 0, 0);

    let nb = w / 8;
    let tail = w % 8;
    let base = tail; // 块覆盖 [tail, w)

    // Phase 1: 局部后缀扫描（块独立，向量化 bot1/bot2 行 min + t0）
    unsafe {
        for b in 0..nb {
            let j = base + 8 * b;
            let bot1 = temp.as_ptr().add(row + step + border + j - 2);
            let bot2 = temp.as_ptr().add(row + 2 * step + border + j - 1);
            let b2m1 = _mm256_loadu_si256(bot2 as *const __m256i);
            let b2p1 = _mm256_loadu_si256(bot2.add(2) as *const __m256i);
            let b1m2 = _mm256_loadu_si256(bot1 as *const __m256i);
            let b1m1 = _mm256_loadu_si256(bot1.add(1) as *const __m256i);
            let b1 = _mm256_loadu_si256(bot1.add(2) as *const __m256i);
            let b1p1 = _mm256_loadu_si256(bot1.add(3) as *const __m256i);
            let b1p2 = _mm256_loadu_si256(bot1.add(4) as *const __m256i);
            let m = _mm256_min_epu32(
                _mm256_min_epu32(
                    _mm256_min_epu32(
                        _mm256_add_epi32(b2m1, v_long),
                        _mm256_add_epi32(b2p1, v_long),
                    ),
                    _mm256_min_epu32(
                        _mm256_add_epi32(b1m2, v_long),
                        _mm256_add_epi32(b1m1, v_diag),
                    ),
                ),
                _mm256_min_epu32(
                    _mm256_min_epu32(
                        _mm256_add_epi32(b1, v_hv),
                        _mm256_add_epi32(b1p1, v_diag),
                    ),
                    _mm256_add_epi32(b1p2, v_long),
                ),
            );
            let t0 = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let mut a = _mm256_min_epu32(m, t0);
            // 后缀扫描：s=1,2,4
            let src = _mm256_permutevar8x32_epi32(a, idx1);
            let src = _mm256_min_epu32(src, v_c1);
            let t = _mm256_add_epi32(src, v_s1);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m1);
            let src = _mm256_permutevar8x32_epi32(a, idx2);
            let src = _mm256_min_epu32(src, v_c2);
            let t = _mm256_add_epi32(src, v_s2);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m2);
            let src = _mm256_permutevar8x32_epi32(a, idx4);
            let src = _mm256_min_epu32(src, v_c4);
            let t = _mm256_add_epi32(src, v_s4);
            let mn = _mm256_min_epu32(a, t);
            a = _mm256_blendv_epi8(a, mn, m4);
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, a);
        }
    }

    // Phase 2+3: 串行 stitch + 折叠 + 定点→浮点（块从右到左，lane0 输出即下一块 incoming）
    let mut r = temp[row + border + w];
    unsafe {
        for b in (0..nb).rev() {
            let j = base + 8 * b;
            let local = _mm256_loadu_si256(temp.as_ptr().add(row + border + j) as *const __m256i);
            let vr = _mm256_set1_epi32(r as i32);
            let t = _mm256_add_epi32(vr, v_ramp2);
            // 回绕检测：t < vr（无符号）
            let tf = _mm256_xor_si256(t, _mm256_set1_epi32(i32::MIN));
            let vrf = _mm256_xor_si256(vr, _mm256_set1_epi32(i32::MIN));
            let wrap = _mm256_cmpgt_epi32(vrf, tf);
            let t = _mm256_blendv_epi8(t, v_max, wrap);
            let v = _mm256_min_epu32(local, t);
            r = _mm256_extract_epi32(v, 0) as u32;
            _mm256_storeu_si256(temp.as_mut_ptr().add(row + border + j) as *mut __m256i, v);
            let f = _mm256_mul_ps(_mm256_cvtepi32_ps(v), v_scale);
            _mm256_storeu_ps(drow.as_mut_ptr().add(j), f);
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

// ==================== cvtColor（RGB2YCrCb / YCrCb2RGB） ====================

/// RGB u8 → YCrCb u8（AVX2，每批 32 像素）。
///
/// 对齐 OpenCV `RGB2YCrCb_i<uchar>` SIMD 路径（color_yuv.simd.hpp:398-566）。
/// 公式（RGB 输入，blueIdx=0）：
///   y  = (R*4899 + G*9617 + B*1868 + 8192) >> 14
///   cr = ((R - y)*11682 + 2105344) >> 14
///   cb = ((B - y)*9241  + 2105344) >> 14
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn rgb_to_ycrcb(src: &[u8], dst: &mut [u8], n: usize) {
    // SAFETY: 循环条件 i+32 <= n 保证读写界内；AVX2 由调用方保证
    unsafe {
        let mut i = 0;
        while i + 32 <= n {
            let a = _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i);
            let (r0, g0, b0) = deinterleave3(a, b, c);
            let (y, cr, cb) = rgb2ycrcb_32(b0, g0, r0);
            // 交织存储 YCrCb
            let out = interleave3(y, cr, cb);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, out.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, out.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, out.2);
            i += 32;
        }
        // 标量尾循环
        for j in i..n {
            let r = src[j * 3] as i32;
            let g = src[j * 3 + 1] as i32;
            let b = src[j * 3 + 2] as i32;
            let y = (r * 4899 + g * 9617 + b * 1868 + 8192) >> 14;
            let cr = ((r - y) * 11682 + 2105344) >> 14;
            let cb = ((b - y) * 9241 + 2105344) >> 14;
            dst[j * 3] = y.clamp(0, 255) as u8;
            dst[j * 3 + 1] = cr.clamp(0, 255) as u8;
            dst[j * 3 + 2] = cb.clamp(0, 255) as u8;
        }
    }
}

/// 32×u8 通道 → YCrCb（AVX2，`_mm256_madd_epi16` 点积）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn rgb2ycrcb_32(b: __m256i, g: __m256i, r: __m256i) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        // 零扩展 u8 → u16
        let b0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(b));
        let b1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(b, 1));
        let g0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(g));
        let g1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(g, 1));
        let r0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(r));
        let r1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(r, 1));

        // 交织 B/G 和 R/descale
        let (bg00, bg01) = v_zip(b0, g0);
        let (bg10, bg11) = v_zip(b1, g1);
        let (rd00, rd01) = v_zip(r0, DESCALE);
        let (rd10, rd11) = v_zip(r1, DESCALE);

        // y = (B*1868 + G*9617 + R*4899 + 8192) >> 14
        let y00 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg00, YC_BG2Y), _mm256_madd_epi16(rd00, YC_R12Y)),
            14,
        );
        let y01 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg01, YC_BG2Y), _mm256_madd_epi16(rd01, YC_R12Y)),
            14,
        );
        let y10 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg10, YC_BG2Y), _mm256_madd_epi16(rd10, YC_R12Y)),
            14,
        );
        let y11 = _mm256_srli_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(bg11, YC_BG2Y), _mm256_madd_epi16(rd11, YC_R12Y)),
            14,
        );

        // 打包 y u32 → u16
        let y0 = pack_u32_to_u16(y00, y01);
        let y1 = pack_u32_to_u16(y10, y11);

        // (r - y) 和 (b - y)，i16
        let sy0 = y0;
        let sy1 = y1;
        let sr0 = _mm256_sub_epi16(r0, sy0);
        let sr1 = _mm256_sub_epi16(r1, sy1);
        let sb0 = _mm256_sub_epi16(b0, sy0);
        let sb1 = _mm256_sub_epi16(b1, sy1);

        // cr = ((r-y)*11682 + 2105344) >> 14
        let (rd00, rd01) = v_zip(sr0, DESCALE);
        let (rd10, rd11) = v_zip(sr1, DESCALE);
        let cr00 = _mm256_srli_epi32(_mm256_madd_epi16(rd00, C3H), 14);
        let cr01 = _mm256_srli_epi32(_mm256_madd_epi16(rd01, C3H), 14);
        let cr10 = _mm256_srli_epi32(_mm256_madd_epi16(rd10, C3H), 14);
        let cr11 = _mm256_srli_epi32(_mm256_madd_epi16(rd11, C3H), 14);
        let cr0 = pack_u32_to_u16(cr00, cr01);
        let cr1 = pack_u32_to_u16(cr10, cr11);

        // cb = ((b-y)*9241 + 2105344) >> 14
        let (bd00, bd01) = v_zip(sb0, DESCALE);
        let (bd10, bd11) = v_zip(sb1, DESCALE);
        let cb00 = _mm256_srli_epi32(_mm256_madd_epi16(bd00, C4H), 14);
        let cb01 = _mm256_srli_epi32(_mm256_madd_epi16(bd01, C4H), 14);
        let cb10 = _mm256_srli_epi32(_mm256_madd_epi16(bd10, C4H), 14);
        let cb11 = _mm256_srli_epi32(_mm256_madd_epi16(bd11, C4H), 14);
        let cb0 = pack_u32_to_u16(cb00, cb01);
        let cb1 = pack_u32_to_u16(cb10, cb11);

        // 饱和打包 u16 → u8
        let y = pack_u16_to_u8(y0, y1);
        let cr = pack_u16_to_u8(cr0, cr1);
        let cb = pack_u16_to_u8(cb0, cb1);
        (y, cr, cb)
    }
}

/// 交织 3 通道 u8（OpenCV `v_store_interleave`，intrin_avx.hpp:2431）。
/// 输入 3 个 32×u8 通道向量，输出 3 个 32 字节交错向量（96 字节）。
#[inline]
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn interleave3(a: __m256i, b: __m256i, c: __m256i) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    // 用 interleave3_store 的正确常量（v_store_interleave）
    const SH_B: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8,
            3, 14, 9, 4, 15, 10, 5,
        ])
    };
    const SH_G: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2, 13,
            8, 3, 14, 9, 4, 15, 10,
        ])
    };
    const SH_R: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            10, 5, 0, 11, 6, 1, 12, 7, 2, 13, 8, 3, 14, 9, 4, 15, 10, 5, 0, 11, 6, 1, 12, 7, 2,
            13, 8, 3, 14, 9, 4, 15,
        ])
    };
    const M0: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0,
            0, -1, 0, 0, -1, 0, 0,
        ])
    };
    const M1: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, -1, 0, 0, 0, -1, 0, 0, -1, 0, 0, -1,
            0, 0, -1, 0, 0, -1, 0,
        ])
    };
    unsafe {
        let b0 = _mm256_shuffle_epi8(a, SH_B);
        let g0 = _mm256_shuffle_epi8(b, SH_G);
        let r0 = _mm256_shuffle_epi8(c, SH_R);
        let p0 = _mm256_blendv_epi8(_mm256_blendv_epi8(b0, g0, M0), r0, M1);
        let p1 = _mm256_blendv_epi8(_mm256_blendv_epi8(g0, r0, M0), b0, M1);
        let p2 = _mm256_blendv_epi8(_mm256_blendv_epi8(r0, b0, M0), g0, M1);
        let o0 = _mm256_permute2x128_si256(p0, p1, 0x20);
        let o1 = _mm256_permute2x128_si256(p2, p0, 0x30);
        let o2 = _mm256_permute2x128_si256(p1, p2, 0x31);
        (o0, o1, o2)
    }
}

/// YCrCb u8 → RGB u8（AVX2，每批 32 像素）。
///
/// 对齐 OpenCV `YCrCb2RGB_i<uchar>` SIMD 路径（color_yuv.simd.hpp:739-886）。
/// 公式（YCrCb 输入，blueIdx=0）：
///   b = Y + ((Cb-128)*29049 + 8192) >> 14
///   g = Y + ((Cb-128)*(-5636) + (Cr-128)*(-11698) + 8192) >> 14
///   r = Y + ((Cr-128)*22987 + 8192) >> 14
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn ycrcb_to_rgb(src: &[u8], dst: &mut [u8], n: usize) {
    // SAFETY: 循环条件 i+32 <= n 保证读写界内；AVX2 由调用方保证
    unsafe {
        let mut i = 0;
        while i + 32 <= n {
            let a = _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i);
            let (y, cr, cb) = deinterleave3(a, b, c);
            let (r, g, b) = ycrcb2rgb_32(y, cr, cb);
            let out = interleave3(r, g, b);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, out.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, out.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, out.2);
            i += 32;
        }
        // 标量尾循环
        for j in i..n {
            let y = src[j * 3] as i32;
            let cr = src[j * 3 + 1] as i32;
            let cb = src[j * 3 + 2] as i32;
            let b = y + (((cb - 128) * 29049 + 8192) >> 14);
            let g = y + ((((cb - 128) * -5636 + (cr - 128) * -11698) + 8192) >> 14);
            let r = y + (((cr - 128) * 22987 + 8192) >> 14);
            dst[j * 3] = r.clamp(0, 255) as u8;
            dst[j * 3 + 1] = g.clamp(0, 255) as u8;
            dst[j * 3 + 2] = b.clamp(0, 255) as u8;
        }
    }
}

/// 32×u8 YCrCb 通道 → RGB（AVX2）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn ycrcb2rgb_32(y: __m256i, cr: __m256i, cb: __m256i) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        // cr/cb 减 128（有符号）
        let cr = _mm256_sub_epi8(cr, DELTA8);
        let cb = _mm256_sub_epi8(cb, DELTA8);
        // 符号扩展 i8 → i16
        let cr0 = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(cr));
        let cr1 = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(cr, 1));
        let cb0 = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(cb));
        let cb1 = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(cb, 1));

        // b = (Cb-128)*29049
        let (b00, b01) = v_mul_expand(cb0, VC3);
        let (b10, b11) = v_mul_expand(cb1, VC3);
        // g = (Cb-128)*(-5636) + (Cr-128)*(-11698)
        let (t00, t01) = v_mul_expand(cb0, VC2);
        let (t10, t11) = v_mul_expand(cb1, VC2);
        let (g00, g01) = v_mul_expand(cr0, VC1);
        let (g10, g11) = v_mul_expand(cr1, VC1);
        let g00 = _mm256_add_epi32(g00, t00);
        let g01 = _mm256_add_epi32(g01, t01);
        let g10 = _mm256_add_epi32(g10, t10);
        let g11 = _mm256_add_epi32(g11, t11);
        // r = (Cr-128)*22987
        let (r00, r01) = v_mul_expand(cr0, VC0);
        let (r10, r11) = v_mul_expand(cr1, VC0);

        // + descale >> 14
        let b00 = _mm256_srai_epi32(_mm256_add_epi32(b00, DESCALE32), 14);
        let b01 = _mm256_srai_epi32(_mm256_add_epi32(b01, DESCALE32), 14);
        let b10 = _mm256_srai_epi32(_mm256_add_epi32(b10, DESCALE32), 14);
        let b11 = _mm256_srai_epi32(_mm256_add_epi32(b11, DESCALE32), 14);
        let g00 = _mm256_srai_epi32(_mm256_add_epi32(g00, DESCALE32), 14);
        let g01 = _mm256_srai_epi32(_mm256_add_epi32(g01, DESCALE32), 14);
        let g10 = _mm256_srai_epi32(_mm256_add_epi32(g10, DESCALE32), 14);
        let g11 = _mm256_srai_epi32(_mm256_add_epi32(g11, DESCALE32), 14);
        let r00 = _mm256_srai_epi32(_mm256_add_epi32(r00, DESCALE32), 14);
        let r01 = _mm256_srai_epi32(_mm256_add_epi32(r01, DESCALE32), 14);
        let r10 = _mm256_srai_epi32(_mm256_add_epi32(r10, DESCALE32), 14);
        let r11 = _mm256_srai_epi32(_mm256_add_epi32(r11, DESCALE32), 14);

        // 打包 i32 → i16
        let b0 = pack_i32_to_i16(b00, b01);
        let b1 = pack_i32_to_i16(b10, b11);
        let g0 = pack_i32_to_i16(g00, g01);
        let g1 = pack_i32_to_i16(g10, g11);
        let r0 = pack_i32_to_i16(r00, r01);
        let r1 = pack_i32_to_i16(r10, r11);

        // + Y（i16）
        let y0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(y));
        let y1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(y, 1));
        let b0 = _mm256_add_epi16(b0, y0);
        let b1 = _mm256_add_epi16(b1, y1);
        let g0 = _mm256_add_epi16(g0, y0);
        let g1 = _mm256_add_epi16(g1, y1);
        let r0 = _mm256_add_epi16(r0, y0);
        let r1 = _mm256_add_epi16(r1, y1);

        // 饱和打包 i16 → u8
        let b = pack_i16_to_u8(b0, b1);
        let g = pack_i16_to_u8(g0, g1);
        let r = pack_i16_to_u8(r0, r1);
        (r, g, b)
    }
}

/// `v_mul_expand`（i16 × i16 → i32 对，intrin_avx.hpp:530）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn v_mul_expand(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let lo = _mm256_mullo_epi16(a, b);
        let hi = _mm256_mulhi_epi16(a, b);
        v_zip(lo, hi)
    }
}

/// i32 → i16 饱和打包（`v_pack`，intrin_avx.hpp:536）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_i32_to_i16(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let packed = _mm256_packs_epi32(a, b);
        _mm256_permute4x64_epi64(packed, 0xD8)
    }
}

/// i16 → u8 饱和打包（`v_pack_u`，intrin_avx.hpp:540）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_i16_to_u8(a: __m256i, b: __m256i) -> __m256i {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let packed = _mm256_packus_epi16(a, b);
        _mm256_permute4x64_epi64(packed, 0xD8)
    }
}

// ---- cvtColor 常量向量 ----
static DESCALE: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([8192; 16]) };
static DESCALE32: __m256i = unsafe { std::mem::transmute::<[i32; 8], __m256i>([8192; 8]) };
static DELTA8: __m256i = unsafe { std::mem::transmute::<[i8; 32], __m256i>([-128; 32]) };
// bg2y = [B2Y, G2Y] 交织 = [1868, 9617]
static YC_BG2Y: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868, 9617, 1868,
        9617,
    ])
};
// r12y = [R2Y, 1] 交织 = [4899, 1]
static YC_R12Y: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1, 4899, 1,
    ])
};
// c3h = [YCRI, h21] = [11682, 257]
static C3H: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682, 257, 11682,
        257,
    ])
};
// c4h = [YCBI, h21] = [9241, 257]
static C4H: __m256i = unsafe {
    std::mem::transmute::<[i16; 16], __m256i>([
        9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257, 9241, 257,
    ])
};
// YCrCb2RGB 系数
static VC0: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([22987; 16]) };
static VC1: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([-11698; 16]) };
static VC2: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([-5636; 16]) };
static VC3: __m256i = unsafe { std::mem::transmute::<[i16; 16], __m256i>([29049; 16]) };

// ==================== cvtColor（RGB2HSV） ====================

/// RGB u8 → HSV u8（AVX2，每批 32 像素）。
///
/// 对齐 OpenCV `RGB2HSV_b` SIMD 路径（color_hsv.simd.hpp:88-232）。
/// 整数定点（HSV_SHIFT=12），用查找表 sdiv/hdiv。
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn rgb_to_hsv(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    // SAFETY: 循环条件 i+32 <= n 保证读写界内；AVX2 由调用方保证
    unsafe {
        let sdiv = crate::imgproc::cvt_color::hsv_sdiv_table();
        let hdiv = crate::imgproc::cvt_color::hsv_hdiv_table(hrange);
        let mut i = 0;
        while i + 32 <= n {
            let a = _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i);
            let (r, g, b) = deinterleave3(a, b, c);
            let (h, s, v) = rgb2hsv_32(r, g, b, &sdiv, &hdiv, hrange);
            let out = interleave3(h, s, v);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, out.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, out.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, out.2);
            i += 32;
        }
        // 标量尾循环
        for j in i..n {
            let r = src[j * 3] as i32;
            let g = src[j * 3 + 1] as i32;
            let b = src[j * 3 + 2] as i32;
            let v = r.max(g).max(b);
            let vmin = r.min(g).min(b);
            let diff = (v - vmin) as u8;
            let vr = if v == r { -1 } else { 0 };
            let vg = if v == g { -1 } else { 0 };
            let s = (diff as i32 * sdiv[v as usize] + 2048) >> 12;
            let h = (vr & (g - b))
                + (!vr & ((vg & (b - r + 2 * diff as i32)) + ((!vg) & (r - g + 4 * diff as i32))));
            let h = (h * hdiv[diff as usize] + 2048) >> 12;
            let h = if h < 0 { h + hrange } else { h };
            dst[j * 3] = h as u8;
            dst[j * 3 + 1] = s as u8;
            dst[j * 3 + 2] = v as u8;
        }
    }
}

/// 32×u8 RGB 通道 → HSV（AVX2，查找表 + 整数定点）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn rgb2hsv_32(
    r: __m256i,
    g: __m256i,
    b: __m256i,
    sdiv: &[i32; 256],
    hdiv: &[i32; 256],
    hrange: i32,
) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        // v = max(r, g, b), vmin = min(r, g, b)
        let v = _mm256_max_epu8(r, _mm256_max_epu8(g, b));
        let vmin = _mm256_min_epu8(r, _mm256_min_epu8(g, b));
        let diff = _mm256_sub_epi8(v, vmin);

        // vr = (v == r) ? 0xFF : 0, vg = (v == g) ? 0xFF : 0
        let vr = _mm256_cmpeq_epi8(v, r);
        let vg = _mm256_cmpeq_epi8(v, g);

        // 展开 v 到 i32（用于 gather sdiv[v]）
        let v0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(v));
        let v1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(v, 1));
        let vq0 = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(v0));
        let vq1 = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(v0, 1));
        let vq2 = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(v1));
        let vq3 = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(v1, 1));

        // gather sdiv[v]
        let sdiv0 = _mm256_i32gather_epi32(sdiv.as_ptr(), vq0, 4);
        let sdiv1 = _mm256_i32gather_epi32(sdiv.as_ptr(), vq1, 4);
        let sdiv2 = _mm256_i32gather_epi32(sdiv.as_ptr(), vq2, 4);
        let sdiv3 = _mm256_i32gather_epi32(sdiv.as_ptr(), vq3, 4);

        // 展开 diff 到 i32（用于 gather hdiv[diff]）
        let diff0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(diff));
        let diff1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(diff, 1));
        let diffq0 = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(diff0));
        let diffq1 = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(diff0, 1));
        let diffq2 = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(diff1));
        let diffq3 = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(diff1, 1));

        // gather hdiv[diff]
        let hdiv0 = _mm256_i32gather_epi32(hdiv.as_ptr(), diffq0, 4);
        let hdiv1 = _mm256_i32gather_epi32(hdiv.as_ptr(), diffq1, 4);
        let hdiv2 = _mm256_i32gather_epi32(hdiv.as_ptr(), diffq2, 4);
        let hdiv3 = _mm256_i32gather_epi32(hdiv.as_ptr(), diffq3, 4);

        // s = (diff * sdiv[v] + 2048) >> 12
        let descale = _mm256_set1_epi32(2048);
        let sq0 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(diffq0, sdiv0), descale), 12);
        let sq1 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(diffq1, sdiv1), descale), 12);
        let sq2 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(diffq2, sdiv2), descale), 12);
        let sq3 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(diffq3, sdiv3), descale), 12);
        let s0 = pack_i32_to_i16(sq0, sq1);
        let s1 = pack_i32_to_i16(sq2, sq3);
        let s = pack_i16_to_u8(s0, s1);

        // 展开 r, g, b, diff 到 i16
        let r0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(r));
        let r1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(r, 1));
        let g0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(g));
        let g1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(g, 1));
        let b0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(b));
        let b1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(b, 1));
        let diff0 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(diff));
        let diff1 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256(diff, 1));

        // vr/vg 符号扩展 i8 → i16
        let vr0 = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vr));
        let vr1 = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vr, 1));
        let vg0 = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vg));
        let vg1 = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vg, 1));

        // h = (vr & (g - b)) + (~vr & ((vg & (b - r + 2*diff)) + ((~vg) & (r - g + 4*diff))))
        let hd0 = h_before_div(r0, g0, b0, diff0, vr0, vg0);
        let hd1 = h_before_div(r1, g1, b1, diff1, vr1, vg1);

        // 展开 hd 到 i32
        let hq0 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(hd0));
        let hq1 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(hd0, 1));
        let hq2 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(hd1));
        let hq3 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(hd1, 1));

        // h = (h * hdiv[diff] + 2048) >> 12
        let hq0 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(hq0, hdiv0), descale), 12);
        let hq1 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(hq1, hdiv1), descale), 12);
        let hq2 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(hq2, hdiv2), descale), 12);
        let hq3 = _mm256_srai_epi32(_mm256_add_epi32(_mm256_mullo_epi32(hq3, hdiv3), descale), 12);

        // h = h < 0 ? h + hrange : h
        let h0 = pack_i32_to_i16(hq0, hq1);
        let h1 = pack_i32_to_i16(hq2, hq3);
        let vhr = _mm256_set1_epi16(hrange as i16);
        let h0 = _mm256_add_epi16(h0, _mm256_and_si256(_mm256_cmpgt_epi16(_mm256_setzero_si256(), h0), vhr));
        let h1 = _mm256_add_epi16(h1, _mm256_and_si256(_mm256_cmpgt_epi16(_mm256_setzero_si256(), h1), vhr));
        let h = pack_i16_to_u8(h0, h1);

        (h, s, v)
    }
}

/// h 除法前的计算（i16）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn h_before_div(
    r: __m256i,
    g: __m256i,
    b: __m256i,
    diff: __m256i,
    vr: __m256i,
    vg: __m256i,
) -> __m256i {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let gb = _mm256_sub_epi16(g, b);
        let br = _mm256_add_epi16(_mm256_sub_epi16(b, r), _mm256_slli_epi16(diff, 1));
        let rg = _mm256_add_epi16(_mm256_sub_epi16(r, g), _mm256_slli_epi16(diff, 2));
        let not_vr = _mm256_xor_si256(vr, _mm256_set1_epi16(-1));
        let not_vg = _mm256_xor_si256(vg, _mm256_set1_epi16(-1));
        let inner = _mm256_add_epi16(
            _mm256_and_si256(vg, br),
            _mm256_and_si256(not_vg, rg),
        );
        _mm256_add_epi16(
            _mm256_and_si256(vr, gb),
            _mm256_and_si256(not_vr, inner),
        )
    }
}

// ==================== cvtColor（HSV2RGB） ====================

/// HSV u8 → RGB u8（AVX2，每批 32 像素）。
///
/// 对齐 OpenCV `HSV2RGB_b` SIMD 路径（color_hsv.simd.hpp:533-645）。
/// 浮点运算，FMA 融合（vfnmadd132ps），输出截断。
///
/// # Safety
/// - `src` 长度 ≥ `n*3`，`dst` 长度 ≥ `n*3`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn hsv_to_rgb(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    // SAFETY: 循环条件 i+32 <= n 保证读写界内；AVX2 由调用方保证
    unsafe {
        let hscale = 6.0f32 / hrange as f32;
        let mut i = 0;
        while i + 32 <= n {
            let a = _mm256_loadu_si256(src.as_ptr().add(i * 3) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.as_ptr().add(i * 3 + 64) as *const __m256i);
            let (h, s, v) = deinterleave3(a, b, c);
            let (r, g, b) = hsv2rgb_32(h, s, v, hscale);
            let out = interleave3(r, g, b);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3) as *mut __m256i, out.0);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 32) as *mut __m256i, out.1);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i * 3 + 64) as *mut __m256i, out.2);
            i += 32;
        }
        // 标量尾循环（RNE，无 FMA）
        for j in i..n {
            let h = src[j * 3] as f32;
            let s = src[j * 3 + 1] as f32 * (1.0f32 / 255.0f32);
            let v = src[j * 3 + 2] as f32 * (1.0f32 / 255.0f32);
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
                let sd = [[1usize, 3, 0], [1, 0, 2], [3, 0, 1], [0, 2, 1], [0, 1, 3], [2, 1, 0]];
                let sd = sd[sector as usize];
                (tab[sd[0]], tab[sd[1]], tab[sd[2]])
            };
            dst[j * 3] = (r * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
            dst[j * 3 + 1] = (g * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
            dst[j * 3 + 2] = (b * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
        }
    }
}

/// 32×u8 HSV 通道 → RGB（AVX2，浮点 + FMA + 截断）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn hsv2rgb_32(h: __m256i, s: __m256i, v: __m256i, hscale: f32) -> (__m256i, __m256i, __m256i) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        // 32 像素分 4 个 8 像素块（低/高 128 位各 16 像素，再分 8 像素）
        let h_lo = _mm256_castsi256_si128(h);
        let h_hi = _mm256_extracti128_si256(h, 1);
        let s_lo = _mm256_castsi256_si128(s);
        let s_hi = _mm256_extracti128_si256(s, 1);
        let v_lo = _mm256_castsi256_si128(v);
        let v_hi = _mm256_extracti128_si256(v, 1);

        let h0 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(h_lo));
        let h1 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(h_lo, 8)));
        let h2 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(h_hi));
        let h3 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(h_hi, 8)));
        let s0 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(s_lo));
        let s1 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(s_lo, 8)));
        let s2 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(s_hi));
        let s3 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(s_hi, 8)));
        let v0 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v_lo));
        let v1 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(v_lo, 8)));
        let v2 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v_hi));
        let v3 = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(_mm_srli_si128(v_hi, 8)));

        let inv255 = _mm256_set1_ps(1.0f32 / 255.0f32);
        let s0 = _mm256_mul_ps(s0, inv255);
        let s1 = _mm256_mul_ps(s1, inv255);
        let s2 = _mm256_mul_ps(s2, inv255);
        let s3 = _mm256_mul_ps(s3, inv255);
        let v0 = _mm256_mul_ps(v0, inv255);
        let v1 = _mm256_mul_ps(v1, inv255);
        let v2 = _mm256_mul_ps(v2, inv255);
        let v3 = _mm256_mul_ps(v3, inv255);

        let (r0, g0, b0) = hsv2rgb_8(h0, s0, v0, hscale);
        let (r1, g1, b1) = hsv2rgb_8(h1, s1, v1, hscale);
        let (r2, g2, b2) = hsv2rgb_8(h2, s2, v2, hscale);
        let (r3, g3, b3) = hsv2rgb_8(h3, s3, v3, hscale);

        // 打包 f32 → u8（截断）
        let r = pack_f32_to_u8_trunc4(r0, r1, r2, r3);
        let g = pack_f32_to_u8_trunc4(g0, g1, g2, g3);
        let b = pack_f32_to_u8_trunc4(b0, b1, b2, b3);
        (r, g, b)
    }
}

/// 8 像素 HSV → RGB（AVX2，浮点 + FMA + 截断）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn hsv2rgb_8(h: __m256, s: __m256, v: __m256, hscale: f32) -> (__m256, __m256, __m256) {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let hscale_v = _mm256_set1_ps(hscale);
        let h = _mm256_mul_ps(h, hscale_v);

        // pre_sector = trunc(h)
        let pre_sector = _mm256_round_ps(h, 0b1011);
        let h = _mm256_sub_ps(h, pre_sector);

        // tab0 = v, tab1 = v*(1-s), tab2 = v*(1-s*h), tab3 = v*(1-s*(1-h))
        let one = _mm256_set1_ps(1.0);
        let tab0 = v;
        let tab1 = _mm256_mul_ps(v, _mm256_sub_ps(one, s));
        // tab2 = v * fnmadd(s, h, 1)  (FMA)
        let tab2 = _mm256_mul_ps(v, _mm256_fnmadd_ps(s, h, one));
        let one_minus_h = _mm256_sub_ps(one, h);
        // tab3 = v * fnmadd(s, (1-h), 1)  (FMA)
        let tab3 = _mm256_mul_ps(v, _mm256_fnmadd_ps(s, one_minus_h, one));

        // sector = trunc(pre_sector * (1/6))
        let one_sixth = _mm256_set1_ps(1.0f32 / 6.0f32);
        let sector = _mm256_round_ps(_mm256_mul_ps(pre_sector, one_sixth), 0b1011);
        let six = _mm256_set1_ps(6.0);
        let sector = _mm256_sub_ps(pre_sector, _mm256_mul_ps(sector, six));

        // 根据 sector 选择 tab（对齐 OpenCV HSV2RGB_simd 的 v_select 链）
        let two = _mm256_set1_ps(2.0);
        let three = _mm256_set1_ps(3.0);
        let four = _mm256_set1_ps(4.0);
        let zero = _mm256_setzero_ps();

        // b = tab1 (sector<2), tab3 (sector==2), tab0 (sector==3), tab0 (sector==4), tab2 (sector>4)
        let mut b = _mm256_blendv_ps(zero, tab1, _mm256_cmp_ps(sector, two, _CMP_LT_OQ));
        b = _mm256_blendv_ps(b, tab3, _mm256_cmp_ps(sector, two, _CMP_EQ_OQ));
        b = _mm256_blendv_ps(b, tab0, _mm256_cmp_ps(sector, three, _CMP_EQ_OQ));
        b = _mm256_blendv_ps(b, tab0, _mm256_cmp_ps(sector, four, _CMP_EQ_OQ));
        b = _mm256_blendv_ps(b, tab2, _mm256_cmp_ps(sector, four, _CMP_GT_OQ));

        // g = tab3 (sector<1), tab0 (sector==1), tab0 (sector==2), tab2 (sector==3), tab1 (sector>3)
        let one_f = _mm256_set1_ps(1.0);
        let mut g = _mm256_blendv_ps(zero, tab3, _mm256_cmp_ps(sector, one_f, _CMP_LT_OQ));
        g = _mm256_blendv_ps(g, tab0, _mm256_cmp_ps(sector, one_f, _CMP_EQ_OQ));
        g = _mm256_blendv_ps(g, tab0, _mm256_cmp_ps(sector, two, _CMP_EQ_OQ));
        g = _mm256_blendv_ps(g, tab2, _mm256_cmp_ps(sector, three, _CMP_EQ_OQ));
        g = _mm256_blendv_ps(g, tab1, _mm256_cmp_ps(sector, three, _CMP_GT_OQ));

        // r = tab0 (sector<1), tab2 (sector==1), tab1 (sector==2), tab1 (sector==3), tab3 (sector==4), tab0 (sector>4)
        let mut r = _mm256_blendv_ps(zero, tab0, _mm256_cmp_ps(sector, one_f, _CMP_LT_OQ));
        r = _mm256_blendv_ps(r, tab2, _mm256_cmp_ps(sector, one_f, _CMP_EQ_OQ));
        r = _mm256_blendv_ps(r, tab1, _mm256_cmp_ps(sector, two, _CMP_EQ_OQ));
        r = _mm256_blendv_ps(r, tab1, _mm256_cmp_ps(sector, three, _CMP_EQ_OQ));
        r = _mm256_blendv_ps(r, tab3, _mm256_cmp_ps(sector, four, _CMP_EQ_OQ));
        r = _mm256_blendv_ps(r, tab0, _mm256_cmp_ps(sector, four, _CMP_GT_OQ));

        (r, g, b)
    }
}

/// f32 → u8 截断打包（对齐 OpenCV SIMD 路径的 v_trunc + v_pack_u）。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn pack_f32_to_u8_trunc4(a: __m256, b: __m256, c: __m256, d: __m256) -> __m256i {
    // SAFETY: 调用方保证 AVX2；纯向量运算
    unsafe {
        let scale = _mm256_set1_ps(255.0);
        let zero = _mm256_setzero_ps();
        let max255 = _mm256_set1_ps(255.0);
        // 截断 + clamp [0, 255]
        let a = _mm256_min_ps(_mm256_max_ps(_mm256_round_ps(_mm256_mul_ps(a, scale), 0b1011), zero), max255);
        let b = _mm256_min_ps(_mm256_max_ps(_mm256_round_ps(_mm256_mul_ps(b, scale), 0b1011), zero), max255);
        let c = _mm256_min_ps(_mm256_max_ps(_mm256_round_ps(_mm256_mul_ps(c, scale), 0b1011), zero), max255);
        let d = _mm256_min_ps(_mm256_max_ps(_mm256_round_ps(_mm256_mul_ps(d, scale), 0b1011), zero), max255);
        let a = _mm256_cvtps_epi32(a);
        let b = _mm256_cvtps_epi32(b);
        let c = _mm256_cvtps_epi32(c);
        let d = _mm256_cvtps_epi32(d);
        // 4×8 i32 → 2×16 u16 → 32 u8
        let ab = _mm256_packus_epi32(a, b);
        let cd = _mm256_packus_epi32(c, d);
        let ab = _mm256_permute4x64_epi64(ab, 0xD8);
        let cd = _mm256_permute4x64_epi64(cd, 0xD8);
        let abcd = _mm256_packus_epi16(ab, cd);
        _mm256_permute4x64_epi64(abcd, 0xD8)
    }
}

// ==================== 直方图（histogram） ====================

/// 8U 直方图（AVX2，查找表 + 局部累加，与 OpenCV `calcHist_8u` 一致）。
///
/// `src` 为灰度像素，`n` 为像素数，`hist` 长度 ≥ `hist_size`，`tab` 为 256 项查找表。
///
/// # Safety
/// - `src` 长度 ≥ `n`，`hist` 长度 ≥ `hist_size`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn calc_hist_8u(src: &[u8], n: usize, hist: &mut [i32], tab: &[usize; 256]) {
    // SAFETY: 循环条件 i+8 <= n 保证读写界内；AVX2 由调用方保证
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

/// 应用 LUT（AVX2，gather 查找表）。
///
/// `src` 为灰度像素，`dst` 为输出，`n` 为像素数，`lut` 为 256 项映射表。
///
/// # Safety
/// - `src` 长度 ≥ `n`，`dst` 长度 ≥ `n`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn apply_lut(src: &[u8], dst: &mut [u8], n: usize, lut: &[u8; 256]) {
    // SAFETY: 循环条件 i+32 <= n 保证读写界内；AVX2 由调用方保证
    unsafe {
        // 把 u8 LUT 展开为 i32（每项 4 字节），供 gather 使用
        let mut lut_i32 = [0i32; 256];
        for (i, &v) in lut.iter().enumerate() {
            lut_i32[i] = v as i32;
        }
        let lut_ptr = lut_i32.as_ptr();
        let mut i = 0;
        while i + 32 <= n {
            // 加载 32 个源像素
            let v = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
            // 展开到 i32（分 4 组，每组 8 个）
            let lo = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(v));
            let hi = _mm256_cvtepu8_epi32(_mm256_extracti128_si256(v, 1));
            let lo2 = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(_mm256_srli_si256(v, 8)));
            let hi2 = _mm256_cvtepu8_epi32(_mm256_castsi256_si128(_mm256_srli_si256(v, 24)));
            // gather 4 组（scale=4，i32 元素）
            let g0 = _mm256_i32gather_epi32(lut_ptr, lo, 4);
            let g1 = _mm256_i32gather_epi32(lut_ptr, hi, 4);
            let g2 = _mm256_i32gather_epi32(lut_ptr, lo2, 4);
            let g3 = _mm256_i32gather_epi32(lut_ptr, hi2, 4);
            // 4×8 i32 → 2×16 u16 → 32 u8
            let ab = _mm256_packus_epi32(g0, g1);
            let cd = _mm256_packus_epi32(g2, g3);
            let ab = _mm256_permute4x64_epi64(ab, 0xD8);
            let cd = _mm256_permute4x64_epi64(cd, 0xD8);
            let abcd = _mm256_packus_epi16(ab, cd);
            let abcd = _mm256_permute4x64_epi64(abcd, 0xD8);
            _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, abcd);
            i += 32;
        }
        while i < n {
            *dst.get_unchecked_mut(i) = *lut.get_unchecked(*src.get_unchecked(i) as usize);
            i += 1;
        }
    }
}

// ==================== 直方图比较（compare_hist） ====================

/// 直方图比较（AVX2，2-lane f64 累加，与 OpenCV SSE SIMD 路径位级一致）。
///
/// `h1`/`h2` 为等长 `f32` 直方图。返回 `f64` 距离/相似度。
///
/// # Safety
/// - `h1`/`h2` 长度相等
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn compare_hist(
    h1: &[f32],
    h2: &[f32],
    method: crate::simd_kernels::histogram::HistCompMethod,
) -> f64 {
    use crate::simd_kernels::histogram::HistCompMethod;
    let len = h1.len();
    let dbl_eps = f64::EPSILON;
    let flt_eps = f32::EPSILON;

    match method {
        HistCompMethod::ChiSqr | HistCompMethod::ChiSqrAlt => {
            // 2-lane f64 累加（模拟 OpenCV SSE SIMD 路径）
            let mut lanes = _mm_setzero_pd();
            let mut j = 0;
            while j + 4 <= len {
                let a = _mm_loadu_ps(h1.as_ptr().add(j));
                let b = _mm_loadu_ps(h2.as_ptr().add(j));
                // 低 2 个 f32 → 2 f64
                let a_lo = _mm_cvtps_pd(a);
                let b_lo = _mm_cvtps_pd(b);
                // 高 2 个 f32 → 2 f64
                let a_hi = _mm_cvtps_pd(_mm_movehl_ps(a, a));
                let b_hi = _mm_cvtps_pd(_mm_movehl_ps(b, b));
                // 低部分
                let da = _mm_sub_pd(a_lo, b_lo);
                let db = match method {
                    HistCompMethod::ChiSqr => a_lo,
                    _ => _mm_add_pd(a_lo, b_lo),
                };
                let term = _mm_mul_pd(_mm_mul_pd(da, da), _mm_div_pd(_mm_set1_pd(1.0), db));
                let mask = _mm_cmpgt_pd(_mm_andnot_pd(_mm_set1_pd(-0.0), db), _mm_set1_pd(dbl_eps));
                lanes = _mm_add_pd(lanes, _mm_and_pd(term, mask));
                // 高部分
                let da = _mm_sub_pd(a_hi, b_hi);
                let db = match method {
                    HistCompMethod::ChiSqr => a_hi,
                    _ => _mm_add_pd(a_hi, b_hi),
                };
                let term = _mm_mul_pd(_mm_mul_pd(da, da), _mm_div_pd(_mm_set1_pd(1.0), db));
                let mask = _mm_cmpgt_pd(_mm_andnot_pd(_mm_set1_pd(-0.0), db), _mm_set1_pd(dbl_eps));
                lanes = _mm_add_pd(lanes, _mm_and_pd(term, mask));
                j += 4;
            }
            let mut result = _mm_cvtsd_f64(_mm_hadd_pd(lanes, lanes));
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
            let mut s1 = _mm_setzero_pd();
            let mut s2 = _mm_setzero_pd();
            let mut s11 = _mm_setzero_pd();
            let mut s12 = _mm_setzero_pd();
            let mut s22 = _mm_setzero_pd();
            let mut j = 0;
            while j + 4 <= len {
                let a = _mm_loadu_ps(h1.as_ptr().add(j));
                let b = _mm_loadu_ps(h2.as_ptr().add(j));
                let a_lo = _mm_cvtps_pd(a);
                let b_lo = _mm_cvtps_pd(b);
                let a_hi = _mm_cvtps_pd(_mm_movehl_ps(a, a));
                let b_hi = _mm_cvtps_pd(_mm_movehl_ps(b, b));
                s12 = _mm_add_pd(s12, _mm_mul_pd(a_lo, b_lo));
                s1 = _mm_add_pd(s1, a_lo);
                s11 = _mm_add_pd(s11, _mm_mul_pd(a_lo, a_lo));
                s2 = _mm_add_pd(s2, b_lo);
                s22 = _mm_add_pd(s22, _mm_mul_pd(b_lo, b_lo));
                s12 = _mm_add_pd(s12, _mm_mul_pd(a_hi, b_hi));
                s1 = _mm_add_pd(s1, a_hi);
                s11 = _mm_add_pd(s11, _mm_mul_pd(a_hi, a_hi));
                s2 = _mm_add_pd(s2, b_hi);
                s22 = _mm_add_pd(s22, _mm_mul_pd(b_hi, b_hi));
                j += 4;
            }
            let mut s1 = _mm_cvtsd_f64(_mm_hadd_pd(s1, s1));
            let mut s2 = _mm_cvtsd_f64(_mm_hadd_pd(s2, s2));
            let mut s11 = _mm_cvtsd_f64(_mm_hadd_pd(s11, s11));
            let mut s12 = _mm_cvtsd_f64(_mm_hadd_pd(s12, s12));
            let mut s22 = _mm_cvtsd_f64(_mm_hadd_pd(s22, s22));
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
            let mut lanes = _mm_setzero_pd();
            let mut j = 0;
            while j + 4 <= len {
                let a = _mm_loadu_ps(h1.as_ptr().add(j));
                let b = _mm_loadu_ps(h2.as_ptr().add(j));
                let mn = _mm_min_ps(a, b);
                let mn_lo = _mm_cvtps_pd(mn);
                let mn_hi = _mm_cvtps_pd(_mm_movehl_ps(mn, mn));
                lanes = _mm_add_pd(lanes, mn_lo);
                lanes = _mm_add_pd(lanes, mn_hi);
                j += 4;
            }
            let mut result = _mm_cvtsd_f64(_mm_hadd_pd(lanes, lanes));
            for idx in j..len {
                result += h1[idx].min(h2[idx]) as f64;
            }
            result
        }
        HistCompMethod::Bhattacharyya => {
            let mut s1 = _mm_setzero_pd();
            let mut s2 = _mm_setzero_pd();
            let mut result = _mm_setzero_pd();
            let mut j = 0;
            while j + 4 <= len {
                let a = _mm_loadu_ps(h1.as_ptr().add(j));
                let b = _mm_loadu_ps(h2.as_ptr().add(j));
                let a_lo = _mm_cvtps_pd(a);
                let b_lo = _mm_cvtps_pd(b);
                let a_hi = _mm_cvtps_pd(_mm_movehl_ps(a, a));
                let b_hi = _mm_cvtps_pd(_mm_movehl_ps(b, b));
                result = _mm_add_pd(result, _mm_sqrt_pd(_mm_mul_pd(a_lo, b_lo)));
                s1 = _mm_add_pd(s1, a_lo);
                s2 = _mm_add_pd(s2, b_lo);
                result = _mm_add_pd(result, _mm_sqrt_pd(_mm_mul_pd(a_hi, b_hi)));
                s1 = _mm_add_pd(s1, a_hi);
                s2 = _mm_add_pd(s2, b_hi);
                j += 4;
            }
            let mut s1 = _mm_cvtsd_f64(_mm_hadd_pd(s1, s1));
            let mut s2 = _mm_cvtsd_f64(_mm_hadd_pd(s2, s2));
            let mut result = _mm_cvtsd_f64(_mm_hadd_pd(result, result));
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
            // OpenCV 无 SIMD 路径，顺序累加
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

// ==================== 积分图（integral） ====================

/// u8→i32 积分图（AVX2，对齐 OpenCV `Integral_SIMD<uchar, int, double>`）。
///
/// `src` 为 `w*h` 灰度像素，`sum` 为 `(w+1)*(h+1)` 输出（首行首列 0）。
///
/// # Safety
/// - `src.len() == w*h`，`sum.len() == (w+1)*(h+1)`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn integral_u8_i32(src: &[u8], w: usize, h: usize, sum: &mut [i32]) {
    use std::arch::x86_64::*;
    let sumstep = w + 1;
    let mut y = 0usize;
    // 每行处理 16 像素（AVX2 一次 16 个 u8 → 16 个 i16 → 前缀和）
    while y < h {
        let src_row = &src[y * w..(y + 1) * w];
        let cur = (y + 1) * sumstep;
        let prev = y * sumstep;
        sum[cur] = 0;

        let mut s = 0i32;
        let mut x = 0usize;
        while x + 16 <= w {
            // 加载 16 个 u8，扩展为 16 个 i16
            let v = _mm_loadu_si128(src_row.as_ptr().add(x) as *const __m128i);
            let v16 = _mm256_cvtepu8_epi16(v);
            // 前缀和：vsum[i] = sum of v[0..=i]（每个 128-bit lane 内）
            let vsum = _mm256_add_epi16(v16, _mm256_slli_si256::<2>(v16));
            let vsum = _mm256_add_epi16(vsum, _mm256_slli_si256::<4>(vsum));
            let vsum = _mm256_add_epi16(vsum, _mm256_slli_si256::<8>(vsum));
            // 低 8 个 i16 → 8 个 i32
            let lo16 = _mm256_castsi256_si128(vsum);
            let lo32 = _mm256_cvtepi16_epi32(lo16);
            // 高 8 个 i16 → 8 个 i32，加上低 lane 的最后一个元素（跨 lane 进位）
            let hi16 = _mm256_extracti128_si256(vsum, 1);
            let hi32 = _mm256_cvtepi16_epi32(hi16);
            let carry = _mm256_set1_epi32(_mm256_extract_epi16::<7>(vsum) as i32);
            let hi32 = _mm256_add_epi32(hi32, carry);
            // 加上行内偏移 s
            let s_vec = _mm256_set1_epi32(s);
            let lo32 = _mm256_add_epi32(lo32, s_vec);
            let hi32 = _mm256_add_epi32(hi32, s_vec);
            // 加上上一行
            let prev_lo = _mm256_loadu_si256(sum.as_ptr().add(prev + x + 1) as *const __m256i);
            let prev_hi = _mm256_loadu_si256(sum.as_ptr().add(prev + x + 9) as *const __m256i);
            let lo32 = _mm256_add_epi32(lo32, prev_lo);
            let hi32 = _mm256_add_epi32(hi32, prev_hi);
            _mm256_storeu_si256(sum.as_mut_ptr().add(cur + x + 1) as *mut __m256i, lo32);
            _mm256_storeu_si256(sum.as_mut_ptr().add(cur + x + 9) as *mut __m256i, hi32);
            // 更新行内偏移 s = 前 16 个像素之和（不含上一行）
            // vsum[15] = 高 lane 前缀和（不含低 lane 进位），需加 carry
            s += _mm256_extract_epi16::<15>(vsum) as i32 + _mm256_extract_epi16::<7>(vsum) as i32;
            x += 16;
        }
        // 尾部标量
        while x < w {
            s += src_row[x] as i32;
            sum[cur + x + 1] = sum[prev + x + 1] + s;
            x += 1;
        }
        y += 1;
    }
}

/// u8→i32+f64 积分图（sum + sqsum，AVX2）。
///
/// `src` 为 `w*h` 灰度像素，`sum` 为 `(w+1)*(h+1)` i32 输出，`sqsum` 为
/// `(w+1)*(h+1)` f64 输出（首行首列 0）。
///
/// # Safety
/// - `src.len() == w*h`，`sum.len() == (w+1)*(h+1)`，`sqsum.len() == (w+1)*(h+1)`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn integral_u8_i32_sqsum(src: &[u8], w: usize, h: usize, sum: &mut [i32], sqsum: &mut [f64]) {
    use std::arch::x86_64::*;
    let sumstep = w + 1;
    let shmask = _mm256_set1_epi32(7);
    for y in 0..h {
        let src_row = &src[y * w..(y + 1) * w];
        let cur = (y + 1) * sumstep;
        let prev = y * sumstep;
        sum[cur] = 0;
        sqsum[cur] = 0.0;

        let mut prev_row = _mm256_setzero_si256();
        let mut prev_sq = _mm256_setzero_pd();
        let mut x = 0usize;
        while x + 16 <= w {
            // 加载 16 个 u8，扩展为 16 个 i16
            let v = _mm_loadu_si128(src_row.as_ptr().add(x) as *const __m128i);
            let v16 = _mm256_cvtepu8_epi16(v);
            // 前缀和（sum）：vsum[i] = sum of v[0..=i]（每个 128-bit lane 内）
            let vsum = _mm256_add_epi16(v16, _mm256_slli_si256::<2>(v16));
            let vsum = _mm256_add_epi16(vsum, _mm256_slli_si256::<4>(vsum));
            let vsum = _mm256_add_epi16(vsum, _mm256_slli_si256::<8>(vsum));
            let lo32 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(vsum));
            let hi32 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(vsum, 1));
            // 行内前缀和：el4l = lo32 + prev_row, el4h = hi32 + broadcast(el4l[7])
            let el4l = _mm256_add_epi32(lo32, prev_row);
            let carry = _mm256_permutevar8x32_epi32(el4l, shmask);
            let el4h = _mm256_add_epi32(hi32, carry);
            prev_row = _mm256_permutevar8x32_epi32(el4h, shmask);

            // 平方前缀和（sqsum）：sq[i] = v[i]^2（i32 避免溢出）
            let vlo32 = _mm256_cvtepi16_epi32(_mm256_castsi256_si128(v16));
            let vhi32 = _mm256_cvtepi16_epi32(_mm256_extracti128_si256(v16, 1));
            let sqlo = _mm256_mullo_epi32(vlo32, vlo32);
            let sqhi = _mm256_mullo_epi32(vhi32, vhi32);
            // i32 前缀和（每个 128-bit lane 内 4 个 i32）
            let sqlo = _mm256_add_epi32(sqlo, _mm256_slli_si256::<4>(sqlo));
            let sqlo = _mm256_add_epi32(sqlo, _mm256_slli_si256::<8>(sqlo));
            let sqlo = _mm256_add_epi32(sqlo, _mm256_slli_si256::<16>(sqlo));
            let sqhi = _mm256_add_epi32(sqhi, _mm256_slli_si256::<4>(sqhi));
            let sqhi = _mm256_add_epi32(sqhi, _mm256_slli_si256::<8>(sqhi));
            let sqhi = _mm256_add_epi32(sqhi, _mm256_slli_si256::<16>(sqhi));
            // 跨 lane 进位：sqlo 高 lane 加低 lane 最后一个元素（sqlo[3]）
            let scarry_lo = _mm256_permutevar8x32_epi32(sqlo, _mm256_set1_epi32(3));
            let scarry_lo = _mm256_blend_epi32(_mm256_setzero_si256(), scarry_lo, 0xF0);
            let sqlo = _mm256_add_epi32(sqlo, scarry_lo);
            // 跨 lane 进位：sqhi 高 lane 加低 lane 最后一个元素（sqhi[3]）
            let scarry_hi = _mm256_permutevar8x32_epi32(sqhi, _mm256_set1_epi32(3));
            let scarry_hi = _mm256_blend_epi32(_mm256_setzero_si256(), scarry_hi, 0xF0);
            let sqhi = _mm256_add_epi32(sqhi, scarry_hi);
            // 跨 lane 进位：sqhi 加 sqlo 的最后一个元素（sqlo[7]）
            let scarry = _mm256_permutevar8x32_epi32(sqlo, shmask);
            let sqhi = _mm256_add_epi32(sqhi, scarry);
            // 转 f64（4 组 × 4 个 f64）
            let sqlo64a = _mm256_cvtepi32_pd(_mm256_castsi256_si128(sqlo));
            let sqlo64b = _mm256_cvtepi32_pd(_mm256_extracti128_si256(sqlo, 1));
            let sqhi64a = _mm256_cvtepi32_pd(_mm256_castsi256_si128(sqhi));
            let sqhi64b = _mm256_cvtepi32_pd(_mm256_extracti128_si256(sqhi, 1));
            // 行内前缀和（sqsum）：加 prev_sq（广播到 4 组）
            let sel4l = _mm256_add_pd(sqlo64a, prev_sq);
            let sel4h = _mm256_add_pd(sqlo64b, prev_sq);
            let sel4l2 = _mm256_add_pd(sqhi64a, prev_sq);
            let sel4h2 = _mm256_add_pd(sqhi64b, prev_sq);
            // 更新 prev_sq = 前 16 像素平方和（广播，累加）
            let total_sq = _mm256_extract_epi32::<7>(sqhi) as f64;
            prev_sq = _mm256_add_pd(prev_sq, _mm256_set1_pd(total_sq));

            // 加上上一行
            let prev_lo = _mm256_loadu_si256(sum.as_ptr().add(prev + x + 1) as *const __m256i);
            let prev_hi = _mm256_loadu_si256(sum.as_ptr().add(prev + x + 9) as *const __m256i);
            let lo32 = _mm256_add_epi32(el4l, prev_lo);
            let hi32 = _mm256_add_epi32(el4h, prev_hi);
            _mm256_storeu_si256(sum.as_mut_ptr().add(cur + x + 1) as *mut __m256i, lo32);
            _mm256_storeu_si256(sum.as_mut_ptr().add(cur + x + 9) as *mut __m256i, hi32);

            let psq_lo = _mm256_loadu_pd(sqsum.as_ptr().add(prev + x + 1));
            let psq_hi = _mm256_loadu_pd(sqsum.as_ptr().add(prev + x + 5));
            let psq_lo2 = _mm256_loadu_pd(sqsum.as_ptr().add(prev + x + 9));
            let psq_hi2 = _mm256_loadu_pd(sqsum.as_ptr().add(prev + x + 13));
            let sel4l = _mm256_add_pd(sel4l, psq_lo);
            let sel4h = _mm256_add_pd(sel4h, psq_hi);
            let sel4l2 = _mm256_add_pd(sel4l2, psq_lo2);
            let sel4h2 = _mm256_add_pd(sel4h2, psq_hi2);
            _mm256_storeu_pd(sqsum.as_mut_ptr().add(cur + x + 1), sel4l);
            _mm256_storeu_pd(sqsum.as_mut_ptr().add(cur + x + 5), sel4h);
            _mm256_storeu_pd(sqsum.as_mut_ptr().add(cur + x + 9), sel4l2);
            _mm256_storeu_pd(sqsum.as_mut_ptr().add(cur + x + 13), sel4h2);

            x += 16;
        }
        // 尾部标量
        let mut s = _mm256_extract_epi32::<0>(prev_row) as i32;
        let mut sq = _mm256_cvtsd_f64(prev_sq);
        while x < w {
            let it = src_row[x] as i32;
            s += it;
            sq += (it as f64) * (it as f64);
            sum[cur + x + 1] = sum[prev + x + 1] + s;
            sqsum[cur + x + 1] = sqsum[prev + x + 1] + sq;
            x += 1;
        }
    }
}

// ==================== 通用二维卷积（filter2d） ====================

/// u8 二维卷积（AVX2，对齐 OpenCV `FilterVec_8u`）。
///
/// `src` 为 `w*h` 灰度像素，`coords` 为非零核位置（相对锚点偏移），`coeffs` 为对应系数。
/// `delta` 为累加偏移。`out` 为 `w*h` 输出。
///
/// # Safety
/// - `src.len() == w*h`，`out.len() == w*h`
/// - `coords.len() == coeffs.len()`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2,fma")]
pub unsafe fn filter2d_u8(
    src: &[u8],
    w: usize,
    h: usize,
    coords: &[(i32, i32)],
    coeffs: &[f32],
    delta: f32,
    out: &mut [u8],
) {
    use std::arch::x86_64::*;
    let nz = coords.len();
    // 计算边界范围
    let mut min_dy = 0i32;
    let mut max_dy = 0i32;
    let mut min_dx = 0i32;
    let mut max_dx = 0i32;
    for &(dx, dy) in coords {
        min_dy = min_dy.min(dy);
        max_dy = max_dy.max(dy);
        min_dx = min_dx.min(dx);
        max_dx = max_dx.max(dx);
    }
    let top = (-min_dy).max(0) as usize;
    let bottom = max_dy.max(0) as usize;
    let left = (-min_dx).max(0) as usize;
    let right = max_dx.max(0) as usize;

    // 预计算边界行（垂直扩展）
    let bw = w + left + right;
    let bh = h + top + bottom;
    let mut brows = vec![0u8; bw * bh];
    for y in 0..bh {
        let sy = y as i32 - top as i32;
        let sy = if sy < 0 { 0 } else if sy >= h as i32 { h as i32 - 1 } else { sy };
        let src_row = &src[sy as usize * w..(sy as usize + 1) * w];
        let dst_row = &mut brows[y * bw..(y + 1) * bw];
        // 水平扩展
        for x in 0..bw {
            let sx = x as i32 - left as i32;
            let sx = if sx < 0 { 0 } else if sx >= w as i32 { w as i32 - 1 } else { sx };
            dst_row[x] = src_row[sx as usize];
        }
    }

    // 预计算系数广播
    let mut fvecs = Vec::with_capacity(nz);
    for k in 0..nz {
        fvecs.push(_mm256_set1_ps(coeffs[k]));
    }
    let d4 = _mm256_set1_ps(delta);
    let mut y = 0usize;
    while y < h {
        let out_row = &mut out[y * w..(y + 1) * w];
        let mut x = 0usize;
        // 每迭代处理 32 像素（4×8）
        while x + 32 <= w {
            let mut s0a = d4;
            let mut s0b = d4;
            let mut s0c = d4;
            let mut s0d = d4;
            for k in 0..nz {
                let (dx, dy) = coords[k];
                let f = fvecs[k];
                let brow_row = &brows[(y as i32 + dy + top as i32) as usize * bw + (x as i32 + dx + left as i32) as usize..];
                let v = _mm_loadu_si128(brow_row.as_ptr() as *const __m128i);
                let vf = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v));
                s0a = _mm256_fmadd_ps(vf, f, s0a);
                let v2 = _mm_loadu_si128(brow_row.as_ptr().add(8) as *const __m128i);
                let vfb = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v2));
                s0b = _mm256_fmadd_ps(vfb, f, s0b);
                let v3 = _mm_loadu_si128(brow_row.as_ptr().add(16) as *const __m128i);
                let vfc = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v3));
                s0c = _mm256_fmadd_ps(vfc, f, s0c);
                let v4 = _mm_loadu_si128(brow_row.as_ptr().add(24) as *const __m128i);
                let vfd = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v4));
                s0d = _mm256_fmadd_ps(vfd, f, s0d);
            }
            let ra = _mm256_cvtps_epi32(s0a);
            let rb = _mm256_cvtps_epi32(s0b);
            let rc = _mm256_cvtps_epi32(s0c);
            let rd = _mm256_cvtps_epi32(s0d);
            let loa = _mm256_castsi256_si128(ra);
            let hia = _mm256_extracti128_si256(ra, 1);
            let lob = _mm256_castsi256_si128(rb);
            let hib = _mm256_extracti128_si256(rb, 1);
            let loc = _mm256_castsi256_si128(rc);
            let hic = _mm256_extracti128_si256(rc, 1);
            let lod = _mm256_castsi256_si128(rd);
            let hid = _mm256_extracti128_si256(rd, 1);
            let pa = _mm_packus_epi32(loa, hia);
            let pb = _mm_packus_epi32(lob, hib);
            let pc = _mm_packus_epi32(loc, hic);
            let pd = _mm_packus_epi32(lod, hid);
            let pa = _mm_packus_epi16(pa, pa);
            let pb = _mm_packus_epi16(pb, pb);
            let pc = _mm_packus_epi16(pc, pc);
            let pd = _mm_packus_epi16(pd, pd);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x) as *mut __m128i, pa);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x + 8) as *mut __m128i, pb);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x + 16) as *mut __m128i, pc);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x + 24) as *mut __m128i, pd);
            x += 32;
        }
        while x + 16 <= w {
            let mut s0a = d4;
            let mut s0b = d4;
            for k in 0..nz {
                let (dx, dy) = coords[k];
                let f = fvecs[k];
                let brow_row = &brows[(y as i32 + dy + top as i32) as usize * bw + (x as i32 + dx + left as i32) as usize..];
                let v = _mm_loadu_si128(brow_row.as_ptr() as *const __m128i);
                let vf = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v));
                s0a = _mm256_fmadd_ps(vf, f, s0a);
                let v2 = _mm_loadu_si128(brow_row.as_ptr().add(8) as *const __m128i);
                let vfb = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v2));
                s0b = _mm256_fmadd_ps(vfb, f, s0b);
            }
            let ra = _mm256_cvtps_epi32(s0a);
            let rb = _mm256_cvtps_epi32(s0b);
            let loa = _mm256_castsi256_si128(ra);
            let hia = _mm256_extracti128_si256(ra, 1);
            let lob = _mm256_castsi256_si128(rb);
            let hib = _mm256_extracti128_si256(rb, 1);
            let pa = _mm_packus_epi32(loa, hia);
            let pb = _mm_packus_epi32(lob, hib);
            let pa = _mm_packus_epi16(pa, pa);
            let pb = _mm_packus_epi16(pb, pb);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x) as *mut __m128i, pa);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x + 8) as *mut __m128i, pb);
            x += 16;
        }
        while x + 8 <= w {
            let mut s0 = d4;
            for k in 0..nz {
                let (dx, dy) = coords[k];
                let f = fvecs[k];
                let brow_row = &brows[(y as i32 + dy + top as i32) as usize * bw + (x as i32 + dx + left as i32) as usize..];
                let v = _mm_loadu_si128(brow_row.as_ptr() as *const __m128i);
                let vf = _mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(v));
                s0 = _mm256_fmadd_ps(vf, f, s0);
            }
            let r = _mm256_cvtps_epi32(s0);
            let lo = _mm256_castsi256_si128(r);
            let hi = _mm256_extracti128_si256(r, 1);
            let packed = _mm_packus_epi32(lo, hi);
            let packed = _mm_packus_epi16(packed, packed);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x) as *mut __m128i, packed);
            x += 8;
        }
        // 尾部标量
        while x < w {
            let mut s0 = delta;
            for k in 0..nz {
                let (dx, dy) = coords[k];
                let v = brows[(y as i32 + dy + top as i32) as usize * bw + (x as i32 + dx + left as i32) as usize];
                s0 += coeffs[k] * v as f32;
            }
            out_row[x] = crate::core::image::Pixel::from_f32(s0);
            x += 1;
        }
        y += 1;
    }
}

/// u8 3x3 二维卷积专用路径（AVX2，行加载 + shuffle 提取 3 列）。
///
/// 仅用于 3x3 全非零核 + Replicate 边界。`k` 为 9 个系数（行优先，
/// 对应 `(-1,-1), (0,-1), (1,-1), (-1,0), (0,0), (1,0), (-1,1), (0,1), (1,1)`）。
///
/// # Safety
/// - `src.len() == w*h`，`out.len() == w*h`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2,fma")]
pub unsafe fn filter2d_3x3_u8(
    src: &[u8],
    w: usize,
    h: usize,
    k: &[f32; 9],
    delta: f32,
    out: &mut [u8],
) {
    use std::arch::x86_64::*;
    // 预计算边界行（Replicate）
    let bw = w + 2;
    let bh = h + 2;
    let mut brows = vec![0u8; bw * bh];
    for y in 0..bh {
        let sy = (y as i32 - 1).clamp(0, h as i32 - 1) as usize;
        let src_row = &src[sy * w..(sy + 1) * w];
        let dst_row = &mut brows[y * bw..(y + 1) * bw];
        dst_row[0] = src_row[0];
        dst_row[1..w + 1].copy_from_slice(src_row);
        dst_row[w + 1] = src_row[w - 1];
    }
    // 系数广播
    let f0 = _mm256_set1_ps(k[0]);
    let f1 = _mm256_set1_ps(k[1]);
    let f2 = _mm256_set1_ps(k[2]);
    let f3 = _mm256_set1_ps(k[3]);
    let f4 = _mm256_set1_ps(k[4]);
    let f5 = _mm256_set1_ps(k[5]);
    let f6 = _mm256_set1_ps(k[6]);
    let f7 = _mm256_set1_ps(k[7]);
    let f8 = _mm256_set1_ps(k[8]);
    let d4 = _mm256_set1_ps(delta);
    // shuffle 索引：从 16 字节行提取 3 列（每列 8 字节）
    let shm1 = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let sh0 = _mm_setr_epi8(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0);
    let shp1 = _mm_setr_epi8(2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 0);
    let mut y = 0usize;
    while y < h {
        let out_row = &mut out[y * w..(y + 1) * w];
        let r0 = &brows[y * bw..y * bw + bw];
        let r1 = &brows[(y + 1) * bw..(y + 1) * bw + bw];
        let r2 = &brows[(y + 2) * bw..(y + 2) * bw + bw];
        let mut x = 0usize;
        while x + 8 <= w {
            let v0 = _mm_loadu_si128(r0.as_ptr().add(x) as *const __m128i);
            let v1 = _mm_loadu_si128(r1.as_ptr().add(x) as *const __m128i);
            let v2 = _mm_loadu_si128(r2.as_ptr().add(x) as *const __m128i);
            let c0m1 = _mm_shuffle_epi8(v0, shm1);
            let c00 = _mm_shuffle_epi8(v0, sh0);
            let c0p1 = _mm_shuffle_epi8(v0, shp1);
            let c1m1 = _mm_shuffle_epi8(v1, shm1);
            let c10 = _mm_shuffle_epi8(v1, sh0);
            let c1p1 = _mm_shuffle_epi8(v1, shp1);
            let c2m1 = _mm_shuffle_epi8(v2, shm1);
            let c20 = _mm_shuffle_epi8(v2, sh0);
            let c2p1 = _mm_shuffle_epi8(v2, shp1);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c0m1)), f0, d4);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c00)), f1, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c0p1)), f2, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c1m1)), f3, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c10)), f4, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c1p1)), f5, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c2m1)), f6, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c20)), f7, s0);
            let s0 = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c2p1)), f8, s0);
            let r = _mm256_cvtps_epi32(s0);
            let lo = _mm256_castsi256_si128(r);
            let hi = _mm256_extracti128_si256(r, 1);
            let packed = _mm_packus_epi32(lo, hi);
            let packed = _mm_packus_epi16(packed, packed);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x) as *mut __m128i, packed);
            x += 8;
        }
        while x < w {
            let mut s = delta;
            for (dy, dx) in [(-1i32, -1i32), (-1, 0), (-1, 1), (0, -1), (0, 0), (0, 1), (1, -1), (1, 0), (1, 1)] {
                let v = brows[(y as i32 + 1 + dy) as usize * bw + (x as i32 + 1 + dx) as usize];
                s += k[((dy + 1) * 3 + (dx + 1)) as usize] * v as f32;
            }
            out_row[x] = crate::core::image::Pixel::from_f32(s);
            x += 1;
        }
        y += 1;
    }
}

/// u8 可分离二维滤波（AVX2）。
///
/// 先对每行用 `kx` 做水平滤波（输出 f32 中间结果），再对每列用 `ky` 做垂直滤波
/// （输出 u8）。仅支持 Replicate 边界。`ax`/`ay` 为核锚点，`delta` 为累加偏移。
///
/// # Safety
/// - `src.len() == w*h`，`out.len() == w*h`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2,fma")]
pub unsafe fn sep_filter2d_u8(
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
    use std::arch::x86_64::*;
    let (kx_len, ky_len) = (kx.len(), ky.len());
    let ax = ax as usize;
    let ay = ay as usize;

    // 3x3 专用路径（shuffle 提取 3 列）
    if kx_len == 3 && ky_len == 3 && ax == 1 && ay == 1 {
        let kx3 = [kx[0], kx[1], kx[2]];
        let ky3 = [ky[0], ky[1], ky[2]];
        return sep_filter2d_3x3_u8(src, w, h, &kx3, &ky3, delta, out);
    }

    // 预计算边界行（Replicate）
    let bw = w + kx_len - 1;
    let mut brows = vec![0u8; bw * h];
    for y in 0..h {
        let src_row = &src[y * w..(y + 1) * w];
        let dst_row = &mut brows[y * bw..(y + 1) * bw];
        for i in 0..ax {
            dst_row[i] = src_row[0];
        }
        dst_row[ax..ax + w].copy_from_slice(src_row);
        for i in 0..(kx_len - 1 - ax) {
            dst_row[ax + w + i] = src_row[w - 1];
        }
    }

    // 水平滤波：tmp[y][x] = sum(kx[i] * brows[y][x + i])，输出 f32
    let mut tmp = vec![0.0f32; w * h];
    let mut kx_bcast: Vec<__m256> = Vec::with_capacity(kx_len);
    for &k in kx {
        kx_bcast.push(_mm256_set1_ps(k));
    }
    for y in 0..h {
        let row = &brows[y * bw..(y + 1) * bw];
        let tmp_row = &mut tmp[y * w..(y + 1) * w];
        let mut x = 0usize;
        while x + 8 <= w {
            let mut acc = _mm256_setzero_ps();
            for i in 0..kx_len {
                let v = _mm_loadu_si128(row.as_ptr().add(x + i) as *const __m128i);
                let v32 = _mm256_cvtepu8_epi32(v);
                let vf = _mm256_cvtepi32_ps(v32);
                acc = _mm256_fmadd_ps(vf, kx_bcast[i], acc);
            }
            _mm256_storeu_ps(tmp_row.as_mut_ptr().add(x), acc);
            x += 8;
        }
        while x < w {
            let mut s0 = 0.0f32;
            for i in 0..kx_len {
                s0 += kx[i] * row[x + i] as f32;
            }
            tmp_row[x] = s0;
            x += 1;
        }
    }

    // 垂直滤波：dst[y][x] = saturate_cast<u8>(sum(ky[j] * tmp[y + j][x]) + delta)
    let mut ky_bcast: Vec<__m256> = Vec::with_capacity(ky_len);
    for &k in ky {
        ky_bcast.push(_mm256_set1_ps(k));
    }
    let d4 = _mm256_set1_ps(delta);
    for y in 0..h {
        let out_row = &mut out[y * w..(y + 1) * w];
        let mut x = 0usize;
        while x + 8 <= w {
            let mut acc = d4;
            for j in 0..ky_len {
                let sy = y + j;
                let sy2 = if sy < ay {
                    ay
                } else if sy >= h + ay {
                    h + ay - 1
                } else {
                    sy
                };
                let v = _mm256_loadu_ps(tmp.as_ptr().add((sy2 - ay) * w + x));
                acc = _mm256_fmadd_ps(v, ky_bcast[j], acc);
            }
            let r = _mm256_cvtps_epi32(acc);
            let lo = _mm256_castsi256_si128(r);
            let hi = _mm256_extracti128_si256(r, 1);
            let packed = _mm_packus_epi32(lo, hi);
            let packed = _mm_packus_epi16(packed, packed);
            _mm_storel_epi64(out_row.as_mut_ptr().add(x) as *mut __m128i, packed);
            x += 8;
        }
        while x < w {
            let mut s0 = delta;
            for j in 0..ky_len {
                let sy = y + j;
                let sy2 = if sy < ay {
                    ay
                } else if sy >= h + ay {
                    h + ay - 1
                } else {
                    sy
                };
                s0 += ky[j] * tmp[(sy2 - ay) * w + x];
            }
            out_row[x] = crate::core::image::Pixel::from_f32(s0);
            x += 1;
        }
    }
}

/// u8 3x3 可分离二维滤波专用路径（AVX2，shuffle 提取 3 列）。
///
/// 仅用于 3x3 核 + 锚点 (1,1) + Replicate 边界。`kx`/`ky` 为 3 元素核。
///
/// # Safety
/// - `src.len() == w*h`，`out.len() == w*h`
/// - 调用方需保证 CPU 支持 AVX2
#[inline]
#[target_feature(enable = "avx2,fma")]
pub unsafe fn sep_filter2d_3x3_u8(
    src: &[u8],
    w: usize,
    h: usize,
    kx: &[f32; 3],
    ky: &[f32; 3],
    delta: f32,
    out: &mut [u8],
) {
    use std::arch::x86_64::*;
    let bw = w + 2;
    let mut brows = vec![0u8; bw * h];
    for y in 0..h {
        let s = &src[y * w..(y + 1) * w];
        let d = &mut brows[y * bw..(y + 1) * bw];
        d[0] = s[0];
        d[1..w + 1].copy_from_slice(s);
        d[w + 1] = s[w - 1];
    }
    // 水平滤波
    let mut tmp = vec![0.0f32; w * h];
    let f0 = _mm256_set1_ps(kx[0]);
    let f1 = _mm256_set1_ps(kx[1]);
    let f2 = _mm256_set1_ps(kx[2]);
    let shm1 = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let sh0 = _mm_setr_epi8(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0);
    let shp1 = _mm_setr_epi8(2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 0);
    for y in 0..h {
        let row = &brows[y * bw..(y + 1) * bw];
        let tr = &mut tmp[y * w..(y + 1) * w];
        let mut x = 0usize;
        while x + 8 <= w {
            let v = _mm_loadu_si128(row.as_ptr().add(x) as *const __m128i);
            let cm1 = _mm_shuffle_epi8(v, shm1);
            let c0 = _mm_shuffle_epi8(v, sh0);
            let cp1 = _mm_shuffle_epi8(v, shp1);
            let a = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(cm1)), f0, _mm256_setzero_ps());
            let a = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(c0)), f1, a);
            let a = _mm256_fmadd_ps(_mm256_cvtepi32_ps(_mm256_cvtepu8_epi32(cp1)), f2, a);
            _mm256_storeu_ps(tr.as_mut_ptr().add(x), a);
            x += 8;
        }
        while x < w {
            tr[x] = kx[0] * row[x] as f32 + kx[1] * row[x + 1] as f32 + kx[2] * row[x + 2] as f32;
            x += 1;
        }
    }
    // 垂直滤波
    let g0 = _mm256_set1_ps(ky[0]);
    let g1 = _mm256_set1_ps(ky[1]);
    let g2 = _mm256_set1_ps(ky[2]);
    let d4 = _mm256_set1_ps(delta);
    for y in 0..h {
        let orow = &mut out[y * w..(y + 1) * w];
        let r0 = &tmp[((y as i32 - 1).clamp(0, h as i32 - 1) as usize) * w..];
        let r1 = &tmp[y * w..];
        let r2 = &tmp[((y as i32 + 1).clamp(0, h as i32 - 1) as usize) * w..];
        let mut x = 0usize;
        while x + 8 <= w {
            let a = _mm256_fmadd_ps(_mm256_loadu_ps(r0.as_ptr().add(x)), g0, d4);
            let a = _mm256_fmadd_ps(_mm256_loadu_ps(r1.as_ptr().add(x)), g1, a);
            let a = _mm256_fmadd_ps(_mm256_loadu_ps(r2.as_ptr().add(x)), g2, a);
            let r = _mm256_cvtps_epi32(a);
            let lo = _mm256_castsi256_si128(r);
            let hi = _mm256_extracti128_si256(r, 1);
            let p = _mm_packus_epi32(lo, hi);
            let p = _mm_packus_epi16(p, p);
            _mm_storel_epi64(orow.as_mut_ptr().add(x) as *mut __m128i, p);
            x += 8;
        }
        while x < w {
            let v0 = tmp[((y as i32 - 1).clamp(0, h as i32 - 1) as usize) * w + x];
            let v1 = tmp[y * w + x];
            let v2 = tmp[((y as i32 + 1).clamp(0, h as i32 - 1) as usize) * w + x];
            orow[x] = crate::core::image::Pixel::from_f32(ky[0] * v0 + ky[1] * v1 + ky[2] * v2 + delta);
            x += 1;
        }
    }
}

/// u8 互相关（CCORR）AVX2 实现。
///
/// `corr[y][x] = sum(T * I)`。对每个输出行，遍历模板行，用 SIMD 一次处理 8 个 x。
///
/// # Safety
/// - `src.len() == w*h`，`templ.len() == tw*th`，`corr.len() == (w-tw+1)*(h-th+1)`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2,fma")]
pub unsafe fn ccorr_u8(
    src: &[u8],
    w: usize,
    h: usize,
    templ: &[u8],
    tw: usize,
    th: usize,
    corr: &mut [f32],
) {
    use std::arch::x86_64::*;
    let (rw, rh) = (w - tw + 1, h - th + 1);
    // 预转换模板为 f32
    let mut tbuf = vec![0.0f32; tw * th];
    for i in 0..tw * th {
        tbuf[i] = templ[i] as f32;
    }
    // 预转换图像为 f32（每像素一次转换，替代每 (x,tx) 组合一次）
    let mut sbuf = vec![0.0f32; w * h];
    {
        let mut i = 0usize;
        while i + 8 <= w * h {
            let v = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
            let v32 = _mm256_cvtepu8_epi32(v);
            _mm256_storeu_ps(sbuf.as_mut_ptr().add(i), _mm256_cvtepi32_ps(v32));
            i += 8;
        }
        while i < w * h {
            sbuf[i] = src[i] as f32;
            i += 1;
        }
    }
    for y in 0..rh {
        let corr_row = &mut corr[y * rw..(y + 1) * rw];
        let mut x = 0usize;
        // 一次处理 64 个 x：8 个累加器提升 ILP，整个模板在寄存器中累加
        while x + 64 <= rw {
            let mut a0 = _mm256_setzero_ps();
            let mut a1 = _mm256_setzero_ps();
            let mut a2 = _mm256_setzero_ps();
            let mut a3 = _mm256_setzero_ps();
            let mut a4 = _mm256_setzero_ps();
            let mut a5 = _mm256_setzero_ps();
            let mut a6 = _mm256_setzero_ps();
            let mut a7 = _mm256_setzero_ps();
            for ty in 0..th {
                let src_row = &sbuf[(y + ty) * w..(y + ty) * w + w];
                let templ_row = &tbuf[ty * tw..(ty + 1) * tw];
                for tx in 0..tw {
                    let t = _mm256_set1_ps(templ_row[tx]);
                    a0 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx)), t, a0);
                    a1 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 8)), t, a1);
                    a2 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 16)), t, a2);
                    a3 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 24)), t, a3);
                    a4 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 32)), t, a4);
                    a5 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 40)), t, a5);
                    a6 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 48)), t, a6);
                    a7 = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx + 56)), t, a7);
                }
            }
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x), a0);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 8), a1);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 16), a2);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 24), a3);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 32), a4);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 40), a5);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 48), a6);
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x + 56), a7);
            x += 64;
        }
        while x + 8 <= rw {
            let mut acc = _mm256_setzero_ps();
            for ty in 0..th {
                let src_row = &sbuf[(y + ty) * w..(y + ty) * w + w];
                let templ_row = &tbuf[ty * tw..(ty + 1) * tw];
                for tx in 0..tw {
                    let t = _mm256_set1_ps(templ_row[tx]);
                    acc = _mm256_fmadd_ps(_mm256_loadu_ps(src_row.as_ptr().add(x + tx)), t, acc);
                }
            }
            _mm256_storeu_ps(corr_row.as_mut_ptr().add(x), acc);
            x += 8;
        }
        while x < rw {
            let mut s = 0.0f32;
            for ty in 0..th {
                let src_row = &sbuf[(y + ty) * w..(y + ty) * w + w];
                let templ_row = &tbuf[ty * tw..(ty + 1) * tw];
                for tx in 0..tw {
                    s += templ_row[tx] * src_row[x + tx];
                }
            }
            corr_row[x] = s;
            x += 1;
        }
    }
}

// ---- FFT 内核（radix-2，一次 4 蝶形）----

// 提取 AoS 复数的 re/im 的 permutevar8x32 索引（跨 128 位通道）
// SAFETY: 位模式确定，无运行时初始化。
static IDX_RE: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([0, 2, 4, 6, 0, 2, 4, 6])
};
static IDX_IM: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([1, 3, 5, 7, 1, 3, 5, 7])
};
// 第一级 stride-2 提取后的通道重排：(a0,a2,b0,b2,a4,a6,b4,b6) → (a0,a2,a4,a6,b0,b2,b4,b6)
static IDX_FIX: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([0, 1, 4, 5, 2, 3, 6, 7])
};
// len=4 级：unpacklo(ab_lo,ab_hi) = (b0u0,b1u0,b0u1,b1u1,b2u0,b3u0,b2u1,b3u1)
// → 重排为 (b0u0,b0u1,b1u0,b1u1,b2u0,b2u1,b3u0,b3u1)
static IDX_U4: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([0, 2, 1, 3, 4, 6, 5, 7])
};
// len=4 级：分离每块内位置 0/1（u0/u1）
static IDX_EVEN2: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([0, 2, 4, 6, 0, 2, 4, 6])
};
static IDX_ODD2: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([1, 3, 5, 7, 1, 3, 5, 7])
};
// 符号掩码（高位置 1）
static SIGN_MASK: __m256 = unsafe {
    std::mem::transmute::<[u32; 8], __m256>([0x8000_0000u32; 8])
};
// len=8 级旋转因子 w^k, k=0..3（对每块重复），w = e^{-iπ/4}
static W8_RE: __m256 = unsafe {
    std::mem::transmute::<[f32; 8], __m256>([
        1.0, 0.70710677, 0.0, -0.70710677,
        1.0, 0.70710677, 0.0, -0.70710677,
    ])
};
static W8_IM: __m256 = unsafe {
    std::mem::transmute::<[f32; 8], __m256>([
        0.0, -0.70710677, -1.0, -0.70710677,
        0.0, -0.70710677, -1.0, -0.70710677,
    ])
};
// IFFT 用正指数：W8_IM 取反
static W8_IM_NEG: __m256 = unsafe {
    std::mem::transmute::<[f32; 8], __m256>([
        0.0, 0.70710677, 1.0, 0.70710677,
        0.0, 0.70710677, 1.0, 0.70710677,
    ])
};


/// AVX2 一维 FFT（radix-2 迭代）。`data` 为 `2n` 个 f32（AoS：re, im 交错），`n` 为 2 的幂。
///
/// # SAFETY
/// - `data.len() == 2*n`，`n` 为 2 的幂且 `n >= 2`。
/// - 调用方保证 `data` 可写。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn fft1d_avx2(data: &mut [f32], n: usize) {
    bit_reverse_avx2(data, n);
    // 预分配最大尺寸旋转因子表（n 个复数 = 2n 个 f32），每级只填充需要的部分
    let mut wbuf = vec![0.0f32; 2 * n];
    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        for k in 0..half {
            let a = ang * k as f32;
            wbuf[2 * k] = a.cos();
            wbuf[2 * k + 1] = a.sin();
        }
        let wptr = wbuf.as_ptr();
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            // 一次处理 4 个蝶形
            while k + 4 <= half {
                butterfly4_avx2(data, i, k, half, wptr);
                k += 4;
            }
            // 剩余蝶形（标量，直接用预计算 wbuf）
            while k < half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                let w_re = wbuf[2 * k];
                let w_im = wbuf[2 * k + 1];
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
}

/// AVX2 一维 FFT（AoS，旋转因子已预计算，拼接布局）。
///
/// 旋转因子表布局：级 `len` 的因子（AoS 交错）在
/// `wbuf[2*(len/2-1) .. 2*(len/2-1)+2*(len/2)]`。
///
/// # SAFETY
/// - `data.len() == 2*n`，`n` 为 2 的幂且 `n >= 2`。
/// - `wbuf` 至少 `2*n` 个 f32（覆盖所有级）。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn fft1d_aos_plan_avx2(data: &mut [f32], n: usize, wbuf: *const f32) {
    bit_reverse_avx2(data, n);
    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let woff = 2 * (half - 1); // 拼接布局中级 len 的偏移（AoS 交错）
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            while k + 4 <= half {
                butterfly4_avx2(data, i, k, half, wbuf.add(woff));
                k += 4;
            }
            while k < half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                let w_re = *wbuf.add(woff + 2 * k);
                let w_im = *wbuf.add(woff + 2 * k + 1);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
}

/// AVX2 一维逆 FFT（AoS，旋转因子已预计算，拼接布局），带 1/n 缩放。
///
/// # SAFETY
/// - `data.len() == 2*n`，`n` 为 2 的幂且 `n >= 2`。
/// - `wbuf` 至少 `2*n` 个 f32（覆盖所有级）。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn ifft1d_aos_plan_avx2(data: &mut [f32], n: usize, wbuf: *const f32) {
    bit_reverse_avx2(data, n);
    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let woff = 2 * (half - 1); // 拼接布局中级 len 的偏移（AoS 交错）
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            while k + 4 <= half {
                butterfly4_avx2(data, i, k, half, wbuf.add(woff));
                k += 4;
            }
            while k < half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                let w_re = *wbuf.add(woff + 2 * k);
                let w_im = *wbuf.add(woff + 2 * k + 1);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
    let inv = 1.0 / n as f32;
    for v in data.iter_mut() {
        *v *= inv;
    }
}

/// AVX2 一维逆 FFT（radix-2 迭代），带 1/n 缩放。
///
/// # SAFETY
/// - `data.len() == 2*n`，`n` 为 2 的幂且 `n >= 2`。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn ifft1d_avx2(data: &mut [f32], n: usize) {
    bit_reverse_avx2(data, n);
    let mut wbuf = vec![0.0f32; 2 * n];
    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let ang = 2.0 * std::f32::consts::PI / len as f32;
        for k in 0..half {
            let a = ang * k as f32;
            wbuf[2 * k] = a.cos();
            wbuf[2 * k + 1] = a.sin();
        }
        let wptr = wbuf.as_ptr();
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            while k + 4 <= half {
                butterfly4_avx2(data, i, k, half, wptr);
                k += 4;
            }
            while k < half {
                let u_re = data[2 * (i + k)];
                let u_im = data[2 * (i + k) + 1];
                let v_re = data[2 * (i + k + half)];
                let v_im = data[2 * (i + k + half) + 1];
                let w_re = wbuf[2 * k];
                let w_im = wbuf[2 * k + 1];
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                data[2 * (i + k)] = u_re + vw_re;
                data[2 * (i + k) + 1] = u_im + vw_im;
                data[2 * (i + k + half)] = u_re - vw_re;
                data[2 * (i + k + half) + 1] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
    let inv = 1.0 / n as f32;
    for v in data.iter_mut() {
        *v *= inv;
    }
}

/// 位反转置换（AoS 布局，`data` 为 `2n` 个 f32）。
///
/// # SAFETY
/// - `data.len() == 2*n`。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn bit_reverse_avx2(data: &mut [f32], n: usize) {
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            data.swap(2 * i, 2 * j);
            data.swap(2 * i + 1, 2 * j + 1);
        }
    }
}

/// 一次处理 4 个蝶形（AVX2）。
///
/// 蝶形：`data[i+k] = u + v*w`, `data[i+k+half] = u - v*w`，k = k0..k0+3。
/// `wptr` 指向旋转因子表（AoS），`wptr[2*k]` = re, `wptr[2*k+1]` = im。
///
/// # SAFETY
/// - `data.len() == 2*n`，`i + k0 + 3 + half < n`。
/// - `wptr` 指向至少 `2*(k0+4)` 个 f32。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn butterfly4_avx2(data: &mut [f32], i: usize, k0: usize, half: usize, wptr: *const f32) {
    let u = _mm256_loadu_ps(data.as_ptr().add(2 * (i + k0)));
    let v = _mm256_loadu_ps(data.as_ptr().add(2 * (i + k0 + half)));
    let w = _mm256_loadu_ps(wptr.add(2 * k0));
    // 复数乘 v*w：提取 re/im（AoS → SoA）
    // 用 permutevar8x32 跨 128 位通道提取：re 取位置 0,2,4,6，im 取 1,3,5,7
    let v_re = _mm256_permutevar8x32_ps(v, IDX_RE);
    let v_im = _mm256_permutevar8x32_ps(v, IDX_IM);
    let w_re = _mm256_permutevar8x32_ps(w, IDX_RE);
    let w_im = _mm256_permutevar8x32_ps(w, IDX_IM);
    let vw_re = _mm256_sub_ps(_mm256_mul_ps(v_re, w_re), _mm256_mul_ps(v_im, w_im));
    let vw_im = _mm256_add_ps(_mm256_mul_ps(v_re, w_im), _mm256_mul_ps(v_im, w_re));
    // 交错回 AoS（SoA → AoS）
    // unpacklo 给出前 2 个复数的交错（低 128 位），unpackhi 给出后 2 个（高 128 位）
    let vw_lo = _mm256_unpacklo_ps(vw_re, vw_im);
    let vw_hi = _mm256_unpackhi_ps(vw_re, vw_im);
    let vw = _mm256_insertf128_ps(
        _mm256_castps128_ps256(_mm256_castps256_ps128(vw_lo)),
        _mm256_castps256_ps128(vw_hi),
        1,
    );
    // out = u + vw, out2 = u - vw
    let out = _mm256_add_ps(u, vw);
    let out2 = _mm256_sub_ps(u, vw);
    _mm256_storeu_ps(data.as_mut_ptr().add(2 * (i + k0)), out);
    _mm256_storeu_ps(data.as_mut_ptr().add(2 * (i + k0 + half)), out2);
}

// ---- SoA 布局 FFT 内核（re/im 分离，无 permutevar 开销）----

/// 一次处理 8 个蝶形（SoA 布局，AVX2，完整 256 位）。
///
/// 蝶形：`re[i+k] = u_re + vw_re` 等，k = k0..k0+7。
/// `re`/`im` 为 `n` 个 f32，`wre`/`wim` 为旋转因子表（各 `half` 个 f32）。
///
/// # SAFETY
/// - `k0 + 8 <= half` 且 `i + 2*half <= n`（保证所有 load/store 8 个 f32 在界内）。
/// - `wre`/`wim` 指向至少 `k0+8` 个 f32。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn butterfly8_soa(
    re: &mut [f32],
    im: &mut [f32],
    i: usize,
    k0: usize,
    half: usize,
    wre: *const f32,
    wim: *const f32,
) {
    debug_assert!(i + k0 + half + 8 <= re.len(), "butterfly8_soa OOB re");
    debug_assert!(i + k0 + half + 8 <= im.len(), "butterfly8_soa OOB im");
    debug_assert!(k0 + 8 <= re.len(), "butterfly8_soa OOB wre");
    let u_re = _mm256_loadu_ps(re.as_ptr().add(i + k0));
    let u_im = _mm256_loadu_ps(im.as_ptr().add(i + k0));
    let v_re = _mm256_loadu_ps(re.as_ptr().add(i + k0 + half));
    let v_im = _mm256_loadu_ps(im.as_ptr().add(i + k0 + half));
    let w_re = _mm256_loadu_ps(wre.add(k0));
    let w_im = _mm256_loadu_ps(wim.add(k0));
    // FMA 融合复数乘与加减（每侧 4 FMA 替代 6 条独立指令）：
    // out  = u + (v_re*w_re - v_im*w_im) = fnmadd(v_im, w_im, fmadd(v_re, w_re, u))
    // out2 = u - (v_re*w_re - v_im*w_im) = fmadd(v_im, w_im, fnmadd(v_re, w_re, u))
    let s_re = _mm256_fnmadd_ps(v_im, w_im, _mm256_fmadd_ps(v_re, w_re, u_re));
    let d_re = _mm256_fmadd_ps(v_im, w_im, _mm256_fnmadd_ps(v_re, w_re, u_re));
    let s_im = _mm256_fmadd_ps(v_im, w_re, _mm256_fmadd_ps(v_re, w_im, u_im));
    let d_im = _mm256_fnmadd_ps(v_im, w_re, _mm256_fnmadd_ps(v_re, w_im, u_im));
    _mm256_storeu_ps(re.as_mut_ptr().add(i + k0), s_re);
    _mm256_storeu_ps(im.as_mut_ptr().add(i + k0), s_im);
    _mm256_storeu_ps(re.as_mut_ptr().add(i + k0 + half), d_re);
    _mm256_storeu_ps(im.as_mut_ptr().add(i + k0 + half), d_im);
}

/// SoA 一维 FFT 内核（旋转因子已预计算，拼接布局）。
///
/// 旋转因子表布局（`FftPlan`）：级 `len` 的因子在
/// `wre[len/2 - 1 .. len/2 - 1 + len/2]`（每级拼接，偏移 = len/2 - 1）。
///
/// # SAFETY
/// - `re.len() == im.len() == n`，`n` 为 2 的幂且 `n >= 2`。
/// - `wre`/`wim` 至少 `n` 个 f32（覆盖所有级）。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn fft1d_soa_plan_avx2(
    re: &mut [f32],
    im: &mut [f32],
    n: usize,
    wre: *const f32,
    wim: *const f32,
    br: &[(u32, u32)],
) {
    bit_reverse_soa(re, im, n, br);
    // 第一级（len=2，w=1）：纯加减，向量化（一次 8 对 = 16 个连续 f32）。
    // 偶数位置是 u，奇数位置是 v；用 shuffle 0x88/0xDD 做 stride-2 提取，
    // unpack + permute2f128 交错写回。
    {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            // u = 偶数位（stride 2），v = 奇数位。
            // shuffle 0x88 给出 (a0,a2,b0,b2,a4,a6,b4,b6)，需 permutevar
            // 重排为 (a0,a2,a4,a6,b0,b2,b4,b6)。
            let u_re = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_re, b_re, 0x88), IDX_FIX);
            let v_re = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_re, b_re, 0xDD), IDX_FIX);
            let u_im = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_im, b_im, 0x88), IDX_FIX);
            let v_im = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_im, b_im, 0xDD), IDX_FIX);
            let s_re = _mm256_add_ps(u_re, v_re);
            let d_re = _mm256_sub_ps(u_re, v_re);
            let s_im = _mm256_add_ps(u_im, v_im);
            let d_im = _mm256_sub_ps(u_im, v_im);
            // 交错：(s0,d0,s1,d1,...)
            let lo_re = _mm256_unpacklo_ps(s_re, d_re);
            let hi_re = _mm256_unpackhi_ps(s_re, d_re);
            let lo_im = _mm256_unpacklo_ps(s_im, d_im);
            let hi_im = _mm256_unpackhi_ps(s_im, d_im);
            let outa_re = _mm256_permute2f128_ps(lo_re, hi_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(lo_re, hi_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(lo_im, hi_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(lo_im, hi_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            let u_re = re[i];
            let u_im = im[i];
            let v_re = re[i + 1];
            let v_im = im[i + 1];
            re[i] = u_re + v_re;
            im[i] = u_im + v_im;
            re[i + 1] = u_re - v_re;
            im[i + 1] = u_im - v_im;
            i += 2;
        }
    }
    // 第二级（len=4，w=(1,-i)）：k=0 乘 1，k=1 乘 -i（交换+变号），无乘法。
    // 一次处理 4 块（16 f32 = 2×256 位），块内 (u0,u1,v0,v1)。
    if n >= 4 {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            // 布局：a = (b0u0,b0u1,b0v0,b0v1, b1u0,b1u1,b1v0,b1v1)
            //       b = (b2u0,b2u1,b2v0,b2v1, b3u0,b3u1,b3v0,b3v1)
            // 提取 u = (b0u0,b0u1,b1u0,b1u1,b2u0,b2u1,b3u0,b3u1)
            // 提取 v = (b0v0,b0v1,b1v0,b1v1,b2v0,b2v1,b3v0,b3v1)
            let ab_lo = _mm256_permute2f128_ps(a_re, b_re, 0x20); // (b0, b2)
            let ab_hi = _mm256_permute2f128_ps(a_re, b_re, 0x31); // (b1, b3)
            let u_re = _mm256_permutevar8x32_ps(
                _mm256_unpacklo_ps(ab_lo, ab_hi), IDX_U4);
            let v_re = _mm256_permutevar8x32_ps(
                _mm256_unpackhi_ps(ab_lo, ab_hi), IDX_U4);
            let ab_lo = _mm256_permute2f128_ps(a_im, b_im, 0x20);
            let ab_hi = _mm256_permute2f128_ps(a_im, b_im, 0x31);
            let u_im = _mm256_permutevar8x32_ps(
                _mm256_unpacklo_ps(ab_lo, ab_hi), IDX_U4);
            let v_im = _mm256_permutevar8x32_ps(
                _mm256_unpackhi_ps(ab_lo, ab_hi), IDX_U4);
            // u = (u0,u1,u0,u1,...) 每块 2 个 u；v 同理。
            // 蝶形：u0' = u0+v0, v0' = u0-v0（k=0，w=1）
            //       u1' = u1 + v1*(-i), v1' = u1 - v1*(-i)，v*(-i) = (v_im, -v_re)
            // 分离 u0/u1 和 v0/v1（每块内位置 0/1）
            let u0_re = _mm256_permutevar8x32_ps(u_re, IDX_EVEN2);
            let u1_re = _mm256_permutevar8x32_ps(u_re, IDX_ODD2);
            let u0_im = _mm256_permutevar8x32_ps(u_im, IDX_EVEN2);
            let u1_im = _mm256_permutevar8x32_ps(u_im, IDX_ODD2);
            let v0_re = _mm256_permutevar8x32_ps(v_re, IDX_EVEN2);
            let v1_re = _mm256_permutevar8x32_ps(v_re, IDX_ODD2);
            let v0_im = _mm256_permutevar8x32_ps(v_im, IDX_EVEN2);
            let v1_im = _mm256_permutevar8x32_ps(v_im, IDX_ODD2);
            // k=0：u0±v0
            let s0_re = _mm256_add_ps(u0_re, v0_re);
            let d0_re = _mm256_sub_ps(u0_re, v0_re);
            let s0_im = _mm256_add_ps(u0_im, v0_im);
            let d0_im = _mm256_sub_ps(u0_im, v0_im);
            // k=1：v1*(-i) = (v1_im, -v1_re)
            let w1_re = v1_im;
            let w1_im = _mm256_xor_ps(v1_re, SIGN_MASK);
            let s1_re = _mm256_add_ps(u1_re, w1_re);
            let d1_re = _mm256_sub_ps(u1_re, w1_re);
            let s1_im = _mm256_add_ps(u1_im, w1_im);
            let d1_im = _mm256_sub_ps(u1_im, w1_im);
            // 交错回块布局：每块 (u0',u1',v0',v1')
            // s0 = (b0u0',b1u0',b2u0',b3u0',...) 需交错 s0/s1 → u'，d0/d1 → v'
            // unpacklo 交错前 2 个，unpackhi 交错后 2 个，permute2f128 拼接
            let u_re2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(s0_re, s1_re), _mm256_unpackhi_ps(s0_re, s1_re), 0x20);
            let v_re2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(d0_re, d1_re), _mm256_unpackhi_ps(d0_re, d1_re), 0x20);
            let u_im2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(s0_im, s1_im), _mm256_unpackhi_ps(s0_im, s1_im), 0x20);
            let v_im2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(d0_im, d1_im), _mm256_unpackhi_ps(d0_im, d1_im), 0x20);
            // u_re2 = (b0u0',b0u1',b1u0',b1u1',b2u0',b2u1',b3u0',b3u1')
            // 写回：a 位置 = (b0u',b0v',b1u',b1v')，b 位置 = (b2u',b2v',b3u',b3v')
            // shuffle 0x44 低128=(b0u',b0v') 高128=(b2u',b2v')；0xEE 低128=(b1u',b1v') 高128=(b3u',b3v')
            let sh44_re = _mm256_shuffle_ps(u_re2, v_re2, 0x44);
            let shEE_re = _mm256_shuffle_ps(u_re2, v_re2, 0xEE);
            let sh44_im = _mm256_shuffle_ps(u_im2, v_im2, 0x44);
            let shEE_im = _mm256_shuffle_ps(u_im2, v_im2, 0xEE);
            let outa_re = _mm256_permute2f128_ps(sh44_re, shEE_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(sh44_re, shEE_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(sh44_im, shEE_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(sh44_im, shEE_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            let u0_re = re[i]; let u0_im = im[i];
            let u1_re = re[i+1]; let u1_im = im[i+1];
            let v0_re = re[i+2]; let v0_im = im[i+2];
            let v1_re = re[i+3]; let v1_im = im[i+3];
            re[i] = u0_re + v0_re; im[i] = u0_im + v0_im;
            re[i+2] = u0_re - v0_re; im[i+2] = u0_im - v0_im;
            let w1_re = v1_im; let w1_im = -v1_re;
            re[i+1] = u1_re + w1_re; im[i+1] = u1_im + w1_im;
            re[i+3] = u1_re - w1_re; im[i+3] = u1_im - w1_im;
            i += 4;
        }
    }
    // 第三级（len=8，w^k, k=0..3）：一次处理 2 个 len=8 块（16 f32 = 2×256 位）。
    // len=8 块 = 连续 8 个元素，u = 前 4 个（低 128），v = 后 4 个（高 128）。
    if n >= 8 {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            // u = (uA0..uA3, uB0..uB3)，v = (vA0..vA3, vB0..vB3)
            let u_re = _mm256_permute2f128_ps(a_re, b_re, 0x20);
            let v_re = _mm256_permute2f128_ps(a_re, b_re, 0x31);
            let u_im = _mm256_permute2f128_ps(a_im, b_im, 0x20);
            let v_im = _mm256_permute2f128_ps(a_im, b_im, 0x31);
            // len=8 蝶形：u' = u + v*w^k, v' = u - v*w^k, k=0..3（对每块重复）
            let vw_re = _mm256_sub_ps(_mm256_mul_ps(v_re, W8_RE), _mm256_mul_ps(v_im, W8_IM));
            let vw_im = _mm256_add_ps(_mm256_mul_ps(v_re, W8_IM), _mm256_mul_ps(v_im, W8_RE));
            let s_re = _mm256_add_ps(u_re, vw_re);
            let d_re = _mm256_sub_ps(u_re, vw_re);
            let s_im = _mm256_add_ps(u_im, vw_im);
            let d_im = _mm256_sub_ps(u_im, vw_im);
            // 写回：a = (uA', vA')，b = (uB', vB')
            let outa_re = _mm256_permute2f128_ps(s_re, d_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(s_re, d_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(s_im, d_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(s_im, d_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            // 标量：块 (u0..u3, v0..v3)，w^k
            for k in 0..4 {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + 4];
                let v_im = im[i + k + 4];
                let w_re = *wre.add(3 + k);
                let w_im = *wim.add(3 + k);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + 4] = u_re - vw_re;
                im[i + k + 4] = u_im - vw_im;
            }
            i += 8;
        }
    }
    let mut len = 16usize;
    while len <= n {
        let half = len >> 1;
        let woff = half - 1; // 拼接布局中级 len 的偏移
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            while k + 8 <= half {
                butterfly8_soa(re, im, i, k, half, wre.add(woff), wim.add(woff));
                k += 8;
            }
            while k < half {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + half];
                let v_im = im[i + k + half];
                let w_re = *wre.add(woff + k);
                let w_im = *wim.add(woff + k);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + half] = u_re - vw_re;
                im[i + k + half] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
}

/// SoA 一维逆 FFT 内核（旋转因子已预计算，拼接布局），带 1/n 缩放。
///
/// # SAFETY
/// - `re.len() == im.len() == n`，`n` 为 2 的幂且 `n >= 2`。
/// - `wre`/`wim` 至少 `n` 个 f32（覆盖所有级）。
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn ifft1d_soa_plan_avx2(
    re: &mut [f32],
    im: &mut [f32],
    n: usize,
    wre: *const f32,
    wim: *const f32,
    br: &[(u32, u32)],
) {
    bit_reverse_soa(re, im, n, br);
    // 第一级（len=2，w=1）：纯加减，向量化（与 FFT 相同）。
    {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            let u_re = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_re, b_re, 0x88), IDX_FIX);
            let v_re = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_re, b_re, 0xDD), IDX_FIX);
            let u_im = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_im, b_im, 0x88), IDX_FIX);
            let v_im = _mm256_permutevar8x32_ps(_mm256_shuffle_ps(a_im, b_im, 0xDD), IDX_FIX);
            let s_re = _mm256_add_ps(u_re, v_re);
            let d_re = _mm256_sub_ps(u_re, v_re);
            let s_im = _mm256_add_ps(u_im, v_im);
            let d_im = _mm256_sub_ps(u_im, v_im);
            let lo_re = _mm256_unpacklo_ps(s_re, d_re);
            let hi_re = _mm256_unpackhi_ps(s_re, d_re);
            let lo_im = _mm256_unpacklo_ps(s_im, d_im);
            let hi_im = _mm256_unpackhi_ps(s_im, d_im);
            let outa_re = _mm256_permute2f128_ps(lo_re, hi_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(lo_re, hi_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(lo_im, hi_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(lo_im, hi_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            let u_re = re[i];
            let u_im = im[i];
            let v_re = re[i + 1];
            let v_im = im[i + 1];
            re[i] = u_re + v_re;
            im[i] = u_im + v_im;
            re[i + 1] = u_re - v_re;
            im[i + 1] = u_im - v_im;
            i += 2;
        }
    }
    // 第二级（len=4，w=(1,i)）：k=0 乘 1，k=1 乘 i（交换+变号，无乘法）。
    if n >= 4 {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            let ab_lo = _mm256_permute2f128_ps(a_re, b_re, 0x20);
            let ab_hi = _mm256_permute2f128_ps(a_re, b_re, 0x31);
            let u_re = _mm256_permutevar8x32_ps(_mm256_unpacklo_ps(ab_lo, ab_hi), IDX_U4);
            let v_re = _mm256_permutevar8x32_ps(_mm256_unpackhi_ps(ab_lo, ab_hi), IDX_U4);
            let ab_lo = _mm256_permute2f128_ps(a_im, b_im, 0x20);
            let ab_hi = _mm256_permute2f128_ps(a_im, b_im, 0x31);
            let u_im = _mm256_permutevar8x32_ps(_mm256_unpacklo_ps(ab_lo, ab_hi), IDX_U4);
            let v_im = _mm256_permutevar8x32_ps(_mm256_unpackhi_ps(ab_lo, ab_hi), IDX_U4);
            let u0_re = _mm256_permutevar8x32_ps(u_re, IDX_EVEN2);
            let u1_re = _mm256_permutevar8x32_ps(u_re, IDX_ODD2);
            let u0_im = _mm256_permutevar8x32_ps(u_im, IDX_EVEN2);
            let u1_im = _mm256_permutevar8x32_ps(u_im, IDX_ODD2);
            let v0_re = _mm256_permutevar8x32_ps(v_re, IDX_EVEN2);
            let v1_re = _mm256_permutevar8x32_ps(v_re, IDX_ODD2);
            let v0_im = _mm256_permutevar8x32_ps(v_im, IDX_EVEN2);
            let v1_im = _mm256_permutevar8x32_ps(v_im, IDX_ODD2);
            let s0_re = _mm256_add_ps(u0_re, v0_re);
            let d0_re = _mm256_sub_ps(u0_re, v0_re);
            let s0_im = _mm256_add_ps(u0_im, v0_im);
            let d0_im = _mm256_sub_ps(u0_im, v0_im);
            // k=1：v1*i = (-v1_im, v1_re)
            let w1_re = _mm256_xor_ps(v1_im, SIGN_MASK);
            let w1_im = v1_re;
            let s1_re = _mm256_add_ps(u1_re, w1_re);
            let d1_re = _mm256_sub_ps(u1_re, w1_re);
            let s1_im = _mm256_add_ps(u1_im, w1_im);
            let d1_im = _mm256_sub_ps(u1_im, w1_im);
            let u_re2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(s0_re, s1_re), _mm256_unpackhi_ps(s0_re, s1_re), 0x20);
            let v_re2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(d0_re, d1_re), _mm256_unpackhi_ps(d0_re, d1_re), 0x20);
            let u_im2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(s0_im, s1_im), _mm256_unpackhi_ps(s0_im, s1_im), 0x20);
            let v_im2 = _mm256_permute2f128_ps(
                _mm256_unpacklo_ps(d0_im, d1_im), _mm256_unpackhi_ps(d0_im, d1_im), 0x20);
            let sh44_re = _mm256_shuffle_ps(u_re2, v_re2, 0x44);
            let shEE_re = _mm256_shuffle_ps(u_re2, v_re2, 0xEE);
            let sh44_im = _mm256_shuffle_ps(u_im2, v_im2, 0x44);
            let shEE_im = _mm256_shuffle_ps(u_im2, v_im2, 0xEE);
            let outa_re = _mm256_permute2f128_ps(sh44_re, shEE_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(sh44_re, shEE_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(sh44_im, shEE_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(sh44_im, shEE_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            let u0_re = re[i]; let u0_im = im[i];
            let u1_re = re[i+1]; let u1_im = im[i+1];
            let v0_re = re[i+2]; let v0_im = im[i+2];
            let v1_re = re[i+3]; let v1_im = im[i+3];
            re[i] = u0_re + v0_re; im[i] = u0_im + v0_im;
            re[i+2] = u0_re - v0_re; im[i+2] = u0_im - v0_im;
            let w1_re = -v1_im; let w1_im = v1_re;
            re[i+1] = u1_re + w1_re; im[i+1] = u1_im + w1_im;
            re[i+3] = u1_re - w1_re; im[i+3] = u1_im - w1_im;
            i += 4;
        }
    }
    // 第三级（len=8，w^k, k=0..3，w=e^{iπ/4}）：一次处理 2 个 len=8 块。
    if n >= 8 {
        let mut i = 0usize;
        while i + 16 <= n {
            let a_re = _mm256_loadu_ps(re.as_ptr().add(i));
            let b_re = _mm256_loadu_ps(re.as_ptr().add(i + 8));
            let a_im = _mm256_loadu_ps(im.as_ptr().add(i));
            let b_im = _mm256_loadu_ps(im.as_ptr().add(i + 8));
            let u_re = _mm256_permute2f128_ps(a_re, b_re, 0x20);
            let v_re = _mm256_permute2f128_ps(a_re, b_re, 0x31);
            let u_im = _mm256_permute2f128_ps(a_im, b_im, 0x20);
            let v_im = _mm256_permute2f128_ps(a_im, b_im, 0x31);
            // IFFT 用正指数：W8_IM 取反
            let vw_re = _mm256_sub_ps(_mm256_mul_ps(v_re, W8_RE), _mm256_mul_ps(v_im, W8_IM_NEG));
            let vw_im = _mm256_add_ps(_mm256_mul_ps(v_re, W8_IM_NEG), _mm256_mul_ps(v_im, W8_RE));
            let s_re = _mm256_add_ps(u_re, vw_re);
            let d_re = _mm256_sub_ps(u_re, vw_re);
            let s_im = _mm256_add_ps(u_im, vw_im);
            let d_im = _mm256_sub_ps(u_im, vw_im);
            let outa_re = _mm256_permute2f128_ps(s_re, d_re, 0x20);
            let outb_re = _mm256_permute2f128_ps(s_re, d_re, 0x31);
            let outa_im = _mm256_permute2f128_ps(s_im, d_im, 0x20);
            let outb_im = _mm256_permute2f128_ps(s_im, d_im, 0x31);
            _mm256_storeu_ps(re.as_mut_ptr().add(i), outa_re);
            _mm256_storeu_ps(re.as_mut_ptr().add(i + 8), outb_re);
            _mm256_storeu_ps(im.as_mut_ptr().add(i), outa_im);
            _mm256_storeu_ps(im.as_mut_ptr().add(i + 8), outb_im);
            i += 16;
        }
        while i < n {
            for k in 0..4 {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + 4];
                let v_im = im[i + k + 4];
                let w_re = *wre.add(3 + k);
                let w_im = *wim.add(3 + k);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + 4] = u_re - vw_re;
                im[i + k + 4] = u_im - vw_im;
            }
            i += 8;
        }
    }
    let mut len = 16usize;
    while len <= n {
        let half = len >> 1;
        let woff = half - 1; // 拼接布局中级 len 的偏移
        for i in (0..n).step_by(len) {
            let mut k = 0usize;
            while k + 8 <= half {
                butterfly8_soa(re, im, i, k, half, wre.add(woff), wim.add(woff));
                k += 8;
            }
            while k < half {
                let u_re = re[i + k];
                let u_im = im[i + k];
                let v_re = re[i + k + half];
                let v_im = im[i + k + half];
                let w_re = *wre.add(woff + k);
                let w_im = *wim.add(woff + k);
                let vw_re = v_re * w_re - v_im * w_im;
                let vw_im = v_re * w_im + v_im * w_re;
                re[i + k] = u_re + vw_re;
                im[i + k] = u_im + vw_im;
                re[i + k + half] = u_re - vw_re;
                im[i + k + half] = u_im - vw_im;
                k += 1;
            }
        }
        len <<= 1;
    }
}

/// SoA 位反转置换（`re`/`im` 各 `n` 个 f32，查表交换）。
///
/// # SAFETY
/// - `re.len() == im.len() == n`，`br` 的所有索引 < n。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn bit_reverse_soa(re: &mut [f32], im: &mut [f32], n: usize, br: &[(u32, u32)]) {
    for &(i, j) in br {
        let (i, j) = (i as usize, j as usize);
        debug_assert!(i < j && j < n);
        re.swap(i, j);
        im.swap(i, j);
    }
}
// ---- 频域互相关解包（AVX2）----

// 8 路 f32 反转索引
static REV8: __m256i = unsafe {
    std::mem::transmute::<[i32; 8], __m256i>([7, 6, 5, 4, 3, 2, 1, 0])
};

/// 频域互相关解包（RFFT 打包的逆）：由打包频谱 Z 直接算 corr_spec。
///
/// 对每对对称点 (Zk, Zm)（Zm = Z[(dh-y)%dh][(dw-x)%dw]）：
///   corr = Im(Zk·Zm)/2 + i(|Zk|²-|Zm|²)/4
/// 自对称点（Zk == Zm）：corr = Re(Zk)·Im(Zk)（实数）。
/// 输出满足共轭对称（供 rifft2d 使用）。
///
/// 行对 (y, dh-y) 跨行配对，行内 x 与 dw-x 反序配对，SIMD 一次 8 对。
///
/// # SAFETY
/// - `z_re.len() == z_im.len() == dw*dh`，dw/dh 为 2 的幂且 >= 16
#[target_feature(enable = "avx2")]
pub unsafe fn ccorr_unpack_avx2(
    z_re: &mut [f32],
    z_im: &mut [f32],
    dw: usize,
    dh: usize,
) {
    // SAFETY: 见函数级 SAFETY 注释（unsafe fn 体，Edition 2024 显式块）
    unsafe {
        debug_assert!(z_re.len() == dw * dh && z_im.len() == dw * dh);
        let half_w = dw / 2;
        let half_h = dh / 2;
        let quarter = 0.25f32;
        let half = 0.5f32;

        // 处理单个对称点对（标量，用于边界）
        #[inline(always)]
        unsafe fn pair(
            z_re: &mut [f32], z_im: &mut [f32],
            idx: usize, midx: usize,
            quarter: f32, half: f32,
        ) {
            let (zk_re, zk_im) = (z_re[idx], z_im[idx]);
            let (zm_re, zm_im) = (z_re[midx], z_im[midx]);
            let p_im = zk_re * zm_im + zk_im * zm_re;
            let c_re = half * p_im;
            let c_im = quarter * (zk_re * zk_re + zk_im * zk_im - zm_re * zm_re - zm_im * zm_im);
            if midx == idx {
                z_re[idx] = zk_re * zk_im;
                z_im[idx] = 0.0;
            } else {
                z_re[idx] = c_re;
                z_im[idx] = c_im;
                z_re[midx] = c_re;
                z_im[midx] = -c_im;
            }
        }

        // 自对称行（y=0 和 y=dh/2）：行内配对 z[y][x] <-> z[y][dw-x]
        for &y in &[0usize, half_h] {
            let row = y * dw;
            // x=0 和 x=dw/2 自对称
            pair(z_re, z_im, row, row, quarter, half);
            pair(z_re, z_im, row + half_w, row + half_w, quarter, half);
            // x=1..half_w-1 与 dw-x 配对（行内反序）
            let mut x = 1usize;
            while x + 8 <= half_w {
                let k = row + x;
                let mbase = row + dw - x - 7; // 反序 load 起点：对应 x+7..x
                let zk_re = _mm256_loadu_ps(z_re.as_ptr().add(k));
                let zk_im = _mm256_loadu_ps(z_im.as_ptr().add(k));
                let zm_re = _mm256_permutevar8x32_ps(_mm256_loadu_ps(z_re.as_ptr().add(mbase)), REV8);
                let zm_im = _mm256_permutevar8x32_ps(_mm256_loadu_ps(z_im.as_ptr().add(mbase)), REV8);
                let p_im = _mm256_add_ps(_mm256_mul_ps(zk_re, zm_im), _mm256_mul_ps(zk_im, zm_re));
                let c_re = _mm256_mul_ps(_mm256_set1_ps(half), p_im);
                let zk2 = _mm256_add_ps(_mm256_mul_ps(zk_re, zk_re), _mm256_mul_ps(zk_im, zk_im));
                let zm2 = _mm256_add_ps(_mm256_mul_ps(zm_re, zm_re), _mm256_mul_ps(zm_im, zm_im));
                let c_im = _mm256_mul_ps(_mm256_set1_ps(quarter), _mm256_sub_ps(zk2, zm2));
                // 写 z[y][x..x+8] = (c_re, c_im)
                _mm256_storeu_ps(z_re.as_mut_ptr().add(k), c_re);
                _mm256_storeu_ps(z_im.as_mut_ptr().add(k), c_im);
                // 写 z[y][dw-x-7..dw-x+1] = reverse(c_re, -c_im)
                let neg_im = _mm256_xor_ps(c_im, SIGN_MASK);
                let out_re = _mm256_permutevar8x32_ps(c_re, REV8);
                let out_im = _mm256_permutevar8x32_ps(neg_im, REV8);
                _mm256_storeu_ps(z_re.as_mut_ptr().add(mbase), out_re);
                _mm256_storeu_ps(z_im.as_mut_ptr().add(mbase), out_im);
                x += 8;
            }
            while x < half_w {
                pair(z_re, z_im, row + x, row + dw - x, quarter, half);
                x += 1;
            }
        }

        // 行对 (y, dh-y)：跨行配对 z[y][x] <-> z[dh-y][dw-x]。
        // 行对间独立，多线程池时按 y 分块并行。
        let nthreads = rayon::current_num_threads();
        const MIN_PAIRS_PER_BLOCK: usize = 32;
        if nthreads > 1 && half_h >= MIN_PAIRS_PER_BLOCK * 2 + 1 {
            use rayon::prelude::*;
            let npairs = half_h - 1;
            let max_blocks = npairs.div_ceil(MIN_PAIRS_PER_BLOCK).min(nthreads * 4);
            let pairs_per_block = npairs.div_ceil(max_blocks);
            let block_stride = pairs_per_block * dw * 2; // 每块覆盖的元素跨度
            let npblocks = npairs.div_ceil(pairs_per_block);
            // SAFETY 包装：裸指针跨线程（块内只读写自己的行对，见下）
            #[derive(Clone, Copy)]
            struct SendPtr(*mut f32);
            unsafe impl Send for SendPtr {}
            unsafe impl Sync for SendPtr {}
            fn run_pairs(
                re_p: SendPtr, im_p: SendPtr,
                dw: usize, dh: usize, y0: usize, y1: usize,
            ) {
                // SAFETY: 块内行对 (y, dh-y) 互不重叠（见上），指针来自调用方切片
                unsafe {
                    let z_re = std::slice::from_raw_parts_mut(re_p.0, dw * dh);
                    let z_im = std::slice::from_raw_parts_mut(im_p.0, dw * dh);
                    ccorr_unpack_row_pairs_avx2(z_re, z_im, dw, dh, y0, y1);
                }
            }
            let re_ptr = SendPtr(z_re.as_mut_ptr());
            let im_ptr = SendPtr(z_im.as_mut_ptr());
            (0..npblocks).into_par_iter().for_each(move |bi| {
                let y0 = 1 + bi * pairs_per_block;
                let y1 = (y0 + pairs_per_block).min(half_h);
                // SAFETY: 块内行对 (y, dh-y) 互不重叠（每块只处理自己的
                // y 范围对应的行对），dw*dh 范围内
                run_pairs(re_ptr, im_ptr, dw, dh, y0, y1);
            });
            let _ = block_stride;
        } else {
            ccorr_unpack_row_pairs_avx2(z_re, z_im, dw, dh, 1, half_h);
        }
    }
}

/// 频域互相关解包：处理行对 y in `y0..y1`（每对 (y, dh-y) 独立）。
///
/// # SAFETY
/// - `z_re.len() == z_im.len() == dw*dh`，dw/dh 为 2 的幂且 >= 16
/// - `1 <= y0 <= y1 <= dh/2`
#[target_feature(enable = "avx2")]
unsafe fn ccorr_unpack_row_pairs_avx2(
    z_re: &mut [f32],
    z_im: &mut [f32],
    dw: usize,
    dh: usize,
    y0: usize,
    y1: usize,
) {
    // SAFETY: 见函数级 SAFETY 注释（unsafe fn 体，Edition 2024 显式块）
    unsafe {
        debug_assert!(z_re.len() == dw * dh && z_im.len() == dw * dh);
        let half_w = dw / 2;
        let quarter = 0.25f32;
        let half = 0.5f32;

        // 处理单个对称点对（标量，用于边界）
        #[inline(always)]
        unsafe fn pair(
            z_re: &mut [f32], z_im: &mut [f32],
            idx: usize, midx: usize,
            quarter: f32, half: f32,
        ) {
            let (zk_re, zk_im) = (z_re[idx], z_im[idx]);
            let (zm_re, zm_im) = (z_re[midx], z_im[midx]);
            let p_im = zk_re * zm_im + zk_im * zm_re;
            let c_re = half * p_im;
            let c_im = quarter * (zk_re * zk_re + zk_im * zk_im - zm_re * zm_re - zm_im * zm_im);
            if midx == idx {
                z_re[idx] = zk_re * zk_im;
                z_im[idx] = 0.0;
            } else {
                z_re[idx] = c_re;
                z_im[idx] = c_im;
                z_re[midx] = c_re;
                z_im[midx] = -c_im;
            }
        }

        // 注意 x 与 dw-x 是不同的对（y != my），需处理全部 x（除 0 和 dw/2）。
        // 分两段（前半 1..half_w，后半 half_w+1..dw）避免读写冲突：
        // 前半写 z[my][half_w+1..dw)，后半读 z[my][1..half_w)，互不覆盖。
        for y in y0..y1 {
            let my = dh - y;
            let row_k = y * dw;
            let row_m = my * dw;
            // x=0：Zk = z[y][0], Zm = z[my][0]
            pair(z_re, z_im, row_k, row_m, quarter, half);
            // x=dw/2：Zk = z[y][dw/2], Zm = z[my][dw/2]
            pair(z_re, z_im, row_k + half_w, row_m + half_w, quarter, half);
            // 两段：x=1..half_w-1 和 x=half_w+1..dw-1
            for &(lo, hi) in &[(1usize, half_w), (half_w + 1, dw)] {
                let mut x = lo;
                while x + 8 <= hi {
                    let k = row_k + x;
                    let mbase = row_m + dw - x - 7;
                    let zk_re = _mm256_loadu_ps(z_re.as_ptr().add(k));
                    let zk_im = _mm256_loadu_ps(z_im.as_ptr().add(k));
                    let zm_re = _mm256_permutevar8x32_ps(_mm256_loadu_ps(z_re.as_ptr().add(mbase)), REV8);
                    let zm_im = _mm256_permutevar8x32_ps(_mm256_loadu_ps(z_im.as_ptr().add(mbase)), REV8);
                    let p_im = _mm256_add_ps(_mm256_mul_ps(zk_re, zm_im), _mm256_mul_ps(zk_im, zm_re));
                    let c_re = _mm256_mul_ps(_mm256_set1_ps(half), p_im);
                    let zk2 = _mm256_add_ps(_mm256_mul_ps(zk_re, zk_re), _mm256_mul_ps(zk_im, zk_im));
                    let zm2 = _mm256_add_ps(_mm256_mul_ps(zm_re, zm_re), _mm256_mul_ps(zm_im, zm_im));
                    let c_im = _mm256_mul_ps(_mm256_set1_ps(quarter), _mm256_sub_ps(zk2, zm2));
                    _mm256_storeu_ps(z_re.as_mut_ptr().add(k), c_re);
                    _mm256_storeu_ps(z_im.as_mut_ptr().add(k), c_im);
                    let neg_im = _mm256_xor_ps(c_im, SIGN_MASK);
                    let out_re = _mm256_permutevar8x32_ps(c_re, REV8);
                    let out_im = _mm256_permutevar8x32_ps(neg_im, REV8);
                    _mm256_storeu_ps(z_re.as_mut_ptr().add(mbase), out_re);
                    _mm256_storeu_ps(z_im.as_mut_ptr().add(mbase), out_im);
                    x += 8;
                }
                while x < hi {
                    pair(z_re, z_im, row_k + x, row_m + dw - x, quarter, half);
                    x += 1;
                }
            }
        }
    }
}

// ---- 实数 IFFT 打包/解包（AVX2）----

/// 实数 IFFT 打包：行频谱 X[0..n]（n=2m，共轭对称）打包为 m 点复数 Z。
///
/// Z[k] = E[k] + i·O[k]，E[k] = (X[k]+X[m+k])/2，O[k] = e^{2πik/n}·(X[k]-X[m+k])/2。
/// 结果写入 re[0..m], im[0..m]（就地，前半覆盖）。
/// k=0 由调用方标量处理（tw[0] 未定义）。
///
/// # SAFETY
/// - `re.len() == im.len() == n == 2m`，`m >= 8`，`tw_re.len() == tw_im.len() == m`
#[target_feature(enable = "avx2")]
pub unsafe fn rifft_pack_avx2(
    re: &mut [f32],
    im: &mut [f32],
    m: usize,
    tw_re: &[f32],
    tw_im: &[f32],
) {
    let n = 2 * m;
    debug_assert!(re.len() == n && im.len() == n);
    let mut k = 1usize;
    while k + 8 <= m {
        let xkr = _mm256_loadu_ps(re.as_ptr().add(k));
        let xki = _mm256_loadu_ps(im.as_ptr().add(k));
        let xmr = _mm256_loadu_ps(re.as_ptr().add(m + k));
        let xmi = _mm256_loadu_ps(im.as_ptr().add(m + k));
        let twr = _mm256_loadu_ps(tw_re.as_ptr().add(k));
        let twi = _mm256_loadu_ps(tw_im.as_ptr().add(k));
        // E = (X + Xm)/2, D = (X - Xm)/2
        let half = _mm256_set1_ps(0.5);
        let e_re = _mm256_mul_ps(_mm256_add_ps(xkr, xmr), half);
        let e_im = _mm256_mul_ps(_mm256_add_ps(xki, xmi), half);
        let d_re = _mm256_mul_ps(_mm256_sub_ps(xkr, xmr), half);
        let d_im = _mm256_mul_ps(_mm256_sub_ps(xki, xmi), half);
        // O = D * tw（复数乘）
        let o_re = _mm256_sub_ps(_mm256_mul_ps(d_re, twr), _mm256_mul_ps(d_im, twi));
        let o_im = _mm256_add_ps(_mm256_mul_ps(d_re, twi), _mm256_mul_ps(d_im, twr));
        // Z = E + i*O = (e_re - o_im, e_im + o_re)
        _mm256_storeu_ps(re.as_mut_ptr().add(k), _mm256_sub_ps(e_re, o_im));
        _mm256_storeu_ps(im.as_mut_ptr().add(k), _mm256_add_ps(e_im, o_re));
        k += 8;
    }
    // 标量尾
    while k < m {
        let (xkr, xki) = (re[k], im[k]);
        let (xmr, xmi) = (re[m + k], im[m + k]);
        let e_re = (xkr + xmr) * 0.5;
        let e_im = (xki + xmi) * 0.5;
        let d_re = (xkr - xmr) * 0.5;
        let d_im = (xki - xmi) * 0.5;
        let (twr, twi) = (tw_re[k], tw_im[k]);
        let o_re = d_re * twr - d_im * twi;
        let o_im = d_re * twi + d_im * twr;
        re[k] = e_re - o_im;
        im[k] = e_im + o_re;
        k += 1;
    }
}

/// 实数 IFFT 解包：m 点复数 z（re[0..m], im[0..m]）交错为 n=2m 实数到 re[0..n]。
///
/// x[2j] = Re(z[j])，x[2j+1] = Im(z[j])。从后往前写避免覆盖未读数据。
///
/// # SAFETY
/// - `re.len() == im.len() == n == 2m`，`m >= 8`
#[target_feature(enable = "avx2")]
pub unsafe fn rifft_unpack_avx2(re: &mut [f32], im: &mut [f32], m: usize) {
    let n = 2 * m;
    debug_assert!(re.len() == n && im.len() == n);
    let mut j = m;
    while j >= 8 {
        j -= 8;
        let zr = _mm256_loadu_ps(re.as_ptr().add(j));
        let zi = _mm256_loadu_ps(im.as_ptr().add(j));
        // 交错：(zr0,zi0,zr1,zi1,...)
        let lo = _mm256_unpacklo_ps(zr, zi);
        let hi = _mm256_unpackhi_ps(zr, zi);
        let outa = _mm256_permute2f128_ps(lo, hi, 0x20);
        let outb = _mm256_permute2f128_ps(lo, hi, 0x31);
        _mm256_storeu_ps(re.as_mut_ptr().add(2 * j), outa);
        _mm256_storeu_ps(re.as_mut_ptr().add(2 * j + 8), outb);
    }
    // 标量尾（j == 0..剩余）
    let mut j = j;
    while j > 0 {
        j -= 1;
        re[2 * j] = re[j];
        re[2 * j + 1] = im[j];
    }
}

// ---- SoA 方阵转置（8x8 分块，AVX2）----

/// 8x8 f32 块转置（寄存器内）。
///
/// 输入 8 行（每行 8 f32），输出转置后的 8 行。
/// 用 unpacklo/unpackhi 两轮交错实现 8x8 转置。
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn transpose8x8_ps(
    r0: __m256, r1: __m256, r2: __m256, r3: __m256,
    r4: __m256, r5: __m256, r6: __m256, r7: __m256,
) -> (__m256, __m256, __m256, __m256, __m256, __m256, __m256, __m256) {
    // 第一轮：交错相邻行对
    let t0 = _mm256_unpacklo_ps(r0, r1); // a0 b0 a1 b1 a4 b4 a5 b5
    let t1 = _mm256_unpackhi_ps(r0, r1); // a2 b2 a3 b3 a6 b6 a7 b7
    let t2 = _mm256_unpacklo_ps(r2, r3);
    let t3 = _mm256_unpackhi_ps(r2, r3);
    let t4 = _mm256_unpacklo_ps(r4, r5);
    let t5 = _mm256_unpackhi_ps(r4, r5);
    let t6 = _mm256_unpacklo_ps(r6, r7);
    let t7 = _mm256_unpackhi_ps(r6, r7);
    // 第二轮：交错 128 位通道
    let u0 = _mm256_shuffle_ps(t0, t2, 0x44); // a0 b0 c0 d0 a4 b4 c4 d4
    let u1 = _mm256_shuffle_ps(t0, t2, 0xEE); // a1 b1 c1 d1 a5 b5 c5 d5
    let u2 = _mm256_shuffle_ps(t1, t3, 0x44); // a2 b2 c2 d2 ...
    let u3 = _mm256_shuffle_ps(t1, t3, 0xEE); // a3 b3 c3 d3 ...
    let u4 = _mm256_shuffle_ps(t4, t6, 0x44);
    let u5 = _mm256_shuffle_ps(t4, t6, 0xEE);
    let u6 = _mm256_shuffle_ps(t5, t7, 0x44);
    let u7 = _mm256_shuffle_ps(t5, t7, 0xEE);
    // 合并高低通道
    let o0 = _mm256_permute2f128_ps(u0, u4, 0x20);
    let o1 = _mm256_permute2f128_ps(u1, u5, 0x20);
    let o2 = _mm256_permute2f128_ps(u2, u6, 0x20);
    let o3 = _mm256_permute2f128_ps(u3, u7, 0x20);
    let o4 = _mm256_permute2f128_ps(u0, u4, 0x31);
    let o5 = _mm256_permute2f128_ps(u1, u5, 0x31);
    let o6 = _mm256_permute2f128_ps(u2, u6, 0x31);
    let o7 = _mm256_permute2f128_ps(u3, u7, 0x31);
    (o0, o1, o2, o3, o4, o5, o6, o7)
}

/// 方阵原地转置（SoA 布局，8x8 分块，AVX2）。
///
/// `re`/`im` 为 `n*n` 个 f32，`n` 为 8 的倍数。
///
/// # SAFETY
/// - `re.len() == im.len() == n*n`，`n % 8 == 0`。
#[target_feature(enable = "avx2")]
pub unsafe fn transpose_square_soa_avx2(re: &mut [f32], im: &mut [f32], n: usize) {
    debug_assert!(re.len() == n * n && im.len() == n * n && n % 8 == 0);
    let nb = n / 8;
    transpose_block_rows_avx2(re, im, n, 0, nb);
}

/// 方阵原地转置：处理块行 `by0..by1`（含与下三角块的交换）。
///
/// 每个块行独立（块对 (by,bx) 只在其 by 行处理），可安全并行。
///
/// # SAFETY
/// - `re.len() == im.len() == n*n`，`n % 8 == 0`，`by1 <= n/8`。
#[target_feature(enable = "avx2")]
pub unsafe fn transpose_block_rows_avx2(
    re: &mut [f32],
    im: &mut [f32],
    n: usize,
    by0: usize,
    by1: usize,
) {
    debug_assert!(re.len() == n * n && im.len() == n * n && n % 8 == 0);
    let nb = n / 8;
    debug_assert!(by0 <= by1 && by1 <= nb);
    for by in by0..by1 {
        for bx in by..nb {
            let y0 = by * 8;
            let x0 = bx * 8;
            // 加载 8x8 块（re 和 im 各一份）
            let (a0, a1, a2, a3, a4, a5, a6, a7) = (
                _mm256_loadu_ps(re.as_ptr().add(y0 * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 1) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 2) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 3) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 4) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 5) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 6) * n + x0)),
                _mm256_loadu_ps(re.as_ptr().add((y0 + 7) * n + x0)),
            );
            let (b0, b1, b2, b3, b4, b5, b6, b7) = (
                _mm256_loadu_ps(im.as_ptr().add(y0 * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 1) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 2) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 3) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 4) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 5) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 6) * n + x0)),
                _mm256_loadu_ps(im.as_ptr().add((y0 + 7) * n + x0)),
            );
            let (ta0, ta1, ta2, ta3, ta4, ta5, ta6, ta7) =
                transpose8x8_ps(a0, a1, a2, a3, a4, a5, a6, a7);
            let (tb0, tb1, tb2, tb3, tb4, tb5, tb6, tb7) =
                transpose8x8_ps(b0, b1, b2, b3, b4, b5, b6, b7);
            if bx == by {
                // 对角块：直接写回
                _mm256_storeu_ps(re.as_mut_ptr().add(y0 * n + x0), ta0);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 1) * n + x0), ta1);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 2) * n + x0), ta2);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 3) * n + x0), ta3);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 4) * n + x0), ta4);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 5) * n + x0), ta5);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 6) * n + x0), ta6);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 7) * n + x0), ta7);
                _mm256_storeu_ps(im.as_mut_ptr().add(y0 * n + x0), tb0);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 1) * n + x0), tb1);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 2) * n + x0), tb2);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 3) * n + x0), tb3);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 4) * n + x0), tb4);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 5) * n + x0), tb5);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 6) * n + x0), tb6);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 7) * n + x0), tb7);
            } else {
                // 非对角块：加载目标块，转置后交换写回
                let (c0, c1, c2, c3, c4, c5, c6, c7) = (
                    _mm256_loadu_ps(re.as_ptr().add(x0 * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 1) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 2) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 3) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 4) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 5) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 6) * n + y0)),
                    _mm256_loadu_ps(re.as_ptr().add((x0 + 7) * n + y0)),
                );
                let (d0, d1, d2, d3, d4, d5, d6, d7) = (
                    _mm256_loadu_ps(im.as_ptr().add(x0 * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 1) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 2) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 3) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 4) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 5) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 6) * n + y0)),
                    _mm256_loadu_ps(im.as_ptr().add((x0 + 7) * n + y0)),
                );
                let (tc0, tc1, tc2, tc3, tc4, tc5, tc6, tc7) =
                    transpose8x8_ps(c0, c1, c2, c3, c4, c5, c6, c7);
                let (td0, td1, td2, td3, td4, td5, td6, td7) =
                    transpose8x8_ps(d0, d1, d2, d3, d4, d5, d6, d7);
                // 写回：A 块位置放 C^T，C 块位置放 A^T
                _mm256_storeu_ps(re.as_mut_ptr().add(y0 * n + x0), tc0);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 1) * n + x0), tc1);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 2) * n + x0), tc2);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 3) * n + x0), tc3);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 4) * n + x0), tc4);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 5) * n + x0), tc5);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 6) * n + x0), tc6);
                _mm256_storeu_ps(re.as_mut_ptr().add((y0 + 7) * n + x0), tc7);
                _mm256_storeu_ps(im.as_mut_ptr().add(y0 * n + x0), td0);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 1) * n + x0), td1);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 2) * n + x0), td2);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 3) * n + x0), td3);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 4) * n + x0), td4);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 5) * n + x0), td5);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 6) * n + x0), td6);
                _mm256_storeu_ps(im.as_mut_ptr().add((y0 + 7) * n + x0), td7);
                _mm256_storeu_ps(re.as_mut_ptr().add(x0 * n + y0), ta0);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 1) * n + y0), ta1);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 2) * n + y0), ta2);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 3) * n + y0), ta3);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 4) * n + y0), ta4);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 5) * n + y0), ta5);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 6) * n + y0), ta6);
                _mm256_storeu_ps(re.as_mut_ptr().add((x0 + 7) * n + y0), ta7);
                _mm256_storeu_ps(im.as_mut_ptr().add(x0 * n + y0), tb0);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 1) * n + y0), tb1);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 2) * n + y0), tb2);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 3) * n + y0), tb3);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 4) * n + y0), tb4);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 5) * n + y0), tb5);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 6) * n + y0), tb6);
                _mm256_storeu_ps(im.as_mut_ptr().add((x0 + 7) * n + y0), tb7);
            }
        }
    }
}

// ---- matchTemplate 归一化（AVX2，f64 积分图）----

/// 归一化互相关结果（AVX2）。一次处理 4 个 x（__m256d，f64）。
///
/// 逻辑对齐 `scalar_normalize_corr`（OpenCV templmatch.cpp）。
///
/// # SAFETY
/// - `corr.len() >= rw*rh`，`out.len() >= rw*rh`。
/// - `sum.len() >= (w+1)*(h+1)`，`sqsum` 同。
#[allow(clippy::too_many_arguments)]
pub unsafe fn normalize_corr_avx2(
    corr: &[f32],
    sum: &[i32],
    sqsum: &[f64],
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    rw: usize,
    rh: usize,
    num_type: i32,
    is_normed: bool,
    is_sqdiff_normed: bool,
    inv_area: f64,
    templ_mean: f64,
    templ_norm: f64,
    templ_sum2: f64,
    out: &mut [f32],
) {
    // 行间独立，多线程池时按行块并行
    let nthreads = rayon::current_num_threads();
    const MIN_ROWS: usize = 64;
    if nthreads > 1 && rh >= MIN_ROWS * 2 {
        use rayon::prelude::*;
        let max_blocks = rh.div_ceil(MIN_ROWS).min(nthreads * 4);
        let rows_per_block = rh.div_ceil(max_blocks);
        let block_len = rows_per_block * rw;
        out.par_chunks_mut(block_len).enumerate().for_each(|(bi, oblk)| {
            let y0 = bi * rows_per_block;
            let rows = oblk.len() / rw;
            normalize_corr_rows_avx2(
                corr, sum, sqsum, w, h, tw, th, rw, rh,
                num_type, is_normed, is_sqdiff_normed,
                inv_area, templ_mean, templ_norm, templ_sum2,
                oblk, y0, y0 + rows,
            );
        });
    } else {
        normalize_corr_rows_avx2(
            corr, sum, sqsum, w, h, tw, th, rw, rh,
            num_type, is_normed, is_sqdiff_normed,
            inv_area, templ_mean, templ_norm, templ_sum2,
            out, 0, rh,
        );
    }
}

unsafe fn normalize_corr_rows_avx2(
    corr: &[f32],
    sum: &[i32],
    sqsum: &[f64],
    w: usize,
    h: usize,
    tw: usize,
    th: usize,
    rw: usize,
    rh: usize,
    num_type: i32,
    is_normed: bool,
    is_sqdiff_normed: bool,
    inv_area: f64,
    templ_mean: f64,
    templ_norm: f64,
    templ_sum2: f64,
    out: &mut [f32],
    y0: usize,
    y1: usize,
) {
    let sw = w + 1;
    let _ = h;
    let has_sum = !sum.is_empty();
    let has_sqsum = !sqsum.is_empty();
    let eps = 10.0 * f32::EPSILON as f64;
    let v_inv_area = _mm256_set1_pd(inv_area);
    let v_templ_mean = _mm256_set1_pd(templ_mean);
    let v_templ_norm = _mm256_set1_pd(templ_norm);
    let v_templ_sum2 = _mm256_set1_pd(templ_sum2);
    let v_eps = _mm256_set1_pd(eps);
    let v_half = _mm256_set1_pd(0.5);
    let v_1125 = _mm256_set1_pd(1.125);
    let v_zero = _mm256_setzero_pd();
    let v_one = _mm256_set1_pd(1.0);
    let v_neg_one = _mm256_set1_pd(-1.0);
    for y in y0..y1 {
        let row0 = y * sw;
        let row1 = (y + th) * sw;
        let corr_row = &corr[y * rw..];
        let out_row = &mut out[(y - y0) * rw..];
        let mut x = 0usize;
        while x + 4 <= rw {
            // 窗口和（4 路 i32 → f64；sum 为空时用 0）
            // wnd_sum = p0 - p1 - p2 + p3 = (p0-p1) - (p2-p3)
            let wnd_sum = if has_sum {
                let p0 = _mm256_cvtepi32_pd(_mm_loadu_si128(sum.as_ptr().add(row0 + x) as *const __m128i));
                let p1 = _mm256_cvtepi32_pd(_mm_loadu_si128(sum.as_ptr().add(row0 + x + tw) as *const __m128i));
                let p2 = _mm256_cvtepi32_pd(_mm_loadu_si128(sum.as_ptr().add(row1 + x) as *const __m128i));
                let p3 = _mm256_cvtepi32_pd(_mm_loadu_si128(sum.as_ptr().add(row1 + x + tw) as *const __m128i));
                _mm256_sub_pd(
                    _mm256_sub_pd(p0, p1),
                    _mm256_sub_pd(p2, p3),
                )
            } else {
                v_zero
            };
            // num = corr（f32 → f64）
            let c = _mm256_cvtps_pd(_mm_loadu_ps(corr_row.as_ptr().add(x)));
            let mut num = c;
            let mut wnd_mean2 = v_zero;
            if num_type == 1 {
                let ws2 = _mm256_mul_pd(wnd_sum, wnd_sum);
                wnd_mean2 = _mm256_mul_pd(ws2, v_inv_area);
                let adj = _mm256_mul_pd(wnd_sum, v_templ_mean);
                num = _mm256_sub_pd(num, adj);
            }
            let mut wnd_sum2 = v_zero;
            if has_sqsum && (is_normed || num_type == 2) {
                let q0 = _mm256_loadu_pd(sqsum.as_ptr().add(row0 + x));
                let q1 = _mm256_loadu_pd(sqsum.as_ptr().add(row0 + x + tw));
                let q2 = _mm256_loadu_pd(sqsum.as_ptr().add(row1 + x));
                let q3 = _mm256_loadu_pd(sqsum.as_ptr().add(row1 + x + tw));
                // wnd_sum2 = q0 - q1 - q2 + q3 = (q0-q1) - (q2-q3)
                wnd_sum2 = _mm256_sub_pd(
                    _mm256_sub_pd(q0, q1),
                    _mm256_sub_pd(q2, q3),
                );
                if num_type == 2 {
                    // num = wnd_sum2 - 2*num + templ_sum2，clamp >= 0
                    let two_n = _mm256_mul_pd(num, _mm256_set1_pd(2.0));
                    num = _mm256_sub_pd(_mm256_add_pd(wnd_sum2, v_templ_sum2), two_n);
                    num = _mm256_max_pd(num, v_zero);
                }
            }
            if is_normed {
                // diff2 = max(wnd_sum2 - wnd_mean2, 0)
                let diff2 = _mm256_max_pd(_mm256_sub_pd(wnd_sum2, wnd_mean2), v_zero);
                // t = diff2 <= min(0.5, eps*wnd_sum2) ? 0 : sqrt(diff2)*templ_norm
                let thresh = _mm256_mul_pd(v_eps, wnd_sum2);
                let thresh = _mm256_min_pd(v_half, thresh);
                let is_zero = _mm256_cmp_pd(diff2, thresh, _CMP_LE_OQ);
                let sq = _mm256_mul_pd(_mm256_sqrt_pd(diff2), v_templ_norm);
                let t = _mm256_blendv_pd(sq, v_zero, is_zero);
                // |num| < t → num/t；|num| < t*1.125 → ±1；否则 0 或 1
                let abs_num = _mm256_andnot_pd(_mm256_set1_pd(-0.0), num);
                let lt_t = _mm256_cmp_pd(abs_num, t, _CMP_LT_OQ);
                let t1125 = _mm256_mul_pd(t, v_1125);
                let lt_1125 = _mm256_cmp_pd(abs_num, t1125, _CMP_LT_OQ);
                // 安全除法：t=0 时 |num|<t 不成立（t=0），走 else 分支
                let quotient = _mm256_div_pd(num, _mm256_max_pd(t, _mm256_set1_pd(f64::MIN_POSITIVE)));
                let sign = _mm256_blendv_pd(v_neg_one, v_one, _mm256_cmp_pd(num, v_zero, _CMP_GE_OQ));
                let fallback = if is_sqdiff_normed { v_one } else { v_zero };
                // 三级选择：lt_t ? quotient : (lt_1125 ? sign : fallback)
                let mid = _mm256_blendv_pd(fallback, sign, lt_1125);
                num = _mm256_blendv_pd(mid, quotient, lt_t);
            }
            // 写回 f32
            let res = _mm256_cvtpd_ps(num);
            _mm_storeu_ps(out_row.as_mut_ptr().add(x), res);
            x += 4;
        }
        while x < rw {
            // 标量尾
            let mut num = corr_row[x] as f64;
            let mut wnd_mean2 = 0.0f64;
            let mut wnd_sum2 = 0.0f64;
            let wnd_sum = if has_sum {
                (sum[row0 + x] - sum[row0 + x + tw] - sum[row1 + x] + sum[row1 + x + tw]) as f64
            } else {
                0.0
            };
            if num_type == 1 {
                wnd_mean2 = wnd_sum * wnd_sum * inv_area;
                num -= wnd_sum * templ_mean;
            }
            if has_sqsum && (is_normed || num_type == 2) {
                wnd_sum2 = sqsum[row0 + x] - sqsum[row0 + x + tw] - sqsum[row1 + x] + sqsum[row1 + x + tw];
                if num_type == 2 {
                    num = (wnd_sum2 - 2.0 * num + templ_sum2).max(0.0);
                }
            }
            if is_normed {
                let diff2 = (wnd_sum2 - wnd_mean2).max(0.0);
                let t = if diff2 <= 0.5f64.min(eps * wnd_sum2) { 0.0 } else { diff2.sqrt() * templ_norm };
                if num.abs() < t {
                    num /= t;
                } else if num.abs() < t * 1.125 {
                    num = if num > 0.0 { 1.0 } else { -1.0 };
                } else {
                    num = if !is_sqdiff_normed { 0.0 } else { 1.0 };
                }
            }
            out_row[x] = num as f32;
            x += 1;
        }
    }
}

/// u8 行转 f32 行（AVX2，一次 8 像素）。
///
/// # SAFETY
/// - `src.len() == dst.len()`。
#[target_feature(enable = "avx2")]
pub unsafe fn u8_to_f32_row(src: &[u8], dst: &mut [f32]) {
    debug_assert_eq!(src.len(), dst.len());
    let n = src.len();
    let mut x = 0usize;
    while x + 8 <= n {
        let v = _mm_loadl_epi64(src.as_ptr().add(x) as *const __m128i);
        let v32 = _mm256_cvtepu8_epi32(v);
        _mm256_storeu_ps(dst.as_mut_ptr().add(x), _mm256_cvtepi32_ps(v32));
        x += 8;
    }
    while x < n {
        dst[x] = src[x] as f32;
        x += 1;
    }
}

/// 逐元素取负：`dst[i] = -src[i]`（AVX2，一次 8 个 f32）。
///
/// # SAFETY
/// - `src.len() == dst.len()`
#[target_feature(enable = "avx2")]
pub unsafe fn negate_f32(src: &[f32], dst: &mut [f32]) {
    debug_assert_eq!(src.len(), dst.len());
    let n = src.len();
    let mut i = 0usize;
    while i + 8 <= n {
        let v = _mm256_loadu_ps(src.as_ptr().add(i));
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), _mm256_xor_ps(v, SIGN_MASK));
        i += 8;
    }
    while i < n {
        dst[i] = -src[i];
        i += 1;
    }
}
