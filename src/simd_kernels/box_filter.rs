//! box_filter 手写内核（对齐 OpenCV `box_filter.simd.hpp`）。
//!
//! u8→u8 且 kw·kh ≤ 256 时走 CV_16U 定点路径（OpenCV `createBoxFilter`）：
//! - 行阶段 `RowSum<uchar, ushort>`：u8 → u16 滑动窗行和（kw=3/5 直接求和特化）
//! - 列阶段 `ColumnSum<ushort, uchar>`：SHIFT=23 定点除法
//!   `D = (SUM + Sp + dd) * ds >> 23`，`SUM' = SUM + Sp - Sm`（wrap u16）
//!
//! 中间缓冲布局：`(w + kw - 1) × (h + kh - 1)` 的 u16（四周含边框），
//! 列阶段顺序访问 kh 行，无需索引映射。内核不做边界（调用方预填充）。

/// 定点除法参数（OpenCV `ColumnSum<ushort,uchar>` 构造函数）。
#[derive(Clone, Copy)]
pub struct BoxFixed {
    /// `divScale`：`(s + dd) * ds >> 23`。
    pub ds: u32,
    /// `divDelta`。
    pub dd: u16,
}

impl BoxFixed {
    /// 从核面积（`kw*kh`）计算定点参数（normalize 场景，scale = 1/area）。
    ///
    /// OpenCV：`d = cvRound(1/scale)`（round-half-even），`scalef = 2^23/d`，
    /// `ds = floor(scalef)`，`dd = d/2`；`frac = scalef - ds`，
    /// `frac < 0.5` 时 `dd += 1`，否则 `ds += 1`。
    pub fn from_area(area: usize) -> Self {
        let scale = 1.0 / area as f64;
        let d = round_half_even(1.0 / scale);
        let scalef = (1u64 << 23) as f64 / d as f64;
        let mut ds = scalef.floor() as u32;
        let frac = scalef - ds as f64;
        let mut dd = (d / 2) as u16;
        if frac < 0.5 {
            dd += 1;
        } else {
            ds += 1;
        }
        Self { ds, dd }
    }
}

/// round-half-to-even（OpenCV `cvRound`，x86 SSE 语义）。
fn round_half_even(v: f64) -> i64 {
    let r = v.round();
    let is_half = (v - v.trunc()).abs() == 0.5;
    if is_half && r % 2.0 != 0.0 {
        (r - v.signum()) as i64
    } else {
        r as i64
    }
}

/// 行阶段（无边框源）：u8 → u16 行和，kw=3/5，内核内部处理首尾边框。
///
/// 与 [`box_row_sum_u8_cn`] 的区别：`src` 是原始行（长度 ≥ `w*cn`，无左右边框），
/// 首尾 `kw-1` 像素的越界采样由 `border` 策略给出（Clamp/Reflect/Constant）。
/// 省去调用方构造 padded 行的 memcpy 中转（读 src + 写 padded + 读 padded → 直接读 src）。
///
/// 主体（无越界区）用直接求和（LLVM 自动向量化），首尾各 `kw-1` 像素逐点特判。
pub fn box_row_sum_u8_direct_cn(
    src: &[u8],
    dst: &mut [u16],
    w: usize,
    kw: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    debug_assert!(kw == 3 || kw == 5);
    let total = w * cn;
    // 主体：像素 [kw-1, w-(kw-1)) 无越界
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
    // 首尾特判：逐像素按 border 采样
    row_sum_edges(src, dst, w, kw, cn, border);
}

