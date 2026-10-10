//! 形态学（对应 OpenCV `modules/imgproc` 的 `erode`/`dilate`/`morphologyEx`）。
//!
//! 公开 API 一律 RGB（铁律 6）。

use crate::core::border::BorderPolicy;
use crate::core::image::{Image, Pixel};

/// 腐蚀（对应 OpenCV `cv::erode`）。
///
/// 对每个像素计算 `kernel` 非零位置覆盖的像素最小值。
/// `kernel` 为结构化元素（0=忽略，非零=参与），`ksize` 为其尺寸。
/// `anchor` 为负时取中心（`ksize/2`）。
///
/// 矩形核（全非零）走可分离两阶段（行 min + 列 min）+ rayon 并行；
/// 非矩形核走通用 2D。
///
/// # Panics
/// - `kernel.len() != ksize.0 * ksize.1`
/// - `ksize.0 == 0 || ksize.1 == 0`
/// - `anchor` 越界
pub fn erode<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    morph_apply(src, kernel, ksize, anchor, iterations, border, MorphOp::Erode)
}

/// 膨胀（对应 OpenCV `cv::dilate`）。
///
/// 对每个像素计算 `kernel` 非零位置（像素值）的最大值。
///
/// # Panics
/// 同 [`erode`]。
pub fn dilate<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    morph_apply(src, kernel, ksize, anchor, iterations, border, MorphOp::Dilate)
}

/// 开运算（对应 OpenCV `cv::morphologyEx` + `MORPH_OPEN`）：先腐蚀后膨胀。
pub fn morph_open<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    // u8 矩形核 3x3 单次迭代居中 anchor：三阶段流水线融合（中间结果不物化）
    if let Some(out) = morph_open_close_fused(src, kernel, ksize, anchor, iterations, &border, true) {
        return out;
    }
    let eroded = erode(src, kernel, ksize, anchor, iterations, border);
    dilate(&eroded, kernel, ksize, anchor, iterations, border)
}

/// 闭运算（对应 OpenCV `cv::morphologyEx` + `MORPH_CLOSE`）：先膨胀后腐蚀。
pub fn morph_close<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    // u8 矩形核 3x3 单次迭代居中 anchor：三阶段流水线融合（中间结果不物化）
    if let Some(out) = morph_open_close_fused(src, kernel, ksize, anchor, iterations, &border, false) {
        return out;
    }
    let dilated = dilate(src, kernel, ksize, anchor, iterations, border);
    erode(&dilated, kernel, ksize, anchor, iterations, border)
}

/// open/close 融合路径特派判断：u8 + 矩形核 3x3 + 单次迭代 + 居中 anchor + cn=1。
/// 满足条件返回融合结果，否则返回 None（走通用路径）。
fn morph_open_close_fused<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: &BorderPolicy<T>,
    open: bool,
) -> Option<Image<T, C>> {
    let (w, h) = (src.width(), src.height());
    if std::any::TypeId::of::<T>() != std::any::TypeId::of::<u8>()
        || !is_rectangular(kernel)
        || ksize != (3, 3)
        || kernel.len() != 9
        || iterations != 1
        || normalize_anchor(anchor, ksize) != (1, 1)
        || w == 0
        || h == 0
        || C != 1
    {
        return None;
    }
    let border_u8: BorderPolicy<u8> = match border {
        BorderPolicy::Clamp => BorderPolicy::Clamp,
        BorderPolicy::Reflect => BorderPolicy::Reflect,
        BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
    };
    let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
    let mut out: Image<u8, C> = Image::new_uninit(w, h);
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
    // open = dilate(erode(x))：第一级 erode（op1_erode=true）；close 反之
    crate::simd_kernels::morphology::morph_open_close_streaming_u8(
        src_bytes, out_bytes, w, h, &border_u8, open,
    );
    let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
    Some(Image::from_vec(w, h, data).expect("u8 morph path length invariant"))
}

