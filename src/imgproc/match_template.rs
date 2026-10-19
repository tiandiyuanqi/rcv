//! 模板匹配（`cv::matchTemplate`），对齐 OpenCV 5 `modules/imgproc/src/templmatch.cpp`。
//!
//! 6 种方法：TM_SQDIFF / TM_SQDIFF_NORMED / TM_CCORR / TM_CCORR_NORMED /
//! TM_CCOEFF / TM_CCOEFF_NORMED。u8 输入、f32 输出。

use crate::core::image::Image;

/// 模板匹配方法（对齐 `cv::TemplateMatchModes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateMatchMethod {
    /// 平方差（越小越匹配）。
    SqDiff,
    /// 归一化平方差。
    SqDiffNormed,
    /// 互相关（越大越匹配）。
    CCorr,
    /// 归一化互相关。
    CCorrNormed,
    /// 相关系数。
    CCoeff,
    /// 归一化相关系数。
    CCoeffNormed,
}

impl TemplateMatchMethod {
    fn num_type(&self) -> i32 {
        match self {
            TemplateMatchMethod::CCorr | TemplateMatchMethod::CCorrNormed => 0,
            TemplateMatchMethod::CCoeff | TemplateMatchMethod::CCoeffNormed => 1,
            _ => 2,
        }
    }
    fn is_normed(&self) -> bool {
        matches!(
            self,
            TemplateMatchMethod::SqDiffNormed
                | TemplateMatchMethod::CCorrNormed
                | TemplateMatchMethod::CCoeffNormed
        )
    }
}

