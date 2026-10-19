//! 通用二维卷积（`cv::filter2D`），对齐 OpenCV 5 `modules/imgproc/src/filter.dispatch.cpp`
//! 的 `Filter2D` 标量路径（u8 输入、f32 核）。
//!
//! 位级对齐：`preprocess2DKernel` 提取非零核位置、f32 累加、`saturate_cast<uchar>`
//! 舍入均与 OpenCV 逐位一致。

use crate::core::image::{Image, Pixel};
use crate::core::point::Point;

/// 边界处理方式（对齐 `cv::BorderTypes` 常用值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderType {
    /// 常量边界（`BORDER_CONSTANT`），默认值 0。
    Constant,
    /// 复制边界（`BORDER_REPLICATE`）。
    Replicate,
    /// 反射边界（`BORDER_REFLECT`）。
    Reflect,
    /// 反射 101 边界（`BORDER_REFLECT_101`）。
    Reflect101,
    /// 环绕边界（`BORDER_WRAP`）。
    Wrap,
}

/// 通用二维卷积（对齐 `cv::filter2D`，u8 输入、f32 核、u8 输出）。
///
/// `kernel` 为 `f32` 二维核，`anchor` 为核锚点（`(-1,-1)` 表示核中心）。
/// `delta` 为累加偏移，`border` 为边界处理方式。
pub fn filter2d(
    src: &Image<u8, 1>,
    kernel: &[f32],
    kernel_w: usize,
    kernel_h: usize,
    anchor: Point,
    delta: f64,
    border: BorderType,
) -> Image<u8, 1> {
    let (w, h) = (src.width(), src.height());
    let (kw, kh) = (kernel_w, kernel_h);
    assert!(kw > 0 && kh > 0, "kernel must be non-empty");
    assert_eq!(kernel.len(), kw * kh, "kernel size mismatch");

    // 归一化锚点（-1,-1 → 核中心）
    let ax = if anchor.x < 0 { kw as i32 / 2 } else { anchor.x };
    let ay = if anchor.y < 0 { kh as i32 / 2 } else { anchor.y };
    assert!(ax >= 0 && ax < kw as i32, "anchor.x out of range");
    assert!(ay >= 0 && ay < kh as i32, "anchor.y out of range");

    // preprocess2DKernel：提取非零核位置
    let mut coords: Vec<(i32, i32)> = Vec::new();
    let mut coeffs: Vec<f32> = Vec::new();
    for i in 0..kh {
        for j in 0..kw {
            let val = kernel[i * kw + j];
            if val != 0.0 {
                coords.push((j as i32 - ax, i as i32 - ay));
                coeffs.push(val);
            }
        }
    }
    if coords.is_empty() {
        coords.push((0, 0));
        coeffs.push(0.0);
    }

    let delta_f = delta as f32;
    let src_data = src.data();
    let mut out = Image::<u8, 1>::new(w, h);
    let out_data = out.data_mut();

    if border == BorderType::Replicate {
        // 3x3 全非零核 + 锚点(1,1) 走专用路径（行加载 + shuffle）
        if kw == 3 && kh == 3 && coords.len() == 9 && ax == 1 && ay == 1 {
            let mut k9 = [0.0f32; 9];
            k9.copy_from_slice(&coeffs[..9]);
            crate::simd_kernels::filter2d::filter2d_3x3_u8(
                src_data,
                w,
                h,
                &k9,
                delta_f,
                out_data,
            );
        } else {
            crate::simd_kernels::filter2d::filter2d_u8(
                src_data,
                w,
                h,
                &coords,
                &coeffs,
                delta_f,
                out_data,
            );
        }
    } else {
        // 其他边界类型走标量路径（含边界处理）
        let nz = coords.len();
        for y in 0..h {
            for x in 0..w {
                let mut s0 = delta_f;
                for k in 0..nz {
                    let (kx, ky) = coords[k];
                    let sx = x as i32 + kx;
                    let sy = y as i32 + ky;
                    let v = sample(src_data, w, h, sx, sy, border);
                    s0 += coeffs[k] * v as f32;
                }
                out_data[y * w + x] = u8::from_f32(s0);
            }
        }
    }

    out
}

