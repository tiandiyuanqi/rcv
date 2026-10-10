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
            crate::simd_kernels::edges::nms_one(
                mag_p.as_ptr(), mag_a.as_ptr(), mag_n.as_ptr(), dx.as_ptr(), dy.as_ptr(),
                pmap.as_mut_ptr(), k, high, stack,
            );
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
