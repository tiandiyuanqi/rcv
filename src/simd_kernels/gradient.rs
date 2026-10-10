//! 梯度内核（对齐 OpenCV `filter.simd.hpp` 的 `SymmRowSmallVec_8u32s` /
//! `SymmColumnSmallVec_32s16s` / `FilterVec_8u16s`）。
//!
//! Sobel/Scharr 8U→16S 走 bit-exact 整数路径（OpenCV `createSeparableLinearFilter`
//! KERNEL_INTEGER 分支）：
//! - 行阶段 u8 → i32：对称核 `k0*S[i] + k1*(S[i-cn] + S[i+cn])`，
//!   反对称核 `k1*(S[i+cn] - S[i-cn])`（kx 中心化存储）
//! - 列阶段 i32 → i16：对称核 `f0*S1 + f1*(S0 + S2) + delta` 饱和到 i16，
//!   反对称核 `f1*(S2 - S0) + delta` 饱和到 i16
//!
//! Laplacian ksize=3 走 5-tap 2D 卷积（`FilterVec_8u16s`，f32 累加 + round），
//! 整数核下 f32 累加精确（值域 < 2^24）。
//!
//! magnitude/phase/cart_to_polar 对齐 `mathfuncs_core.simd.hpp`：
//! magnitude = sqrt(fma(x,x,y*y))；phase = atan 多项式（FMA 链）。

/// 行阶段（u8 → i32，对称核 ksize=3，cn=1）：`D[i] = k0*S[i] + k1*(S[i-1] + S[i+1])`。
///
/// `src` 为原始行（长度 ≥ w，无左右边框），`dst` 长度 ≥ w。
/// 首尾像素的越界采样由 `border` 给出（Clamp/Reflect/Constant）。
pub fn deriv_row_symm_k3_cn1(
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
    for x in 0..w {
        let l = if x >= 1 { src[x - 1] as i32 } else { sample(-1) };
        let r = if x + 1 < w { src[x + 1] as i32 } else { sample(w as i64) };
        dst[x] = k0 * src[x] as i32 + k1 * (l + r);
    }
}

/// 行阶段（u8 → i32，反对称核 ksize=3，cn=1）：`D[i] = k1*(S[i+1] - S[i-1])`。
pub fn deriv_row_asymm_k3_cn1(
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
    for x in 0..w {
        let l = if x >= 1 { src[x - 1] as i32 } else { sample(-1) };
        let r = if x + 1 < w { src[x + 1] as i32 } else { sample(w as i64) };
        dst[x] = k1 * (r - l);
    }
}