/// 可分离二维滤波（对齐 `cv::sepFilter2D`，u8 输入、f32 核、u8 输出）。
///
/// `kernel_x` 为水平 1D 核（长度 `kx_len`），`kernel_y` 为垂直 1D 核（长度 `ky_len`）。
/// 先对每行用 `kernel_x` 做水平滤波（输出 f32 中间结果），再对每列用 `kernel_y`
/// 做垂直滤波（输出 u8）。`anchor` 为核锚点（`(-1,-1)` 表示核中心）。
/// `delta` 为累加偏移，`border` 为边界处理方式。
pub fn sep_filter2d(
    src: &Image<u8, 1>,
    kernel_x: &[f32],
    kernel_y: &[f32],
    anchor: Point,
    delta: f64,
    border: BorderType,
) -> Image<u8, 1> {
    let (w, h) = (src.width(), src.height());
    let (kx_len, ky_len) = (kernel_x.len(), kernel_y.len());
    assert!(kx_len > 0 && ky_len > 0, "kernel must be non-empty");

    // 归一化锚点（-1,-1 → 核中心）
    let ax = if anchor.x < 0 { kx_len as i32 / 2 } else { anchor.x };
    let ay = if anchor.y < 0 { ky_len as i32 / 2 } else { anchor.y };
    assert!(ax >= 0 && ax < kx_len as i32, "anchor.x out of range");
    assert!(ay >= 0 && ay < ky_len as i32, "anchor.y out of range");

    let delta_f = delta as f32;
    let src_data = src.data();

    // Replicate 边界走 SIMD 路径
    if border == BorderType::Replicate {
        let mut out = Image::<u8, 1>::new_uninit(w, h);
        crate::simd_kernels::filter2d::sep_filter2d_u8(
            src_data,
            w,
            h,
            kernel_x,
            kernel_y,
            ax,
            ay,
            delta_f,
            out.data_mut(),
        );
        return out;
    }

    // 水平滤波：tmp[y][x] = sum(kx[i] * src[y][x + i - ax])，输出 f32
    let mut tmp = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s0 = 0.0f32;
            for i in 0..kx_len {
                let sx = x as i32 + i as i32 - ax;
                let v = sample(src_data, w, h, sx, y as i32, border);
                s0 += kernel_x[i] * v as f32;
            }
            tmp[y * w + x] = s0;
        }
    }

    // 垂直滤波：dst[y][x] = saturate_cast<u8>(sum(ky[j] * tmp[y + j - ay][x]) + delta)
    let mut out = Image::<u8, 1>::new(w, h);
    let out_data = out.data_mut();
    for y in 0..h {
        for x in 0..w {
            let mut s0 = delta_f;
            for j in 0..ky_len {
                let sy = y as i32 + j as i32 - ay;
                let v = if sy >= 0 && sy < h as i32 {
                    tmp[sy as usize * w + x]
                } else if border == BorderType::Constant {
                    0.0
                } else {
                    // 垂直边界：用 border 采样 tmp
                    let sy2 = interpolate(sy, h as i32, border);
                    tmp[sy2 as usize * w + x]
                };
                s0 += kernel_y[j] * v;
            }
            out_data[y * w + x] = u8::from_f32(s0);
        }
    }

    out
}

/// 采样像素（含边界处理）。
#[inline]
fn sample(data: &[u8], w: usize, h: usize, x: i32, y: i32, border: BorderType) -> u8 {
    if x >= 0 && x < w as i32 && y >= 0 && y < h as i32 {
        return data[y as usize * w + x as usize];
    }
    if border == BorderType::Constant {
        return 0;
    }
    let (bx, by) = border_index(x, y, w, h, border);
    data[by * w + bx]
}

