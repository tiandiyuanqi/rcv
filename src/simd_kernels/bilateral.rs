//! 双边滤波内核（对应 OpenCV `bilateral_filter.simd.hpp`）。
//!
//! 算法对齐 OpenCV `bilateralFilter`：
//! - 8u：`color_weight[i] = exp(i²·gc)` LUT（256·cn 项，cn=3 时索引为
//!   三通道 absdiff 之和 ≤ 765）；空间核为圆形（`r > radius` 跳过），
//!   **含中心点**（weight=1）；输出 `cvRound(sum/wsum)`（round-half-even）
//! - 32f：插值 expLUT（kExpNumBinsPerChannel=4096）+ NaN 语义
//!   （邻域 NaN 跳过，中心 NaN 时 `sum/wsum`，否则 `(sum+rval)/(wsum+1)`）；
//!   空间核**不含中心点**（中心在归一化时单独加）
//! - radius：`d<=0 ? round(σs·1.5) : d/2`，最小 1
//!
//! 输入为已物化的 padded 图（`copyMakeBorder` 等价，四周各 radius 像素），
//! `space_ofs` 为 padded 行距下的字节偏移（8u）/元素偏移（32f）。

/// 双边滤波公共参数（8u/32f 共用，8u 用 color_weight，32f 用 exp_lut）。
pub struct BilateralParams {
    pub radius: usize,
    /// 圆形核内点数（8u 含中心；32f 不含中心）
    pub maxk: usize,
    /// 核点在 padded 图中的偏移（8u：字节；32f：f32 元素）
    pub space_ofs: Vec<usize>,
    pub space_weight: Vec<f32>,
    /// 8u 颜色权重 LUT（256·cn 项）
    pub color_weight: Vec<f32>,
    /// 32f 插值 exp LUT（kExpNumBins+2 项）与缩放
    pub exp_lut: Vec<f32>,
    pub scale_index: f32,
    pub k_exp_num_bins: usize,
}

/// 计算 radius（对齐 OpenCV：`d<=0 ? cvRound(σs*1.5) : d/2`，最小 1）。
pub fn bilateral_radius(d: i32, sigma_space: f64) -> usize {
    let r = if d <= 0 {
        (sigma_space * 1.5).round() as i32
    } else {
        d / 2
    };
    r.max(1) as usize
}

/// 构建空间核（圆形，`r > radius` 跳过）。
///
/// `include_center`：8u 路径含中心（weight=exp(0)=1）；32f 路径不含
/// （中心在归一化时单独加）。偏移按 padded 行距 `padded_stride`（元素数）
/// 与通道数 `cn` 计算。
///
/// `gauss_space_coeff`：8u 路径传 **f32 截断值**（OpenCV dispatch.cpp:179
/// 声明为 float，`exp(r*r*coeff)` 中 r*r 是 double、coeff 是 float →
/// 提升 double 后 std::exp）；32f 路径传 f64 全精度（dispatch.cpp:264
/// 声明为 double）。
fn build_space_kernel(
    radius: usize,
    cn: usize,
    padded_stride: usize,
    include_center: bool,
    gauss_space_coeff: f64,
) -> (Vec<usize>, Vec<f32>) {
    let mut ofs = Vec::new();
    let mut wgt = Vec::new();
    let r = radius as i64;
    for i in -r..=r {
        for j in -r..=r {
            let dist = ((i * i + j * j) as f64).sqrt();
            if dist > radius as f64 {
                continue;
            }
            if !include_center && i == 0 && j == 0 {
                continue;
            }
            wgt.push((dist * dist * gauss_space_coeff).exp() as f32);
            ofs.push((i * padded_stride as i64 + j * cn as i64) as usize);
        }
    }
    (ofs, wgt)
}