/// 行阶段首尾特判：`kw-1` 像素逐点按 border 采样求和（供各 ISA 版复用）。
pub fn row_sum_edges(
    src: &[u8],
    dst: &mut [u16],
    w: usize,
    kw: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let sample = |x: i64, c: usize| -> u8 {
        match border.interpolate(x as isize, w) {
            Some(sx) => src[sx * cn + c],
            None => match border {
                crate::core::border::BorderPolicy::Constant(v) => v,
                _ => unreachable!("non-constant border always interpolates"),
            },
        }
    };
    for x in 0..(kw - 1).min(w) {
        for c in 0..cn {
            let mut s = 0u16;
            for k in 0..kw {
                let sx = x as i64 + k as i64 - (kw as i64 / 2);
                s += if sx >= 0 && (sx as usize) < w {
                    src[sx as usize * cn + c]
                } else {
                    sample(sx, c)
                } as u16;
            }
            dst[x * cn + c] = s;
        }
    }
    for x in (w.saturating_sub(kw - 1))..w {
        for c in 0..cn {
            let mut s = 0u16;
            for k in 0..kw {
                let sx = x as i64 + k as i64 - (kw as i64 / 2);
                s += if sx >= 0 && (sx as usize) < w {
                    src[sx as usize * cn + c]
                } else {
                    sample(sx, c)
                } as u16;
            }
            dst[x * cn + c] = s;
        }
    }
}

/// 行阶段：u8 → u16 滑动窗行和（标量，通道感知，正确性基准）。
///
/// 对齐 OpenCV `RowSum<uchar, ushort>`（box_filter.simd.hpp:68-176）：
/// 每通道独立滑动窗；kw=3/5 用直接求和特化（无依赖链，LLVM 可自动向量化，
/// 与 OpenCV 的 gcc 自动向量化等价）。`src` 为带左右边框的行
/// （长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`（交错布局）。
pub fn box_row_sum_u8_cn(src: &[u8], dst: &mut [u16], w: usize, kw: usize, cn: usize) {
    let ksz_cn = kw * cn;
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
        match cn {
            1 => {
                let mut s: u32 = 0;
                for &v in &src[..kw] {
                    s += v as u32;
                }
                dst[0] = s as u16;
                for i in 1..w {
                    s += src[i + kw - 1] as u32;
                    s -= src[i - 1] as u32;
                    dst[i] = s as u16;
                }
            }
            3 => {
                let mut s0: u32 = 0;
                let mut s1: u32 = 0;
                let mut s2: u32 = 0;
                for i in (0..ksz_cn).step_by(3) {
                    s0 += src[i] as u32;
                    s1 += src[i + 1] as u32;
                    s2 += src[i + 2] as u32;
                }
                dst[0] = s0 as u16;
                dst[1] = s1 as u16;
                dst[2] = s2 as u16;
                for i in (0..(w - 1) * cn).step_by(3) {
                    s0 += src[i + ksz_cn] as u32;
                    s1 += src[i + ksz_cn + 1] as u32;
                    s2 += src[i + ksz_cn + 2] as u32;
                    s0 -= src[i] as u32;
                    s1 -= src[i + 1] as u32;
                    s2 -= src[i + 2] as u32;
                    dst[i + 3] = s0 as u16;
                    dst[i + 4] = s1 as u16;
                    dst[i + 5] = s2 as u16;
                }
            }
            4 => {
                let mut s = [0u32; 4];
                for i in (0..ksz_cn).step_by(4) {
                    for c in 0..4 {
                        s[c] += src[i + c] as u32;
                    }
                }
                for c in 0..4 {
                    dst[c] = s[c] as u16;
                }
                for i in (0..(w - 1) * cn).step_by(4) {
                    for c in 0..4 {
                        s[c] += src[i + ksz_cn + c] as u32;
                        s[c] -= src[i + c] as u32;
                        dst[i + 4 + c] = s[c] as u16;
                    }
                }
            }
            _ => {
                for k in 0..cn {
                    let mut s: u32 = 0;
                    for i in (0..ksz_cn).step_by(cn) {
                        s += src[i + k] as u32;
                    }
                    dst[k] = s as u16;
                    for i in (0..(w - 1) * cn).step_by(cn) {
                        s += src[i + ksz_cn + k] as u32;
                        s -= src[i + k] as u32;
                        dst[i + cn + k] = s as u16;
                    }
                }
            }
        }
    }
}