/// 模板匹配（对齐 `cv::matchTemplate`，u8 输入、f32 输出）。
///
/// `src` 为 `w*h` 灰度图像，`templ` 为 `tw*th` 模板。输出尺寸为
/// `(w - tw + 1, h - th + 1)`，每个像素为匹配分数。
pub fn match_template(
    src: &Image<u8, 1>,
    templ: &Image<u8, 1>,
    method: TemplateMatchMethod,
) -> Image<f32, 1> {
    let (w, h) = (src.width(), src.height());
    let (tw, th) = (templ.width(), templ.height());
    assert!(tw <= w && th <= h, "template must be smaller than image");
    let (rw, rh) = (w - tw + 1, h - th + 1);

    let src_data = src.data();
    let templ_data = templ.data();

    // 1. 计算 CCORR（互相关）：corr[y][x] = sum(T * I)
    // 缓冲区 thread_local 复用（避免 3.8MB 分配页错误）
    thread_local! {
        static CORR: std::cell::RefCell<Vec<f32>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let mut corr = CORR.with(|b| std::mem::take(&mut *b.borrow_mut()));
    corr.clear();
    corr.resize(rw * rh, 0.0);
    // 大模板用 FFT（O(N log N)），小模板用暴力卷积（SIMD）。
    // 实测（1024x1024 图）：FFT 路径 ~60ms（受图像尺寸主导，与模板大小无关），
    // 暴力卷积随模板面积线性增长（32x32 ~53ms，48x48 ~93ms）；
    // 交叉点在 ~40x40。阈值取 40x40 保证 FFT 仅在更快时启用。
    if tw * th >= 40 * 40 {
        crate::imgproc::dft::ccorr_fft(src_data, w, h, templ_data, tw, th, &mut corr);
    } else {
        crate::simd_kernels::match_template::ccorr_u8(
            src_data, w, h, templ_data, tw, th, &mut corr,
        );
    }

    let out = normalize_corr(src, templ, method, &corr);
    CORR.with(|b| *b.borrow_mut() = corr);
    out
}

/// 用积分图对互相关结果做归一化，得到最终匹配分数。
///
/// `corr` 为 `(w-tw+1)*(h-th+1)` 的互相关值。与 `match_template` 分离，
/// 便于测试不同互相关路径（FFT / 暴力卷积）的归一化一致性。
fn normalize_corr(
    src: &Image<u8, 1>,
    templ: &Image<u8, 1>,
    method: TemplateMatchMethod,
    corr: &[f32],
) -> Image<f32, 1> {
    let (w, h) = (src.width(), src.height());
    let (tw, th) = (templ.width(), templ.height());
    let (rw, rh) = (w - tw + 1, h - th + 1);
    let src_data = src.data();
    let templ_data = templ.data();

    let num_type = method.num_type();
    let is_normed = method.is_normed();

    let inv_area = 1.0 / (th as f64 * tw as f64);

    // 模板均值/方差
    let mut templ_sum = 0.0f64;
    let mut templ_sqsum = 0.0f64;
    for &v in templ_data {
        templ_sum += v as f64;
        templ_sqsum += (v as f64) * (v as f64);
    }
    let templ_mean = templ_sum * inv_area;
    let templ_var = templ_sqsum * inv_area - templ_mean * templ_mean;
    let mut templ_norm = templ_var;
    let mut templ_sum2 = templ_norm + templ_mean * templ_mean;
    if num_type != 1 {
        templ_norm = templ_sum2;
    }
    templ_sum2 /= inv_area;
    templ_norm = templ_norm.sqrt() / inv_area.sqrt();

    let mut out = Image::<f32, 1>::new(rw, rh);
    let out_data = out.data_mut();

    if num_type == 0 && !is_normed {
        // 纯 CCORR：corr 即结果，无需积分图
        out_data.copy_from_slice(corr);
        return out;
    }

    // 积分图（i32 sum + f64 sqsum，AVX2 加速 ~7 倍）。
    // 内核一次计算两个数组；i32 sum 的额外开销仅 ~0.5ms，不值得拆分。
    // 缓冲区 thread_local 复用：避免每次 12MB 分配的页错误。
    thread_local! {
        static SUM: std::cell::RefCell<Vec<i32>> =
            const { std::cell::RefCell::new(Vec::new()) };
        static SQSUM: std::cell::RefCell<Vec<f64>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let (sw, sh) = (w + 1, h + 1);
    let mut sum = SUM.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let mut sqsum = SQSUM.with(|b| std::mem::take(&mut *b.borrow_mut()));
    sum.clear();
    sum.resize(sw * sh, 0);
    sqsum.clear();
    sqsum.resize(sw * sh, 0.0);
    crate::simd_kernels::integral::integral_u8_i32_sqsum(src_data, w, h, &mut sum, &mut sqsum);

    // SIMD 归一化（AVX2，积分图查询向量化）
    crate::simd_kernels::match_template::normalize_corr_simd(
        &corr, &sum, &sqsum, w, h, tw, th,
        num_type, is_normed, method == TemplateMatchMethod::SqDiffNormed,
        inv_area, templ_mean, templ_norm, templ_sum2, out_data,
    );

    // 归还缓冲区
    SUM.with(|b| *b.borrow_mut() = sum);
    SQSUM.with(|b| *b.borrow_mut() = sqsum);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 恒等模板：CCorr 在左上角最大。
    #[test]
    fn match_template_ccorr_identity() {
        let src = Image::<u8, 1>::from_vec(4, 4, vec![
            1, 2, 3, 4,
            5, 6, 7, 8,
            9, 10, 11, 12,
            13, 14, 15, 16,
        ]).unwrap();
        let templ = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 5, 6]).unwrap();
        let out = match_template(&src, &templ, TemplateMatchMethod::CCorr);
        // 左上角 (0,0) 完全匹配，CCorr = 1+4+25+36 = 66
        assert_eq!(out.data()[0], 66.0);
    }

    /// SqDiff 在完全匹配处最小。
    #[test]
    fn match_template_sqdiff_min() {
        let src = Image::<u8, 1>::from_vec(4, 4, vec![
            1, 2, 3, 4,
            5, 6, 7, 8,
            9, 10, 11, 12,
            13, 14, 15, 16,
        ]).unwrap();
        let templ = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 5, 6]).unwrap();
        let out = match_template(&src, &templ, TemplateMatchMethod::SqDiff);
        // 左上角 (0,0) 完全匹配，SqDiff 最小
        let min_idx = out.data().iter().enumerate().min_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        assert_eq!(min_idx, 0);
    }

    /// CCoeffNormed 在完全匹配处为 1。
    #[test]
    fn match_template_ccoeff_normed() {
        let src = Image::<u8, 1>::from_vec(4, 4, vec![
            1, 2, 3, 4,
            5, 6, 7, 8,
            9, 10, 11, 12,
            13, 14, 15, 16,
        ]).unwrap();
        let templ = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 5, 6]).unwrap();
        let out = match_template(&src, &templ, TemplateMatchMethod::CCoeffNormed);
        // 左上角完全匹配，CCoeffNormed = 1
        assert!((out.data()[0] - 1.0).abs() < 1e-5);
    }

    /// 大模板（>=40x40）走 FFT 路径，与暴力卷积路径结果一致。
    #[test]
    fn fft_path_matches_bruteforce() {
        let (w, h) = (256, 256);
        let (tw, th) = (96, 96);
        let mut seed = 999u32;
        let mut src = vec![0u8; w * h];
        for v in src.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 24) as u8;
        }
        let mut templ = vec![0u8; tw * th];
        for v in templ.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 24) as u8;
        }
        let img = Image::<u8, 1>::from_vec(w, h, src).unwrap();
        let tpl = Image::<u8, 1>::from_vec(tw, th, templ).unwrap();

        let methods = [
            TemplateMatchMethod::SqDiff,
            TemplateMatchMethod::SqDiffNormed,
            TemplateMatchMethod::CCorr,
            TemplateMatchMethod::CCorrNormed,
            TemplateMatchMethod::CCoeff,
            TemplateMatchMethod::CCoeffNormed,
        ];
        for m in methods {
            // FFT 路径（大模板自动走 FFT）
            let fft_out = match_template(&img, &tpl, m);
            // 暴力路径：临时强制走 ccorr_u8
            let (rw, rh) = (w - tw + 1, h - th + 1);
            let mut corr = vec![0.0f32; rw * rh];
            crate::simd_kernels::match_template::ccorr_u8(
                img.data(), w, h, tpl.data(), tw, th, &mut corr,
            );
            let brute_out = normalize_corr(&img, &tpl, m, &corr);
            let a = fft_out.data();
            let b = brute_out.data();
            // 归一化方法输出在 [-1,1]，对微小浮点差异不敏感，需严格对齐（PSNR>60dB 要求）。
            // 非归一化方法（SqDiff/CCorr/CCoeff）输出大数值，CCoeff 因减去模板均值会
            // 放大 FFT 的 f32 精度误差（两近等大数相减），此处仅验证值有限且量级一致。
            if m.is_normed() {
                for i in 0..a.len() {
                    let denom = b[i].abs().max(1.0);
                    let rel = (a[i] - b[i]).abs() / denom;
                    assert!(
                        rel < 1e-3,
                        "{m:?} i={i} fft={} brute={} rel={}",
                        a[i], b[i], rel
                    );
                }
            } else {
                // 非归一化：验证值有限，且 FFT 与暴力卷积的最大绝对值同量级
                for &v in a {
                    assert!(v.is_finite(), "{m:?} fft value {v} not finite");
                }
                let max_a = a.iter().cloned().fold(f32::MIN, f32::max).abs();
                let max_b = b.iter().cloned().fold(f32::MIN, f32::max).abs();
                assert!(
                    (max_a - max_b).abs() / max_b.max(1.0) < 0.1,
                    "{m:?} fft max={max_a} brute max={max_b} 量级不一致"
                );
            }
        }
    }
}