/// 构建 8u 全套参数（color LUT + 空间核含中心）。
///
/// 对齐 OpenCV dispatch.cpp:178-213：两个 coeff 均先截断为 **f32** 再
/// 参与运算（`exp(i*i*coeff)` 中 int*float → float；`exp(r*r*coeff)` 中
/// double*float → double）。
///
/// color LUT 的混合精度（对齐 dispatch.cpp:198-216）：SIMD 可用时前
/// `256*cn - nlanes` 项用 `v_exp`（多项式近似，见 [`v_exp_f32`]），
/// 尾部 `nlanes` 项用 `std::exp`。两者位不同，必须逐段复刻。
/// nlanes 取运行时 ISA 的 v_float32 宽度（AVX512=16，AVX2=8，标量=0）。
pub fn bilateral_params_8u(
    radius: usize,
    cn: usize,
    padded_stride: usize,
    sigma_color: f64,
    sigma_space: f64,
) -> BilateralParams {
    let gauss_color_coeff = (-0.5 / (sigma_color * sigma_color)) as f32;
    let gauss_space_coeff = ((-0.5 / (sigma_space * sigma_space)) as f32) as f64;
    let nlanes = match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => 16,
        crate::simd_kernels::x86::Isa::Avx2 => 8,
        crate::simd_kernels::x86::Isa::Scalar => 0,
    };
    let total = 256 * cn;
    let simd_end = total.saturating_sub(nlanes); // i < (256*cn) - nlanes 用 v_exp
    let mut color_weight = vec![0f32; total];
    for (i, w) in color_weight.iter_mut().enumerate() {
        let i = i as f32;
        *w = if i < simd_end as f32 {
            v_exp_f32(i * i * gauss_color_coeff)
        } else {
            (i * i * gauss_color_coeff).exp()
        };
    }
    let (mut space_ofs, mut space_weight) =
        build_space_kernel(radius, cn, padded_stride, true, gauss_space_coeff);
    // cn=1、maxk==13（d=5/radius=2）时对齐 OpenCV SIMD 特化分支的累加顺序
    // （bilateral_filter.simd.hpp:200-272：line1/line5 → line2 → line4 → line3，
    // 即 k 序 [0,12,1,2,3,9,10,11,4,5,6,7,8]）。浮点加法不可结合，
    // 顺序不同 → sum 位差 → round 后差 1。重排数组让所有内核自动对齐。
    if cn == 1 && space_ofs.len() == 13 {
        const K13_ORDER: [usize; 13] = [0, 12, 1, 2, 3, 9, 10, 11, 4, 5, 6, 7, 8];
        let ofs: Vec<usize> = K13_ORDER.iter().map(|&k| space_ofs[k]).collect();
        let wgt: Vec<f32> = K13_ORDER.iter().map(|&k| space_weight[k]).collect();
        space_ofs = ofs;
        space_weight = wgt;
    }
    BilateralParams {
        radius,
        maxk: space_ofs.len(),
        space_ofs,
        space_weight,
        color_weight,
        exp_lut: Vec::new(),
        scale_index: 0.0,
        k_exp_num_bins: 0,
    }
}

/// OpenCV `v_exp_default_32f`（intrin_math.hpp:89-136）的标量复刻。
///
/// 多项式近似 exp：与 `std::exp` 位不同（~1ulp 级差异），OpenCV 的
/// SIMD 路径（`v_exp`）用它构建 color LUT，对拍必须逐位复刻。
/// 输入域 [-88.38, 89] 钳位；NaN 透传。
#[allow(clippy::excessive_precision, clippy::approx_constant)]
pub fn v_exp_f32(x: f32) -> f32 {
    const LO: f32 = -88.3762626647949;
    const HI: f32 = 89.0;
    const HALF: f32 = 0.5;
    const LOG2EF: f32 = 1.44269504088896341;
    const C1: f32 = -6.93359375E-1;
    const C2: f32 = 2.12194440E-4;
    const P0: f32 = 1.9875691500E-4;
    const P1: f32 = 1.3981999507E-3;
    const P2: f32 = 8.3334519073E-3;
    const P3: f32 = 4.1665795894E-2;
    const P4: f32 = 1.6666665459E-1;
    const P5: f32 = 5.0000001201E-1;

    let x = x.clamp(LO, HI);
    // v_fma：fused multiply-add（Rust f32::mul_add 等价）
    let t = x.mul_add(LOG2EF, HALF).floor();
    let mm = (((t as i32) + 0x7f) as u32) << 23;
    let mut x = t.mul_add(C1, x);
    x = t.mul_add(C2, x);
    let xx = x * x;
    let mut y = x.mul_add(P0, P1);
    y = y.mul_add(x, P2);
    y = y.mul_add(x, P3);
    y = y.mul_add(x, P4);
    y = y.mul_add(x, P5);
    y = y.mul_add(xx, x);
    y += 1.0;
    // 2^mm：位重组（mm 已是合法指数域）
    let scale = f32::from_bits(mm);
    y * scale
}

