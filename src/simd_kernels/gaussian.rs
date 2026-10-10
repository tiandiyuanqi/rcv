//! gaussian_blur 定点路径（对齐 OpenCV `GaussianBlurFixedPoint` u8/ufixedpoint16）。
//!
//! 核生成：`getGaussianKernelBitExact`（softdouble 精度）+ 误差扩散定点化
//! （`getGaussianKernelFixedPoint_ED`，fractionBits=8）。
//! 滤波：可分离两阶段，水平 u8→u16 定点（SHIFT=8，饱和加），
//! 垂直 u16→u8（`(Σ m·s + 128·Σm) >> 8` 饱和）。

/// softdouble 精度的 exp（对齐 OpenCV `softdouble::exp`，f64 逐位一致验证见单测）。
///
/// OpenCV softdouble::exp 用泰勒级数（x^k/k! 累积到收敛），与 libm exp
/// 在个别输入上可能差 1ulp。此处直接用 f64 exp，对拍验证核值一致。
fn soft_exp(x: f64) -> f64 {
    x.exp()
}

/// `getGaussianKernelBitExact`：返回 softdouble 精度核（和为 1）。
///
/// sigma <= 0 且 n ∈ {1,3,5,7,9}：查表（2 的幂核）。
/// 否则：`t = exp(-x²/(2σ²))` 归一化（σ<=0 时 σ = 0.15n + 0.35）。
pub(crate) fn gaussian_kernel_bitexact(n: usize, sigma: f64) -> Vec<f64> {
    debug_assert!(n > 0);
    if sigma <= 0.0 {
        match n {
            1 => return vec![1.0],
            3 => return vec![0.25, 0.5, 0.25],
            5 => return vec![0.0625, 0.25, 0.375, 0.25, 0.0625],
            7 => {
                return vec![
                    0.03125, 0.109375, 0.21875, 0.28125, 0.21875, 0.109375, 0.03125,
                ];
            }
            9 => {
                return vec![
                    4.0 / 256.0,
                    13.0 / 256.0,
                    30.0 / 256.0,
                    51.0 / 256.0,
                    60.0 / 256.0,
                    51.0 / 256.0,
                    30.0 / 256.0,
                    13.0 / 256.0,
                    4.0 / 256.0,
                ];
            }
            _ => {}
        }
    }
    // sigmaX = sigma > 0 ? sigma : 0.15*n + 0.35（等价 ((n-1)*0.5-1)*0.3+0.8）
    let sigma_x = if sigma > 0.0 {
        sigma
    } else {
        0.15 * n as f64 + 0.35
    };
    let scale2 = -0.125 / (sigma_x * sigma_x);
    let n2 = (n - 1) / 2;
    let mut values = vec![0.0f64; n2 + 1];
    let mut sum = 0.0f64;
    for (i, v) in values.iter_mut().enumerate().take(n2) {
        // x = i - (n-1)*0.5，步进 2（OpenCV：x = 1-n, 3-n, ...）
        let x = (2 * i + 1) as f64 - n as f64;
        let t = soft_exp(x * x * scale2);
        *v = t;
        sum += t;
    }
    sum *= 2.0;
    values[n2] = 1.0; // x = 0
    sum += 1.0;
    if n.is_multiple_of(2) {
        sum += 1.0;
    }
    let mul1 = 1.0 / sum;
    let mut result = vec![0.0f64; n];
    for i in 0..n2 {
        let t = values[i] * mul1;
        result[i] = t;
        result[n - 1 - i] = t;
    }
    result[n2] = mul1;
    if n.is_multiple_of(2) {
        result[n2 + 1] = result[n2];
    }
    result
}

/// `getGaussianKernelFixedPoint_ED`：误差扩散定点化（fractionBits=8）。
///
/// `v0 = round(k[i]·256 + err)`，err 累积；中心 = 256 - 2·Σv0（保证和恰为 256）。
/// 返回 (定点核, 中心残差)。
fn gaussian_kernel_fixed_point_ed(kernel: &[f64]) -> (Vec<u16>, f64) {
    let n = kernel.len();
    debug_assert!(n % 2 == 1);
    let fraction_multiplier = 256.0f64;
    let n2 = n / 2;
    let mut result = vec![0u16; n];
    let mut err = 0.0f64;
    let mut sum: i64 = 0;
    for i in 0..n2 {
        let adj_v = kernel[i] * fraction_multiplier + err;
        let v0 = adj_v.round(); // cvRound：round-half-away-from-zero（正值 = half-up）
        err = adj_v - v0;
        result[i] = v0 as u16;
        result[n - 1 - i] = v0 as u16;
        sum += v0 as i64;
    }
    sum *= 2;
    let adj_v_center = kernel[n2] * fraction_multiplier + err;
    let v_center = 256 - sum;
    result[n2] = v_center as u16;
    (result, adj_v_center - v_center as f64)
}

