//! 中值滤波内核（对应 OpenCV `median_blur.simd.hpp`）。
//!
//! 策略对齐 OpenCV5 `medianBlur` 派发：
//! - ksize 3：SortNet（排序网络，所有类型）
//! - ksize 5：SortNet（非 u8 / 2 通道 / >4 通道）；u8 1/3/4 通道走直方图
//! - ksize > 5：直方图（u8 1/3/4 通道）
//!
//! 边界一律 `BORDER_REPLICATE`（Clamp），与 OpenCV `medianBlur` 一致。

use crate::core::border::BorderPolicy;

/// 3x3 中值滤波单行（u8，运行时派发：AVX512 → AVX2 → 标量）。
///
/// 处理第 `y` 行，`y0`/`y1`/`y2` 为三行源偏移（已 Clamp），`dst_row` 为输出行。
pub fn median_row_u8_3x3(
    src: &[u8],
    dst_row: &mut [u8],
    w: usize,
    cn: usize,
    y0: usize,
    y1: usize,
    y2: usize,
) {
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => unsafe {
            crate::simd_kernels::x86::avx512::median_row_u8_3x3(src, dst_row, w, cn, y0, y1, y2)
        },
        crate::simd_kernels::x86::Isa::Avx2 => unsafe {
            crate::simd_kernels::x86::avx2::median_row_u8_3x3(src, dst_row, w, cn, y0, y1, y2)
        },
        crate::simd_kernels::x86::Isa::Scalar => {
            let row_bytes = w * cn;
            for j in 0..row_bytes {
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
                dst_row[j] = median3(p0, p1, p2, p3, p4, p5, p6, p7, p8);
            }
        }
    }
}

/// 3x3 排序网络求中值（OpenCV `medianBlur_SortNet` m==3 标量路径）。
/// 输入 9 个窗口值，返回中值（第 5 小）。
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn median3(a0: u8, a1: u8, a2: u8, a3: u8, a4: u8, a5: u8, a6: u8, a7: u8, a8: u8) -> u8 {
    let mut p = [a0, a1, a2, a3, a4, a5, a6, a7, a8];
    // op(i,j): 使 p[i] <= p[j]
    macro_rules! op {
        ($i:expr, $j:expr) => {
            if p[$i] > p[$j] {
                p.swap($i, $j);
            }
        };
    }
    op!(1, 2);
    op!(4, 5);
    op!(7, 8);
    op!(0, 1);
    op!(3, 4);
    op!(6, 7);
    op!(1, 2);
    op!(4, 5);
    op!(7, 8);
    op!(0, 3);
    op!(5, 8);
    op!(4, 7);
    op!(3, 6);
    op!(1, 4);
    op!(2, 5);
    op!(4, 7);
    op!(4, 2);
    op!(6, 4);
    op!(4, 2);
    p[4]
}