/// 形态学梯度（对应 OpenCV `cv::morphologyEx` + `MORPH_GRADIENT`）：膨胀 - 腐蚀。
pub fn morph_gradient<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    let (w, h) = (src.width(), src.height());
    // u8 矩形核 3x3 单次迭代居中 anchor：融合流式路径
    // （一次行滤波同时算 min/max，两个环形缓冲，一次列阶段，立即相减）
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>()
        && is_rectangular(kernel)
        && ksize == (3, 3)
        && kernel.len() == 9
        && iterations == 1
        && normalize_anchor(anchor, ksize) == (1, 1)
        && w > 0
        && h > 0
    {
        let border_u8: BorderPolicy<u8> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
        };
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let mut out: Image<u8, C> = Image::new_uninit(w, h);
        let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        if C == 1 {
            crate::simd_kernels::morphology::morph_gradient_streaming_u8(
                src_bytes, out_bytes, w, h, &border_u8,
            );
            let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
            return Image::from_vec(w, h, data).expect("u8 morph path length invariant");
        }
    }
    let eroded = erode(src, kernel, ksize, anchor, iterations, border);
    let dilated = dilate(src, kernel, ksize, anchor, iterations, border);
    let mut out = Image::new(w, h);
    // u8 特派：SIMD 饱和减
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        let d: &[u8] = bytemuck::cast_slice(dilated.data());
        let e: &[u8] = bytemuck::cast_slice(eroded.data());
        let o: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        crate::simd_kernels::gradient_u8(d, e, o, w * h * C);
        return out;
    }
    let e = eroded.data();
    let d = dilated.data();
    let o = out.data_mut();
    for i in 0..(w * h * C) {
        o[i] = T::from_f32(d[i].to_f32() - e[i].to_f32());
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum MorphOp {
    Erode,
    Dilate,
}

/// 归一化 anchor（负值取中心）。
fn normalize_anchor(anchor: (i32, i32), ksize: (usize, usize)) -> (usize, usize) {
    let ax = if anchor.0 < 0 {
        ksize.0 as i32 / 2
    } else {
        anchor.0
    };
    let ay = if anchor.1 < 0 {
        ksize.1 as i32 / 2
    } else {
        anchor.1
    };
    assert!(
        ax >= 0 && (ax as usize) < ksize.0 && ay >= 0 && (ay as usize) < ksize.1,
        "anchor ({ax},{ay}) out of range for kernel {ksize:?}"
    );
    (ax as usize, ay as usize)
}

/// 判断核是否全非零（矩形核 → 可分离）。
fn is_rectangular(kernel: &[u8]) -> bool {
    kernel.iter().all(|&v| v != 0)
}

/// 形态学主入口：矩形核走可分离两段，否则通用 2D。
fn morph_apply<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (i32, i32),
    iterations: usize,
    border: BorderPolicy<T>,
    op: MorphOp,
) -> Image<T, C> {
    let (kw, kh) = ksize;
    assert!(kw > 0 && kh > 0, "kernel size must be positive");
    assert!(
        kernel.len() == kw * kh,
        "kernel length {} != ksize {}x{}",
        kernel.len(),
        kw,
        kh
    );
    let (ax, ay) = normalize_anchor(anchor, ksize);

    // iterations == 0 或核为 1x1：直接复制
    if iterations == 0 || (kw == 1 && kh == 1) {
        return src.clone();
    }

    // 矩形核：可分离两段
    if is_rectangular(kernel) {
        // 迭代优化：矩形核多次迭代等价于扩大核（OpenCV morphOp:1010-1018）
        let (ekw, ekh) = (
            kw + (iterations - 1) * (kw - 1),
            kh + (iterations - 1) * (kh - 1),
        );
        let eax = ax * iterations;
        let eay = ay * iterations;
        return morph_separable::<T, C>(src, (ekw, ekh), (eax, eay), border, op);
    }

    // 非矩形核：通用 2D，逐次迭代
    let mut cur = src.clone();
    for _ in 0..iterations {
        cur = morph_2d::<T, C>(&cur, kernel, ksize, (ax, ay), border, op);
    }
    cur
}

