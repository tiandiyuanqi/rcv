//! 阈值化（对应 OpenCV `modules/imgproc` 的 `threshold`，thresh.cpp）。
//!
//! 公开 API 一律 RGB（铁律 6）；`threshold` 逐通道独立处理，通道数任意。

use crate::core::border::BorderPolicy;
use crate::core::image::{Image, Pixel};
use crate::imgproc::filter::box_filter;

/// 阈值化类型（对应 OpenCV `THRESH_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdType {
    /// `src > thresh ? maxval : 0`
    Binary,
    /// `src <= thresh ? maxval : 0`
    BinaryInv,
    /// `min(src, thresh)`
    Trunc,
    /// `src > thresh ? src : 0`
    ToZero,
    /// `src <= thresh ? src : 0`
    ToZeroInv,
}

/// 阈值化（对应 OpenCV `cv::threshold`）。
///
/// 返回 `(实际阈值, 输出图)`。实际阈值：整数类型为 `floor(thresh)`，浮点类型为输入 `thresh`。
///
/// 与 OpenCV 逐像素一致，含边界情形（`thresh` 越界时整图置常量或复制）。
/// 复杂度 O(width·height·C)。
pub fn threshold<T: Thresholdable + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    thresh: f64,
    maxval: f64,
    type_: ThresholdType,
) -> (f64, Image<T, C>) {
    let (t, mv, ret) = T::params(thresh, maxval, type_);
    // 所有分支（Set/Copy/逐像素）均全量覆写输出，可用未初始化缓冲
    let mut out = Image::new_uninit(src.width(), src.height());
    match T::edge_case(thresh, mv, type_) {
        Some(EdgeCase::Set(v)) => {
            for px in out.pixels_mut() {
                px.fill(v);
            }
        }
        Some(EdgeCase::Copy) => {
            for (dst, s) in out.pixels_mut().zip(src.pixels()) {
                dst.copy_from_slice(s);
            }
        }
        None => {
            // u8 特派：SIMD 派发（AVX512 → AVX2 → 标量查找表）+ rayon 行块并行
            if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
                let ty = match type_ {
                    ThresholdType::Binary => 0u8,
                    ThresholdType::BinaryInv => 1,
                    ThresholdType::Trunc => 2,
                    ThresholdType::ToZero => 3,
                    ThresholdType::ToZeroInv => 4,
                };
                let (w, h) = (src.width(), src.height());
                // 此分支 T == u8（TypeId 已验证），按位转换参数
                let t: u8 = bytemuck::cast(t);
                let mv: u8 = bytemuck::cast(mv);
                let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
                let dst_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
                let total = w * h * C;
                if total > 0 {
                    // 行块并行：带宽型用例用大块（64 行 = 64KB）减少调度开销、
                    // 提升 L3 局部性（实测 16 行块在 MT 下损失 ~2x 带宽）
                    const MIN_ROWS_PER_BLOCK: usize = 64;
                    let rows = h.max(1);
                    let chunk = MIN_ROWS_PER_BLOCK.min(rows);
                    let row_bytes = w * C;
                    // 块数不超过并行度：过订阅（64 块挤 16 核）反而损失带宽
                    let max_blocks = rayon::current_num_threads().max(1);
                    let nblocks = dst_bytes.len().div_ceil(row_bytes * chunk).min(max_blocks);
                    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
                        // 单块或单线程池：直接串行，避免任务分发开销
                        crate::simd_kernels::threshold_u8(src_bytes, dst_bytes, total, t, mv, ty);
                    } else if dst_bytes.len().is_multiple_of(nblocks) {
                        // 带宽型小任务：自旋池（rayon work-stealing 的 fork-join
                        // ~33us/次超过 1MB 级任务的计算时间，MT 反而慢于 ST）
                        let ctx = t as usize | ((mv as usize) << 8) | ((ty as usize) << 16);
                        crate::simd_kernels::spin_pool::run_blocks(
                            src_bytes,
                            dst_bytes,
                            nblocks,
                            crate::simd_kernels::threshold_block,
                            ctx,
                        );
                    } else {
                        use rayon::prelude::*;
                        dst_bytes
                            .par_chunks_mut(row_bytes * chunk)
                            .enumerate()
                            .for_each(|(i, d)| {
                                let start = i * row_bytes * chunk;
                                let n = d.len();
                                crate::simd_kernels::threshold_u8(
                                    &src_bytes[start..start + n],
                                    d,
                                    n,
                                    t,
                                    mv,
                                    ty,
                                );
                            });
                    }
                }
                return (ret, out);
            }
            for (dst, s) in out.pixels_mut().zip(src.pixels()) {
                for (d, sv) in dst.iter_mut().zip(s.iter()) {
                    *d = apply_pixel(*sv, t, mv, type_);
                }
            }
        }
    }
    (ret, out)
}

