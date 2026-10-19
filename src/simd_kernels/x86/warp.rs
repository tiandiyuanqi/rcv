//! AVX2 warp/resize 内核（位级对齐 OpenCV5）。
//!
//! 语义基准（实测确认）：
//! - **resize**：OpenCV5 的 resize.cpp 无 `CV_CPU_DISPATCH`，仅 baseline SSE2 编译。
//!   v_int16 = 8 lanes、v_float32 = 4 lanes、v_muladd = 分离乘加（无 FMA）、
//!   v_round = SSE2 cvtps2dq（ties-even，溢出 → i32::MIN）。
//! - **warp**：warp_kernels.simd.hpp 用 `CV_CPU_OPTIMIZATION_NAMESPACE`，AVX2 dispatch。
//!   uf = 16、v_float32 = 8 lanes、FMA。
//!
//! 所有运算逐 lane 独立，AVX2 8/16-lane 批处理与标量复刻逐位一致。

use std::arch::x86_64::*;

// ---- 常量向量（.rodata，编译期物化）----
// SAFETY: transmute 从 const 字节数组构造 __m256i，位模式确定，无运行时初始化。
// 0xFF 掩码（取 gather 低字节）
static MASK_FF: __m256i = unsafe { std::mem::transmute::<[i32; 8], __m256i>([0xFF; 8]) };
// i16 交错掩码：把 [s0_0..s0_7, s1_0..s1_7] 重排为 [s0_0,s1_0,s0_1,s1_1,...]
// 每 128-bit lane：字节 [0,1, 8,9, 2,3, 10,11, 4,5, 12,13, 6,7, 14,15]
static INTERLEAVE: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        0, 1, 8, 9, 2, 3, 10, 11, 4, 5, 12, 13, 6, 7, 14, 15, 0, 1, 8, 9, 2, 3, 10, 11, 4, 5, 12,
        13, 6, 7, 14, 15,
    ])
};
// gather dword 对提取掩码（hresize_cubic cn=3 用）：dword i 的字节 0/3 是
// 位置 i 的 [tap0, tap1]（同通道），提取为 i16 对 [tap0, 0, tap1, 0]
// （每值独立 i16，高字节零扩展）。每 lane 字节 [4i, -1, 4i+3, -1]（i = 0..3）。
static SH_PAIR: __m256i = unsafe {
    std::mem::transmute::<[i8; 32], __m256i>([
        0, -1, 3, -1, 4, -1, 7, -1, 8, -1, 11, -1, 12, -1, 15, -1, 0, -1, 3, -1, 4, -1, 7, -1,
        8, -1, 11, -1, 12, -1, 15, -1,
    ])
};

/// HResize LINEAR（u8 → i32 缓冲），处理扁平域 `[start, end)`，每批 8 元素。
///
/// 对齐 OpenCV `HResizeLinear<uchar,int,short>` + `HResizeLinearVecU8_X4`：
/// `D[dx] = S[xofs[dx]]*ialpha[dx*2] + S[xofs[dx]+cn]*ialpha[dx*2+1]`（i32）。
///
/// `srow_len` 为源行扁平长度（`sw*C`）。gather 读 4 字节，需保证 `xofs[dx]+cn+3 < srow_len`
/// 才安全；越界 lane 回退标量（xofs 单调不减，检查每批末 lane 即可）。
///
/// # Safety
/// - `src` 长度 ≥ `srow_len`；`xofs` 长度 ≥ `end`；`ialpha` 长度 ≥ `end*2`；`dst` 长度 ≥ `end`
/// - `start`/`end` 为扁平域索引（已 ×C）
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn hresize_linear_u8(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    let mut dx = start;
    let cnv = _mm256_set1_epi32(cn as i32);
    let safe_limit = (srow_len as i32) - 4;
    // SAFETY: 循环条件 dx+8 <= end 保证 xofs/ialpha/dst 读写在界内；
    // gather 前检查 xofs[dx+7]+cn+3 < srow_len 保证 4 字节读不越界（xofs 单调不减）
    unsafe {
        while dx + 8 <= end {
            // xofs 单调不减：末 lane 最大，检查其 gather 是否越界
            if xofs[dx + 7] + cn as i32 + 3 >= safe_limit {
                break;
            }
            let idx = _mm256_loadu_si256(xofs[dx..].as_ptr() as *const __m256i);
            let idx1 = _mm256_add_epi32(idx, cnv);
            // gather 4 字节：lane = [S[xofs], S[xofs]+1, +2, +3]（cn=1 时 +1 即右采样）
            let g0 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idx, 1);
            let g1 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idx1, 1);
            let s0 = _mm256_and_si256(g0, MASK_FF);
            let s1 = _mm256_and_si256(g1, MASK_FF);
            // 打包为 i16 对并交错：[s0_0,s1_0,s0_1,s1_1,...]
            let packed = _mm256_packs_epi32(s0, s1);
            let inter = _mm256_shuffle_epi8(packed, INTERLEAVE);
            // alpha 已是交错布局 [a0_0,a1_0,a0_1,a1_1,...]
            let al = _mm256_loadu_si256(ialpha[dx * 2..].as_ptr() as *const __m256i);
            let d = _mm256_madd_epi16(inter, al);
            _mm256_storeu_si256(dst[dx..].as_mut_ptr() as *mut __m256i, d);
            dx += 8;
        }
    }
    // 标量尾段（含 area_mode 越界读：sx+cn 跨行时权重为 0，用 0 替代）
    while dx < end {
        let sx = xofs[dx] as usize;
        let a0 = ialpha[dx * 2] as i32;
        let a1 = ialpha[dx * 2 + 1] as i32;
        let s0 = if sx < srow_len { src[sx] as i32 } else { 0 };
        let s1 = if sx + cn < srow_len { src[sx + cn] as i32 } else { 0 };
        dst[dx] = s0 * a0 + s1 * a1;
        dx += 1;
    }
}

/// VResize LINEAR（i32 缓冲 → u8），每批 16 输出。
///
/// 对齐 OpenCV `VResizeLinear<uchar,int,short,FixedPtCast>` + `VResizeLinearVec_32s8u`：
/// `dst[x] = sat_u8(((b0*(S0[x]>>4))>>16 + (b1*(S1[x]>>4))>>16 + 2)>>2)`。
///
/// # Safety
/// - `s0`/`s1` 长度 ≥ `width`；`dst` 长度 ≥ `width`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn vresize_linear_u8(s0: &[i32], s1: &[i32], dst: &mut [u8], b0: i16, b1: i16, width: usize) {
    let b0v = _mm256_set1_epi16(b0);
    let b1v = _mm256_set1_epi16(b1);
    let two = _mm256_set1_epi16(2);
    let mut x = 0;
    // SAFETY: 循环条件 x+16 <= width 保证 s0/s1 读 [x, x+16) 与 dst 写 [x, x+16) 不越界
    unsafe {
        while x + 16 <= width {
            let s0a = _mm256_loadu_si256(s0[x..].as_ptr() as *const __m256i);
            let s0b = _mm256_loadu_si256(s0[x + 8..].as_ptr() as *const __m256i);
            let s1a = _mm256_loadu_si256(s1[x..].as_ptr() as *const __m256i);
            let s1b = _mm256_loadu_si256(s1[x + 8..].as_ptr() as *const __m256i);
            let s0a = _mm256_srai_epi32(s0a, 4);
            let s0b = _mm256_srai_epi32(s0b, 4);
            let s1a = _mm256_srai_epi32(s1a, 4);
            let s1b = _mm256_srai_epi32(s1b, 4);
            // pack i32→i16（饱和，值 ≤ 32640 无饱和），修正 lane 顺序
            let p0 = _mm256_permute4x64_epi64(_mm256_packs_epi32(s0a, s0b), 0xD8);
            let p1 = _mm256_permute4x64_epi64(_mm256_packs_epi32(s1a, s1b), 0xD8);
            let m0 = _mm256_mulhi_epi16(p0, b0v);
            let m1 = _mm256_mulhi_epi16(p1, b1v);
            let sum = _mm256_add_epi16(_mm256_add_epi16(m0, m1), two);
            let sum = _mm256_srli_epi16(sum, 2);
            // pack u16→u8（饱和），取低 128 位 = 16 输出
            let out = _mm256_packus_epi16(sum, sum);
            let out = _mm256_permute4x64_epi64(out, 0xD8);
            _mm_storeu_si128(dst[x..].as_mut_ptr() as *mut __m128i, _mm256_castsi256_si128(out));
            x += 16;
        }
    }
    // 标量尾段
    while x < width {
        let v = ((b0 as i32 * (s0[x] >> 4)) >> 16) + ((b1 as i32 * (s1[x] >> 4)) >> 16) + 2;
        dst[x] = ((v >> 2).clamp(0, 255)) as u8;
        x += 1;
    }
}