/// 构建 32f 全套参数（插值 expLUT + 空间核不含中心）。
///
/// `len = (max - min)·cn`（值域跨度），`kExpNumBins = 4096·cn`，
/// `scale_index = kExpNumBins/len`。
pub fn bilateral_params_32f(
    radius: usize,
    cn: usize,
    padded_stride: usize,
    sigma_color: f64,
    sigma_space: f64,
    min_val: f32,
    max_val: f32,
) -> BilateralParams {
    let gauss_color_coeff = -0.5 / (sigma_color * sigma_color);
    let gauss_space_coeff = -0.5 / (sigma_space * sigma_space);
    let k_exp_num_bins = (1usize << 12) * cn;
    let len = (max_val - min_val) * cn as f32;
    let scale_index = k_exp_num_bins as f32 / len;
    // LUT：expLUT[i] = exp((i/scale)²·gc)，衰减到 0 后固定 0
    let mut exp_lut = vec![0f32; k_exp_num_bins + 2];
    let mut last = 1f32;
    for (i, v) in exp_lut.iter_mut().enumerate() {
        if last > 0.0 {
            let val = i as f64 / scale_index as f64;
            last = (val * val * gauss_color_coeff).exp() as f32;
            *v = last;
        } else {
            *v = 0.0;
        }
    }
    let (space_ofs, space_weight) =
        build_space_kernel(radius, cn, padded_stride, false, gauss_space_coeff);
    BilateralParams {
        radius,
        maxk: space_ofs.len(),
        space_ofs,
        space_weight,
        color_weight: Vec::new(),
        exp_lut,
        scale_index,
        k_exp_num_bins,
    }
}

/// 8u 双边滤波单行（标量基准）。
///
/// `padded` 为 (w+2r)×(h+2r)×cn 连续缓冲，`padded_stride` 为其行字节数；
/// 处理输出行 `y`（0 基，原图坐标），写 `dst_row`（长度 w·cn）。
pub fn bilateral_row_u8_scalar(
    padded: &[u8],
    dst_row: &mut [u8],
    w: usize,
    cn: usize,
    y: usize,
    params: &BilateralParams,
) {
    bilateral_row_u8_scalar_tail(padded, dst_row, w, cn, y, params, 0)
}

/// 8u 双边滤波单行（标量，从像素 `j_start` 开始；SIMD 尾部复用）。
pub fn bilateral_row_u8_scalar_tail(
    padded: &[u8],
    dst_row: &mut [u8],
    w: usize,
    cn: usize,
    y: usize,
    params: &BilateralParams,
    j_start: usize,
) {
    let r = params.radius;
    let stride = (w + 2 * r) * cn;
    let base = (y + r) * stride + r * cn;
    match cn {
        1 => {
            for j in j_start..w {
                let val0 = padded[base + j];
                let mut wsum = 0f32;
                let mut sum = 0f32;
                for k in 0..params.maxk {
                    let val = padded[base + j + params.space_ofs[k]];
                    let w = params.space_weight[k]
                        * params.color_weight[val.abs_diff(val0) as usize];
                    wsum += w;
                    sum += val as f32 * w;
                }
                dst_row[j] = round_ties_even_f32(sum / wsum) as u8;
            }
        }
        3 => {
            for j in j_start..w {
                let p = base + j * 3;
                let (rb, rg, rr) = (padded[p], padded[p + 1], padded[p + 2]);
                let mut wsum = 0f32;
                let mut sum_b = 0f32;
                let mut sum_g = 0f32;
                let mut sum_r = 0f32;
                for k in 0..params.maxk {
                    let q = p + params.space_ofs[k];
                    let b = padded[q];
                    let g = padded[q + 1];
                    let rr2 = padded[q + 2];
                    let idx = b.abs_diff(rb) as usize
                        + g.abs_diff(rg) as usize
                        + rr2.abs_diff(rr) as usize;
                    let w = params.space_weight[k] * params.color_weight[idx];
                    wsum += w;
                    sum_b += b as f32 * w;
                    sum_g += g as f32 * w;
                    sum_r += rr2 as f32 * w;
                }
                let inv = 1.0 / wsum;
                let o = j * 3;
                dst_row[o] = round_ties_even_f32(sum_b * inv) as u8;
                dst_row[o + 1] = round_ties_even_f32(sum_g * inv) as u8;
                dst_row[o + 2] = round_ties_even_f32(sum_r * inv) as u8;
            }
        }
        _ => unreachable!("bilateral 8u 仅支持 cn=1/3"),
    }
}