/// 逐像素阈值公式。
fn apply_pixel<T: Pixel>(v: T, t: T, mv: T, type_: ThresholdType) -> T {
    match type_ {
        ThresholdType::Binary => {
            if v > t {
                mv
            } else {
                T::zero()
            }
        }
        ThresholdType::BinaryInv => {
            if v <= t {
                mv
            } else {
                T::zero()
            }
        }
        ThresholdType::Trunc => {
            if v < t {
                v
            } else {
                t
            }
        }
        ThresholdType::ToZero => {
            if v > t {
                v
            } else {
                T::zero()
            }
        }
        ThresholdType::ToZeroInv => {
            if v <= t {
                v
            } else {
                T::zero()
            }
        }
    }
}

/// 自适应阈值均值计算方式（对应 OpenCV `ADAPTIVE_THRESH_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveMethod {
    /// 邻域均值（`boxFilter`，normalize）。
    MeanC,
    /// 邻域高斯加权均值（`GaussianBlur`）。
    GaussianC,
}

/// 自适应阈值化（对应 OpenCV `cv::adaptiveThreshold`）。
///
/// 仅支持单通道 u8。`type_` 仅支持 [`ThresholdType::Binary`] /
/// [`ThresholdType::BinaryInv`]。`block_size` 必须为奇数且 > 1。
///
/// 与 OpenCV 逐像素一致：`src - mean > -idelta ? maxval : 0`（Binary），
/// 其中 `idelta = ceil(delta)`（Binary）/ `floor(delta)`（BinaryInv）。
///
/// # Panics
/// `block_size` 非奇数或 ≤ 1，或 `type_` 非 Binary/BinaryInv。
pub fn adaptive_threshold(
    src: &Image<u8, 1>,
    max_value: f64,
    method: AdaptiveMethod,
    type_: ThresholdType,
    block_size: usize,
    delta: f64,
) -> Image<u8, 1> {
    assert!(
        block_size % 2 == 1 && block_size > 1,
        "block_size must be odd and > 1"
    );
    assert!(
        matches!(type_, ThresholdType::Binary | ThresholdType::BinaryInv),
        "adaptive_threshold only supports Binary/BinaryInv"
    );

    if max_value < 0.0 {
        return Image::new(src.width(), src.height());
    }

    let imax = u8::from_f32(max_value as f32);
    let idelta = match type_ {
        ThresholdType::Binary => delta.ceil() as i32,
        ThresholdType::BinaryInv => delta.floor() as i32,
        _ => unreachable!(),
    };

    let mean = match method {
        AdaptiveMethod::MeanC => box_filter(
            src,
            (block_size, block_size),
            (-1, -1),
            true,
            BorderPolicy::Clamp,
        ),
        // 对齐 thresh.cpp:1936：GaussianC 走 f32 路径
        // （convertTo(CV_32F) → sepFilter2D f32 → saturate_cast<uchar>），
        // 不是 u8 定点 gaussian
        AdaptiveMethod::GaussianC => crate::imgproc::filter::gaussian_blur_f32_bitexact(
            src,
            (block_size, block_size),
            0.0,
        ),
    };

    let mut out = Image::new(src.width(), src.height());
    let n = src.width() * src.height();
    // SIMD + rayon 行块并行（带宽型：src/mean/dst 各 1 字节/像素）。
    // 块大小 16 行（实测 32/64 行块在 MT 下因 L2 冲突损失带宽，16 行最优）。
    // 不用 spin_pool：adaptive 的 mean 由 box_filter（rayon）先算，若尾循环
    // 再切 spin_pool 会与 rayon 池 127 线程抢核（实测 tail 崩到 ~1500us）。
    {
        let src_bytes: &[u8] = src.data();
        let mean_bytes: &[u8] = mean.data();
        let dst_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        let inv = type_ == ThresholdType::BinaryInv;
        use rayon::prelude::*;
        const MIN_ROWS_PER_BLOCK: usize = 16;
        let chunk = MIN_ROWS_PER_BLOCK.min(src.height().max(1));
        let nblocks = n.div_ceil(src.width() * chunk);
        if nblocks <= 1 || rayon::current_num_threads() <= 1 {
            crate::simd_kernels::adaptive_thresh_u8(
                src_bytes, mean_bytes, dst_bytes, n, imax, idelta, inv,
            );
        } else {
            dst_bytes
                .par_chunks_mut(src.width() * chunk)
                .enumerate()
                .for_each(|(bi, d)| {
                    let start = bi * src.width() * chunk;
                    let cnt = d.len();
                    crate::simd_kernels::adaptive_thresh_u8(
                        &src_bytes[start..start + cnt],
                        &mean_bytes[start..start + cnt],
                        d,
                        cnt,
                        imax,
                        idelta,
                        inv,
                    );
                });
        }
    }
    out
}