/// 5x5 排序网络求中值（OpenCV `medianBlur_SortNet` m==5 标量分支）。
/// 25 个元素，输出中值（第 13 小，p[12]）。
#[inline]
pub fn median5(v: &[u8; 25]) -> u8 {
    let mut p = *v;
    macro_rules! op {
        ($i:expr, $j:expr) => {
            if p[$i] > p[$j] {
                let t = p[$i];
                p[$i] = p[$j];
                p[$j] = t;
            }
        };
    }
    op!(1, 2);
    op!(0, 1);
    op!(1, 2);
    op!(4, 5);
    op!(3, 4);
    op!(4, 5);
    op!(0, 3);
    op!(2, 5);
    op!(2, 3);
    op!(1, 4);
    op!(1, 2);
    op!(3, 4);
    op!(7, 8);
    op!(6, 7);
    op!(7, 8);
    op!(10, 11);
    op!(9, 10);
    op!(10, 11);
    op!(6, 9);
    op!(8, 11);
    op!(8, 9);
    op!(7, 10);
    op!(7, 8);
    op!(9, 10);
    op!(0, 6);
    op!(4, 10);
    op!(4, 6);
    op!(2, 8);
    op!(2, 4);
    op!(6, 8);
    op!(1, 7);
    op!(5, 11);
    op!(5, 7);
    op!(3, 9);
    op!(3, 5);
    op!(7, 9);
    op!(1, 2);
    op!(3, 4);
    op!(5, 6);
    op!(7, 8);
    op!(9, 10);
    op!(13, 14);
    op!(12, 13);
    op!(13, 14);
    op!(16, 17);
    op!(15, 16);
    op!(16, 17);
    op!(12, 15);
    op!(14, 17);
    op!(14, 15);
    op!(13, 16);
    op!(13, 14);
    op!(15, 16);
    op!(19, 20);
    op!(18, 19);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(21, 23);
    op!(22, 24);
    op!(22, 23);
    op!(18, 21);
    op!(20, 23);
    op!(20, 21);
    op!(19, 22);
    op!(22, 24);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(12, 18);
    op!(16, 22);
    op!(16, 18);
    op!(14, 20);
    op!(20, 24);
    op!(14, 16);
    op!(18, 20);
    op!(22, 24);
    op!(13, 19);
    op!(17, 23);
    op!(17, 19);
    op!(15, 21);
    op!(15, 17);
    op!(19, 21);
    op!(13, 14);
    op!(15, 16);
    op!(17, 18);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(0, 12);
    op!(8, 20);
    op!(8, 12);
    op!(4, 16);
    op!(16, 24);
    op!(12, 16);
    op!(2, 14);
    op!(10, 22);
    op!(10, 14);
    op!(6, 18);
    op!(6, 10);
    op!(10, 12);
    op!(1, 13);
    op!(9, 21);
    op!(9, 13);
    op!(5, 17);
    op!(13, 17);
    op!(3, 15);
    op!(11, 23);
    op!(11, 15);
    op!(7, 19);
    op!(7, 11);
    op!(11, 13);
    op!(11, 12);
    p[12]
}

/// 标量中值滤波（u8，SortNet ksize 3/5，Clamp 边界）。
///
/// `src` 为连续像素（每像素 `cn` 字节），`dst` 同尺寸。`w`/`h` 为宽高。
/// 逐像素取 `ksize x ksize` 窗口，边界 Clamp（BORDER_REPLICATE）。
pub fn median_blur_u8_sortnet(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    cn: usize,
    ksize: usize,
) {
    let r = ksize / 2;
    let row_bytes = w * cn;
    for y in 0..h {
        for x in 0..w {
            for c in 0..cn {
                let mut win = [0u8; 25];
                let mut idx = 0;
                for dy in 0..ksize {
                    let sy = (y as isize + dy as isize - r as isize).clamp(0, h as isize - 1) as usize;
                    for dx in 0..ksize {
                        let sx = (x as isize + dx as isize - r as isize).clamp(0, w as isize - 1) as usize;
                        win[idx] = src[sy * row_bytes + sx * cn + c];
                        idx += 1;
                    }
                }
                let med = if ksize == 3 {
                    median3(win[0], win[1], win[2], win[3], win[4], win[5], win[6], win[7], win[8])
                } else {
                    median5(&win)
                };
                dst[y * row_bytes + x * cn + c] = med;
            }
        }
    }
}

/// 直方图法 u8 中值滤波（OpenCV `medianBlur_8u_Om` 的简化标量版）。
///
/// 用 256 桶直方图，逐像素统计窗口内像素分布，找中值。
/// 支持多通道（cn==1/3/4）。Clamp 边界。
pub fn median_blur_u8_hist(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    cn: usize,
    ksize: usize,
) {
    let r = ksize / 2;
    let n2 = ((ksize * ksize) / 2) as u32;
    let row_bytes = w * cn;
    for y in 0..h {
        for x in 0..w {
            for c in 0..cn {
                let mut hist = [0u32; 256];
                for ky in 0..ksize {
                    let sy = (y as isize + ky as isize - r as isize).clamp(0, h as isize - 1) as usize;
                    for dx in 0..ksize {
                        let sx = (x as isize + dx as isize - r as isize).clamp(0, w as isize - 1) as usize;
                        hist[src[sy * row_bytes + sx * cn + c] as usize] += 1;
                    }
                }
                let mut sum = 0u32;
                let mut med = 0u8;
                for (v, &cnt) in hist.iter().enumerate() {
                    sum += cnt;
                    if sum > n2 {
                        med = v as u8;
                        break;
                    }
                }
                dst[y * row_bytes + x * cn + c] = med;
            }
        }
    }
}