/// 32f 双边滤波单行（标量基准，含 NaN 语义）。
///
/// `padded` 为 f32 连续缓冲，`padded_stride` 为行元素数（(w+2r)·cn）。
pub fn bilateral_row_f32_scalar(
    padded: &[f32],
    dst_row: &mut [f32],
    w: usize,
    cn: usize,
    y: usize,
    params: &BilateralParams,
) {
    let r = params.radius;
    let stride = (w + 2 * r) * cn;
    let base = (y + r) * stride + r * cn;
    match cn {
        1 => {
            for j in 0..w {
                let rval = padded[base + j];
                let mut wsum = 0f32;
                let mut sum = 0f32;
                for k in 0..params.maxk {
                    let val = padded[base + j + params.space_ofs[k]];
                    if val.is_nan() {
                        continue;
                    }
                    let mut alpha = (val - rval).abs() * params.scale_index;
                    if alpha > params.k_exp_num_bins as f32 {
                        alpha = params.k_exp_num_bins as f32;
                    }
                    let idx = alpha.floor();
                    alpha -= idx;
                    let idx = idx as usize;
                    let lut = &params.exp_lut;
                    let w = if rval.is_nan() {
                        params.space_weight[k]
                    } else {
                        params.space_weight[k]
                            * (lut[idx] + alpha * (lut[idx + 1] - lut[idx]))
                    };
                    wsum += w;
                    sum += val * w;
                }
                dst_row[j] = if rval.is_nan() {
                    sum / wsum
                } else {
                    (sum + rval) / (wsum + 1.0)
                };
            }
        }
        3 => {
            for j in 0..w {
                let p = base + j * 3;
                let (rb, rg, rr) = (padded[p], padded[p + 1], padded[p + 2]);
                let r_nan = rb.is_nan() || rg.is_nan() || rr.is_nan();
                let mut wsum = 0f32;
                let mut sum_b = 0f32;
                let mut sum_g = 0f32;
                let mut sum_r = 0f32;
                for k in 0..params.maxk {
                    let q = p + params.space_ofs[k];
                    let b = padded[q];
                    let g = padded[q + 1];
                    let rr2 = padded[q + 2];
                    if b.is_nan() || g.is_nan() || rr2.is_nan() {
                        continue;
                    }
                    let mut alpha = ((b - rb).abs() + (g - rg).abs() + (rr2 - rr).abs())
                        * params.scale_index;
                    if alpha > params.k_exp_num_bins as f32 {
                        alpha = params.k_exp_num_bins as f32;
                    }
                    let idx = alpha.floor();
                    alpha -= idx;
                    let idx = idx as usize;
                    let lut = &params.exp_lut;
                    let w = if r_nan {
                        params.space_weight[k]
                    } else {
                        params.space_weight[k]
                            * (lut[idx] + alpha * (lut[idx + 1] - lut[idx]))
                    };
                    wsum += w;
                    sum_b += b * w;
                    sum_g += g * w;
                    sum_r += rr2 * w;
                }
                let o = j * 3;
                if r_nan {
                    let inv = 1.0 / wsum;
                    dst_row[o] = sum_b * inv;
                    dst_row[o + 1] = sum_g * inv;
                    dst_row[o + 2] = sum_r * inv;
                } else {
                    let inv = 1.0 / (wsum + 1.0);
                    dst_row[o] = (sum_b + rb) * inv;
                    dst_row[o + 1] = (sum_g + rg) * inv;
                    dst_row[o + 2] = (sum_r + rr) * inv;
                }
            }
        }
        _ => unreachable!("bilateral 32f 仅支持 cn=1/3"),
    }
}

/// `cvRound` 语义：round-half-to-even（SSE `cvtps2dq`）。
#[inline]
pub fn round_ties_even_f32(v: f32) -> f32 {
    v.round_ties_even()
}