/// 可分离两段形态学（矩形核）：行 min/max + 列 min/max。
fn morph_separable<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    ksize: (usize, usize),
    anchor: (usize, usize),
    border: BorderPolicy<T>,
    op: MorphOp,
) -> Image<T, C> {
    let (kw, kh) = ksize;
    let (ax, ay) = anchor;
    let (w, h) = (src.width(), src.height());
    let row_w = w * C;

    // u8 特派：SIMD 两段 + rayon 并行
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        let border_u8: BorderPolicy<u8> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
        };
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let out = morph_separable_u8::<C>(src_bytes, w, h, kw, kh, ax, ay, &border_u8, op);
        let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
        return Image::from_vec(w, h, data).expect("u8 morph path length invariant");
    }

    // 通用类型：标量两段
    let dx1 = ax; // 左侧边框宽
    let dx2 = kw - 1 - dx1; // 右侧
    let dy1 = ay; // 上侧
    let ph = h + kh - 1;
    let pw = w + kw - 1;

    // 行阶段：ph 行，每行做水平 min/max
    let mut mid: Vec<T> = crate::simd_kernels::alloc_uninit(row_w * ph);
    let row_map: Vec<usize> = (0..ph)
        .map(|r| {
            let sy = r as i64 - dy1 as i64;
            border_index(sy, h, &border)
        })
        .collect();
    let src_data = src.data();
    for (r, dst_row) in mid.chunks_exact_mut(row_w).enumerate() {
        match row_map[r] {
            usize::MAX => {
                if let BorderPolicy::Constant(v) = border {
                    dst_row.fill(v);
                }
            }
            sy => {
                let src_row = &src_data[sy * row_w..(sy + 1) * row_w];
                let mut padded: Vec<T> = crate::simd_kernels::alloc_uninit(pw * C);
                fill_padded_row::<T, C>(&mut padded, src_row, w, dx1, dx2, &border);
                row_morph_scalar::<T, C>(&padded, dst_row, w, kw, op);
            }
        }
    }

    // 列阶段：每输出行做垂直 min/max
    let mut out = Image::new(w, h);
    let out_data = out.data_mut();
    for y in 0..h {
        let dst_row = &mut out_data[y * row_w..(y + 1) * row_w];
        col_morph_scalar::<T, C>(&mid, dst_row, y, kh, row_w, op);
    }
    out
}

/// 通用 2D 形态学（非矩形核）。
fn morph_2d<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kernel: &[u8],
    ksize: (usize, usize),
    anchor: (usize, usize),
    border: BorderPolicy<T>,
    op: MorphOp,
) -> Image<T, C> {
    let (kw, kh) = ksize;
    let (ax, ay) = anchor;
    let (w, h) = (src.width(), src.height());
    let src_data = src.data();
    let mut out = Image::new(w, h);
    let out_data = out.data_mut();
    for y in 0..h {
        for x in 0..w {
            for c in 0..C {
                let mut acc = if op == MorphOp::Erode {
                    T::max_value()
                } else {
                    T::zero()
                };
                for ky in 0..kh {
                    let sy = y as i64 + ky as i64 - ay as i64;
                    for kx in 0..kw {
                        if kernel[ky * kw + kx] == 0 {
                            continue;
                        }
                        let sx = x as i64 + kx as i64 - ax as i64;
                        let v = sample_pixel::<T, C>(src_data, w, h, sx, sy, &border, c);
                        acc = if op == MorphOp::Erode {
                            if v < acc { v } else { acc }
                        } else {
                            if v > acc { v } else { acc }
                        };
                    }
                }
                out_data[(y * w + x) * C + c] = acc;
            }
        }
    }
    out
}

/// 行阶段标量：水平 min/max（padded 行）。
fn row_morph_scalar<T: Pixel, const C: usize>(
    padded: &[T],
    dst: &mut [T],
    w: usize,
    kw: usize,
    op: MorphOp,
) {
    for x in 0..w {
        for c in 0..C {
            let mut acc = padded[x * C + c];
            for k in 1..kw {
                let v = padded[(x + k) * C + c];
                acc = if op == MorphOp::Erode {
                    if v < acc { v } else { acc }
                } else {
                    if v > acc { v } else { acc }
                };
            }
            dst[x * C + c] = acc;
        }
    }
}