/// 列阶段（i32 → i16，对称核 ksize=3）：`D = f0*S1 + f1*(S0 + S2) + delta`，饱和 i16。
pub fn deriv_col_symm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f0: i32, f1: i32, delta: i32) {
    for x in 0..w {
        let s = f0 * rows[1][x] + f1 * (rows[0][x] + rows[2][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// 列阶段（i32 → i16，反对称核 ksize=3）：`D = f1*(S2 - S0) + delta`，饱和 i16。
pub fn deriv_col_asymm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f1: i32, delta: i32) {
    for x in 0..w {
        let s = f1 * (rows[2][x] - rows[0][x]) + delta;
        dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}

/// Laplacian 3x3（u8 → i16，5-tap）。
///
/// `rows` 为 3 行源（已按 border 映射），`dst` 长度 ≥ w。
/// ksize1 核 [0,1,0; 1,-4,1; 0,1,0]（十字）；
/// ksize3 核 [2,0,2; 0,-8,0; 2,0,2]（对角）。
/// 行内左右越界 clamp（x=0 取 x=1，x=w-1 取 x=w-2，w=1 取自身）。
pub fn laplacian3_rows_u8(
    rows: [&[u8]; 3],
    dst: &mut [i16],
    w: usize,
    ksize1: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let sample = |x: i64| -> i32 {
        match border.interpolate(x as isize, w) {
            Some(sx) => rows[1][sx] as i32,
            None => match border {
                crate::core::border::BorderPolicy::Constant(v) => v as i32,
                _ => unreachable!("non-constant border always interpolates"),
            },
        }
    };
    if ksize1 {
        // 十字：u + d + l + r - 4c
        for x in 0..w {
            let l = if x >= 1 { rows[1][x - 1] as i32 } else { sample(-1) };
            let r = if x + 1 < w { rows[1][x + 1] as i32 } else { sample(w as i64) };
            let s = rows[0][x] as i32 + rows[2][x] as i32 + l + r - 4 * rows[1][x] as i32;
            dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    } else {
        // 对角：2*(r0[x-1] + r0[x+1] + r2[x-1] + r2[x+1]) - 8*r1[x]
        let sample_r = |ri: usize, x: i64| -> i32 {
            match border.interpolate(x as isize, w) {
                Some(sx) => rows[ri][sx] as i32,
                None => match border {
                    crate::core::border::BorderPolicy::Constant(v) => v as i32,
                    _ => unreachable!("non-constant border always interpolates"),
                },
            }
        };
        for x in 0..w {
            let l0 = if x >= 1 { rows[0][x - 1] as i32 } else { sample_r(0, -1) };
            let r0 = if x + 1 < w { rows[0][x + 1] as i32 } else { sample_r(0, w as i64) };
            let l2 = if x >= 1 { rows[2][x - 1] as i32 } else { sample_r(2, -1) };
            let r2 = if x + 1 < w { rows[2][x + 1] as i32 } else { sample_r(2, w as i64) };
            let s = 2 * (l0 + r0 + l2 + r2) - 8 * rows[1][x] as i32;
            dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
}

/// magnitude（f32）：`dst = sqrt(x*x + y*y)`（标量基准，对齐 `magnitude32f`）。
pub fn magnitude32f_scalar(x: &[f32], y: &[f32], dst: &mut [f32], n: usize) {
    for i in 0..n {
        dst[i] = (x[i] * x[i] + y[i] * y[i]).sqrt();
    }
}

/// atan2 多项式近似（对齐 `atan_f32`，返回度数 [0, 360)）。
///
/// `c = min(|x|,|y|) / (max(|x|,|y|) + DBL_EPSILON)`，
/// `a = ((p7*c2 + p5)*c2 + p3)*c2 + p1)*c`，再按象限修正。
pub fn atan_f32_scalar(y: f32, x: f32) -> f32 {
    // 系数来自 OpenCV v_atan_f32（f64 精度常量，乘 180/π 转度数）
    #[allow(clippy::excessive_precision)]
    const P1: f32 = 0.9997878412794807 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P3: f32 = -0.3258083974640975 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P5: f32 = 0.1555786518463281 * (180.0 / std::f32::consts::PI);
    #[allow(clippy::excessive_precision)]
    const P7: f32 = -0.04432655554792128 * (180.0 / std::f32::consts::PI);
    let ax = x.abs();
    let ay = y.abs();
    let a = if ax >= ay {
        let c = ay / (ax + f64::EPSILON as f32);
        let c2 = c * c;
        (((P7 * c2 + P5) * c2 + P3) * c2 + P1) * c
    } else {
        let c = ax / (ay + f64::EPSILON as f32);
        let c2 = c * c;
        90.0 - (((P7 * c2 + P5) * c2 + P3) * c2 + P1) * c
    };
    let a = if x < 0.0 { 180.0 - a } else { a };
    if y < 0.0 {
        360.0 - a
    } else {
        a
    }
}

/// phase（f32）：`dst = atan2(y, x) * scale`（标量基准，对齐 `fastAtan32f_`）。
pub fn fast_atan2_32f_scalar(y: &[f32], x: &[f32], dst: &mut [f32], n: usize, angle_in_degrees: bool) {
    let scale = if angle_in_degrees {
        1.0f32
    } else {
        std::f32::consts::PI / 180.0
    };
    for i in 0..n {
        dst[i] = atan_f32_scalar(y[i], x[i]) * scale;
    }
}

/// cart_to_polar（f32）：一次扫描同时产出 magnitude 与 angle（标量基准）。
pub fn cart_to_polar32f_scalar(
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
    for i in 0..n {
        mag[i] = (x[i] * x[i] + y[i] * y[i]).sqrt();
        angle[i] = atan_f32_scalar(y[i], x[i]) * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deriv_row_symm_121() {
        // kx = [1,2,1]：D[i] = 2*S[i] + S[i-1] + S[i+1]（Clamp 边框）
        let src = [1u8, 2, 3, 4, 5];
        let mut dst = [0i32; 5];
        deriv_row_symm_k3_cn1(&src, &mut dst, 5, 2, 1, crate::core::border::BorderPolicy::Clamp);
        // x=0: 2*1 + 1 + 2 = 5；x=1: 2*2 + 1 + 3 = 8；x=2: 2*3+2+4=12；x=3: 2*4+3+5=16；x=4: 2*5+4+5=19
        assert_eq!(dst, [5, 8, 12, 16, 19]);
    }

    #[test]
    fn deriv_row_asymm_101() {
        // kx = [-1,0,1]：D[i] = S[i+1] - S[i-1]（Clamp 边框）
        let src = [1u8, 2, 3, 4, 5];
        let mut dst = [0i32; 5];
        deriv_row_asymm_k3_cn1(&src, &mut dst, 5, 1, crate::core::border::BorderPolicy::Clamp);
        // x=0: 2-1=1；x=1: 3-1=2；x=2: 4-2=2；x=3: 5-3=2；x=4: 5-4=1
        assert_eq!(dst, [1, 2, 2, 2, 1]);
    }

    #[test]
    fn deriv_col_symm_121() {
        let r0 = [1i32, 2];
        let r1 = [3i32, 4];
        let r2 = [5i32, 6];
        let mut dst = [0i16; 2];
        deriv_col_symm_k3([&r0, &r1, &r2], &mut dst, 2, 2, 1, 0);
        // x=0: 2*3 + 1*(1+5) = 12；x=1: 2*4 + 1*(2+6) = 16
        assert_eq!(dst, [12, 16]);
    }

    #[test]
    fn deriv_col_asymm_101() {
        let r0 = [1i32, 2];
        let r1 = [3i32, 4];
        let r2 = [5i32, 6];
        let mut dst = [0i16; 2];
        deriv_col_asymm_k3([&r0, &r1, &r2], &mut dst, 2, 1, 0);
        // x=0: 5-1=4；x=1: 6-2=4
        assert_eq!(dst, [4, 4]);
    }

    #[test]
    fn deriv_col_saturates() {
        let r0 = [0i32; 2];
        let r1 = [i32::MAX / 2, 0];
        let r2 = [0i32; 2];
        let mut dst = [0i16; 2];
        deriv_col_symm_k3([&r0, &r1, &r2], &mut dst, 2, 2, 1, 0);
        assert_eq!(dst[0], i16::MAX);
        let r1n = [i32::MIN / 2, 0];
        deriv_col_symm_k3([&r0, &r1n, &r2], &mut dst, 2, 2, 1, 0);
        assert_eq!(dst[0], i16::MIN);
    }

    #[test]
    fn laplacian3_basic() {
        // 3 行：中心行 [5,5,5]，上下 [1,1,1]
        let r0 = [1u8, 1, 1];
        let r1 = [5u8, 5, 5];
        let r2 = [1u8, 1, 1];
        let mut dst = [0i16; 3];
        laplacian3_rows_u8(
            [&r0, &r1, &r2],
            &mut dst,
            3,
            true,
            crate::core::border::BorderPolicy::Clamp,
        );
        // x=0: 1+1+5+5-20 = -8；x=1: 1+1+5+5-20 = -8（左右 clamp）
        assert_eq!(dst, [-8, -8, -8]);
    }

    #[test]
    fn atan_known_values() {
        let eps = 1e-2;
        assert!((atan_f32_scalar(1.0, 1.0) - 45.0).abs() < eps);
        assert!((atan_f32_scalar(0.0, 1.0) - 0.0).abs() < eps);
        assert!((atan_f32_scalar(1.0, 0.0) - 90.0).abs() < eps);
        assert!((atan_f32_scalar(0.0, -1.0) - 180.0).abs() < eps);
        assert!((atan_f32_scalar(-1.0, 0.0) - 270.0).abs() < eps);
        assert!((atan_f32_scalar(-1.0, -1.0) - 225.0).abs() < eps);
    }

    #[test]
    fn magnitude_basic() {
        let x = [3.0f32, 0.0];
        let y = [4.0f32, 0.0];
        let mut dst = [0f32; 2];
        magnitude32f_scalar(&x, &y, &mut dst, 2);
        assert_eq!(dst, [5.0, 0.0]);
    }
}

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

    fn rand_vec(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (x >> 33) as u8
            })
            .collect()
    }

    /// 三路径交叉验证：行阶段（对称/反对称）。
    #[test]
    fn deriv_row_cross_isa() {
        for w in [1usize, 2, 5, 16, 31, 32, 33, 64, 100, 128] {
            for border in [
                BorderPolicy::Clamp,
                BorderPolicy::Reflect,
                BorderPolicy::Constant(7),
            ] {
                let src = rand_vec(w, w as u64 + 1);
                for (k0, k1) in [(2i32, 1i32), (3, 10), (-2, 1)] {
                    let mut ref_dst = vec![0i32; w];
                    deriv_row_symm_k3_cn1(&src, &mut ref_dst, w, k0, k1, border);
                    if has_avx2() {
                        let mut dst = vec![0i32; w];
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::deriv_row_symm_k3_cn1(
                                &src, &mut dst, w, k0, k1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx2 symm w={w} k=({k0},{k1}) {border:?}");
                    }
                    if has_avx512() {
                        let mut dst = vec![0i32; w];
                        // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::deriv_row_symm_k3_cn1(
                                &src, &mut dst, w, k0, k1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx512 symm w={w} k=({k0},{k1}) {border:?}");
                    }
                }
                for k1 in [1i32, -1, 3] {
                    let mut ref_dst = vec![0i32; w];
                    deriv_row_asymm_k3_cn1(&src, &mut ref_dst, w, k1, border);
                    if has_avx2() {
                        let mut dst = vec![0i32; w];
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::deriv_row_asymm_k3_cn1(
                                &src, &mut dst, w, k1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx2 asymm w={w} k1={k1} {border:?}");
                    }
                    if has_avx512() {
                        let mut dst = vec![0i32; w];
                        // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::deriv_row_asymm_k3_cn1(
                                &src, &mut dst, w, k1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx512 asymm w={w} k1={k1} {border:?}");
                    }
                }
            }
        }
    }

    /// 三路径交叉验证：列阶段（对称/反对称）。
    #[test]
    fn deriv_col_cross_isa() {
        for w in [1usize, 2, 5, 16, 31, 32, 33, 64, 100] {
            let r0: Vec<i32> = (0..w).map(|i| i as i32 * 137 % 2001 - 1000).collect();
            let r1: Vec<i32> = (0..w).map(|i| i as i32 * 91 % 2001 - 1000).collect();
            let r2: Vec<i32> = (0..w).map(|i| i as i32 * 353 % 2001 - 1000).collect();
            let rows = [&r0[..], &r1[..], &r2[..]];
            for (f0, f1) in [(2i32, 1i32), (3, 10), (-2, 1)] {
                let mut ref_dst = vec![0i16; w];
                deriv_col_symm_k3(rows, &mut ref_dst, w, f0, f1, 0);
                if has_avx2() {
                    let mut dst = vec![0i16; w];
                    // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                    unsafe { crate::simd_kernels::x86::avx2::deriv_col_symm_k3(rows, &mut dst, w, f0, f1, 0) };
                    assert_eq!(dst, ref_dst, "avx2 col symm w={w} f=({f0},{f1})");
                }
                if has_avx512() {
                    let mut dst = vec![0i16; w];
                    // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx512::deriv_col_symm_k3(rows, &mut dst, w, f0, f1, 0)
                    };
                    assert_eq!(dst, ref_dst, "avx512 col symm w={w} f=({f0},{f1})");
                }
            }
            for f1 in [1i32, -1, 3] {
                let mut ref_dst = vec![0i16; w];
                deriv_col_asymm_k3(rows, &mut ref_dst, w, f1, 0);
                if has_avx2() {
                    let mut dst = vec![0i16; w];
                    // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                    unsafe { crate::simd_kernels::x86::avx2::deriv_col_asymm_k3(rows, &mut dst, w, f1, 0) };
                    assert_eq!(dst, ref_dst, "avx2 col asymm w={w} f1={f1}");
                }
                if has_avx512() {
                    let mut dst = vec![0i16; w];
                    // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx512::deriv_col_asymm_k3(rows, &mut dst, w, f1, 0)
                    };
                    assert_eq!(dst, ref_dst, "avx512 col asymm w={w} f1={f1}");
                }
            }
        }
    }

    /// 三路径交叉验证：Laplacian（十字/对角）。
    #[test]
    fn laplacian_cross_isa() {
        for w in [1usize, 2, 5, 16, 31, 32, 33, 64, 100] {
            let r0 = rand_vec(w, w as u64 + 11);
            let r1 = rand_vec(w, w as u64 + 22);
            let r2 = rand_vec(w, w as u64 + 33);
            let rows = [&r0[..], &r1[..], &r2[..]];
            for ksize1 in [true, false] {
                for border in [
                    BorderPolicy::Clamp,
                    BorderPolicy::Reflect,
                    BorderPolicy::Constant(7),
                ] {
                    let mut ref_dst = vec![0i16; w];
                    laplacian3_rows_u8(rows, &mut ref_dst, w, ksize1, border);
                    if has_avx2() {
                        let mut dst = vec![0i16; w];
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::laplacian3_rows_u8(
                                rows, &mut dst, w, ksize1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx2 lap w={w} k1={ksize1} {border:?}");
                    }
                    if has_avx512() {
                        let mut dst = vec![0i16; w];
                        // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::laplacian3_rows_u8(
                                rows, &mut dst, w, ksize1, border,
                            )
                        };
                        assert_eq!(dst, ref_dst, "avx512 lap w={w} k1={ksize1} {border:?}");
                    }
                }
            }
        }
    }

    /// 三路径交叉验证：magnitude / atan2 / cart_to_polar。
    #[test]
    fn float_ops_cross_isa() {
        let n = 257;
        let xs: Vec<f32> = (0..n).map(|i| (i as f32 * 0.7).sin() * 100.0 - 30.0).collect();
        let ys: Vec<f32> = (0..n).map(|i| (i as f32 * 1.3).cos() * 90.0 + 10.0).collect();
        // magnitude
        let mut ref_m = vec![0f32; n];
        magnitude32f_scalar(&xs, &ys, &mut ref_m, n);
        if has_avx2() {
            let mut m = vec![0f32; n];
            // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
            unsafe { crate::simd_kernels::x86::avx2::magnitude32f(&xs, &ys, &mut m, n) };
            for i in 0..n {
                assert!((m[i] - ref_m[i]).abs() < 1e-3, "avx2 mag i={i}");
            }
        }
        if has_avx512() {
            let mut m = vec![0f32; n];
            // SAFETY: 测试环境已探测 AVX512；切片长度与内核契约一致
            unsafe { crate::simd_kernels::x86::avx512::magnitude32f(&xs, &ys, &mut m, n) };
            for i in 0..n {
                assert!((m[i] - ref_m[i]).abs() < 1e-3, "avx512 mag i={i}");
            }
        }
        // atan2（度）
        let mut ref_a = vec![0f32; n];
        fast_atan2_32f_scalar(&ys, &xs, &mut ref_a, n, true);
        for (isa, has) in [("avx2", has_avx2()), ("avx512", has_avx512())] {
            if !has {
                continue;
            }
            let mut a = vec![0f32; n];
            // SAFETY: 测试环境已探测对应 ISA；切片长度与内核契约一致
            unsafe {
                match isa {
                    "avx2" => crate::simd_kernels::x86::avx2::fast_atan2_32f(&ys, &xs, &mut a, n, true),
                    _ => crate::simd_kernels::x86::avx512::fast_atan2_32f(&ys, &xs, &mut a, n, true),
                }
            };
            for i in 0..n {
                let d = (a[i] - ref_a[i]).abs().min((a[i] - ref_a[i] + 360.0).abs()).min((a[i] - ref_a[i] - 360.0).abs());
                assert!(d < 0.02, "{isa} atan i={i}: {a:?} vs {ref_a:?}");
            }
        }
        // cart_to_polar
        let mut ref_m = vec![0f32; n];
        let mut ref_a = vec![0f32; n];
        cart_to_polar32f_scalar(&xs, &ys, &mut ref_m, &mut ref_a, n, false);
        for (isa, has) in [("avx2", has_avx2()), ("avx512", has_avx512())] {
            if !has {
                continue;
            }
            let mut m = vec![0f32; n];
            let mut a = vec![0f32; n];
            // SAFETY: 测试环境已探测对应 ISA；切片长度与内核契约一致
            unsafe {
                match isa {
                    "avx2" => {
                        crate::simd_kernels::x86::avx2::cart_to_polar32f(&xs, &ys, &mut m, &mut a, n, false)
                    }
                    _ => {
                        crate::simd_kernels::x86::avx512::cart_to_polar32f(&xs, &ys, &mut m, &mut a, n, false)
                    }
                }
            };
            for i in 0..n {
                assert!((m[i] - ref_m[i]).abs() < 1e-3, "{isa} ctp mag i={i}");
                let d = (a[i] - ref_a[i]).abs();
                assert!(d < 5e-4, "{isa} ctp angle i={i}: {} vs {}", a[i], ref_a[i]);
            }
        }
    }
}
