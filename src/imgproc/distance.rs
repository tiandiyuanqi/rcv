//! 距离变换（对应 OpenCV `modules/imgproc` 的 `distanceTransform`，distransform.cpp）。
//!
//! 公开 API 输入为单通道 u8 二值图，输出为单通道 f32 距离图。
//! 采用 OpenCV 的 chamfer 两遍扫描（前向 + 后向），定点算术（DIST_SHIFT=16），
//! 与 OpenCV 逐位一致。

use crate::core::image::Image;

use std::cell::RefCell;

thread_local! {
    /// 距离变换临时缓冲（u32 定点），跨调用复用避免冷启动 cache miss。
    static TEMP_BUF: RefCell<Vec<u32>> = RefCell::new(Vec::new());
}

/// 距离类型（对应 OpenCV `DIST_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceType {
    /// 切比雪夫距离（`DIST_C`）。
    C,
    /// 曼哈顿距离（`DIST_L1`）。
    L1,
    /// 欧氏距离（`DIST_L2`）。
    L2,
}

/// 掩码大小（对应 OpenCV `DIST_MASK_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMask {
    /// 3×3 掩码。
    Mask3,
    /// 5×5 掩码。
    Mask5,
}

const DIST_SHIFT: i32 = 16;

/// 定点转换：`cvRound(x * 2^16)`（round-to-nearest、ties-to-even，对齐 OpenCV `CV_FLT_TO_FIX`）。
fn flt_to_fix(x: f32) -> u32 {
    (x * (1u32 << DIST_SHIFT) as f32).round_ties_even() as u32
}

/// 距离变换（对应 OpenCV `cv::distanceTransform`）。
///
/// 输入 `src` 为单通道 u8 二值图（非零为前景，零为背景），
/// 输出为单通道 f32 距离图（背景像素到最近前景像素的距离）。
///
/// 与 OpenCV 逐位一致（chamfer 两遍扫描，定点算术）。
/// 复杂度 O(width·height)。
pub fn distance_transform(
    src: &Image<u8, 1>,
    distance_type: DistanceType,
    mask: DistanceMask,
) -> Image<f32, 1> {
    let (w, h) = (src.width(), src.height());
    let mut dst = Image::new_uninit(w, h);

    // 掩码值（对齐 OpenCV getDistanceTransformMask）
    let (hv, diag, long) = match (distance_type, mask) {
        (DistanceType::C, DistanceMask::Mask3) => (1.0f32, 1.0f32, 0.0f32),
        (DistanceType::L1, DistanceMask::Mask3) => (1.0f32, 2.0f32, 0.0f32),
        (DistanceType::L2, DistanceMask::Mask3) => (0.955f32, 1.3693f32, 0.0f32),
        (DistanceType::C, DistanceMask::Mask5) => (1.0f32, 1.0f32, 2.0f32),
        (DistanceType::L1, DistanceMask::Mask5) => (1.0f32, 2.0f32, 3.0f32),
        (DistanceType::L2, DistanceMask::Mask5) => (1.0f32, 1.4f32, 2.1969f32),
    };

    let hv_dist = flt_to_fix(hv);
    let diag_dist = flt_to_fix(diag);
    let long_dist = flt_to_fix(long);

    match mask {
        DistanceMask::Mask3 => {
            let dist_max = u32::MAX - diag_dist;
            let scale = 1.0f32 / (1u32 << DIST_SHIFT) as f32;
            distance_transform_3x3(src, &mut dst, hv_dist, diag_dist, dist_max, scale);
        }
        DistanceMask::Mask5 => {
            let dist_max = u32::MAX - long_dist;
            let scale = 1.0f32 / (1u32 << DIST_SHIFT) as f32;
            distance_transform_5x5(src, &mut dst, hv_dist, diag_dist, long_dist, dist_max, scale);
        }
    }

    dst
}

/// 3×3 chamfer 距离变换（对齐 OpenCV `distanceTransform_3x3`）。
fn distance_transform_3x3(
    src: &Image<u8, 1>,
    dst: &mut Image<f32, 1>,
    hv_dist: u32,
    diag_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    let (w, h) = (src.width(), src.height());
    let border = 1usize;
    let step = w + 2 * border;

    // temp 缓冲（含 border 哨兵），仅初始化 top/bottom border 行（对齐 OpenCV initTopBottom）
    let temp_len = step * (h + 2 * border);
    let mut temp = TEMP_BUF.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < temp_len {
            *b = crate::simd_kernels::alloc_uninit(temp_len);
        }
        std::mem::take(&mut *b)
    });
    for i in 0..border {
        for j in 0..step {
            temp[i * step + j] = dist_max;
            temp[(h + 2 * border - 1 - i) * step + j] = dist_max;
        }
    }

    let sdata = src.data();
    let ddata = dst.data_mut();

    // 前向扫描
    for i in 0..h {
        let row = (i + border) * step;
        let s = &sdata[i * w..(i + 1) * w];

        // 水平边界哨兵
        temp[row + border - 1] = dist_max;
        temp[row + border + w] = dist_max;

        crate::simd_kernels::dist_forward_3x3(
            &mut temp, s, w, step, row, border, hv_dist, diag_dist, dist_max,
        );
    }

    // 后向扫描
    for i in (0..h).rev() {
        let row = (i + border) * step;
        let drow = &mut ddata[i * w..(i + 1) * w];
        crate::simd_kernels::dist_backward_3x3(
            &mut temp, drow, w, step, row, border, hv_dist, diag_dist, dist_max, scale,
        );
    }
    TEMP_BUF.with(|b| *b.borrow_mut() = temp);
}

/// 5×5 chamfer 距离变换（对齐 OpenCV `distanceTransform_5x5`）。
fn distance_transform_5x5(
    src: &Image<u8, 1>,
    dst: &mut Image<f32, 1>,
    hv_dist: u32,
    diag_dist: u32,
    long_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    let (w, h) = (src.width(), src.height());
    let border = 2usize;
    let step = w + 2 * border;

    let temp_len = step * (h + 2 * border);
    let mut temp = TEMP_BUF.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < temp_len {
            *b = crate::simd_kernels::alloc_uninit(temp_len);
        }
        std::mem::take(&mut *b)
    });
    for i in 0..border {
        for j in 0..step {
            temp[i * step + j] = dist_max;
            temp[(h + 2 * border - 1 - i) * step + j] = dist_max;
        }
    }

    let sdata = src.data();
    let ddata = dst.data_mut();

    // 前向扫描
    for i in 0..h {
        let row = (i + border) * step;
        let s = &sdata[i * w..(i + 1) * w];

        temp[row + border - 1] = dist_max;
        temp[row + border - 2] = dist_max;
        temp[row + border + w] = dist_max;
        temp[row + border + w + 1] = dist_max;

        crate::simd_kernels::dist_forward_5x5(
            &mut temp, s, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max,
        );
    }

    // 后向扫描
    for i in (0..h).rev() {
        let row = (i + border) * step;
        let drow = &mut ddata[i * w..(i + 1) * w];
        crate::simd_kernels::dist_backward_5x5(
            &mut temp, drow, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max, scale,
        );
    }
    TEMP_BUF.with(|b| *b.borrow_mut() = temp);
}