/// 列垂直 min/max（标量）。
fn col_morph_scalar<T: Pixel, const C: usize>(
    mid: &[T],
    dst: &mut [T],
    w: usize,
    kh: usize,
    row_w: usize,
    op: MorphOp,
) {
    for x in 0..w {
        for c in 0..C {
            let mut acc = mid[x * C + c];
            for k in 1..kh {
                let v = mid[k * row_w + x * C + c];
                acc = if op == MorphOp::Erode {
                    if v < acc { v } else { acc }
                } else {
                    if v > acc { v } else { acc }
                };
            }
            dst[x * C + c] = acc;
        }
    }
}

/// 填充带左右边框的行（`padded` 长度 ≥ `(w + kw - 1)*C`）。
fn fill_padded_row<T: Pixel, const C: usize>(
    padded: &mut [T],
    src_row: &[T],
    w: usize,
    dx1: usize,
    dx2: usize,
    border: &BorderPolicy<T>,
) {
    let pw = w + dx1 + dx2;
    for x in 0..pw {
        let sx = x as i64 - dx1 as i64;
        for c in 0..C {
            padded[x * C + c] = match border.interpolate(sx as isize, w) {
                Some(sx) => src_row[sx * C + c],
                None => match border {
                    BorderPolicy::Constant(v) => *v,
                    _ => unreachable!("non-constant border always interpolates"),
                },
            };
        }
    }
}

/// 垂直边框行索引映射（`usize::MAX` = 常量边框行）。
pub(crate) fn border_index<T: Pixel>(p: i64, len: usize, border: &BorderPolicy<T>) -> usize {
    match border.interpolate(p as isize, len) {
        Some(i) => i,
        None => usize::MAX,
    }
}

/// 采样像素（通用 2D 用）。
fn sample_pixel<T: Pixel, const C: usize>(
    data: &[T],
    w: usize,
    h: usize,
    sx: i64,
    sy: i64,
    border: &BorderPolicy<T>,
    c: usize,
) -> T {
    match border.interpolate(sx as isize, w) {
        Some(sx) => match border.interpolate(sy as isize, h) {
            Some(sy) => data[(sy * w + sx) * C + c],
            None => match border {
                BorderPolicy::Constant(v) => *v,
                _ => unreachable!(),
            },
        },
        None => match border {
            BorderPolicy::Constant(v) => *v,
            _ => unreachable!(),
        },
    }
}

/// u8 可分离两段形态学（SIMD + rayon 并行）。
///
/// 对齐 OpenCV FilterEngine：行阶段（水平 min/max）→ 中间缓冲 → 列阶段（垂直 min/max）。
/// 中间缓冲 `w*C × (h + kh - 1)` 的 u8（上下含边框行）。
#[allow(clippy::too_many_arguments)]
fn morph_separable_u8<const C: usize>(
    src: &[u8],
    w: usize,
    h: usize,
    kw: usize,
    kh: usize,
    ax: usize,
    ay: usize,
    border: &BorderPolicy<u8>,
    op: MorphOp,
) -> Image<u8, C> {
    let erode = op == MorphOp::Erode;
    let row_w = w * C;
    let dx1 = ax; // 左侧边框宽
    let dx2 = kw - 1 - dx1; // 右侧
    let dy1 = ay; // 上侧

    let mut out: Image<u8, C> = Image::new_uninit(w, h);
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());

    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        // 单线程：流式环形缓冲（kh 行 L1 常驻）
        morph_materialized::<C>(src, out_bytes, w, h, kw, kh, dx1, dx2, dy1, row_w, border, erode);
    } else {
        // 多线程：流式行块并行（每块独立环形缓冲，块首多算 kh-1 行）
        out_bytes
            .par_chunks_mut(row_w * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let rows = d.len() / row_w;
                crate::simd_kernels::morphology::morph_streaming_u8(
                    src, d, w, h, C, kw, kh, dx1, dx2, dy1, y0, rows, border, erode,
                );
            });
    }
    out
}