/// 计算边界索引（对齐 `cv::borderInterpolate`）。
#[inline]
fn border_index(x: i32, y: i32, w: usize, h: usize, border: BorderType) -> (usize, usize) {
    let bx = interpolate(x, w as i32, border);
    let by = interpolate(y, h as i32, border);
    (bx as usize, by as usize)
}

/// 单维边界插值（对齐 `cv::borderInterpolate`）。
#[inline]
fn interpolate(p: i32, len: i32, border: BorderType) -> i32 {
    if p >= 0 && p < len {
        return p;
    }
    match border {
        BorderType::Replicate => {
            if p < 0 {
                0
            } else {
                len - 1
            }
        }
        BorderType::Reflect | BorderType::Reflect101 => {
            let delta = if border == BorderType::Reflect101 { 1 } else { 0 };
            if len == 1 {
                return 0;
            }
            let mut p = p;
            loop {
                if p < 0 {
                    p = -p - 1 + delta;
                } else {
                    p = len - 1 - (p - len) - delta;
                }
                if p >= 0 && p < len {
                    break;
                }
            }
            p
        }
        BorderType::Wrap => {
            let mut p = p;
            if p < 0 {
                p -= ((p - len + 1) / len) * len;
            }
            if p >= len {
                p %= len;
            }
            p
        }
        BorderType::Constant => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3x3 恒等核（中心 1）。
    #[test]
    fn filter2d_identity() {
        let src = Image::<u8, 1>::from_vec(3, 3, vec![
            1, 2, 3,
            4, 5, 6,
            7, 8, 9,
        ]).unwrap();
        let kernel = [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let out = filter2d(&src, &kernel, 3, 3, Point::new(-1, -1), 0.0, BorderType::Constant);
        assert_eq!(out.data(), src.data());
    }

    /// 3x3 均值核。
    #[test]
    fn filter2d_box() {
        let src = Image::<u8, 1>::from_vec(3, 3, vec![
            1, 2, 3,
            4, 5, 6,
            7, 8, 9,
        ]).unwrap();
        let kernel = [1.0 / 9.0; 9];
        let out = filter2d(&src, &kernel, 3, 3, Point::new(-1, -1), 0.0, BorderType::Constant);
        // 中心像素 = (1+2+3+4+5+6+7+8+9)/9 = 5
        assert_eq!(out.data()[1 * 3 + 1], 5);
    }

    /// 边界处理：Constant。
    #[test]
    fn filter2d_border_constant() {
        let src = Image::<u8, 1>::from_vec(1, 1, vec![5]).unwrap();
        let kernel = [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        // 3x3 全 1 核，constant 边界 0 → 中心 = 5
        let out = filter2d(&src, &kernel, 3, 3, Point::new(-1, -1), 0.0, BorderType::Constant);
        assert_eq!(out.data()[0], 5);
    }

    /// sepFilter2D 3x3 均值核（等价于 filter2d 3x3 均值核）。
    #[test]
    fn sep_filter2d_box() {
        let src = Image::<u8, 1>::from_vec(3, 3, vec![
            1, 2, 3,
            4, 5, 6,
            7, 8, 9,
        ]).unwrap();
        let kx = [1.0 / 3.0; 3];
        let ky = [1.0 / 3.0; 3];
        let out = sep_filter2d(&src, &kx, &ky, Point::new(-1, -1), 0.0, BorderType::Constant);
        // 中心像素 = (1+2+3+4+5+6+7+8+9)/9 = 5
        assert_eq!(out.data()[1 * 3 + 1], 5);
    }

    /// sepFilter2D 恒等核。
    #[test]
    fn sep_filter2d_identity() {
        let src = Image::<u8, 1>::from_vec(3, 3, vec![
            1, 2, 3,
            4, 5, 6,
            7, 8, 9,
        ]).unwrap();
        let kx = [0.0, 1.0, 0.0];
        let ky = [0.0, 1.0, 0.0];
        let out = sep_filter2d(&src, &kx, &ky, Point::new(-1, -1), 0.0, BorderType::Constant);
        assert_eq!(out.data(), src.data());
    }
}