/// 2×2 area-fast 缩小（cn=1），每批 16 输出。
///
/// 对齐 OpenCV `ResizeAreaFastVec_SIMD_8u`（cn=1）：
/// `dst[dx] = (S0[2dx] + S0[2dx+1] + S1[2dx] + S1[2dx+1] + 2) >> 2`。
///
/// # Safety
/// - `src` 长度 ≥ `(2*dh)*sw`；`dst` 长度 ≥ `dw*dh`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn area_fast_2x2_cn1(src: &[u8], dst: &mut [u8], sw: usize, dw: usize, dh: usize) {
    let mask = _mm256_set1_epi16(0x00FF);
    let two = _mm256_set1_epi16(2);
    for dy in 0..dh {
        let s0 = &src[dy * 2 * sw..];
        let s1 = &src[(dy * 2 + 1) * sw..];
        let drow = &mut dst[dy * dw..];
        let mut dx = 0;
        // SAFETY: dx+16 <= dw 保证 s0/s1 读 [dx*2, dx*2+32) 与 drow 写 [dx, dx+16) 不越界
        unsafe {
            while dx + 16 <= dw {
                let v0 = _mm256_loadu_si256(s0[dx * 2..].as_ptr() as *const __m256i);
                let v1 = _mm256_loadu_si256(s1[dx * 2..].as_ptr() as *const __m256i);
                let e0 = _mm256_and_si256(v0, mask);
                let o0 = _mm256_srli_epi16(v0, 8);
                let e1 = _mm256_and_si256(v1, mask);
                let o1 = _mm256_srli_epi16(v1, 8);
                let sum = _mm256_add_epi16(_mm256_add_epi16(e0, o0), _mm256_add_epi16(e1, o1));
                let sum = _mm256_srli_epi16(_mm256_add_epi16(sum, two), 2);
                let out = _mm256_packus_epi16(sum, sum);
                let out = _mm256_permute4x64_epi64(out, 0xD8);
                _mm_storeu_si128(drow[dx..].as_mut_ptr() as *mut __m128i, _mm256_castsi256_si128(out));
                dx += 16;
            }
        }
        // 标量尾段
        while dx < dw {
            let i = dx * 2;
            drow[dx] = ((s0[i] as u32 + s0[i + 1] as u32 + s1[i] as u32 + s1[i + 1] as u32 + 2) >> 2) as u8;
            dx += 1;
        }
    }
}

/// 2×2 area-fast 缩小（cn=3），每批 32 输出。
///
/// 对齐 OpenCV `ResizeAreaFastVec_SIMD_8u`（cn=3）：
/// `dst[dx*3+k] = (S0[2dx*3+k] + S0[2dx*3+3+k] + S1[2dx*3+k] + S1[2dx*3+3+k] + 2) >> 2`。
///
/// # Safety
/// - `src` 长度 ≥ `(2*dh)*sw*3`；`dst` 长度 ≥ `dw*dh*3`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn area_fast_2x2_cn3(src: &[u8], dst: &mut [u8], sw: usize, dw: usize, dh: usize) {
    let ones = _mm256_set1_epi8(1);
    let two = _mm256_set1_epi16(2);
    for dy in 0..dh {
        let s0 = &src[dy * 2 * sw * 3..];
        let s1 = &src[(dy * 2 + 1) * sw * 3..];
        let drow = &mut dst[dy * dw * 3..];
        let mut dx = 0;
        // SAFETY: dx+32 <= dw 保证 s0/s1 读 [dx*6, dx*6+192) 与 drow 写 [dx*3, dx*3+96) 不越界
        unsafe {
            while dx + 32 <= dw {
                let a0 = _mm256_loadu_si256(s0[dx * 6..].as_ptr() as *const __m256i);
                let b0 = _mm256_loadu_si256(s0[dx * 6 + 32..].as_ptr() as *const __m256i);
                let c0 = _mm256_loadu_si256(s0[dx * 6 + 64..].as_ptr() as *const __m256i);
                let d0 = _mm256_loadu_si256(s0[dx * 6 + 96..].as_ptr() as *const __m256i);
                let e0 = _mm256_loadu_si256(s0[dx * 6 + 128..].as_ptr() as *const __m256i);
                let f0 = _mm256_loadu_si256(s0[dx * 6 + 160..].as_ptr() as *const __m256i);
                let a1 = _mm256_loadu_si256(s1[dx * 6..].as_ptr() as *const __m256i);
                let b1 = _mm256_loadu_si256(s1[dx * 6 + 32..].as_ptr() as *const __m256i);
                let c1 = _mm256_loadu_si256(s1[dx * 6 + 64..].as_ptr() as *const __m256i);
                let d1 = _mm256_loadu_si256(s1[dx * 6 + 96..].as_ptr() as *const __m256i);
                let e1 = _mm256_loadu_si256(s1[dx * 6 + 128..].as_ptr() as *const __m256i);
                let f1 = _mm256_loadu_si256(s1[dx * 6 + 160..].as_ptr() as *const __m256i);
                // 解交织 3 通道（32 源像素 → 32 u8/通道）
                let (b0ch, g0ch, r0ch) = super::avx2::deinterleave3(a0, b0, c0);
                let (b0ch2, g0ch2, r0ch2) = super::avx2::deinterleave3(d0, e0, f0);
                let (b1ch, g1ch, r1ch) = super::avx2::deinterleave3(a1, b1, c1);
                let (b1ch2, g1ch2, r1ch2) = super::avx2::deinterleave3(d1, e1, f1);
                // 每通道：相邻对求和（maddubs），row0+row1，+2，>>2
                let r0s = _mm256_maddubs_epi16(r0ch, ones);
                let r0s2 = _mm256_maddubs_epi16(r0ch2, ones);
                let r1s = _mm256_maddubs_epi16(r1ch, ones);
                let r1s2 = _mm256_maddubs_epi16(r1ch2, ones);
                let r_sum = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(r0s, r1s), two), 2);
                let r_sum2 = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(r0s2, r1s2), two), 2);
                let r_out = _mm256_permute4x64_epi64(_mm256_packus_epi16(r_sum, r_sum2), 0xD8);
                let g0s = _mm256_maddubs_epi16(g0ch, ones);
                let g0s2 = _mm256_maddubs_epi16(g0ch2, ones);
                let g1s = _mm256_maddubs_epi16(g1ch, ones);
                let g1s2 = _mm256_maddubs_epi16(g1ch2, ones);
                let g_sum = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(g0s, g1s), two), 2);
                let g_sum2 = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(g0s2, g1s2), two), 2);
                let g_out = _mm256_permute4x64_epi64(_mm256_packus_epi16(g_sum, g_sum2), 0xD8);
                let b0s = _mm256_maddubs_epi16(b0ch, ones);
                let b0s2 = _mm256_maddubs_epi16(b0ch2, ones);
                let b1s = _mm256_maddubs_epi16(b1ch, ones);
                let b1s2 = _mm256_maddubs_epi16(b1ch2, ones);
                let b_sum = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(b0s, b1s), two), 2);
                let b_sum2 = _mm256_srli_epi16(_mm256_add_epi16(_mm256_add_epi16(b0s2, b1s2), two), 2);
                let b_out = _mm256_permute4x64_epi64(_mm256_packus_epi16(b_sum, b_sum2), 0xD8);
                super::avx2::interleave3_store(drow[dx * 3..].as_mut_ptr(), b_out, g_out, r_out);
                dx += 32;
            }
        }
        // 标量尾段
        while dx < dw {
            let i = dx * 6;
            for k in 0..3 {
                drow[dx * 3 + k] = ((s0[i + k] as u32 + s0[i + 3 + k] as u32 + s1[i + k] as u32
                    + s1[i + 3 + k] as u32
                    + 2)
                    >> 2) as u8;
            }
            dx += 1;
        }
    }
}