/// 生成 u8 定点高斯核（SHIFT=8，和恰为 256）。
pub fn gaussian_kernel_u8_fixed(n: usize, sigma: f64) -> Vec<u16> {
    let bitexact = gaussian_kernel_bitexact(n, sigma);
    let (fixed, _err) = gaussian_kernel_fixed_point_ed(&bitexact);
    fixed
}

/// 水平阶段（标量基准）：u8 → u16 定点（SHIFT=8）。
///
/// 对齐 `hlineSmooth<u8_t, ufixedpoint16>`（smooth.simd.hpp:1336）：
/// `dst[i] = Σ m[k]·src[i + (k - n/2)·cn]`，越界项按 border 采样
/// （CONSTANT 跳过 = 0）。u16 饱和加（ufixedpoint16 operator+）。
pub fn hline_smooth_u8(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    m: &[u16],
    border: crate::core::border::BorderPolicy<u8>,
) {
    let n = m.len();
    let total = len * cn;
    for i in 0..total {
        let mut acc: u32 = 0;
        for (k, &mk) in m.iter().enumerate() {
            let off = (k as i64) - (n as i64 / 2);
            let idx = i as i64 + off * (cn as i64);
            let v: u32 = if idx >= 0 && (idx as usize) < total {
                src[idx as usize] as u32
            } else {
                // 列方向边框：x = i/cn + off
                let x = (i / cn) as i64 + off;
                match border.interpolate(x as isize, len) {
                    Some(sx) => src[sx * cn + i % cn] as u32,
                    None => 0, // CONSTANT：越界 = 0
                }
            };
            // ufixedpoint16 * u8 = (val * v) 饱和 u16；累加也饱和 u16
            let prod = (mk as u32 * v).min(0xFFFF);
            acc = (acc + prod).min(0xFFFF);
        }
        dst[i] = acc as u16;
    }
}

/// 水平阶段首尾特判：`pre = n/2` 像素逐点按 border 采样（供 SIMD 版复用）。
///
/// 主体（`[pre·cn, (len-post+1)·cn)`）由 SIMD 内核填充后，本函数补
/// 首部 `[0, pre·cn)` 与尾部 `[(len-post+1)·cn, len·cn)`。
pub fn hline_smooth_u8_edges(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    m: &[u16],
    border: crate::core::border::BorderPolicy<u8>,
) {
    let n = m.len();
    let total = len * cn;
    let sample = |x: i64, c: usize| -> u32 {
        match border.interpolate(x as isize, len) {
            Some(sx) => src[sx * cn + c] as u32,
            None => 0,
        }
    };
    let eval = |i: usize| -> u16 {
        let mut acc: u32 = 0;
        for (k, &mk) in m.iter().enumerate() {
            let off = (k as i64) - (n as i64 / 2);
            let idx = i as i64 + off * (cn as i64);
            let v: u32 = if idx >= 0 && (idx as usize) < total {
                src[idx as usize] as u32
            } else {
                sample((i / cn) as i64 + off, i % cn)
            };
            let prod = (mk as u32 * v).min(0xFFFF);
            acc = (acc + prod).min(0xFFFF);
        }
        acc as u16
    };
    let pre = (n / 2) * cn;
    for (i, d) in dst.iter_mut().enumerate().take(pre.min(total)) {
        *d = eval(i);
    }
    let tail_start = ((len - (n - n / 2)) * cn).min(total);
    for (i, d) in dst.iter_mut().enumerate().skip(tail_start) {
        *d = eval(i);
    }
}