/// 中值滤波派发（标量，全类型）。
///
/// 对齐 OpenCV `medianBlur`：
/// - ksize 3：SortNet
/// - ksize 5：SortNet（非 u8 / 2ch / >4ch）；u8 1/3/4ch 直方图
/// - ksize > 5：直方图（u8 单通道）
///
/// `T` 为像素类型，`C` 为通道数。`border` 仅支持 Clamp（BORDER_REPLICATE）。
pub fn median_blur_scalar<T: crate::core::image::Pixel + bytemuck::Pod, const C: usize>(
    src: &[T],
    dst: &mut [T],
    w: usize,
    h: usize,
    ksize: usize,
    _border: &BorderPolicy<T>,
) {
    let row_bytes = w * C;
    let is_u8 = std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>();
    let use_hist = ksize > 5 || (ksize == 5 && is_u8 && (C == 1 || C == 3 || C == 4));
    if use_hist && is_u8 && (C == 1 || C == 3 || C == 4) {
        // u8 直方图（1/3/4 通道）
        let src_b: &[u8] = bytemuck::cast_slice(src);
        let dst_b: &mut [u8] = bytemuck::cast_slice_mut(dst);
        median_blur_u8_hist(src_b, dst_b, w, h, C, ksize);
        return;
    }
    // SortNet 路径（ksize 3/5，全类型）
    for y in 0..h {
        for x in 0..w {
            for c in 0..C {
                if ksize == 3 {
                    let mut win = [T::zero(); 9];
                    let mut idx = 0;
                    for ky in 0..3 {
                        let sy = (y as isize + ky as isize - 1).clamp(0, h as isize - 1) as usize;
                        for dx in 0..3 {
                            let sx = (x as isize + dx as isize - 1).clamp(0, w as isize - 1) as usize;
                            win[idx] = src[sy * row_bytes + sx * C + c];
                            idx += 1;
                        }
                    }
                    dst[y * row_bytes + x * C + c] = sortnet3(&win);
                } else {
                    let mut win = [T::zero(); 25];
                    let mut idx = 0;
                    for ky in 0..5 {
                        let sy = (y as isize + ky as isize - 2).clamp(0, h as isize - 1) as usize;
                        for dx in 0..5 {
                            let sx = (x as isize + dx as isize - 2).clamp(0, w as isize - 1) as usize;
                            win[idx] = src[sy * row_bytes + sx * C + c];
                            idx += 1;
                        }
                    }
                    dst[y * row_bytes + x * C + c] = sortnet5(&win);
                }
            }
        }
    }
}

/// 泛型 3x3 排序网络（OpenCV `medianBlur_SortNet` m==3）。
#[inline]
fn sortnet3<T: PartialOrd + Copy>(win: &[T; 9]) -> T {
    let mut p = *win;
    macro_rules! op {
        ($i:expr, $j:expr) => {
            if p[$i] > p[$j] {
                let t = p[$i];
                p[$i] = p[$j];
                p[$j] = t;
            }
        };
    }
    op!(1, 2);
    op!(4, 5);
    op!(7, 8);
    op!(0, 1);
    op!(3, 4);
    op!(6, 7);
    op!(1, 2);
    op!(4, 5);
    op!(7, 8);
    op!(0, 3);
    op!(5, 8);
    op!(4, 7);
    op!(3, 6);
    op!(1, 4);
    op!(2, 5);
    op!(4, 7);
    op!(4, 2);
    op!(6, 4);
    op!(4, 2);
    p[4]
}