/// HResize CUBIC（u8 → i32 缓冲），处理扁平域 `[start, end)`，每批 8 元素。
///
/// 对齐 OpenCV `HResizeCubic<uchar,int,short>`（纯标量，可自由向量化）：
/// `D[dx] = Σ_{j=0..3} S[sx + j*C] * ialpha[dx*4 + j]`，其中 `sx = xofs[dx] - C`。
///
/// `srow_len` 为源行扁平长度（`sw*C`）。gather 读 4 字节，需保证越界 lane 回退标量。
///
/// cn=3 且 xofs 呈"像素间等距 stride ≤ 6、组内连续"模式（整数倍下采样，
/// 如 scale=2 时 stride=6）时走 loadu+shuffle 快路径（消除 gather）。
///
/// # Safety
/// - `src` 长度 ≥ `srow_len`；`xofs` 长度 ≥ `end`；`ialpha` 长度 ≥ `end*4`；`dst` 长度 ≥ `end`
/// - `start`/`end` 为扁平域索引（已 ×C）
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn hresize_cubic_u8(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    ialpha01: &[i16],
    ialpha23: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    let mut dx = start;
    let safe_limit = (srow_len as i32) - 4;
    // SAFETY: 循环条件 dx+16 <= end 保证 xofs/ialpha/dst 读写在界内；
    // gather 前检查 xofs[dx+15]+cn*2+3 < srow_len 保证 4 字节读不越界（xofs 单调不减）
    unsafe {
        // cn=3：gather dword 的字节 0/3 即 [tap0, tap1]（同通道），SH_PAIR 一步提取
        // cn=1：单 gather 4 字节 = 4 taps，packs+INTERLEAVE 交错
        while dx + 16 <= end {
            // 末 tap 位置 xofs[dx+15]-C + 3C = xofs[dx+15]+2C，读 4 字节到 +2C+3
            if xofs[dx + 15] + (cn as i32) * 2 + 3 >= safe_limit {
                break;
            }
            // 组 0（dx..dx+8）
            let idx0 = _mm256_loadu_si256(xofs[dx..].as_ptr() as *const __m256i);
            let (src01, src23) = if cn == 1 {
                let idxm = _mm256_sub_epi32(idx0, _mm256_set1_epi32(1));
                let g = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g0 = _mm256_and_si256(g, MASK_FF);
                let g1 = _mm256_and_si256(_mm256_srli_epi32(g, 8), MASK_FF);
                let g2 = _mm256_and_si256(_mm256_srli_epi32(g, 16), MASK_FF);
                let g3 = _mm256_srli_epi32(g, 24);
                (
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g0, g1), INTERLEAVE),
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g2, g3), INTERLEAVE),
                )
            } else {
                let idxm = _mm256_sub_epi32(idx0, _mm256_set1_epi32(3));
                let g01 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g23 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, _mm256_add_epi32(idx0, _mm256_set1_epi32(3)), 1);
                (
                    _mm256_shuffle_epi8(g01, SH_PAIR),
                    _mm256_shuffle_epi8(g23, SH_PAIR),
                )
            };
            let a01 = _mm256_loadu_si256(ialpha01[dx * 2..].as_ptr() as *const __m256i);
            let a23 = _mm256_loadu_si256(ialpha23[dx * 2..].as_ptr() as *const __m256i);
            let d0 = _mm256_add_epi32(
                _mm256_madd_epi16(src01, a01),
                _mm256_madd_epi16(src23, a23),
            );
            _mm256_storeu_si256(dst[dx..].as_mut_ptr() as *mut __m256i, d0);
            // 组 1（dx+8..dx+16）
            let idx1 = _mm256_loadu_si256(xofs[dx + 8..].as_ptr() as *const __m256i);
            let (src01, src23) = if cn == 1 {
                let idxm = _mm256_sub_epi32(idx1, _mm256_set1_epi32(1));
                let g = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g0 = _mm256_and_si256(g, MASK_FF);
                let g1 = _mm256_and_si256(_mm256_srli_epi32(g, 8), MASK_FF);
                let g2 = _mm256_and_si256(_mm256_srli_epi32(g, 16), MASK_FF);
                let g3 = _mm256_srli_epi32(g, 24);
                (
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g0, g1), INTERLEAVE),
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g2, g3), INTERLEAVE),
                )
            } else {
                let idxm = _mm256_sub_epi32(idx1, _mm256_set1_epi32(3));
                let g01 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g23 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, _mm256_add_epi32(idx1, _mm256_set1_epi32(3)), 1);
                (
                    _mm256_shuffle_epi8(g01, SH_PAIR),
                    _mm256_shuffle_epi8(g23, SH_PAIR),
                )
            };
            let a01 = _mm256_loadu_si256(ialpha01[(dx + 8) * 2..].as_ptr() as *const __m256i);
            let a23 = _mm256_loadu_si256(ialpha23[(dx + 8) * 2..].as_ptr() as *const __m256i);
            let d1 = _mm256_add_epi32(
                _mm256_madd_epi16(src01, a01),
                _mm256_madd_epi16(src23, a23),
            );
            _mm256_storeu_si256(dst[dx + 8..].as_mut_ptr() as *mut __m256i, d1);
            dx += 16;
        }
        // 8 位置尾段（dx+8 <= end）
        while dx + 8 <= end {
            if xofs[dx + 7] + (cn as i32) * 2 + 3 >= safe_limit {
                break;
            }
            let idx = _mm256_loadu_si256(xofs[dx..].as_ptr() as *const __m256i);
            let (src01, src23) = if cn == 1 {
                let idxm = _mm256_sub_epi32(idx, _mm256_set1_epi32(1));
                let g = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g0 = _mm256_and_si256(g, MASK_FF);
                let g1 = _mm256_and_si256(_mm256_srli_epi32(g, 8), MASK_FF);
                let g2 = _mm256_and_si256(_mm256_srli_epi32(g, 16), MASK_FF);
                let g3 = _mm256_srli_epi32(g, 24);
                (
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g0, g1), INTERLEAVE),
                    _mm256_shuffle_epi8(_mm256_packs_epi32(g2, g3), INTERLEAVE),
                )
            } else {
                let idxm = _mm256_sub_epi32(idx, _mm256_set1_epi32(3));
                let g01 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, idxm, 1);
                let g23 = _mm256_i32gather_epi32(src.as_ptr() as *const i32, _mm256_add_epi32(idx, _mm256_set1_epi32(3)), 1);
                (
                    _mm256_shuffle_epi8(g01, SH_PAIR),
                    _mm256_shuffle_epi8(g23, SH_PAIR),
                )
            };
            let a01 = _mm256_loadu_si256(ialpha01[dx * 2..].as_ptr() as *const __m256i);
            let a23 = _mm256_loadu_si256(ialpha23[dx * 2..].as_ptr() as *const __m256i);
            let d = _mm256_add_epi32(
                _mm256_madd_epi16(src01, a01),
                _mm256_madd_epi16(src23, a23),
            );
            _mm256_storeu_si256(dst[dx..].as_mut_ptr() as *mut __m256i, d);
            dx += 8;
        }
    }
    // 标量尾段（含越界读：sx+3C 跨行时权重为 0，用 0 替代）
    while dx < end {
        let sx = xofs[dx] as usize - cn;
        let mut v: i32 = 0;
        for j in 0..4 {
            let idx = sx + j * cn;
            let sval = if idx < srow_len { src[idx] as i32 } else { 0 };
            v += sval * ialpha[dx * 4 + j] as i32;
        }
        dst[dx] = v;
        dx += 1;
    }
}

/// VResize CUBIC（i32 缓冲 → u8），每批 8 输出。
///
/// 对齐 OpenCV `VResizeCubicVec_32s8u`（SSE2 baseline，v_int16 = 8 lanes）：
/// f32 分离乘加链（无 FMA，每步舍入）+ v_round（cvtps2dq ties-even）+ 饱和 pack。
/// 边界为 8 的倍数（与 SSE2 一致），尾段走标量 FixedPtCast。
///
/// `b` 为整数 ibeta（`[i32; 4]`），`scale = 1/(COEF_SCALE²)`。
///
/// # Safety
/// - `s0`/`s1`/`s2`/`s3` 长度 ≥ `width`；`dst` 长度 ≥ `width`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn vresize_cubic_u8(
    s0: &[i32],
    s1: &[i32],
    s2: &[i32],
    s3: &[i32],
    dst: &mut [u8],
    b: [i32; 4],
    scale: f32,
    width: usize,
) {
    let b0v = _mm256_set1_ps(b[0] as f32 * scale);
    let b1v = _mm256_set1_ps(b[1] as f32 * scale);
    let b2v = _mm256_set1_ps(b[2] as f32 * scale);
    let b3v = _mm256_set1_ps(b[3] as f32 * scale);
    let mut x = 0;
    // SAFETY: x+8 <= width 保证 s0..s3 读 [x, x+8) 与 dst 写 [x, x+8) 不越界
    unsafe {
        while x + 8 <= width {
            let s3v = _mm256_cvtepi32_ps(_mm256_loadu_si256(s3[x..].as_ptr() as *const __m256i));
            let acc = _mm256_mul_ps(s3v, b3v);
            let s2v = _mm256_cvtepi32_ps(_mm256_loadu_si256(s2[x..].as_ptr() as *const __m256i));
            let acc = _mm256_add_ps(_mm256_mul_ps(s2v, b2v), acc);
            let s1v = _mm256_cvtepi32_ps(_mm256_loadu_si256(s1[x..].as_ptr() as *const __m256i));
            let acc = _mm256_add_ps(_mm256_mul_ps(s1v, b1v), acc);
            let s0v = _mm256_cvtepi32_ps(_mm256_loadu_si256(s0[x..].as_ptr() as *const __m256i));
            let acc = _mm256_add_ps(_mm256_mul_ps(s0v, b0v), acc);
            // v_round（cvtps2dq ties-even）
            let r = _mm256_cvtps_epi32(acc);
            // pack i32→i16（饱和），修正 lane 顺序
            let p = _mm256_permute4x64_epi64(_mm256_packs_epi32(r, r), 0xD8);
            // pack i16→u8（饱和），取低 128 位 = 8 输出
            let lo = _mm256_castsi256_si128(p);
            let out = _mm_packus_epi16(lo, lo);
            _mm_storel_epi64(dst[x..].as_mut_ptr() as *mut __m128i, out);
            x += 8;
        }
    }
    // 标量尾段（FixedPtCast (val+2^21)>>22）
    while x < width {
        let acc = s0[x] * b[0] + s1[x] * b[1] + s2[x] * b[2] + s3[x] * b[3];
        dst[x] = (((acc + (1 << 21)) >> 22).clamp(0, 255)) as u8;
        x += 1;
    }
}