/// 边界情形：`thresh` 越界时 OpenCV 整图置常量或复制。
/// 内部实现细节，公开以支持 `Thresholdable` 的泛型约束。
pub enum EdgeCase<T> {
    /// 整图置为常量。
    Set(T),
    /// 整图复制源。
    Copy,
}

/// 阈值化的类型相关参数转换（内部实现细节，公开以支持泛型约束）。
pub trait Thresholdable: Pixel + bytemuck::Pod {
    /// 将 `thresh`/`maxval` 转为原生类型，返回 `(thresh, maxval, 实际阈值)`。
    fn params(thresh: f64, maxval: f64, type_: ThresholdType) -> (Self, Self, f64);
    /// 判断 `thresh` 越界时的整图处理；`None` 表示无边界情形。
    fn edge_case(thresh: f64, maxval: Self, type_: ThresholdType) -> Option<EdgeCase<Self>>;
}

impl Thresholdable for u8 {
    fn params(thresh: f64, maxval: f64, type_: ThresholdType) -> (u8, u8, f64) {
        let ithresh = thresh.floor();
        let imaxval = if type_ == ThresholdType::Trunc {
            ithresh
        } else {
            maxval.round()
        };
        (ithresh as u8, imaxval.clamp(0.0, 255.0) as u8, ithresh)
    }

    fn edge_case(thresh: f64, maxval: u8, type_: ThresholdType) -> Option<EdgeCase<u8>> {
        let ithresh = thresh.floor();
        if !(0.0..255.0).contains(&ithresh) {
            let applies = matches!(type_, ThresholdType::Binary | ThresholdType::BinaryInv)
                || (matches!(type_, ThresholdType::Trunc | ThresholdType::ToZeroInv)
                    && ithresh < 0.0)
                || (matches!(type_, ThresholdType::ToZero) && ithresh >= 255.0);
            if applies {
                let v = match type_ {
                    ThresholdType::Binary if ithresh >= 255.0 => 0,
                    ThresholdType::Binary => maxval,
                    ThresholdType::BinaryInv if ithresh >= 255.0 => maxval,
                    ThresholdType::BinaryInv => 0,
                    _ => 0,
                };
                Some(EdgeCase::Set(v))
            } else {
                Some(EdgeCase::Copy)
            }
        } else {
            None
        }
    }
}