/// 8u 双边滤波单行（运行时派发：AVX512 → AVX2 → 标量）。
pub fn bilateral_row_u8(
    padded: &[u8],
    dst_row: &mut [u8],
    w: usize,
    cn: usize,
    y: usize,
    params: &BilateralParams,
) {
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => {
            if cn == 1 {
                // SAFETY: isa() 已验证 AVX512；长度契约由调用方保证
                unsafe {
                    crate::simd_kernels::x86::avx512::bilateral_row_u8_cn1(
                        padded, dst_row, w, y, params,
                    )
                }
            } else {
                // SAFETY: 同上
                unsafe {
                    crate::simd_kernels::x86::avx512::bilateral_row_u8_cn3(
                        padded, dst_row, w, y, params,
                    )
                }
            }
        }
        crate::simd_kernels::x86::Isa::Avx2 => {
            if cn == 1 {
                // SAFETY: isa() 已验证 AVX2；长度契约由调用方保证
                unsafe {
                    crate::simd_kernels::x86::avx2::bilateral_row_u8_cn1(
                        padded, dst_row, w, y, params,
                    )
                }
            } else {
                // SAFETY: 同上
                unsafe {
                    crate::simd_kernels::x86::avx2::bilateral_row_u8_cn3(
                        padded, dst_row, w, y, params,
                    )
                }
            }
        }
        crate::simd_kernels::x86::Isa::Scalar => {
            bilateral_row_u8_scalar(padded, dst_row, w, cn, y, params)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_matches_opencv() {
        // d<=0: round(σs*1.5)；d>0: d/2；最小 1
        assert_eq!(bilateral_radius(0, 2.0), 3); // round(3.0)
        assert_eq!(bilateral_radius(0, 1.0), 2); // round(1.5) = 2（half-even）
        assert_eq!(bilateral_radius(0, 0.3), 1); // round(0.45)=0 → max 1
        assert_eq!(bilateral_radius(5, 100.0), 2);
        assert_eq!(bilateral_radius(1, 100.0), 1);
        assert_eq!(bilateral_radius(-1, 4.0), 6);
    }

    #[test]
    fn space_kernel_circular_and_center() {
        // radius=1：角点 √2 ≈ 1.414 > 1 跳过 → 十字形 5 点
        let (ofs, wgt) = build_space_kernel(1, 1, 10, true, -0.1);
        assert_eq!(ofs.len(), 5);
        // 中心点 (i=0,j=0) 在扫描序中第 3 个（(-1,0),(0,-1),(0,0),...）
        assert_eq!(wgt[2], 1.0); // exp(0)=1
        // 不含中心（32f 路径）：4 点
        let (ofs_nc, _) = build_space_kernel(1, 1, 10, false, -0.1);
        assert_eq!(ofs_nc.len(), 4);
        // radius=2：√5 ≈ 2.236 > 2 与 √8 > 2 跳过 → 13 点
        let (ofs2, _) = build_space_kernel(2, 1, 10, true, -0.1);
        assert_eq!(ofs2.len(), 13);
    }

    #[test]
    fn bilateral_8u_constant_image_is_identity() {
        // 常量图：所有 w = space_weight·color_weight[0]，sum/wsum = 常量
        let (w, h, r) = (6usize, 5usize, 1usize);
        let cn = 1usize;
        let stride = (w + 2 * r) * cn;
        let padded = vec![100u8; stride * (h + 2 * r)];
        let params = bilateral_params_8u(r, cn, stride, 50.0, 2.0);
        let mut dst = vec![0u8; w * h];
        for y in 0..h {
            bilateral_row_u8_scalar(&padded, &mut dst[y * w..(y + 1) * w], w, cn, y, &params);
        }
        assert!(dst.iter().all(|&v| v == 100));
    }

    #[test]
    fn bilateral_8u_impulse_smoothed() {
        // 单点冲激：中心像素的输出被邻域拉低（权重和 > 1）
        let (w, h, r) = (5usize, 5usize, 2usize);
        let cn = 1usize;
        let stride = (w + 2 * r) * cn;
        let mut padded = vec![0u8; stride * (h + 2 * r)];
        // 中心 (2,2) = 255（padded 坐标 (4,4)）
        padded[4 * stride + 4] = 255;
        let params = bilateral_params_8u(r, cn, stride, 75.5, 75.5);
        let mut dst = vec![0u8; w * h];
        for y in 0..h {
            bilateral_row_u8_scalar(&padded, &mut dst[y * w..(y + 1) * w], w, cn, y, &params);
        }
        // 冲激点输出 < 255（被 0 邻域稀释）；远处仍 0
        assert!(dst[2 * w + 2] < 255);
        assert_eq!(dst[0], 0);
        assert_eq!(dst[4 * w + 4], 0);
    }

    #[test]
    fn bilateral_f32_nan_semantics() {
        // 中心 NaN：输出 = sum/wsum（邻域均值）；邻域 NaN：跳过
        let (w, h, r) = (5usize, 5usize, 1usize);
        let cn = 1usize;
        let stride = (w + 2 * r) * cn;
        let mut padded = vec![0f32; stride * (h + 2 * r)];
        for (i, v) in padded.iter_mut().enumerate() {
            *v = (i % 7) as f32;
        }
        // 中心 (2,2)（padded (3,3)）设 NaN
        padded[3 * stride + 3] = f32::NAN;
        let params = bilateral_params_32f(r, cn, stride, 10.0, 10.0, 0.0, 6.0);
        let mut dst = vec![0f32; w * h];
        for y in 0..h {
            bilateral_row_f32_scalar(&padded, &mut dst[y * w..(y + 1) * w], w, cn, y, &params);
        }
        // NaN 中心输出有限（邻域加权和）
        assert!(dst[2 * w + 2].is_finite());
        // 非 NaN 中心输出有限
        assert!(dst[0].is_finite());
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod isa_cross_tests {
    use super::*;

    fn make_padded(w: usize, h: usize, r: usize, cn: usize) -> Vec<u8> {
        let stride = (w + 2 * r) * cn;
        let mut v = vec![0u8; stride * (h + 2 * r)];
        for (i, x) in v.iter_mut().enumerate() {
            *x = (i * 31 + 7) as u8;
        }
        v
    }

    /// 多宽度 cross-test：AVX2/AVX512 与标量逐位一致。
    #[test]
    fn bilateral_8u_multi_width_scalar_vs_avx2_vs_avx512() {
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        let widths = [
            1usize, 2, 3, 5, 8, 16, 31, 32, 33, 40, 63, 64, 65, 66, 100, 130,
        ];
        for cn in [1usize, 3] {
            for &w in &widths {
                for &r in &[1usize, 2, 3] {
                    let h = 5usize;
                    let stride = (w + 2 * r) * cn;
                    let padded = make_padded(w, h, r, cn);
                    let params = bilateral_params_8u(r, cn, stride, 50.0, 2.0);
                    let mut dst_scalar = vec![0u8; w * h * cn];
                    for y in 0..h {
                        bilateral_row_u8_scalar(
                            &padded,
                            &mut dst_scalar[y * w * cn..(y + 1) * w * cn],
                            w,
                            cn,
                            y,
                            &params,
                        );
                    }
                    if has_avx2 {
                        let mut dst = vec![0u8; w * h * cn];
                        for y in 0..h {
                            let row = &mut dst[y * w * cn..(y + 1) * w * cn];
                            // SAFETY: has_avx2 已验证 CPU 支持；长度由构造保证
                            unsafe {
                                if cn == 1 {
                                    crate::simd_kernels::x86::avx2::bilateral_row_u8_cn1(
                                        &padded, row, w, y, &params,
                                    )
                                } else {
                                    crate::simd_kernels::x86::avx2::bilateral_row_u8_cn3(
                                        &padded, row, w, y, &params,
                                    )
                                }
                            }
                        }
                        assert_eq!(dst_scalar, dst, "AVX2 w={w} cn={cn} r={r}");
                    }
                    if has_avx512 {
                        let mut dst = vec![0u8; w * h * cn];
                        for y in 0..h {
                            let row = &mut dst[y * w * cn..(y + 1) * w * cn];
                            // SAFETY: has_avx512 已验证 CPU 支持；长度由构造保证
                            unsafe {
                                if cn == 1 {
                                    crate::simd_kernels::x86::avx512::bilateral_row_u8_cn1(
                                        &padded, row, w, y, &params,
                                    )
                                } else {
                                    crate::simd_kernels::x86::avx512::bilateral_row_u8_cn3(
                                        &padded, row, w, y, &params,
                                    )
                                }
                            }
                        }
                        assert_eq!(dst_scalar, dst, "AVX512 w={w} cn={cn} r={r}");
                    }
                }
            }
        }
    }
}
