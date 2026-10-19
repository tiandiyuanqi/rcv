//! 直方图计算与均衡化（对齐 OpenCV 5 `modules/imgproc/src/histogram.cpp`）。
//!
//! 支持 8U 单通道图像的直方图计算（均匀 bin、可选 mask）与直方图均衡化。

use crate::core::image::Image;
use crate::simd_kernels::histogram as simd_hist;

/// 计算单通道 8U 图像的直方图（对齐 `cv::calcHist` 单图像单通道均匀 bin 情形）。
///
/// `hist_size` 为 bin 数，`range` 为 `[low, high)` 值域。输出 `Vec<f32>`（长度
/// `hist_size`）。`mask` 非空时仅统计 mask 非零像素。
///
/// 位级对齐：bin 索引 = `(v - low) * hist_size / (high - low)`，越界值忽略。
/// 用查找表（f64，与 OpenCV `calcHistLookupTables_8u` 一致）避免逐像素浮点。
pub fn calc_hist(
    src: &Image<u8, 1>,
    hist_size: usize,
    range: (f32, f32),
    mask: Option<&Image<u8, 1>>,
) -> Vec<f32> {
    assert!(hist_size > 0, "hist_size must be > 0");
    assert!(range.1 > range.0, "range must be [low, high) with high > low");
    let (low, high) = range;
    let w = src.width();
    let h = src.height();
    let sdata = src.data();
    let n = w * h;

    // 构建查找表：像素值 → bin 偏移（越界为 usize::MAX）
    // 与 OpenCV `calcHistLookupTables_8u` 一致（f64 计算）
    let a = hist_size as f64 / (high - low) as f64;
    let b = -a * low as f64;
    let mut tab = [usize::MAX; 256];
    for j in 0..256 {
        let jf = j as f64;
        if jf >= low as f64 && jf < high as f64 {
            let idx = (jf * a + b).floor() as i32;
            let idx = idx.clamp(0, hist_size as i32 - 1) as usize;
            tab[j] = idx;
        }
    }

    if let Some(m) = mask {
        // 带 mask：逐像素判断
        let mdata = m.data();
        let mut hist = vec![0i32; hist_size];
        for i in 0..n {
            if mdata[i] != 0 {
                let hidx = tab[sdata[i] as usize];
                if hidx != usize::MAX {
                    hist[hidx] += 1;
                }
            }
        }
        hist.into_iter().map(|v| v as f32).collect()
    } else {
        // 无 mask：SIMD 直方图
        let mut hist = vec![0i32; hist_size];
        simd_hist::calc_hist_8u(sdata, n, &mut hist, &tab);
        hist.into_iter().map(|v| v as f32).collect()
    }
}

/// 直方图均衡化（对齐 `cv::equalizeHist`，8U 单通道）。
///
/// 算法：计算 256-bin 直方图 → 累积分布 → LUT 映射。
/// 位级对齐 OpenCV：`scale = 255 / (total - hist[first_nonzero])`，
/// `lut[first_nonzero] = 0`，`lut[j] = round(sum * scale)`（RNE）。
pub fn equalize_hist(src: &Image<u8, 1>) -> Image<u8, 1> {
    let w = src.width();
    let h = src.height();
    let total = w * h;
    let sdata = src.data();

    // 1. 计算 256-bin 直方图（SIMD，恒等查找表）
    let mut hist = [0i32; 256];
    let mut tab = [0usize; 256];
    for (j, t) in tab.iter_mut().enumerate() {
        *t = j;
    }
    simd_hist::calc_hist_8u(sdata, total, &mut hist, &tab);

    // 2. 找到第一个非零 bin
    let mut i = 0;
    while i < 256 && hist[i] == 0 {
        i += 1;
    }
    if i == 256 {
        // 全零图像
        return Image::<u8, 1>::new(w, h);
    }

    // 3. 如果所有像素同值，直接填充该值
    if hist[i] as usize == total {
        return Image::<u8, 1>::from_pixel(w, h, [i as u8]);
    }

    // 4. 构建 LUT
    let mut lut = [0u8; 256];
    let scale = 255.0f32 / (total - hist[i] as usize) as f32;
    let mut sum = 0u32;
    lut[i] = 0;
    for j in (i + 1)..256 {
        sum += hist[j] as u32;
        lut[j] = (sum as f32 * scale).round_ties_even() as u8;
    }

    // 5. 应用 LUT（SIMD）
    let mut dst = Image::<u8, 1>::new(w, h);
    simd_hist::apply_lut(sdata, dst.data_mut(), total, &lut);
    dst
}