// ---- Warp LINEAR 快路径（AVX2，每批 8 像素）----

/// 8-lane 双线性插值（fma 语义，round ties-even）。
///
/// `p00`/`p01`/`p10`/`p11` 为 8 个 i32（各含一个 u8 值），`sx_frac`/`sy_frac` 为 8 个 f32。
/// 返回 8 个 i32（已 round ties-even，未饱和）。
#[inline(always)]
unsafe fn interp8(
    sx_frac: __m256,
    sy_frac: __m256,
    p00: __m256i,
    p01: __m256i,
    p10: __m256i,
    p11: __m256i,
) -> __m256i {
    let p00f = _mm256_cvtepi32_ps(p00);
    let p01f = _mm256_cvtepi32_ps(p01);
    let p10f = _mm256_cvtepi32_ps(p10);
    let p11f = _mm256_cvtepi32_ps(p11);
    let v0 = _mm256_fmadd_ps(sx_frac, _mm256_sub_ps(p01f, p00f), p00f);
    let v1 = _mm256_fmadd_ps(sx_frac, _mm256_sub_ps(p11f, p10f), p10f);
    let v = _mm256_fmadd_ps(sy_frac, _mm256_sub_ps(v1, v0), v0);
    _mm256_cvtps_epi32(v)
}

/// 8 个 i32 → 8 个 u8（低 64 位）。
///
/// `_mm256_packs_epi32(r, r)` 会复制低 4 个元素，需结合低/高 128 位取前 8 个。
#[inline(always)]
unsafe fn pack8_u8(r: __m256i) -> __m128i {
    let p = _mm256_packs_epi32(r, r);
    let p = _mm256_packus_epi16(p, p);
    let lo = _mm256_castsi256_si128(p);
    let hi = _mm256_extracti128_si256(p, 1);
    _mm_unpacklo_epi32(lo, hi)
}

/// 3×8 u8 → 24 字节交错存储（RGB）。
///
/// `r0`/`r1`/`r2` 各含 8 个 u8（低 64 位），输出 `[r0_0,r1_0,r2_0, r0_1,r1_1,r2_1, ...]`。
#[inline(always)]
unsafe fn interleave3_8(dst: *mut u8, r0: __m256i, r1: __m256i, r2: __m256i) {
    // 每通道低 128 位复制到两个 lane（shuffle_epi8 不能跨 lane）
    let r0d = _mm256_permute2x128_si256(r0, r0, 0x00);
    let r1d = _mm256_permute2x128_si256(r1, r1, 0x00);
    let r2d = _mm256_permute2x128_si256(r2, r2, 0x00);
    // r0 → byte 3i；r1 → byte 3i+1；r2 → byte 3i+2
    const SH0: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            0, -1, -1, 1, -1, -1, 2, -1, -1, 3, -1, -1, 4, -1, -1, 5, -1, -1, 6, -1, -1, 7, -1,
            -1, -1, -1, -1, -1, -1, -1, -1, -1,
        ])
    };
    const SH1: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            -1, 0, -1, -1, 1, -1, -1, 2, -1, -1, 3, -1, -1, 4, -1, -1, 5, -1, -1, 6, -1, -1, 7,
            -1, -1, -1, -1, -1, -1, -1, -1, -1,
        ])
    };
    const SH2: __m256i = unsafe {
        std::mem::transmute::<[i8; 32], __m256i>([
            -1, -1, 0, -1, -1, 1, -1, -1, 2, -1, -1, 3, -1, -1, 4, -1, -1, 5, -1, -1, 6, -1, -1,
            7, -1, -1, -1, -1, -1, -1, -1, -1,
        ])
    };
    let o0 = _mm256_shuffle_epi8(r0d, SH0);
    let o1 = _mm256_shuffle_epi8(r1d, SH1);
    let o2 = _mm256_shuffle_epi8(r2d, SH2);
    let out = _mm256_or_si256(_mm256_or_si256(o0, o1), o2);
    let lo = _mm256_castsi256_si128(out);
    let hi = _mm256_extracti128_si256(out, 1);
    _mm_storeu_si128(dst as *mut __m128i, lo);
    _mm_storel_epi64(dst.add(16) as *mut __m128i, hi);
}

