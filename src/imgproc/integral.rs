//! 积分图（`cv::integral`），对齐 OpenCV 5 `modules/imgproc/src/sumpixels.dispatch.cpp`
//! 的 `integral_` 标量路径。
//!
//! 位级对齐：累加顺序、`sum[x] = sum[x - sumstep] + s` 递推公式均与 OpenCV 逐位一致。

use crate::core::image::Image;

/// 计算积分图（对齐 `cv::integral` 的 sum 输出）。
///
/// 输出尺寸为 `(width+1, height+1)`，首行首列为 0。
/// `src` 为 `u8` 单通道，`sum` 为 `i32` 单通道（对齐 OpenCV 默认 `sdepth=CV_32S`）。
pub fn integral(src: &Image<u8, 1>) -> Image<i32, 1> {
    let (w, h) = (src.width(), src.height());
    let mut sum = Image::<i32, 1>::new_uninit(w + 1, h + 1);
    // 首行置 0（SIMD 内核只写首列 + 其余行）
    sum.data_mut()[..w + 1].fill(0);
    crate::simd_kernels::integral::integral_u8_i32(src.data(), w, h, sum.data_mut());
    sum
}

/// 计算积分图（含平方和，对齐 `cv::integral` 的 sum + sqsum 输出）。
///
/// 输出尺寸均为 `(width+1, height+1)`，首行首列为 0。
/// `src` 为 `u8` 单通道，`sum` 为 `i32`，`sqsum` 为 `f64`（对齐 OpenCV 默认
/// `sdepth=CV_32S`、`sqdepth=CV_64F`）。
pub fn integral_sqsum(src: &Image<u8, 1>) -> (Image<i32, 1>, Image<f64, 1>) {
    let (w, h) = (src.width(), src.height());
    let mut sum = Image::<i32, 1>::new_uninit(w + 1, h + 1);
    let mut sqsum = Image::<f64, 1>::new_uninit(w + 1, h + 1);
    // 首行置 0（SIMD 内核只写首列 + 其余行）
    sum.data_mut()[..w + 1].fill(0);
    sqsum.data_mut()[..w + 1].fill(0.0);
    crate::simd_kernels::integral::integral_u8_i32_sqsum(
        src.data(),
        w,
        h,
        sum.data_mut(),
        sqsum.data_mut(),
    );
    (sum, sqsum)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 图像。
    #[test]
    fn integral_1x1() {
        let src = Image::<u8, 1>::from_vec(1, 1, vec![5]).unwrap();
        let sum = integral(&src);
        assert_eq!(sum.width(), 2);
        assert_eq!(sum.height(), 2);
        assert_eq!(sum.data(), &[0, 0, 0, 5]);
    }

    /// 2x2 图像。
    #[test]
    fn integral_2x2() {
        let src = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 3, 4]).unwrap();
        let sum = integral(&src);
        // 首行首列 0
        // sum[1][1] = 1, sum[1][2] = 1+2=3
        // sum[2][1] = 1+3=4, sum[2][2] = 1+2+3+4=10
        assert_eq!(sum.data(), &[0, 0, 0, 0, 1, 3, 0, 4, 10]);
    }

    /// 3x3 图像（含平方和）。
    #[test]
    fn integral_sqsum_3x3() {
        let src = Image::<u8, 1>::from_vec(3, 3, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        let (sum, sqsum) = integral_sqsum(&src);
        // sum[3][3] = 45
        assert_eq!(sum.data()[sum.width() * sum.height() - 1], 45);
        // sqsum[3][3] = 1+4+9+16+25+36+49+64+81 = 285
        assert!((sqsum.data()[sqsum.width() * sqsum.height() - 1] - 285.0).abs() < 1e-9);
    }
}