/// 直方图比较方法（对齐 OpenCV `HistCompMethods`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistCompMethod {
    /// 相关性（`HISTCMP_CORREL`）
    Correl,
    /// 卡方（`HISTCMP_CHISQR`）
    ChiSqr,
    /// 交集（`HISTCMP_INTERSECT`）
    Intersect,
    /// Bhattacharyya 距离（`HISTCMP_BHATTACHARYYA`）
    Bhattacharyya,
    /// 替代卡方（`HISTCMP_CHISQR_ALT`）
    ChiSqrAlt,
    /// KL 散度（`HISTCMP_KL_DIV`）
    KlDiv,
}

/// 2-lane f64 归约（模拟 OpenCV SSE `v_reduce_sum`：`a0 + a1`）。
#[inline]
fn reduce2(a: [f64; 2]) -> f64 {
    a[0] + a[1]
}

/// 比较两个直方图（对齐 `cv::compareHist`，CV_32F）。
///
/// `h1`/`h2` 为等长 `f32` 直方图。返回 `f64` 距离/相似度（依方法而定）。
/// 位级对齐 OpenCV：累加用 `f64` 2-lane 并行（模拟 OpenCV SSE SIMD 路径的求和顺序），
/// `DBL_EPSILON`/`FLT_EPSILON` 阈值一致。
pub fn compare_hist(h1: &[f32], h2: &[f32], method: HistCompMethod) -> f64 {
    assert_eq!(h1.len(), h2.len(), "histograms must have equal length");
    let m = match method {
        HistCompMethod::Correl => simd_hist::HistCompMethod::Correl,
        HistCompMethod::ChiSqr => simd_hist::HistCompMethod::ChiSqr,
        HistCompMethod::Intersect => simd_hist::HistCompMethod::Intersect,
        HistCompMethod::Bhattacharyya => simd_hist::HistCompMethod::Bhattacharyya,
        HistCompMethod::ChiSqrAlt => simd_hist::HistCompMethod::ChiSqrAlt,
        HistCompMethod::KlDiv => simd_hist::HistCompMethod::KlDiv,
    };
    simd_hist::compare_hist(h1, h2, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calc_hist_basic() {
        let mut img = Image::<u8, 1>::new(4, 1);
        img.data_mut().copy_from_slice(&[0, 64, 128, 255]);
        let hist = calc_hist(&img, 4, (0.0, 256.0), None);
        // bin 0: [0,64), bin 1: [64,128), bin 2: [128,192), bin 3: [192,256)
        assert_eq!(hist, vec![1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn calc_hist_with_mask() {
        let mut img = Image::<u8, 1>::new(4, 1);
        img.data_mut().copy_from_slice(&[0, 64, 128, 255]);
        let mut mask = Image::<u8, 1>::new(4, 1);
        mask.data_mut().copy_from_slice(&[1, 0, 1, 0]);
        let hist = calc_hist(&img, 4, (0.0, 256.0), Some(&mask));
        assert_eq!(hist, vec![1.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn calc_hist_out_of_range() {
        let mut img = Image::<u8, 1>::new(4, 1);
        img.data_mut().copy_from_slice(&[0, 64, 128, 255]);
        // range [0, 128)：128 和 255 越界
        let hist = calc_hist(&img, 2, (0.0, 128.0), None);
        assert_eq!(hist, vec![1.0, 1.0]);
    }

    #[test]
    fn equalize_hist_constant() {
        // 全同值图像：输出全为该值
        let mut img = Image::<u8, 1>::new(4, 4);
        img.data_mut().fill(100);
        let dst = equalize_hist(&img);
        assert!(dst.data().iter().all(|&v| v == 100));
    }

    #[test]
    fn equalize_hist_black() {
        // 全零图像：输出全零
        let img = Image::<u8, 1>::new(4, 4);
        let dst = equalize_hist(&img);
        assert!(dst.data().iter().all(|&v| v == 0));
    }

    #[test]
    fn equalize_hist_spread() {
        // 两值图像：0 和 255，各一半
        let mut img = Image::<u8, 1>::new(4, 1);
        img.data_mut().copy_from_slice(&[0, 0, 255, 255]);
        let dst = equalize_hist(&img);
        // hist[0]=2, hist[255]=2, total=4
        // i=0, hist[0]=2 != 4
        // scale = 255/(4-2) = 127.5
        // lut[0]=0, sum=0
        // j=1..254: sum=0, lut=0
        // j=255: sum=2, lut=round(2*127.5)=round(255)=255
        assert_eq!(dst.data(), &[0, 0, 255, 255]);
    }

    #[test]
    fn equalize_hist_gradient() {
        // 渐变图像：0..255
        let mut img = Image::<u8, 1>::new(256, 1);
        for (i, v) in img.data_mut().iter_mut().enumerate() {
            *v = i as u8;
        }
        let dst = equalize_hist(&img);
        // 每个 bin 1 个像素，total=256
        // i=0, hist[0]=1 != 256
        // scale = 255/(256-1) = 1.0
        // lut[0]=0, sum=0
        // j=1: sum=1, lut=round(1)=1
        // j=2: sum=2, lut=2
        // ... j=255: sum=255, lut=255
        // 所以输出 = 输入（恒等映射）
        assert_eq!(dst.data(), img.data());
    }

    #[test]
    fn compare_hist_identical() {
        // 相同直方图：Correl=1, ChiSqr=0, Intersect=sum, Bhattacharyya=0, KlDiv=0
        let h = vec![1.0f32, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(compare_hist(&h, &h, HistCompMethod::Correl), 1.0);
        assert_eq!(compare_hist(&h, &h, HistCompMethod::ChiSqr), 0.0);
        assert_eq!(compare_hist(&h, &h, HistCompMethod::ChiSqrAlt), 0.0);
        assert_eq!(compare_hist(&h, &h, HistCompMethod::Intersect), 15.0);
        assert_eq!(compare_hist(&h, &h, HistCompMethod::Bhattacharyya), 0.0);
        assert_eq!(compare_hist(&h, &h, HistCompMethod::KlDiv), 0.0);
    }

    #[test]
    fn compare_hist_intersect() {
        // 交集 = sum(min(h1, h2))
        let h1 = vec![1.0f32, 5.0, 3.0];
        let h2 = vec![4.0f32, 2.0, 3.0];
        assert_eq!(compare_hist(&h1, &h2, HistCompMethod::Intersect), 6.0);
    }

    #[test]
    fn compare_hist_chisqr() {
        // ChiSqr = sum((h1-h2)^2 / h1)
        let h1 = vec![4.0f32, 9.0];
        let h2 = vec![2.0f32, 3.0];
        // (4-2)^2/4 + (9-3)^2/9 = 1 + 4 = 5
        assert_eq!(compare_hist(&h1, &h2, HistCompMethod::ChiSqr), 5.0);
    }

    #[test]
    fn compare_hist_kl_div() {
        // KL = sum(p * log(p/q))
        let h1 = vec![4.0f32, 6.0];
        let h2 = vec![2.0f32, 3.0];
        // 4*log(4/2) + 6*log(6/3) = 4*log(2) + 6*log(2) = 10*log(2)
        let expected = 10.0 * 2.0f64.ln();
        assert!((compare_hist(&h1, &h2, HistCompMethod::KlDiv) - expected).abs() < 1e-12);
    }

    #[test]
    fn compare_hist_kl_div_zero() {
        // p=0 时跳过；q=0 时 q=1e-10
        let h1 = vec![0.0f32, 4.0];
        let h2 = vec![5.0f32, 0.0];
        // j=0: p=0, skip
        // j=1: p=4, q=1e-10, result += 4*log(4/1e-10)
        let expected = 4.0f64 * (4.0f64 / 1e-10).ln();
        assert!((compare_hist(&h1, &h2, HistCompMethod::KlDiv) - expected).abs() < 1e-9);
    }
}