/// 泛型 5x5 排序网络（OpenCV `medianBlur_SortNet` m==5）。
#[inline]
fn sortnet5<T: crate::core::image::Pixel + Copy>(win: &[T; 25]) -> T {
    let mut p = *win;
    macro_rules! op {
        ($i:expr, $j:expr) => {
            if p[$i] > p[$j] {
                let t = p[$i];
                p[$i] = p[$j];
                p[$j] = t;
            }
        };
    }
    op!(1, 2);
    op!(0, 1);
    op!(1, 2);
    op!(4, 5);
    op!(3, 4);
    op!(4, 5);
    op!(0, 3);
    op!(2, 5);
    op!(2, 3);
    op!(1, 4);
    op!(1, 2);
    op!(3, 4);
    op!(7, 8);
    op!(6, 7);
    op!(7, 8);
    op!(10, 11);
    op!(9, 10);
    op!(10, 11);
    op!(6, 9);
    op!(8, 11);
    op!(8, 9);
    op!(7, 10);
    op!(7, 8);
    op!(9, 10);
    op!(0, 6);
    op!(4, 10);
    op!(4, 6);
    op!(2, 8);
    op!(2, 4);
    op!(6, 8);
    op!(1, 7);
    op!(5, 11);
    op!(5, 7);
    op!(3, 9);
    op!(3, 5);
    op!(7, 9);
    op!(1, 2);
    op!(3, 4);
    op!(5, 6);
    op!(7, 8);
    op!(9, 10);
    op!(13, 14);
    op!(12, 13);
    op!(13, 14);
    op!(16, 17);
    op!(15, 16);
    op!(16, 17);
    op!(12, 15);
    op!(14, 17);
    op!(14, 15);
    op!(13, 16);
    op!(13, 14);
    op!(15, 16);
    op!(19, 20);
    op!(18, 19);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(21, 23);
    op!(22, 24);
    op!(22, 23);
    op!(18, 21);
    op!(20, 23);
    op!(20, 21);
    op!(19, 22);
    op!(22, 24);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(12, 18);
    op!(16, 22);
    op!(16, 18);
    op!(14, 20);
    op!(20, 24);
    op!(14, 16);
    op!(18, 20);
    op!(22, 24);
    op!(13, 19);
    op!(17, 23);
    op!(17, 19);
    op!(15, 21);
    op!(15, 17);
    op!(19, 21);
    op!(13, 14);
    op!(15, 16);
    op!(17, 18);
    op!(19, 20);
    op!(21, 22);
    op!(23, 24);
    op!(0, 12);
    op!(8, 20);
    op!(8, 12);
    op!(4, 16);
    op!(16, 24);
    op!(12, 16);
    op!(2, 14);
    op!(10, 22);
    op!(10, 14);
    op!(6, 18);
    op!(6, 10);
    op!(10, 12);
    op!(1, 13);
    op!(9, 21);
    op!(9, 13);
    op!(5, 17);
    op!(13, 17);
    op!(3, 15);
    op!(11, 23);
    op!(11, 15);
    op!(7, 19);
    op!(7, 11);
    op!(11, 13);
    op!(11, 12);
    p[12]
}
#[cfg(test)]
mod isa_cross_tests {
    use super::*;