/// 列阶段：u16 行和 → u8（SHIFT=23 定点除法，标量基准）。
///
/// `rows` 为 kh 行 u16（每行长度 ≥ w，stride 相同），`dst` 长度 ≥ w。
/// `have_scale=false` 时直接饱和（`saturate_cast<uchar>`）。
pub fn box_col_u16_to_u8_scalar(
    rows: &[&[u16]],
    dst: &mut [u8],
    w: usize,
    fx: BoxFixed,
    have_scale: bool,
) {
    let kh = rows.len();
    let mut sum = vec![0u16; w];
    for row in &rows[..kh - 1] {
        for i in 0..w {
            sum[i] = sum[i].wrapping_add(row[i]);
        }
    }
    for i in 0..w {
        let s0 = sum[i].wrapping_add(rows[kh - 1][i]);
        dst[i] = if have_scale {
            (((s0 as u32 + fx.dd as u32) * fx.ds) >> 23) as u8
        } else {
            s0.min(255) as u8
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_point_params_3x3() {
        // 与 C++ 实验一致：d=9, ds=932068, dd=4
        let fx = BoxFixed::from_area(9);
        assert_eq!(fx.ds, 932068);
        assert_eq!(fx.dd, 4);
        // s0=18 -> 2, s0=4 -> 0, s0=5 -> 1
        assert_eq!((((18 + fx.dd as u32) * fx.ds) >> 23) as u8, 2);
        assert_eq!((((4 + fx.dd as u32) * fx.ds) >> 23) as u8, 0);
        assert_eq!((((5 + fx.dd as u32) * fx.ds) >> 23) as u8, 1);
    }

    #[test]
    fn row_sum_sliding_matches_direct() {
        let w = 64;
        let kw = 7;
        let mut src = vec![0u8; w + kw - 1];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 31 % 256) as u8;
        }
        let mut d1 = vec![0u16; w];
        let mut d2 = vec![0u16; w];
        // 直接求和
        for i in 0..w {
            let mut s = 0u32;
            for k in 0..kw {
                s += src[i + k] as u32;
            }
            d1[i] = s as u16;
        }
        box_row_sum_u8_cn(&src, &mut d2, w, kw, 1);
        assert_eq!(d1, d2);
    }

    #[test]
    fn row_sum_cn3_matches_direct() {
        let w = 16;
        let kw = 3;
        let cn = 3;
        let mut src = vec![0u8; (w + kw - 1) * cn];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i * 29 % 256) as u8;
        }
        let mut d1 = vec![0u16; w * cn];
        let mut d2 = vec![0u16; w * cn];
        // 直接求和（每通道独立）
        for x in 0..w {
            for c in 0..cn {
                let mut s = 0u32;
                for k in 0..kw {
                    s += src[(x + k) * cn + c] as u32;
                }
                d1[x * cn + c] = s as u16;
            }
        }
        box_row_sum_u8_cn(&src, &mut d2, w, kw, cn);
        assert_eq!(d1, d2);
    }

    #[test]
    fn col_sum_basic() {
        // 3 行 [3,3,3] [3,3,3] [3,3,3]，3x3 窗 sum=9，/9 = 1
        let rows: Vec<Vec<u16>> = vec![vec![3, 3, 3]; 3];
        let rrefs: Vec<&[u16]> = rows.iter().map(|v| v.as_slice()).collect();
        let mut dst = vec![0u8; 3];
        let fx = BoxFixed::from_area(9);
        box_col_u16_to_u8_scalar(&rrefs, &mut dst, 3, fx, true);
        assert_eq!(dst, vec![1, 1, 1]);
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg(test)]
mod isa_cross_tests {
    use super::*;
    use crate::core::border::BorderPolicy;