/// 垂直阶段（标量基准）：kylen 行 u16 定点 → u8。
///
/// 对齐 `vlineSmooth<u8_t, ufixedpoint16>`（smooth.simd.hpp:2083）：
/// `m[j]·src[j][i]` 为 ufixedpoint32（raw16×raw16，SHIFT=16 域），
/// u32 饱和累加后 `dst = saturate_u8((Σ + 2^15) >> 16)`。
pub fn vline_smooth_u8(rows: &[&[u16]], dst: &mut [u8], m: &[u16]) {
    let len = dst.len();
    for i in 0..len {
        let mut acc: u32 = 0;
        for (k, &mk) in m.iter().enumerate() {
            // ufixedpoint16 * ufixedpoint16 → ufixedpoint32（u32 饱和）
            let prod = (mk as u64 * rows[k][i] as u64).min(0xFFFF_FFFF) as u32;
            acc = acc.saturating_add(prod);
        }
        // ufixedpoint32 → u8：saturate((val + 2^15) >> 16)
        dst[i] = (((acc as u64 + (1 << 15)) >> 16).min(0xFF)) as u8;
    }
}

/// 水平阶段 121 核特化（[64,128,64]）：`dst = (a + 2b + c) << 6`。
///
/// 对齐 `hlineSmooth3N121Impl`（smooth.simd.hpp:213）：u16 域和 ≤ 1020，
/// 左移 6 后 ≤ 65280 不溢出。首尾像素按 border 采样。
pub fn hline_smooth_3n121(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    if len == 1 {
        // OpenCV：非 CONSTANT 时 dst = src（核和 = 1）；CONSTANT 时 src>>1
        for k in 0..cn {
            dst[k] = match border {
                crate::core::border::BorderPolicy::Constant(_) => (src[k] as u16) << 7,
                _ => (src[k] as u16) << 8,
            };
        }
        return;
    }
    // x=0：邻域 x=-1（border）、x=0、x=1
    for k in 0..cn {
        let a = match border.interpolate(-1, len) {
            Some(sx) => src[sx * cn + k] as u16,
            None => 0,
        };
        let b = src[k] as u16;
        let c = src[cn + k] as u16;
        dst[k] = (a + 2 * b + c) << 6;
    }
    // 主体 x ∈ [1, len-1)
    for i in cn..(total - cn) {
        let (a, b, c) = (src[i - cn] as u16, src[i] as u16, src[i + cn] as u16);
        dst[i] = (a + 2 * b + c) << 6;
    }
    // x=len-1
    for k in 0..cn {
        let idx = total - cn + k;
        let a = src[idx - cn] as u16;
        let b = src[idx] as u16;
        let c = match border.interpolate(len as isize, len) {
            Some(sx) => src[sx * cn + k] as u16,
            None => 0,
        };
        dst[idx] = (a + 2 * b + c) << 6;
    }
}

/// 垂直阶段 121 核特化：`dst = (r0 + 2r1 + r2 + 512) >> 10`。
///
/// 对齐 `vlineSmooth3N121<u8,ufixedpoint16>`（smooth.simd.hpp:1755）：
/// u32 域求和后右移 10（定点 SHIFT=8 + 核 SHIFT=2）。
pub fn vline_smooth_3n121(rows: &[&[u16]; 3], dst: &mut [u8]) {
    let len = dst.len();
    for i in 0..len {
        let v = (rows[0][i] as u32 + 2 * rows[1][i] as u32 + rows[2][i] as u32 + (1 << 9)) >> 10;
        dst[i] = v.min(0xFF) as u8;
    }
}

/// 121 核水平阶段首尾特判（供 SIMD 版复用）：x=0 与 x=len-1。
pub fn hline_smooth_3n121_edges(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    if len == 1 {
        for k in 0..cn {
            dst[k] = match border {
                crate::core::border::BorderPolicy::Constant(_) => (src[k] as u16) << 7,
                _ => (src[k] as u16) << 8,
            };
        }
        return;
    }
    for k in 0..cn {
        let a = match border.interpolate(-1, len) {
            Some(sx) => src[sx * cn + k] as u16,
            None => 0,
        };
        let b = src[k] as u16;
        let c = src[cn + k] as u16;
        dst[k] = (a + 2 * b + c) << 6;
    }
    for k in 0..cn {
        let idx = total - cn + k;
        let a = src[idx - cn] as u16;
        let b = src[idx] as u16;
        let c = match border.interpolate(len as isize, len) {
            Some(sx) => src[sx * cn + k] as u16,
            None => 0,
        };
        dst[idx] = (a + 2 * b + c) << 6;
    }
}