impl Thresholdable for u16 {
    fn params(thresh: f64, maxval: f64, type_: ThresholdType) -> (u16, u16, f64) {
        let ithresh = thresh.floor();
        let imaxval = if type_ == ThresholdType::Trunc {
            ithresh
        } else {
            maxval.round()
        };
        (ithresh as u16, imaxval.clamp(0.0, 65535.0) as u16, ithresh)
    }

    fn edge_case(thresh: f64, maxval: u16, type_: ThresholdType) -> Option<EdgeCase<u16>> {
        let ithresh = thresh.floor();
        if !(0.0..65535.0).contains(&ithresh) {
            let applies = matches!(type_, ThresholdType::Binary | ThresholdType::BinaryInv)
                || (matches!(type_, ThresholdType::Trunc | ThresholdType::ToZeroInv)
                    && ithresh < 0.0)
                || (matches!(type_, ThresholdType::ToZero) && ithresh >= 65535.0);
            if applies {
                let v = match type_ {
                    ThresholdType::Binary if ithresh >= 65535.0 => 0,
                    ThresholdType::Binary => maxval,
                    ThresholdType::BinaryInv if ithresh >= 65535.0 => maxval,
                    ThresholdType::BinaryInv => 0,
                    _ => 0,
                };
                Some(EdgeCase::Set(v))
            } else {
                Some(EdgeCase::Copy)
            }
        } else {
            None
        }
    }
}

impl Thresholdable for f32 {
    fn params(thresh: f64, maxval: f64, _type_: ThresholdType) -> (f32, f32, f64) {
        (thresh as f32, maxval as f32, thresh)
    }