    fn has_avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }
    fn has_avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl")
    }

    /// 三路径交叉验证：box_row_sum_u8（带边框源，kw 任意）。
    #[test]
    fn box_row_sum_cross_isa() {
        for cn in [1usize, 3] {
            for w in [1usize, 5, 16, 17, 32, 33, 64, 100] {
                for kw in [1usize, 3, 5, 7, 9, 15] {
                    let padded = w + kw - 1;
                    let src: Vec<u8> = (0..padded * cn).map(|i| (i * 37 % 256) as u8).collect();
                    let mut dst_scalar = vec![0u16; w * cn];
                    box_row_sum_u8_cn(&src, &mut dst_scalar, w, kw, cn);
                    if has_avx2() {
                        let mut dst = vec![0u16; w * cn];
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::box_row_sum_u8(
                                &src, &mut dst, w, kw, cn,
                            )
                        };
                        assert_eq!(dst_scalar, dst, "avx2 w={w} cn={cn} kw={kw}");
                    }
                    if has_avx512() {
                        let mut dst = vec![0u16; w * cn];
                        // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::box_row_sum_u8(
                                &src, &mut dst, w, kw, cn,
                            )
                        };
                        assert_eq!(dst_scalar, dst, "avx512 w={w} cn={cn} kw={kw}");
                    }
                }
            }
        }
    }

    /// 三路径交叉验证：box_row_sum_u8_direct_cn（无边框源，kw ∈ {3,5}）。
    #[test]
    fn box_row_sum_direct_cross_isa() {
        for cn in [1usize, 3] {
            for w in [1usize, 5, 16, 17, 32, 33, 64, 100] {
                for kw in [3usize, 5] {
                    let src: Vec<u8> = (0..w * cn).map(|i| (i * 41 % 256) as u8).collect();
                    for border in [
                        BorderPolicy::Clamp,
                        BorderPolicy::Reflect,
                        BorderPolicy::Constant(7),
                    ] {
                        let mut dst_scalar = vec![0u16; w * cn];
                        box_row_sum_u8_direct_cn(&src, &mut dst_scalar, w, kw, cn, border);
                        if has_avx2() {
                            let mut dst = vec![0u16; w * cn];
                            // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx2::box_row_sum_u8_direct_cn(
                                    &src, &mut dst, w, kw, cn, border,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx2 w={w} cn={cn} kw={kw} {border:?}");
                        }
                        if has_avx512() {
                            let mut dst = vec![0u16; w * cn];
                            // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx512::box_row_sum_u8_direct_cn(
                                    &src, &mut dst, w, kw, cn, border,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx512 w={w} cn={cn} kw={kw} {border:?}");
                        }
                    }
                }
            }
        }
    }

    /// 三路径交叉验证：box_col_step_u16（列阶段单步，含 scale/无 scale）。
    #[test]
    fn box_col_step_cross_isa() {
        for w in [1usize, 16, 17, 32, 33, 64, 100] {
            for area in [1usize, 9, 25, 49] {
                let fx = BoxFixed::from_area(area);
                for have_scale in [true, false] {
                    let mk = |seed: u32| -> Vec<u16> {
                        (0..w)
                            .map(|i| ((i * 131 + seed as usize) % 5000) as u16)
                            .collect()
                    };
                    let mut sum = mk(1);
                    let sp = mk(2);
                    let sm = mk(3);
                    let mut dst_scalar = vec![0u8; w];
                    // 标量基准（mod.rs 派发 Scalar 分支的逐点逻辑）
                    let s0v: Vec<u16> = (0..w).map(|i| sum[i].wrapping_add(sp[i])).collect();
                    for i in 0..w {
                        let s0 = s0v[i];
                        dst_scalar[i] = if have_scale {
                            (((s0 as u32 + fx.dd as u32) * fx.ds) >> 23) as u8
                        } else {
                            s0.min(255) as u8
                        };
                        sum[i] = s0.wrapping_sub(sm[i]);
                    }
                    // SIMD 版（重置 sum）
                    let mut sum2 = mk(1);
                    let mut dst2 = vec![0u8; w];
                    if has_avx2() {
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::box_col_step_u16(
                                &mut sum2, &sp, &sm, &mut dst2, w, fx, have_scale,
                            )
                        };
                        assert_eq!(
                            dst_scalar, dst2,
                            "avx2 w={w} area={area} scale={have_scale}"
                        );
                        assert_eq!(sum, sum2, "avx2 sum w={w} area={area} scale={have_scale}");
                    }
                    let mut sum3 = mk(1);
                    let mut dst3 = vec![0u8; w];
                    if has_avx512() {
                        // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::box_col_step_u16(
                                &mut sum3, &sp, &sm, &mut dst3, w, fx, have_scale,
                            )
                        };
                        assert_eq!(
                            dst_scalar, dst3,
                            "avx512 w={w} area={area} scale={have_scale}"
                        );
                        assert_eq!(sum, sum3, "avx512 sum w={w} area={area} scale={have_scale}");
                    }
                }
            }
        }
    }
}