/// Warp LINEAR 快路径（AVX2，每批 8 像素）。
///
/// 对齐 warp_kernels.simd.hpp AVX2 语义：坐标 fma、floor、fma 插值、round ties-even。
/// `replicate=true` 时 clamp 坐标处理 Replicate 边界（覆盖全部像素）；否则仅处理
/// all-inlier 批（`ix<sw-1 && iy<sh-1`），不满足返回 `false` 由调用方回退标量。
///
/// `m` 为 affine 6 元素（已求逆转 f32）或 perspective 9 元素（`pm` 为 Some）。
///
/// # Safety
/// - `src` 长度 ≥ `sh*sw*cn`；`dst` 长度 ≥ `(y+1)*row + (x+8)*cn`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn warp_linear_fast_u8(
    src: &[u8],
    sw: usize,
    sh: usize,
    _step: usize,
    dst: &mut [u8],
    m: &[f32],
    pm: Option<&[f32; 9]>,
    y: usize,
    x: usize,
    cn: usize,
    replicate: bool,
) -> bool {
    let yf = y as f32;
    let xv = _mm256_setr_ps(
        x as f32,
        (x + 1) as f32,
        (x + 2) as f32,
        (x + 3) as f32,
        (x + 4) as f32,
        (x + 5) as f32,
        (x + 6) as f32,
        (x + 7) as f32,
    );
    let (sx, sy) = match pm {
        None => {
            let m_x = _mm256_set1_ps(yf * m[1] + m[2]);
            let m_y = _mm256_set1_ps(yf * m[4] + m[5]);
            let sx = _mm256_fmadd_ps(_mm256_set1_ps(m[0]), xv, m_x);
            let sy = _mm256_fmadd_ps(_mm256_set1_ps(m[3]), xv, m_y);
            (sx, sy)
        }
        Some(p) => {
            let m_x = _mm256_set1_ps(yf * p[1] + p[2]);
            let m_y = _mm256_set1_ps(yf * p[4] + p[5]);
            let m_w = _mm256_set1_ps(yf * p[7] + p[8]);
            let w = _mm256_fmadd_ps(_mm256_set1_ps(p[6]), xv, m_w);
            let xf = _mm256_fmadd_ps(_mm256_set1_ps(p[0]), xv, m_x);
            let yf2 = _mm256_fmadd_ps(_mm256_set1_ps(p[3]), xv, m_y);
            (_mm256_div_ps(xf, w), _mm256_div_ps(yf2, w))
        }
    };
    let sxf = _mm256_floor_ps(sx);
    let syf = _mm256_floor_ps(sy);
    let ix = _mm256_cvtps_epi32(sxf);
    let iy = _mm256_cvtps_epi32(syf);
    let sx_frac = _mm256_sub_ps(sx, sxf);
    let sy_frac = _mm256_sub_ps(sy, syf);
    if !replicate {
        // inlier：ix>=0 && ix<sw-1 && iy>=0 && iy<sh-1
        let ix_ge_0 = _mm256_cmpgt_epi32(ix, _mm256_set1_epi32(-1));
        let ix_lt = _mm256_cmpgt_epi32(_mm256_set1_epi32((sw - 1) as i32), ix);
        let iy_ge_0 = _mm256_cmpgt_epi32(iy, _mm256_set1_epi32(-1));
        let iy_lt = _mm256_cmpgt_epi32(_mm256_set1_epi32((sh - 1) as i32), iy);
        let mask = _mm256_and_si256(
            _mm256_and_si256(ix_ge_0, ix_lt),
            _mm256_and_si256(iy_ge_0, iy_lt),
        );
        if _mm256_testc_si256(mask, _mm256_set1_epi32(-1)) == 0 {
            return false;
        }
    }
    // clamp 坐标到 [0, sw-1] / [0, sh-1]（replicate 边界；all-inlier 时是 no-op）
    let zero = _mm256_set1_epi32(0);
    let swm1 = _mm256_set1_epi32((sw - 1) as i32);
    let shm1 = _mm256_set1_epi32((sh - 1) as i32);
    let ix_c = _mm256_min_epi32(_mm256_max_epi32(ix, zero), swm1);
    let iy_c = _mm256_min_epi32(_mm256_max_epi32(iy, zero), shm1);
    let ix1_c = _mm256_min_epi32(
        _mm256_max_epi32(_mm256_add_epi32(ix, _mm256_set1_epi32(1)), zero),
        swm1,
    );
    let iy1_c = _mm256_min_epi32(
        _mm256_max_epi32(_mm256_add_epi32(iy, _mm256_set1_epi32(1)), zero),
        shm1,
    );
    // 4 个 gather 地址（p00/p01/p10/p11）
    let swv = _mm256_set1_epi32(sw as i32);
    let cnv = _mm256_set1_epi32(cn as i32);
    let addr = _mm256_mullo_epi32(
        _mm256_add_epi32(_mm256_mullo_epi32(iy_c, swv), ix_c),
        cnv,
    );
    let addr1 = _mm256_mullo_epi32(
        _mm256_add_epi32(_mm256_mullo_epi32(iy_c, swv), ix1_c),
        cnv,
    );
    let addr_bot = _mm256_mullo_epi32(
        _mm256_add_epi32(_mm256_mullo_epi32(iy1_c, swv), ix_c),
        cnv,
    );
    let addr_bot1 = _mm256_mullo_epi32(
        _mm256_add_epi32(_mm256_mullo_epi32(iy1_c, swv), ix1_c),
        cnv,
    );
    let base = src.as_ptr() as *const i32;
    let dst_ptr = dst.as_mut_ptr().add(x * cn);
    match cn {
        1 => {
            let g_top = _mm256_i32gather_epi32(base, addr, 1);
            let g_bot = _mm256_i32gather_epi32(base, addr_bot, 1);
            let p00 = _mm256_and_si256(g_top, MASK_FF);
            let p10 = _mm256_and_si256(g_bot, MASK_FF);
            // x clamp 重合时 addr+1 跨行，p01/p11 回退为 p00/p10
            let same_x = _mm256_cmpeq_epi32(ix1_c, ix_c);
            let p01_raw = _mm256_and_si256(_mm256_srli_epi32(g_top, 8), MASK_FF);
            let p01 = _mm256_blendv_epi8(p01_raw, p00, same_x);
            let p11_raw = _mm256_and_si256(_mm256_srli_epi32(g_bot, 8), MASK_FF);
            let p11 = _mm256_blendv_epi8(p11_raw, p10, same_x);
            let r = interp8(sx_frac, sy_frac, p00, p01, p10, p11);
            let out = pack8_u8(r);
            _mm_storel_epi64(dst_ptr as *mut __m128i, out);
        }
        3 => {
            let g_top = _mm256_i32gather_epi32(base, addr, 1);
            let g_bot = _mm256_i32gather_epi32(base, addr_bot, 1);
            let g_top2 = _mm256_i32gather_epi32(base, addr1, 1);
            let g_bot2 = _mm256_i32gather_epi32(base, addr_bot1, 1);
            // x clamp 重合（ix1_c==ix_c，即 ix>=sw-1）时 addr+3 跨行，
            // c0_p01/c0_p11 需回退为 p00/p10（同像素）
            let same_x = _mm256_cmpeq_epi32(ix1_c, ix_c);
            // channel 0：p00 = g&0xFF，p01 = (g>>24)&0xFF
            let c0_p00 = _mm256_and_si256(g_top, MASK_FF);
            let c0_p01_raw = _mm256_and_si256(_mm256_srli_epi32(g_top, 24), MASK_FF);
            let c0_p01 = _mm256_blendv_epi8(c0_p01_raw, c0_p00, same_x);
            let c0_p10 = _mm256_and_si256(g_bot, MASK_FF);
            let c0_p11_raw = _mm256_and_si256(_mm256_srli_epi32(g_bot, 24), MASK_FF);
            let c0_p11 = _mm256_blendv_epi8(c0_p11_raw, c0_p10, same_x);
            let r0 = interp8(sx_frac, sy_frac, c0_p00, c0_p01, c0_p10, c0_p11);
            // channel 1：p00 = (g>>8)&0xFF，p01 = (g2>>8)&0xFF
            let c1_p00 = _mm256_and_si256(_mm256_srli_epi32(g_top, 8), MASK_FF);
            let c1_p01 = _mm256_and_si256(_mm256_srli_epi32(g_top2, 8), MASK_FF);
            let c1_p10 = _mm256_and_si256(_mm256_srli_epi32(g_bot, 8), MASK_FF);
            let c1_p11 = _mm256_and_si256(_mm256_srli_epi32(g_bot2, 8), MASK_FF);
            let r1 = interp8(sx_frac, sy_frac, c1_p00, c1_p01, c1_p10, c1_p11);
            // channel 2：p00 = (g>>16)&0xFF，p01 = (g2>>16)&0xFF
            let c2_p00 = _mm256_and_si256(_mm256_srli_epi32(g_top, 16), MASK_FF);
            let c2_p01 = _mm256_and_si256(_mm256_srli_epi32(g_top2, 16), MASK_FF);
            let c2_p10 = _mm256_and_si256(_mm256_srli_epi32(g_bot, 16), MASK_FF);
            let c2_p11 = _mm256_and_si256(_mm256_srli_epi32(g_bot2, 16), MASK_FF);
            let r2 = interp8(sx_frac, sy_frac, c2_p00, c2_p01, c2_p10, c2_p11);
// 每通道 pack 到 8 u8（低 64 位）
            let p0 = _mm256_castsi128_si256(pack8_u8(r0));
            let p1 = _mm256_castsi128_si256(pack8_u8(r1));
            let p2 = _mm256_castsi128_si256(pack8_u8(r2));
            interleave3_8(dst_ptr, p0, p1, p2);
        }
        _ => return false,
    }
    true
}

// ---- Warp CUBIC 快路径（AVX2，每批 8 像素）----

const CUBIC_A: f32 = -0.75;

/// 8-lane 三次权重（对齐 bicubicWeights v_float32 版：乘法结合顺序影响 1ulp）。
#[inline(always)]
unsafe fn cubic_weights(alpha: __m256) -> [__m256; 4] {
    let a2 = _mm256_mul_ps(alpha, alpha);
    let b = _mm256_sub_ps(_mm256_set1_ps(1.0), alpha);
    let b2 = _mm256_mul_ps(b, b);
    let w0 = _mm256_mul_ps(_mm256_set1_ps(CUBIC_A), _mm256_mul_ps(alpha, b2));
    let w3 = _mm256_mul_ps(_mm256_set1_ps(CUBIC_A), _mm256_mul_ps(b, a2));
    let w1 = _mm256_fmadd_ps(
        a2,
        _mm256_fmadd_ps(
            _mm256_set1_ps(CUBIC_A + 2.0),
            alpha,
            _mm256_set1_ps(-(CUBIC_A + 3.0)),
        ),
        _mm256_set1_ps(1.0),
    );
    let w2 = _mm256_sub_ps(
        _mm256_sub_ps(_mm256_sub_ps(_mm256_set1_ps(1.0), w0), w1),
        w3,
    );
    [w0, w1, w2, w3]
}