    fn edge_case(_thresh: f64, _maxval: f32, _type_: ThresholdType) -> Option<EdgeCase<f32>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_basic() {
        let img = Image::<u8, 1>::from_fn(4, 1, |x, _| [x as u8 * 50]);
        // [0, 50, 100, 150], thresh=100, maxval=255
        let (ret, out) = threshold(&img, 100.0, 255.0, ThresholdType::Binary);
        assert_eq!(ret, 100.0);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(2, 0).unwrap(), &[0]); // 100 > 100? no
        assert_eq!(out.pixel(3, 0).unwrap(), &[255]);
    }

    #[test]
    fn binary_inv() {
        let img = Image::<u8, 1>::from_fn(4, 1, |x, _| [x as u8 * 50]);
        let (_, out) = threshold(&img, 100.0, 255.0, ThresholdType::BinaryInv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[255]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[255]);
        assert_eq!(out.pixel(2, 0).unwrap(), &[255]); // 100 <= 100
        assert_eq!(out.pixel(3, 0).unwrap(), &[0]);
    }

    #[test]
    fn trunc() {
        let img = Image::<u8, 1>::from_fn(4, 1, |x, _| [x as u8 * 50]);
        let (_, out) = threshold(&img, 100.0, 255.0, ThresholdType::Trunc);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[50]);
        assert_eq!(out.pixel(2, 0).unwrap(), &[100]);
        assert_eq!(out.pixel(3, 0).unwrap(), &[100]);
    }

    #[test]
    fn to_zero() {
        let img = Image::<u8, 1>::from_fn(4, 1, |x, _| [x as u8 * 50]);
        let (_, out) = threshold(&img, 100.0, 255.0, ThresholdType::ToZero);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(2, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(3, 0).unwrap(), &[150]);
    }

    #[test]
    fn to_zero_inv() {
        let img = Image::<u8, 1>::from_fn(4, 1, |x, _| [x as u8 * 50]);
        let (_, out) = threshold(&img, 100.0, 255.0, ThresholdType::ToZeroInv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[50]);
        assert_eq!(out.pixel(2, 0).unwrap(), &[100]);
        assert_eq!(out.pixel(3, 0).unwrap(), &[0]);
    }

    #[test]
    fn edge_thresh_above_max() {
        // thresh >= 255: Binary -> all 0, BinaryInv -> all maxval
        let img = Image::<u8, 1>::from_fn(2, 1, |x, _| [if x == 0 { 10 } else { 200 }]);
        let (_, out) = threshold(&img, 300.0, 255.0, ThresholdType::Binary);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[0]);

        let (_, out) = threshold(&img, 300.0, 255.0, ThresholdType::BinaryInv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[255]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[255]);
    }

    #[test]
    fn edge_thresh_below_min() {
        // thresh < 0: Binary -> all maxval, BinaryInv -> all 0
        let img = Image::<u8, 1>::from_fn(2, 1, |x, _| [if x == 0 { 10 } else { 200 }]);
        let (_, out) = threshold(&img, -5.0, 255.0, ThresholdType::Binary);
        assert_eq!(out.pixel(0, 0).unwrap(), &[255]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[255]);

        let (_, out) = threshold(&img, -5.0, 255.0, ThresholdType::BinaryInv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[0]);
    }

    #[test]
    fn edge_trunc_below_min_sets_zero() {
        // Trunc with thresh < 0: min(src, negative) clamped to 0 -> whole image 0
        let img = Image::<u8, 1>::from_fn(2, 1, |x, _| [if x == 0 { 10 } else { 200 }]);
        let (_, out) = threshold(&img, -5.0, 255.0, ThresholdType::Trunc);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
        assert_eq!(out.pixel(1, 0).unwrap(), &[0]);
    }

    #[test]
    fn multi_channel() {
        let img = Image::<u8, 3>::from_pixel(1, 1, [10, 200, 100]);
        let (_, out) = threshold(&img, 50.0, 255.0, ThresholdType::Binary);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0, 255, 255]);
    }

    #[test]
    fn f32_no_rounding() {
        let img = Image::<f32, 1>::from_pixel(1, 1, [0.5]);
        let (ret, out) = threshold(&img, 0.3, 1.0, ThresholdType::Binary);
        assert_eq!(ret, 0.3);
        assert_eq!(out.pixel(0, 0).unwrap(), &[1.0]);
    }

    #[test]
    fn empty_image() {
        let img = Image::<u8, 1>::new(0, 0);
        let (_, out) = threshold(&img, 100.0, 255.0, ThresholdType::Binary);
        assert!(out.is_empty());
    }

    #[test]
    fn adaptive_mean_c_constant_image() {
        // 全 100 的图，3x3 均值 = 100，delta=0，Binary：src-mean=0 > 0? no -> 0
        let img = Image::<u8, 1>::from_pixel(3, 3, [100]);
        let out = adaptive_threshold(
            &img,
            255.0,
            AdaptiveMethod::MeanC,
            ThresholdType::Binary,
            3,
            0.0,
        );
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[0]);
            }
        }
    }

    #[test]
    fn adaptive_mean_c_brighter_center() {
        // 中心亮于邻域 -> Binary 输出 maxval
        let img =
            Image::<u8, 1>::from_fn(3, 3, |x, y| if x == 1 && y == 1 { [200] } else { [100] });
        let out = adaptive_threshold(
            &img,
            255.0,
            AdaptiveMethod::MeanC,
            ThresholdType::Binary,
            3,
            0.0,
        );
        // 中心 200 > 均值(约 111) -> 255
        assert_eq!(out.pixel(1, 1).unwrap(), &[255]);
        // 角落 100 < 均值 -> 0
        assert_eq!(out.pixel(0, 0).unwrap(), &[0]);
    }

    #[test]
    fn adaptive_max_value_negative_all_zero() {
        let img = Image::<u8, 1>::from_pixel(3, 3, [100]);
        let out = adaptive_threshold(
            &img,
            -1.0,
            AdaptiveMethod::MeanC,
            ThresholdType::Binary,
            3,
            0.0,
        );
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[0]);
            }
        }
    }

    #[test]
    #[should_panic]
    fn adaptive_even_block_size_panics() {
        let img = Image::<u8, 1>::from_pixel(3, 3, [100]);
        let _ = adaptive_threshold(
            &img,
            255.0,
            AdaptiveMethod::MeanC,
            ThresholdType::Binary,
            4,
            0.0,
        );
    }
}