/// 水平阶段 14641 核特化（[16,64,96,64,16]）。
///
/// 对齐 `hlineSmooth5N14641<u8,ufixedpoint16>`（smooth.simd.hpp:946）：
/// **每项独立移位截断**（`(c>>4)·6 + (b>>2) + (d>>2) + (a>>4) + (e>>4)`），
/// 与精确加权 `(a+4b+6c+4d+e)<<4` 相差逐项截断误差——必须逐位对齐。
pub fn hline_smooth_5n14641(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let total = len * cn;
    // 主体 x ∈ [2, len-2)：(c·6 + ((b+d)<<2) + a + e) << 4
    //（u16 域：c·6 ≤ 1530，(b+d)<<2 ≤ 2040，和 ≤ 4080，<<4 ≤ 65280）
    let body_end = total.saturating_sub(2 * cn);
    for i in (2 * cn)..body_end {
        let (a, b) = (src[i - 2 * cn] as u16, src[i - cn] as u16);
        let (c, d) = (src[i] as u16, src[i + cn] as u16);
        let e = src[i + 2 * cn] as u16;
        dst[i] = (c * 6 + ((b + d) << 2) + a + e) << 4;
    }
    hline_smooth_5n14641_edges(src, dst, len, cn, border);
}

/// 14641 核水平阶段首尾特判（x ∈ {0,1,len-2,len-1}）。
pub fn hline_smooth_5n14641_edges(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let sample = |x: i64, c: usize| -> u16 {
        match border.interpolate(x as isize, len) {
            Some(sx) => src[sx * cn + c] as u16,
            None => 0,
        }
    };
    // 边框列精确公式（对齐 OpenCV：ufixedpoint16 移位 = (v<<8)>>n，无截断）：
    // dst = (a + 4(b+d) + 6c + e) << 4（与主体一致）
    for x in 0..2.min(len) {
        for c in 0..cn {
            let i = x * cn + c;
            let mut s = 0u16;
            for (k, w) in [1u16, 4, 6, 4, 1].iter().enumerate() {
                let sx = x as i64 + k as i64 - 2;
                let v: u16 = if sx >= 0 && (sx as usize) < len {
                    src[sx as usize * cn + c] as u16
                } else {
                    sample(sx, c)
                };
                s += w * v;
            }
            dst[i] = s << 4;
        }
    }
    for x in (len.saturating_sub(2))..len {
        for c in 0..cn {
            let i = x * cn + c;
            let mut s = 0u16;
            for (k, w) in [1u16, 4, 6, 4, 1].iter().enumerate() {
                let sx = x as i64 + k as i64 - 2;
                let v: u16 = if sx >= 0 && (sx as usize) < len {
                    src[sx as usize * cn + c] as u16
                } else {
                    sample(sx, c)
                };
                s += w * v;
            }
            dst[i] = s << 4;
        }
    }
}

/// 垂直阶段 14641 核特化：`dst = (r2·6 + (r1+r3)<<2 + r0 + r4 + 2^11) >> 12`。
///
/// 对齐 `vlineSmooth5N14641<u8,ufixedpoint16>`（smooth.simd.hpp:1955）。
pub fn vline_smooth_5n14641(rows: &[&[u16]; 5], dst: &mut [u8]) {
    let len = dst.len();
    for i in 0..len {
        let v = (rows[2][i] as u32 * 6
            + ((rows[1][i] as u32 + rows[3][i] as u32) << 2)
            + rows[0][i] as u32
            + rows[4][i] as u32
            + (1 << 11))
            >> 12;
        dst[i] = v.min(0xFF) as u8;
    }
}