/// Warp CUBIC 快路径（AVX2，每批 8 像素，affine/perspective + replicate 边界）。
///
/// 对齐 warp_kernels.simd.hpp bicubicVec AVX2 语义：坐标分离乘加（affine）或
/// f64 域（perspective）、clamp、floor、f16 权重（f32 域）、4×4 gather + FMA 链、
/// round ties-even。`replicate=true` 时 clamp 坐标处理 Replicate 边界（覆盖全部像素）；
/// 否则仅处理 all-inlier 批，不满足返回 `false`。
///
/// `m` 为 affine 6 元素（已求逆转 f32）或 perspective 9 元素（`pm` 为 Some）。
///
/// # Safety
/// - `src` 长度 ≥ `sh*sw*cn`；`dst` 长度 ≥ `(y+1)*row + (x+8)*cn`
/// - 调用方需保证 CPU 支持 AVX2
#[inline(never)]
#[target_feature(enable = "avx2")]
pub unsafe fn warp_cubic_fast_u8(
    src: &[u8],
    sw: usize,
    sh: usize,
    _step: usize,
    dst: &mut [u8],
    m: &[f32],
    pm: Option<&[f32; 9]>,
    y: usize,
    x: usize,
    cn: usize,
    replicate: bool,
) -> bool {
    let yf = y as f32;
    let xv = _mm256_setr_ps(
        x as f32,
        (x + 1) as f32,
        (x + 2) as f32,
        (x + 3) as f32,
        (x + 4) as f32,
        (x + 5) as f32,
        (x + 6) as f32,
        (x + 7) as f32,
    );
    // 坐标：affine 分离乘加；perspective f64 域（4-lane × 2 组）
    let (cx, cy) = match pm {
        None => {
            let m_x = _mm256_set1_ps(yf * m[1] + m[2]);
            let m_y = _mm256_set1_ps(yf * m[4] + m[5]);
            let cx = _mm256_add_ps(_mm256_mul_ps(_mm256_set1_ps(m[0]), xv), m_x);
            let cy = _mm256_add_ps(_mm256_mul_ps(_mm256_set1_ps(m[3]), xv), m_y);
            (cx, cy)
        }
        Some(p) => {
            let xf = x as f64;
            let m_x = (yf * p[1] + p[2]) as f64;
            let m_y = (yf * p[4] + p[5]) as f64;
            let m_z = (yf * p[7] + p[8]) as f64;
            // 4-lane f64 组 0（x..x+4）
            let xv0 = _mm256_setr_pd(xf, xf + 1.0, xf + 2.0, xf + 3.0);
            let w0 = _mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[6] as f64), xv0), _mm256_set1_pd(m_z));
            let invz0 = _mm256_div_pd(_mm256_set1_pd(1.0), w0);
            let cx0 = _mm256_mul_pd(_mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[0] as f64), xv0), _mm256_set1_pd(m_x)), invz0);
            let cy0 = _mm256_mul_pd(_mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[3] as f64), xv0), _mm256_set1_pd(m_y)), invz0);
            // 4-lane f64 组 1（x+4..x+8）
            let xv1 = _mm256_setr_pd(xf + 4.0, xf + 5.0, xf + 6.0, xf + 7.0);
            let w1 = _mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[6] as f64), xv1), _mm256_set1_pd(m_z));
            let invz1 = _mm256_div_pd(_mm256_set1_pd(1.0), w1);
            let cx1 = _mm256_mul_pd(_mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[0] as f64), xv1), _mm256_set1_pd(m_x)), invz1);
            let cy1 = _mm256_mul_pd(_mm256_add_pd(_mm256_mul_pd(_mm256_set1_pd(p[3] as f64), xv1), _mm256_set1_pd(m_y)), invz1);
            // 合并为 8-lane f32
            let cx0f = _mm256_cvtpd_ps(cx0);
            let cx1f = _mm256_cvtpd_ps(cx1);
            let cx = _mm256_insertf128_ps(_mm256_castps128_ps256(cx0f), cx1f, 1);
            let cy0f = _mm256_cvtpd_ps(cy0);
            let cy1f = _mm256_cvtpd_ps(cy1);
            let cy = _mm256_insertf128_ps(_mm256_castps128_ps256(cy0f), cy1f, 1);
            (cx, cy)
        }
    };
    // clamp 到 [-bigw, bigw*2] / [-bigh, bigh*2]
    let bigw = (sw.max(16)) as f32;
    let bigh = (sh.max(16)) as f32;
    let cx = _mm256_min_ps(_mm256_max_ps(cx, _mm256_set1_ps(-bigw)), _mm256_set1_ps(bigw * 2.0));
    let cy = _mm256_min_ps(_mm256_max_ps(cy, _mm256_set1_ps(-bigh)), _mm256_set1_ps(bigh * 2.0));
    let cxf = _mm256_floor_ps(cx);
    let cyf = _mm256_floor_ps(cy);
    let ix0 = _mm256_cvtps_epi32(cxf);
    let iy0 = _mm256_cvtps_epi32(cyf);
    let tl_x = _mm256_sub_epi32(ix0, _mm256_set1_epi32(1));
    let tl_y = _mm256_sub_epi32(iy0, _mm256_set1_epi32(1));
    let alpha_x = _mm256_sub_ps(cx, cxf);
    let alpha_y = _mm256_sub_ps(cy, cyf);
    if !replicate {
        // all-inlier：tl_x>=0 && tl_x+3<sw && tl_y>=0 && tl_y+3<sh
        let swm3 = _mm256_set1_epi32((sw as i32) - 3);
        let shm3 = _mm256_set1_epi32((sh as i32) - 3);
        let x_ge = _mm256_cmpgt_epi32(tl_x, _mm256_set1_epi32(-1));
        let x_lt = _mm256_cmpgt_epi32(swm3, tl_x);
        let y_ge = _mm256_cmpgt_epi32(tl_y, _mm256_set1_epi32(-1));
        let y_lt = _mm256_cmpgt_epi32(shm3, tl_y);
        let mask = _mm256_and_si256(
            _mm256_and_si256(x_ge, x_lt),
            _mm256_and_si256(y_ge, y_lt),
        );
        if _mm256_testc_si256(mask, _mm256_set1_epi32(-1)) == 0 {
            return false;
        }
    }
    // 权重
    let wx = cubic_weights(alpha_x);
    let wy = cubic_weights(alpha_y);
    // clamp tl_x+j / tl_y+r 到 [0, sw-1] / [0, sh-1]（replicate；all-inlier 时 no-op）
    let zero = _mm256_set1_epi32(0);
    let swm1 = _mm256_set1_epi32((sw - 1) as i32);
    let shm1 = _mm256_set1_epi32((sh - 1) as i32);
    let swv = _mm256_set1_epi32(sw as i32);
    let cnv = _mm256_set1_epi32(cn as i32);
    let one = _mm256_set1_epi32(1);
    let two = _mm256_set1_epi32(2);
    let three = _mm256_set1_epi32(3);
    // x_j = clamp(tl_x + j, 0, sw-1) for j in 0..4
    let x0 = _mm256_min_epi32(_mm256_max_epi32(tl_x, zero), swm1);
    let x1 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_x, one), zero), swm1);
    let x2 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_x, two), zero), swm1);
    let x3 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_x, three), zero), swm1);
    // y_r = clamp(tl_y + r, 0, sh-1) for r in 0..4
    let y0 = _mm256_min_epi32(_mm256_max_epi32(tl_y, zero), shm1);
    let y1 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_y, one), zero), shm1);
    let y2 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_y, two), zero), shm1);
    let y3 = _mm256_min_epi32(_mm256_max_epi32(_mm256_add_epi32(tl_y, three), zero), shm1);
    // addr_rj = (y_r*sw + x_j)*cn
    let y0s = _mm256_mullo_epi32(y0, swv);
    let y1s = _mm256_mullo_epi32(y1, swv);
    let y2s = _mm256_mullo_epi32(y2, swv);
    let y3s = _mm256_mullo_epi32(y3, swv);
    let a00 = _mm256_mullo_epi32(_mm256_add_epi32(y0s, x0), cnv);
    let a01 = _mm256_mullo_epi32(_mm256_add_epi32(y0s, x1), cnv);
    let a02 = _mm256_mullo_epi32(_mm256_add_epi32(y0s, x2), cnv);
    let a03 = _mm256_mullo_epi32(_mm256_add_epi32(y0s, x3), cnv);
    let a10 = _mm256_mullo_epi32(_mm256_add_epi32(y1s, x0), cnv);
    let a11 = _mm256_mullo_epi32(_mm256_add_epi32(y1s, x1), cnv);
    let a12 = _mm256_mullo_epi32(_mm256_add_epi32(y1s, x2), cnv);
    let a13 = _mm256_mullo_epi32(_mm256_add_epi32(y1s, x3), cnv);
    let a20 = _mm256_mullo_epi32(_mm256_add_epi32(y2s, x0), cnv);
    let a21 = _mm256_mullo_epi32(_mm256_add_epi32(y2s, x1), cnv);
    let a22 = _mm256_mullo_epi32(_mm256_add_epi32(y2s, x2), cnv);
    let a23 = _mm256_mullo_epi32(_mm256_add_epi32(y2s, x3), cnv);
    let a30 = _mm256_mullo_epi32(_mm256_add_epi32(y3s, x0), cnv);
    let a31 = _mm256_mullo_epi32(_mm256_add_epi32(y3s, x1), cnv);
    let a32 = _mm256_mullo_epi32(_mm256_add_epi32(y3s, x2), cnv);
    let a33 = _mm256_mullo_epi32(_mm256_add_epi32(y3s, x3), cnv);
    let base = src.as_ptr() as *const i32;
    let dst_ptr = dst.as_mut_ptr().add(x * cn);
    // 每行 gather 4 taps（4 字节/lane，含 3 通道 + 1 冗余）
    let g00 = _mm256_i32gather_epi32(base, a00, 1);
    let g01 = _mm256_i32gather_epi32(base, a01, 1);
    let g02 = _mm256_i32gather_epi32(base, a02, 1);
    let g03 = _mm256_i32gather_epi32(base, a03, 1);
    let g10 = _mm256_i32gather_epi32(base, a10, 1);
    let g11 = _mm256_i32gather_epi32(base, a11, 1);
    let g12 = _mm256_i32gather_epi32(base, a12, 1);
    let g13 = _mm256_i32gather_epi32(base, a13, 1);
    let g20 = _mm256_i32gather_epi32(base, a20, 1);
    let g21 = _mm256_i32gather_epi32(base, a21, 1);
    let g22 = _mm256_i32gather_epi32(base, a22, 1);
    let g23 = _mm256_i32gather_epi32(base, a23, 1);
    let g30 = _mm256_i32gather_epi32(base, a30, 1);
    let g31 = _mm256_i32gather_epi32(base, a31, 1);
    let g32 = _mm256_i32gather_epi32(base, a32, 1);
    let g33 = _mm256_i32gather_epi32(base, a33, 1);
    // 每通道：4 行 × 4 taps → FMA 链
    // channel 0：tap_j = byte 0 of g_rj
    let c0 = cubic_fma_chain(
        g00, g01, g02, g03, g10, g11, g12, g13, g20, g21, g22, g23, g30, g31, g32, g33,
        &wx, &wy, 0,
    );
    let c1 = cubic_fma_chain(
        g00, g01, g02, g03, g10, g11, g12, g13, g20, g21, g22, g23, g30, g31, g32, g33,
        &wx, &wy, 8,
    );
    let c2 = cubic_fma_chain(
        g00, g01, g02, g03, g10, g11, g12, g13, g20, g21, g22, g23, g30, g31, g32, g33,
        &wx, &wy, 16,
    );
    // pack 到 8 u8/通道 + 交错
    let p0 = _mm256_castsi128_si256(pack8_u8(c0));
    let p1 = _mm256_castsi128_si256(pack8_u8(c1));
    let p2 = _mm256_castsi128_si256(pack8_u8(c2));
    interleave3_8(dst_ptr, p0, p1, p2);
    true
}