    fn make_img(w: usize, h: usize, cn: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * cn];
        for (i, x) in v.iter_mut().enumerate() {
            *x = (i * 7 + 13) as u8;
        }
        v
    }

    #[test]
    fn median_3x3_scalar_vs_avx2_vs_avx512() {
        let (w, h, cn) = (64usize, 32usize, 1usize);
        let src = make_img(w, h, cn);
        let mut dst_scalar = vec![0u8; w * h * cn];
        let mut dst_avx2 = vec![0u8; w * h * cn];
        let mut dst_avx512 = vec![0u8; w * h * cn];

        median_blur_u8_sortnet(&src, &mut dst_scalar, w, h, cn, 3);

        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                unsafe {
                    crate::simd_kernels::x86::avx2::median_blur_u8_3x3(
                        &src, &mut dst_avx2, w, h, cn,
                    );
                }
                assert_eq!(dst_scalar, dst_avx2, "AVX2 mismatch");
            }
            if std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512bw")
                && std::arch::is_x86_feature_detected!("avx512vl")
            {
                unsafe {
                    crate::simd_kernels::x86::avx512::median_blur_u8_3x3(
                        &src, &mut dst_avx512, w, h, cn,
                    );
                }
                assert_eq!(dst_scalar, dst_avx512, "AVX512 mismatch");
            }
        }
    }

    /// 多宽度/多通道 cross-test：覆盖尾部回退的各种余数（0..nlanes-1）、
    /// 窄行全标量路径（row_bytes < nlanes+cn）与 cn=1/3/4。
    #[test]
    fn median_3x3_multi_width_scalar_vs_avx2_vs_avx512() {
        #[cfg(target_arch = "x86_64")]
        {
            let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
            let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512bw")
                && std::arch::is_x86_feature_detected!("avx512vl");
            // w 覆盖：1（单列）、2..8（窄行）、9..40（跨 32/64 批界 + 各种余数）、
            // 65/66（跨 64 批界）、100（多批 + 回退）
            let widths = [
                1usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 20, 21, 30, 31, 32, 33, 34, 35,
                40, 63, 64, 65, 66, 100,
            ];
            for cn in [1usize, 3, 4] {
                for &w in &widths {
                    let h = 5usize;
                    let src = make_img(w, h, cn);
                    let mut dst_scalar = vec![0u8; w * h * cn];
                    median_blur_u8_sortnet(&src, &mut dst_scalar, w, h, cn, 3);
                    if has_avx2 {
                        let mut dst = vec![0u8; w * h * cn];
                        // SAFETY: has_avx2 已验证 CPU 支持；长度由构造保证
                        unsafe {
                            crate::simd_kernels::x86::avx2::median_blur_u8_3x3(
                                &src, &mut dst, w, h, cn,
                            );
                        }
                        assert_eq!(dst_scalar, dst, "AVX2 w={w} cn={cn}");
                    }
                    if has_avx512 {
                        let mut dst = vec![0u8; w * h * cn];
                        // SAFETY: has_avx512 已验证 CPU 支持；长度由构造保证
                        unsafe {
                            crate::simd_kernels::x86::avx512::median_blur_u8_3x3(
                                &src, &mut dst, w, h, cn,
                            );
                        }
                        assert_eq!(dst_scalar, dst, "AVX512 w={w} cn={cn}");
                    }
                }
            }
        }
    }

    #[test]
    fn median_3x3_row_scalar_vs_avx2_vs_avx512() {
        let (w, h, cn) = (64usize, 5usize, 3usize);
        let src = make_img(w, h, cn);
        let row_bytes = w * cn;
        let mut dst_scalar = vec![0u8; row_bytes];
        let mut dst_avx2 = vec![0u8; row_bytes];
        let mut dst_avx512 = vec![0u8; row_bytes];
        let y = 2usize;
        let y0 = y.saturating_sub(1) * row_bytes;
        let y1 = y * row_bytes;
        let y2 = (y + 1).min(h - 1) * row_bytes;

        median_row_u8_3x3_scalar(&src, &mut dst_scalar, w, cn, y0, y1, y2);

        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                unsafe {
                    crate::simd_kernels::x86::avx2::median_row_u8_3x3(
                        &src, &mut dst_avx2, w, cn, y0, y1, y2,
                    );
                }
                assert_eq!(dst_scalar, dst_avx2, "AVX2 row mismatch");
            }
            if std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512bw")
                && std::arch::is_x86_feature_detected!("avx512vl")
            {
                unsafe {
                    crate::simd_kernels::x86::avx512::median_row_u8_3x3(
                        &src, &mut dst_avx512, w, cn, y0, y1, y2,
                    );
                }
                assert_eq!(dst_scalar, dst_avx512, "AVX512 row mismatch");
            }
        }
    }

    #[allow(dead_code)]
    fn median_blur_u8_scalar(src: &[u8], dst: &mut [u8], w: usize, h: usize, cn: usize, ksize: usize) {
        let row_bytes = w * cn;
        let r = ksize / 2;
        for y in 0..h {
            for x in 0..w {
                for c in 0..cn {
                    let mut win = [0u8; 9];
                    let mut idx = 0;
                    for ky in 0..ksize {
                        let sy = (y as isize + ky as isize - r as isize).clamp(0, h as isize - 1) as usize;
                        for dx in 0..ksize {
                            let sx = (x as isize + dx as isize - r as isize).clamp(0, w as isize - 1) as usize;
                            win[idx] = src[sy * row_bytes + sx * cn + c];
                            idx += 1;
                        }
                    }
                    dst[y * row_bytes + x * cn + c] = median3(
                        win[0], win[1], win[2], win[3], win[4], win[5], win[6], win[7], win[8],
                    );
                }
            }
        }
    }

    fn median_row_u8_3x3_scalar(
        src: &[u8],
        dst_row: &mut [u8],
        w: usize,
        cn: usize,
        y0: usize,
        y1: usize,
        y2: usize,
    ) {
        let row_bytes = w * cn;
        for j in 0..row_bytes {
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
            dst_row[j] = median3(p0, p1, p2, p3, p4, p5, p6, p7, p8);
        }
    }
}