/// 14641 二维融合（主体区）：x ∈ [2, w-2)，y ∈ [2, h-2)。
///
/// 对齐 `smooth5N14641Impl<u8,ufixedpoint16>`（smooth.simd.hpp:710）：
/// 行和 `b = 6s + 4(s[-cn]+s[cn]) + s[-2cn] + s[2cn]`（u16 域 ≤ 4080），
/// 列归约 `dst = (6b2 + 4(b1+b3) + b0 + b4 + 128) >> 8`。
/// 边框区（x/y 距边 < 2）由调用方用 hline+vline 路径处理。
pub fn smooth_5n14641<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    y_lo: usize,
    y_hi: usize,
) {
    let row_w = w * C;
    if w < 5 || h < 5 {
        return;
    }
    let cn = C;
    let y_start = y_lo.max(2);
    let y_end = y_hi.min(h - 2);
    for y in y_start..y_end {
        let dst_row = &mut dst[(y - y_lo) * row_w..(y - y_lo + 1) * row_w];
        for x in (2 * cn)..(row_w - 2 * cn) {
            // 行和 b(yy) = 6s + 4(s[-cn]+s[cn]) + s[-2cn] + s[2cn]
            let bsum = |yy: usize| -> u32 {
                let s = &src[yy * row_w + x..];
                let center = s[0] as u32;
                let near = (src[yy * row_w + x - cn] as u32 + src[yy * row_w + x + cn] as u32) << 2;
                let far = src[yy * row_w + x - 2 * cn] as u32 + src[yy * row_w + x + 2 * cn] as u32;
                center * 6 + near + far
            };
            let b0 = bsum(y - 2);
            let b1 = bsum(y - 1);
            let b2 = bsum(y);
            let b3 = bsum(y + 1);
            let b4 = bsum(y + 2);
            let v = (b2 * 6 + ((b1 + b3) << 2) + b0 + b4 + 128) >> 8;
            dst_row[x] = v.min(0xFF) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_bitexact_tables() {
        assert_eq!(gaussian_kernel_bitexact(3, 0.0), vec![0.25, 0.5, 0.25]);
        assert_eq!(
            gaussian_kernel_bitexact(5, 0.0),
            vec![0.0625, 0.25, 0.375, 0.25, 0.0625]
        );
        let k7 = gaussian_kernel_bitexact(7, 0.0);
        assert!((k7.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn kernel_fixed_sums_to_256() {
        for (n, sigma) in [
            (3, 0.0),
            (5, 0.0),
            (7, 0.0),
            (9, 0.0),
            (11, 0.0),
            (3, 1.5),
            (7, 2.0),
        ] {
            let k = gaussian_kernel_u8_fixed(n, sigma);
            assert_eq!(k.len(), n);
            let sum: u32 = k.iter().map(|&v| v as u32).sum();
            assert_eq!(sum, 256, "n={n} sigma={sigma}");
            // 对称
            for i in 0..n / 2 {
                assert_eq!(k[i], k[n - 1 - i]);
            }
        }
    }

    #[test]
    fn kernel_3_5_7_9_tables_fixed() {
        // sigma=0 查表核的定点化（2 的幂，无舍入误差）
        assert_eq!(gaussian_kernel_u8_fixed(3, 0.0), vec![64, 128, 64]);
        assert_eq!(gaussian_kernel_u8_fixed(5, 0.0), vec![16, 64, 96, 64, 16]);
    }

    #[test]
    fn hline_identity_kernel() {
        // n=1 核 [256]：恒等
        let src = [1u8, 2, 3, 4, 5, 6];
        let mut dst = [0u16; 6];
        hline_smooth_u8(
            &src,
            &mut dst,
            2,
            3,
            &[256],
            crate::core::border::BorderPolicy::Clamp,
        );
        assert_eq!(dst, [256, 512, 768, 1024, 1280, 1536]);
    }

    #[test]
    fn hline_121_kernel() {
        // [64,128,64]：dst = 64·a + 128·b + 64·c（a/b/c 为邻域采样）
        let src = [10u8, 20, 30, 40, 50, 60, 60, 50, 40]; // 3x3
        let mut dst = [0u16; 9];
        hline_smooth_u8(
            &src,
            &mut dst,
            3,
            3,
            &[64, 128, 64],
            crate::core::border::BorderPolicy::Clamp,
        );
        // x=0 通道 0：邻域 x=-1(Clamp→x0)=10, x=0=10, x=1=40
        assert_eq!(dst[0], 64 * 10 + 128 * 10 + 64 * 40);
        // x=1 通道 1：邻域 = 20, 50, 50
        assert_eq!(dst[4], 64 * 20 + 128 * 50 + 64 * 50);
        // x=2 通道 2：邻域 x=1=60, x=2=40, x=3(Clamp→x2)=40
        assert_eq!(dst[8], 64 * 60 + 128 * 40 + 64 * 40);
    }

    #[test]
    fn vline_121_kernel() {
        // [64,128,64]：dst = (64·r0 + 128·r1 + 64·r2 + 2^15) >> 16 饱和
        let r0 = [256u16; 4]; // 1.0
        let r1 = [512u16; 4]; // 2.0
        let r2 = [256u16; 4];
        let rows: [&[u16]; 3] = [&r0, &r1, &r2];
        let mut dst = [0u8; 4];
        vline_smooth_u8(&rows, &mut dst, &[64, 128, 64]);
        // acc = 64·256 + 128·512 + 64·256 = 98304（= 1.5）
        // dst = (98304 + 32768) >> 16 = 131072 >> 16 = 2（half-up）
        assert_eq!(dst, [2, 2, 2, 2]);
        // 饱和：全 255 输入
        let r = [65280u16; 4]; // 255.0
        let rows: [&[u16]; 3] = [&r, &r, &r];
        vline_smooth_u8(&rows, &mut dst, &[64, 128, 64]);
        // acc = 256·65280 = 16711680 → (16711680+32768)>>16 = 255.5 → 255（饱和）
        assert_eq!(dst, [255, 255, 255, 255]);
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg(test)]
mod isa_cross_tests {
    use super::*;

    /// 三路径交叉验证：标量 vs AVX2 vs AVX512（CPU 不支持则 skip）。
    /// DESIGN.md §2.1：AVX2 与 AVX512 内核同等测试覆盖，防止迁移到
    /// 仅支持 AVX2 的机器时暴露未测路径缺陷。
    #[test]
    fn vline_smooth_u8_cross_isa() {
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        for n in [1usize, 3, 5, 7, 9, 11, 17, 21, 57] {
            let m = gaussian_kernel_u8_fixed(n, 1.5);
            for len in [1usize, 8, 16, 17, 31, 32, 33, 64, 100, 257] {
                // 覆盖 u16 全值域段（含 ≥32768 高位段，验证 dotprod 偏置）
                let rows_data: Vec<Vec<u16>> = (0..n)
                    .map(|r| {
                        (0..len)
                            .map(|i| ((i * 997 + r * 131 + 30000) % 65536) as u16)
                            .collect()
                    })
                    .collect();
                let rows: Vec<&[u16]> = rows_data.iter().map(|r| r.as_slice()).collect();
                let mut dst_scalar = vec![0u8; len];
                vline_smooth_u8(&rows, &mut dst_scalar, &m);
                if has_avx2 {
                    let mut dst = vec![0u8; len];
                    // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                    unsafe { crate::simd_kernels::x86::avx2::vline_smooth_u8(&rows, &mut dst, &m) };
                    assert_eq!(dst_scalar, dst, "avx2 n={n} len={len}");
                }
                if has_avx512 {
                    let mut dst = vec![0u8; len];
                    // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                    unsafe {
                        crate::simd_kernels::x86::avx512::vline_smooth_u8(&rows, &mut dst, &m)
                    };
                    assert_eq!(dst_scalar, dst, "avx512 n={n} len={len}");
                }
            }
        }
    }

    #[test]
    fn hline_smooth_u8_cross_isa() {
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        for cn in [1usize, 3] {
            for w in [1usize, 5, 8, 16, 17, 32, 33, 64, 100] {
                let src: Vec<u8> = (0..w * cn).map(|i| (i * 37 % 256) as u8).collect();
                for n in [1usize, 3, 5, 7, 9, 11, 17, 21, 57] {
                    let m = gaussian_kernel_u8_fixed(n, 1.5);
                    for border in [
                        crate::core::border::BorderPolicy::Clamp,
                        crate::core::border::BorderPolicy::Reflect,
                        crate::core::border::BorderPolicy::Constant(9),
                    ] {
                        let mut dst_scalar = vec![0u16; w * cn];
                        hline_smooth_u8(&src, &mut dst_scalar, w, cn, &m, border);
                        if has_avx2 {
                            let mut dst = vec![0u16; w * cn];
                            // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx2::hline_smooth_u8(
                                    &src, &mut dst, w, cn, &m, border,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx2 w={w} cn={cn} n={n} {border:?}");
                        }
                        if has_avx512 {
                            let mut dst = vec![0u16; w * cn];
                            // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx512::hline_smooth_u8(
                                    &src, &mut dst, w, cn, &m, border,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx512 w={w} cn={cn} n={n} {border:?}");
                        }
                    }
                }
            }
        }
    }
}