/// 8-lane 三次 FMA 链（4 行 × 4 taps）。
///
/// `shift` 为通道字节偏移（0/8/16），从 gather 结果提取对应通道的 4 taps。
#[inline(always)]
unsafe fn cubic_fma_chain(
    g00: __m256i, g01: __m256i, g02: __m256i, g03: __m256i,
    g10: __m256i, g11: __m256i, g12: __m256i, g13: __m256i,
    g20: __m256i, g21: __m256i, g22: __m256i, g23: __m256i,
    g30: __m256i, g31: __m256i, g32: __m256i, g33: __m256i,
    wx: &[__m256; 4],
    wy: &[__m256; 4],
    shift: i32,
) -> __m256i {
    let shv = _mm256_set1_epi32(shift);
    // 提取通道 taps（byte shift 的 u8 → i32）
    let t00 = _mm256_and_si256(_mm256_srlv_epi32(g00, shv), MASK_FF);
    let t01 = _mm256_and_si256(_mm256_srlv_epi32(g01, shv), MASK_FF);
    let t02 = _mm256_and_si256(_mm256_srlv_epi32(g02, shv), MASK_FF);
    let t03 = _mm256_and_si256(_mm256_srlv_epi32(g03, shv), MASK_FF);
    let t10 = _mm256_and_si256(_mm256_srlv_epi32(g10, shv), MASK_FF);
    let t11 = _mm256_and_si256(_mm256_srlv_epi32(g11, shv), MASK_FF);
    let t12 = _mm256_and_si256(_mm256_srlv_epi32(g12, shv), MASK_FF);
    let t13 = _mm256_and_si256(_mm256_srlv_epi32(g13, shv), MASK_FF);
    let t20 = _mm256_and_si256(_mm256_srlv_epi32(g20, shv), MASK_FF);
    let t21 = _mm256_and_si256(_mm256_srlv_epi32(g21, shv), MASK_FF);
    let t22 = _mm256_and_si256(_mm256_srlv_epi32(g22, shv), MASK_FF);
    let t23 = _mm256_and_si256(_mm256_srlv_epi32(g23, shv), MASK_FF);
    let t30 = _mm256_and_si256(_mm256_srlv_epi32(g30, shv), MASK_FF);
    let t31 = _mm256_and_si256(_mm256_srlv_epi32(g31, shv), MASK_FF);
    let t32 = _mm256_and_si256(_mm256_srlv_epi32(g32, shv), MASK_FF);
    let t33 = _mm256_and_si256(_mm256_srlv_epi32(g33, shv), MASK_FF);
    // 每行：sumwx = fma(t1, wx1, t0*wx0); fma(t2, wx2, sumwx); fma(t3, wx3, sumwx)
    // acc = fma(sumwx, wy[r], acc)
    let row0 = cubic_row(t00, t01, t02, t03, wx);
    let row1 = cubic_row(t10, t11, t12, t13, wx);
    let row2 = cubic_row(t20, t21, t22, t23, wx);
    let row3 = cubic_row(t30, t31, t32, t33, wx);
    let acc = _mm256_fmadd_ps(row3, wy[3], _mm256_fmadd_ps(row2, wy[2], _mm256_fmadd_ps(row1, wy[1], _mm256_mul_ps(row0, wy[0]))));
    _mm256_cvtps_epi32(acc)
}

/// 单行 4-tap 三次 FMA（i32 taps → f32）。
#[inline(always)]
unsafe fn cubic_row(t0: __m256i, t1: __m256i, t2: __m256i, t3: __m256i, wx: &[__m256; 4]) -> __m256 {
    let t0f = _mm256_cvtepi32_ps(t0);
    let t1f = _mm256_cvtepi32_ps(t1);
    let t2f = _mm256_cvtepi32_ps(t2);
    let t3f = _mm256_cvtepi32_ps(t3);
    let sumwx = _mm256_fmadd_ps(t1f, wx[1], _mm256_mul_ps(t0f, wx[0]));
    let sumwx = _mm256_fmadd_ps(t2f, wx[2], sumwx);
    _mm256_fmadd_ps(t3f, wx[3], sumwx)
}
/// HResize CUBIC AVX512VBMI 版（u8 → i32 缓冲），每批 16 元素。
///
/// 仅处理 cn=3 且 xofs 呈"像素间等距 stride=6、组内连续"模式（scale=2 下采样）。
/// 16 位置的全部 taps 落在 [xofs[dx]-3, xofs[dx]+40) 的 43 字节窗口内（< 64），
/// 单个 64 字节 loadu + 2×vpermb（紧凑 [t0,t1]/[t2,t3] 字节对）+ 2×cvtepu8_epi16
/// （零扩展到 i16）+ 2×madd，零 gather。
///
/// 返回 true 表示已处理完 `[start, end)`（含尾段标量），false 表示模式不匹配
/// （调用方回退 AVX2 gather 路径）。
///
/// # Safety
/// - 同 `hresize_cubic_u8`；调用方需保证 CPU 支持 AVX512F+BW+VBMI
#[inline(never)]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
pub unsafe fn hresize_cubic_vbmi_u8(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    ialpha01: &[i16],
    ialpha23: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) -> bool {
    if cn != 3 || end < start + 16 {
        return false;
    }
    // 检测：像素间步长恒定 6，组内 +1（scale=2 下采样的扁平域模式）
    let npix = end / 3;
    let regular = npix >= 2
        && xofs[3] - xofs[0] == 6
        && (1..npix).all(|p| xofs[p * 3] - xofs[(p - 1) * 3] == 6)
        && (0..npix).all(|p| xofs[p * 3 + 2] - xofs[p * 3] == 2);
    if !regular {
        return false;
    }
    unsafe { hresize_cubic_vbmi_regular(src, xofs, ialpha, ialpha01, ialpha23, dst, start, end, cn, srow_len) };
    true
}