/// 单线程流式路径：委托 simd_kernels 的环形缓冲内核（kh 行 L1 常驻，行滤波立即被列滤波消费）。
///
/// 对齐 OpenCV FilterEngine 数据流：避免物化整个 mid（1MB）导致列阶段从 L3 读冷数据。
#[allow(clippy::too_many_arguments)]
fn morph_materialized<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    kw: usize,
    kh: usize,
    dx1: usize,
    dx2: usize,
    dy1: usize,
    row_w: usize,
    border: &BorderPolicy<u8>,
    erode: bool,
) {
    crate::simd_kernels::morphology::morph_streaming_u8(
        src, dst, w, h, C, kw, kh, dx1, dx2, dy1, 0, h, border, erode,
    );
    let _ = row_w; // row_w = w*C 由内核内部推导
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img3x3(v: &[u8]) -> Image<u8, 1> {
        Image::from_vec(3, 3, v.to_vec()).unwrap()
    }

    #[test]
    fn erode_3x3_rect() {
        // 3x3 全 1 核，anchor 中心。erode = 3x3 窗口 min
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        let out = erode(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // 中心 (1,1) 窗口 = 全部 9 个值，min=1
        // 角 (0,0) 窗口（clamp）= [1,1,2,1,1,2,4,4,5] min=1
        assert_eq!(out.data(), &[1, 1, 2, 1, 1, 2, 4, 4, 5]);
    }

    #[test]
    fn dilate_3x3_rect() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        let out = dilate(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // 中心 max=9；角 (0,0) clamp 窗口 max=5
        assert_eq!(out.data(), &[5, 6, 6, 8, 9, 9, 8, 9, 9]);
    }

    #[test]
    fn erode_3x3_cross_kernel() {
        // 十字核：只取中心 + 上下左右
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [0u8, 1, 0, 1, 1, 1, 0, 1, 0];
        let out = erode(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // 中心 (1,1)：min(4,5,6,2,8)=2
        assert_eq!(out.data()[4], 2);
    }

    #[test]
    fn morph_open_close() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        let opened = morph_open(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        let closed = morph_close(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // open = erode then dilate；close = dilate then erode
        assert_eq!(opened.data(), &[1, 2, 2, 4, 5, 5, 4, 5, 5]);
        assert_eq!(closed.data(), &[5, 5, 6, 5, 5, 6, 8, 8, 9]);
    }

    #[test]
    fn morph_gradient_test() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        let grad = morph_gradient(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // gradient = dilate - erode
        let e = erode(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        let d = dilate(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        for i in 0..9 {
            assert_eq!(grad.data()[i], d.data()[i] - e.data()[i]);
        }
    }

    #[test]
    fn erode_constant_border() {
        // 常量边框 255（erode 恒等）：边界像素不受影响
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        let out = erode(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Constant(255));
        // 角 (0,0) 窗口含 255（恒等），min = min(1,2,4,5)=1
        assert_eq!(out.data(), &[1, 1, 2, 1, 1, 2, 4, 4, 5]);
    }

    #[test]
    fn erode_iterations() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8; 9];
        // 2 次迭代 = 5x5 核（OpenCV 优化）
        let out2 = erode(&src, &kernel, (3, 3), (-1, -1), 2, BorderPolicy::Clamp);
        // 5x5 窗口 min（clamp）全图 = 1
        assert_eq!(out2.data(), &[1, 1, 1, 1, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn erode_1x1_kernel_copy() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let kernel = [1u8];
        let out = erode(&src, &kernel, (1, 1), (0, 0), 1, BorderPolicy::Clamp);
        assert_eq!(out.data(), &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn erode_color() {
        // 彩色（C=3）：每通道独立 min
        let data: Vec<u8> = (0..27).map(|i| i as u8).collect();
        let img = Image::<u8, 3>::from_vec(3, 3, data).unwrap();
        let kernel = [1u8; 9];
        let out = erode(&img, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        // 中心像素 (1,1) = (12,13,14)，3x3 窗口 min 每通道
        // 通道0: min(0,3,6,9,12,15,18,21,24)=0
        assert_eq!(out.data()[12], 0);
        assert_eq!(out.data()[13], 1);
        assert_eq!(out.data()[14], 2);
    }

}