/// `hresize_cubic_vbmi_u8` 的无检测版（调用方已确认 stride=6 等距模式）。
///
/// # Safety
/// - 同 `hresize_cubic_u8`；xofs 必须满足 stride=6 等距模式；CPU 需支持 AVX512F+BW+VBMI
#[inline(never)]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
pub unsafe fn hresize_cubic_vbmi_regular(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    ialpha01: &[i16],
    ialpha23: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    // d_i（位置 i 相对批起点的 xofs 偏移）按 o=dx%3 三种模式（stride=6）：
    //   o=0: [0,1,2,6,7,8,12,13,14,18,19,20,24,25,26,30]
    //   o=1: [0,1,5,6,7,11,12,13,17,18,19,23,24,25,29,30]
    //   o=2: [0,4,5,6,10,11,12,16,17,18,22,23,24,28,29,30]
    // 掩码与数据无关（常量），进程内缓存一次
    static PERMS: std::sync::OnceLock<([__m512i; 3], [__m512i; 3])> = std::sync::OnceLock::new();
    let (perms01, perms23) = PERMS.get_or_init(|| {
        let dseq = |o: usize| -> [i32; 16] {
            let base = 6 * (o / 3) + (o % 3);
            let mut d = [0i32; 16];
            for (i, v) in d.iter_mut().enumerate() {
                let oi = o + i;
                *v = (6 * (oi / 3) + (oi % 3) - base) as i32;
            }
            d
        };
        // 紧凑掩码：输出字节 [t0_i, t1_i]（低 32 字节），off=0 → tap01 对，off=6 → tap23 对
        let mk = |o: usize, off: i32| -> __m512i {
            let d = dseq(o);
            let mut m = [0u8; 64];
            for i in 0..16 {
                m[i * 2] = (d[i] + off) as u8;
                m[i * 2 + 1] = (d[i] + off + 3) as u8;
            }
            std::mem::transmute(m)
        };
        (
            [mk(0, 0), mk(1, 0), mk(2, 0)],
            [mk(0, 6), mk(1, 6), mk(2, 6)],
        )
    });
    let mut dx = start;
    unsafe {
        while dx + 16 <= end {
            // 窗口末 tap：xofs[dx+15]+6，64 字节 load 到 xofs[dx]-3+63
            // 需 xofs[dx+15]+6+1 ≤ srow_len 且 xofs[dx]-3+64 ≤ src.len()
            // （行缓冲按 srow_len 对齐分配，读越行无害但需界内；保守检查）
            if xofs[dx + 15] + 8 >= srow_len as i32 {
                break;
            }
            let o = dx % 3;
            let base = (xofs[dx] - 3) as usize;
            let win = _mm512_loadu_si512(src.as_ptr().add(base) as *const __m512i);
            let t01c = _mm512_permutexvar_epi8(perms01[o], win);
            let t23c = _mm512_permutexvar_epi8(perms23[o], win);
            let t01 = _mm512_cvtepu8_epi16(_mm512_castsi512_si256(t01c));
            let t23 = _mm512_cvtepu8_epi16(_mm512_castsi512_si256(t23c));
            let a01 = _mm512_loadu_si512(ialpha01[dx * 2..].as_ptr() as *const __m512i);
            let a23 = _mm512_loadu_si512(ialpha23[dx * 2..].as_ptr() as *const __m512i);
            let r = _mm512_add_epi32(
                _mm512_madd_epi16(t01, a01),
                _mm512_madd_epi16(t23, a23),
            );
            _mm512_storeu_si512(dst[dx..].as_mut_ptr() as *mut __m512i, r);
            dx += 16;
        }
    }
    // 尾段标量（与 AVX2 版语义一致：越界 tap 权重为 0 用 0 替代）
    while dx < end {
        let sx = xofs[dx] as usize - cn;
        let mut v: i32 = 0;
        for j in 0..4 {
            let idx = sx + j * cn;
            let sval = if idx < srow_len { src[idx] as i32 } else { 0 };
            v += sval * ialpha[dx * 4 + j] as i32;
        }
        dst[dx] = v;
        dx += 1;
    }
}

/// VResize CUBIC AVX512 版（i32 缓冲 → u8），每批 16 输出。
///
/// 语义与 AVX2 版一致（f32 分离乘加链 + cvtps2dq ties-even + 饱和 pack），
/// 16 元素/批且 pack 无 lane 顺序问题（`vpmovdb` 截断前先饱和）。
/// 与 `hresize_cubic_vbmi_u8` 配对使用可避免 AVX512/AVX2 混用切换 penalty。
///
/// # Safety
/// - `s0`/`s1`/`s2`/`s3` 长度 ≥ `width`；`dst` 长度 ≥ `width`
/// - 调用方需保证 CPU 支持 AVX512F+BW
#[inline(never)]
#[target_feature(enable = "avx512f,avx512bw")]
pub unsafe fn vresize_cubic_avx512_u8(
    s0: &[i32],
    s1: &[i32],
    s2: &[i32],
    s3: &[i32],
    dst: &mut [u8],
    b: [i32; 4],
    scale: f32,
    width: usize,
) {
    let b0v = _mm512_set1_ps(b[0] as f32 * scale);
    let b1v = _mm512_set1_ps(b[1] as f32 * scale);
    let b2v = _mm512_set1_ps(b[2] as f32 * scale);
    let b3v = _mm512_set1_ps(b[3] as f32 * scale);
    let mut x = 0;
    // SAFETY: x+16 <= width 保证 s0..s3 读 [x, x+16) 与 dst 写 [x, x+16) 不越界
    unsafe {
        while x + 16 <= width {
            let s3v = _mm512_cvtepi32_ps(_mm512_loadu_si512(s3[x..].as_ptr() as *const __m512i));
            let acc = _mm512_mul_ps(s3v, b3v);
            let s2v = _mm512_cvtepi32_ps(_mm512_loadu_si512(s2[x..].as_ptr() as *const __m512i));
            let acc = _mm512_add_ps(_mm512_mul_ps(s2v, b2v), acc);
            let s1v = _mm512_cvtepi32_ps(_mm512_loadu_si512(s1[x..].as_ptr() as *const __m512i));
            let acc = _mm512_add_ps(_mm512_mul_ps(s1v, b1v), acc);
            let s0v = _mm512_cvtepi32_ps(_mm512_loadu_si512(s0[x..].as_ptr() as *const __m512i));
            let acc = _mm512_add_ps(_mm512_mul_ps(s0v, b0v), acc);
            // v_round（cvtps2dq ties-even）+ 饱和 pack i32→u8。
            // vpmovusdb 按无符号解释源（负 i32 → 255），需先有符号 clamp 到 ≥0
            let r = _mm512_cvtps_epi32(acc);
            let r = _mm512_max_epi32(r, _mm512_setzero_si512());
            let out = _mm512_cvtusepi32_epi8(r);
            _mm_storeu_si128(dst[x..].as_mut_ptr() as *mut __m128i, out);
            x += 16;
        }
        // 尾段 [x, width)：对齐 OpenCV VResizeCubicVec_32s8u 的边界语义——
        // SIMD 段为 8 的倍数（v_int16 vlanes），[width&~7, width) 才走定点标量。
        // 剩余 ≥8 时用 masked 512-bit 批（f32 链，与 SIMD 语义一致）
        if width - x >= 8 {
            let m: u16 = 0xFF;
            let s3v = _mm512_maskz_cvtepi32_ps(m, _mm512_castsi256_si512(_mm256_loadu_si256(s3[x..].as_ptr() as *const __m256i)));
            let acc = _mm512_mul_ps(s3v, b3v);
            let s2v = _mm512_maskz_cvtepi32_ps(m, _mm512_castsi256_si512(_mm256_loadu_si256(s2[x..].as_ptr() as *const __m256i)));
            let acc = _mm512_add_ps(_mm512_mul_ps(s2v, b2v), acc);
            let s1v = _mm512_maskz_cvtepi32_ps(m, _mm512_castsi256_si512(_mm256_loadu_si256(s1[x..].as_ptr() as *const __m256i)));
            let acc = _mm512_add_ps(_mm512_mul_ps(s1v, b1v), acc);
            let s0v = _mm512_maskz_cvtepi32_ps(m, _mm512_castsi256_si512(_mm256_loadu_si256(s0[x..].as_ptr() as *const __m256i)));
            let acc = _mm512_add_ps(_mm512_mul_ps(s0v, b0v), acc);
            let r = _mm512_cvtps_epi32(acc);
            let r = _mm512_max_epi32(r, _mm512_setzero_si512());
            let out = _mm512_cvtusepi32_epi8(r);
            _mm_mask_storeu_epi8(dst[x..].as_mut_ptr() as *mut i8, m, out);
            x += 8;
        }
    }
    // 标量尾段（FixedPtCast (val+2^21)>>22）
    while x < width {
        let acc = s0[x] * b[0] + s1[x] * b[1] + s2[x] * b[2] + s3[x] * b[3];
        dst[x] = (((acc + (1 << 21)) >> 22).clamp(0, 255)) as u8;
        x += 1;
    }
}
