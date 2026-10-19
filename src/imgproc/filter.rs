//! 滤波（对应 OpenCV `modules/imgproc` 的 `boxFilter`/`GaussianBlur` 等）。
//!
//! 公开 API 一律 RGB（铁律 6）。

use crate::core::border::BorderPolicy;
use crate::core::image::{Image, Pixel};

/// 方框滤波（对应 OpenCV `cv::boxFilter`）。
///
/// 对每个像素计算 `ksize` 窗口内像素和；`normalize=true` 时除以窗口面积。
/// 边界用 `border` 处理。
///
/// `anchor` 为负时取窗口中心（`ksize/2`）。
///
/// u8 路径与 OpenCV 逐位一致（`kw*kh <= 256` 时 CV_16U 定点除法，
/// 否则 CV_32S + float scale）；其他类型为浮点近似。
///
/// 复杂度 O(width·height·(ksize.w + ksize.h)·C)。
pub fn box_filter<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    ksize: (usize, usize),
    anchor: (i32, i32),
    normalize: bool,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    let (kw, kh) = ksize;
    let ax = if anchor.0 < 0 {
        kw as i32 / 2
    } else {
        anchor.0
    };
    let ay = if anchor.1 < 0 {
        kh as i32 / 2
    } else {
        anchor.1
    };
    // u8 特派：SIMD 两阶段（行和 + 定点列归约）+ rayon 并行。
    // area > 256 时 u16 中间缓冲会溢出，走标量 f64 路径
    // （对齐 OpenCV：sumType = CV_32S，s0·scale double 乘 + cvRound）。
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>()
        && kw * kh <= 256
    {
        let (w, h) = (src.width(), src.height());
        if w > 0 && h > 0 && ax >= 0 && (ax as usize) < kw && ay >= 0 && (ay as usize) < kh {
            // 此分支 T == u8（TypeId 已验证），按位转换参数
            let border_u8: BorderPolicy<u8> = match &border {
                BorderPolicy::Clamp => BorderPolicy::Clamp,
                BorderPolicy::Reflect => BorderPolicy::Reflect,
                BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
            };
            let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
            let out =
                box_filter_u8_simd::<C>(src_bytes, w, h, kw, kh, ax, ay, normalize, &border_u8);
            // 此分支 T == u8（TypeId 已验证），按位重组
            let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
            return Image::from_vec(w, h, data).expect("u8 simd path length invariant");
        }
    }
    let scale = if normalize {
        1.0 / (kw as f64 * kh as f64)
    } else {
        1.0
    };
    let (w, h) = (src.width(), src.height());
    let mut out = Image::new(w, h);
    let src_data = src.data();
    let out_data = out.data_mut();
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0.0f64; C];
            for ky in 0..kh {
                let sy = y as i32 + ky as i32 - ay;
                for kx in 0..kw {
                    let sx = x as i32 + kx as i32 - ax;
                    let px: [T; C] = sample_pixel(src_data, w, h, sx, sy, &border);
                    for c in 0..C {
                        sum[c] += px[c].to_f32() as f64;
                    }
                }
            }
            let oi = (y * w + x) * C;
            for c in 0..C {
                // 对齐 OpenCV CV_32S 路径：s0(int)·scale(double) → cvRound
                // （f64 全程，不转 f32——f64→f32 中转会让 .5 边界错位）
                let v = sum[c] * scale;
                out_data[oi + c] = T::from_f64_round_even(v);
            }
        }
    }
    out
}

/// u8 box_filter SIMD 两阶段实现（对齐 OpenCV FilterEngine + CV_16U 定点路径）。
///
/// 中间缓冲：`w*C × (h + kh - 1)` 的 u16（上下边框行已做行求和）。
/// 列阶段按行块并行，每块独立滑动累加（块首多算 kh-1 行）。
#[allow(clippy::too_many_arguments)]
fn box_filter_u8_simd<const C: usize>(
    src: &[u8],
    w: usize,
    h: usize,
    kw: usize,
    kh: usize,
    ax: i32,
    ay: i32,
    normalize: bool,
    border: &BorderPolicy<u8>,
) -> Image<u8, C> {
    let area = kw * kh;
    // 定点参数（kw*kh <= 256 时 CV_16U 路径；更大核走 CV_32S，此处统一用 u16 中间
    // 缓冲但仅当 area <= 256 时行和才不溢出 u16：255*256 = 65280 < 65535）
    assert!(
        area <= 256,
        "u8 SIMD path requires kw*kh <= 256 (got {area}); caller must dispatch to scalar"
    );
    let fx = crate::simd_kernels::box_filter::BoxFixed::from_area(area);
    // OpenCV：haveScale = scale != 1（area == 1 时 scale == 1，走无缩放路径）
    let have_scale = normalize && area != 1;

    let dx1 = ax as usize; // 窗口左侧边框宽
    let dx2 = kw - 1 - dx1; // 右侧
    let dy1 = ay as usize; // 上侧
    let _dy2 = kh - 1 - dy1; // 下侧（列阶段由 mid 缓冲布局隐含）

    // 1. 带左右边框的行宽
    let pw = w + kw - 1;
    let mut out: Image<u8, C> = Image::new_uninit(w, h);

    // 2+3. 单线程：流式 ring buffer（kh 行 u16 常驻 L1/L2，对齐 OpenCV FilterEngine）；
    //      多线程：物化 mid + 行块并行（每块独立滑动累加）
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
    let row_w = w * C;
    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        box_streaming::<C>(
            src, out_bytes, w, h, kw, kh, dx1, dx2, dy1, row_w, fx, have_scale, border,
        );
    } else {
        let ph = h + kh - 1;
        let mut mid: Vec<u16> = crate::simd_kernels::alloc_uninit(w * C * ph);
        // 行索引映射（上下边框）：padded 行 r 对应源行 borderInterpolate(r - dy1, h)
        let row_map: Vec<usize> = (0..ph)
            .map(|r| {
                let sy = r as i64 - dy1 as i64;
                border_index(sy, h, border)
            })
            .collect();
        // 行阶段（rayon 行块并行）：padded 行任务改块任务，
        // padded 缓冲块级分配（每块一次，避免每行 malloc）。
        // par_chunks_mut：mid 长度（row_w*ph）可能不整除块大小，
        // 尾块由 chunks_exact_mut(row_w) 覆盖（尾块长度必为 row_w 倍数）
        let rows_per_task = 16.min(ph).max(1);
        mid.par_chunks_mut(row_w * rows_per_task)
            .enumerate()
            .for_each(|(ti, dst_rows)| {
                let r0 = ti * rows_per_task;
                let mut padded: Vec<u8> = crate::simd_kernels::alloc_uninit(pw * C);
                for (j, dst_row) in dst_rows.chunks_exact_mut(row_w).enumerate() {
                    let r = r0 + j;
                    match row_map[r] {
                        usize::MAX => {
                            // 常量边框行：行和 = 常量 * kw（每通道）
                            if let BorderPolicy::Constant(v) = border {
                                let s = (*v as u16) * (kw as u16);
                                dst_row.iter_mut().for_each(|d| *d = s);
                            }
                        }
                        sy => {
                            let src_row = &src[sy * row_w..(sy + 1) * row_w];
                            fill_padded_row::<C>(&mut padded, src_row, w, dx1, dx2, border);
                            crate::simd_kernels::box_row_sum_u8(&padded, dst_row, w, kw, C);
                        }
                    }
                }
            });
        // 列阶段（rayon 行块并行）：每块独立滑动累加
        out_bytes
            .par_chunks_mut(row_w * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let rows = d.len() / row_w;
                box_col_block(&mid, d, rows, kh, row_w, fx, have_scale, y0);
            });
    }
    out
}

/// 单线程流式路径：kh 行 ring buffer + 跨行滑动累加（不物化全量 mid）。
///
/// 对齐 OpenCV FilterEngine：逐行做行和进 ring，ring 满后每行做一次
/// 列步进（SUM 滑动累加跨行保持）。中间行序列 = 上下边框映射后的
/// `[borderInterpolate(r - dy1, h) for r in 0..h+kh-1]`。
///
/// kh=3 特化：三行滚动变量（零槽位计算、零 split），源行索引预计算。
#[allow(clippy::too_many_arguments)]
fn box_streaming<const C: usize>(
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
    fx: crate::simd_kernels::box_filter::BoxFixed,
    have_scale: bool,
    border: &BorderPolicy<u8>,
) {
    if kh == 3 && C == 3 && (kw == 3 || kw == 5) {
        box_streaming_kh3_c3(
            src, dst, w, h, kw, dx1, dx2, dy1, row_w, fx, have_scale, border,
        );
        return;
    }
    let pw = w + kw - 1;
    let mut padded: Vec<u8> = crate::simd_kernels::alloc_uninit(pw * C);
    // ring：kh 行 u16 行和（槽位 r%kh）
    let mut ring: Vec<u16> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut ring_rows: Vec<&mut [u16]> = ring.chunks_exact_mut(row_w).collect();
    // SUM：滑动累加（前 kh-1 个中间行之和）
    let mut sum: Vec<u16> = vec![0; row_w];

    // 生成第 mid_idx 个中间行（行和）写入 slot；常量边框行填 v*kw
    let mut fill_mid_row = |slot: &mut [u16], mid_idx: usize| {
        let sy = mid_idx as i64 - dy1 as i64;
        match border_index(sy, h, border) {
            usize::MAX => {
                if let BorderPolicy::Constant(v) = border {
                    let s = (*v as u16) * (kw as u16);
                    slot.fill(s);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                if kw == 3 || kw == 5 {
                    // 无边框源直读（省 padded 中转）
                    crate::simd_kernels::box_row_sum_u8_direct_cn(src_row, slot, w, kw, C, *border);
                } else {
                    fill_padded_row::<C>(&mut padded, src_row, w, dx1, dx2, border);
                    crate::simd_kernels::box_row_sum_u8(&padded, slot, w, kw, C);
                }
            }
        }
    };

    let total_mid = h + kh - 1;
    // 预填前 kh-1 个中间行进 ring 并累加进 SUM
    for (r, slot) in ring_rows.iter_mut().enumerate().take(kh - 1) {
        fill_mid_row(slot, r);
        for (s, v) in sum.iter_mut().zip(&**slot) {
            *s = s.wrapping_add(*v);
        }
    }
    // 流式主循环：第 r 个中间行进 ring 槽 r%kh，输出行 r-(kh-1)。
    // 减行 = 中间行 r-(kh-1)，其槽位 (r-(kh-1))%kh == (r+1)%kh（mod kh 恒等）。
    for r in kh - 1..total_mid {
        fill_mid_row(ring_rows[r % kh], r);
        let out_y = r - (kh - 1);
        let dst_row = &mut dst[out_y * row_w..(out_y + 1) * row_w];
        if kh == 1 {
            // kh=1：sp 与 sm 同一行（s0 = sum+sp，sum' = s0-sp = sum）
            // SAFETY-free：直接用行和（sum 恒为 0）
            let sp = &*ring_rows[0];
            crate::simd_kernels::box_col_step_u16(&mut sum, sp, sp, dst_row, row_w, fx, have_scale);
            continue;
        }
        // sp（新行，槽 r%kh）与 sm（减行，槽 (r+1)%kh）分属不同槽位，
        // 用 split_at_mut 拿两个不相交借用
        let (sp, sm) = {
            let a_idx = r % kh;
            let b_idx = (r + 1) % kh;
            debug_assert_ne!(a_idx, b_idx);
            if a_idx < b_idx {
                let (a, b) = ring_rows.split_at_mut(b_idx);
                (&a[a_idx], &b[0])
            } else {
                let (a, b) = ring_rows.split_at_mut(a_idx);
                (&b[0], &a[b_idx])
            }
        };
        crate::simd_kernels::box_col_step_u16(&mut sum, sp, sm, dst_row, row_w, fx, have_scale);
    }
}

/// kh=3、C=3 流式特化：三行滚动 + 源行索引预计算（最常见路径）。
#[allow(clippy::too_many_arguments)]
fn box_streaming_kh3_c3(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    kw: usize,
    _dx1: usize,
    _dx2: usize,
    dy1: usize,
    row_w: usize,
    fx: crate::simd_kernels::box_filter::BoxFixed,
    have_scale: bool,
    border: &BorderPolicy<u8>,
) {
    debug_assert!(kw == 3 || kw == 5);
    // ring：3 行 u16 行和
    let mut ring: Vec<u16> = crate::simd_kernels::alloc_uninit(row_w * 3);
    let (mut r0, rest) = ring.split_at_mut(row_w);
    let (mut r1, mut r2) = rest.split_at_mut(row_w);
    let mut sum: Vec<u16> = vec![0; row_w];

    // 源行索引预计算（usize::MAX = 常量边框行）
    let total_mid = h + 2;
    let src_map: Vec<usize> = (0..total_mid)
        .map(|r| border_index(r as i64 - dy1 as i64, h, border))
        .collect();
    let const_val: Option<u16> = if let BorderPolicy::Constant(v) = border {
        Some((*v as u16) * (kw as u16))
    } else {
        None
    };

    // 生成第 mid_idx 个中间行（行和）写入 slot（kw∈{3,5}：无边框源直读）
    #[inline]
    fn fill_mid_row(
        slot: &mut [u16],
        mid_idx: usize,
        src: &[u8],
        src_map: &[usize],
        const_val: Option<u16>,
        w: usize,
        kw: usize,
        row_w: usize,
        border: &BorderPolicy<u8>,
    ) {
        match src_map[mid_idx] {
            usize::MAX => {
                if let Some(s) = const_val {
                    slot.fill(s);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                crate::simd_kernels::box_row_sum_u8_direct_cn(src_row, slot, w, kw, 3, *border);
            }
        }
    }

    // 预填中间行 0、1 进 r0、r1，累加进 SUM
    fill_mid_row(r0, 0, src, &src_map, const_val, w, kw, row_w, border);
    for (s, v) in sum.iter_mut().zip(&*r0) {
        *s = s.wrapping_add(*v);
    }
    fill_mid_row(r1, 1, src, &src_map, const_val, w, kw, row_w, border);
    for (s, v) in sum.iter_mut().zip(&*r1) {
        *s = s.wrapping_add(*v);
    }
    // 主循环：中间行 r 进 r2（滚动），输出行 r-2。
    // 滚动不变式：处理中间行 r 时，r0=行 r-2（减行），r1=行 r-1，r2=行 r（加行）。
    // 处理完后轮转绑定 (r0,r1,r2) <- (r1,r2,r0)（零数据拷贝）。
    for r in 2..total_mid {
        fill_mid_row(r2, r, src, &src_map, const_val, w, kw, row_w, border);
        let out_y = r - 2;
        let dst_row = &mut dst[out_y * row_w..(out_y + 1) * row_w];
        crate::simd_kernels::box_col_step_u16(&mut sum, r2, r0, dst_row, row_w, fx, have_scale);
        std::mem::swap(&mut r0, &mut r1);
        std::mem::swap(&mut r1, &mut r2);
    }
}

/// 列阶段单个行块：滑动累加 + 定点归约。
///
/// 处理输出行 `[y0, y0+rows)`，需要中间行 `[y0, y0 + rows + kh - 1)`。
#[allow(clippy::too_many_arguments)]
fn box_col_block(
    mid: &[u16],
    dst: &mut [u8],
    rows: usize,
    kh: usize,
    row_w: usize,
    fx: crate::simd_kernels::box_filter::BoxFixed,
    have_scale: bool,
    y0: usize,
) {
    // SUM 初始化为前 kh-1 行之和
    let mut sum: Vec<u16> = vec![0; row_w];
    for r in 0..kh - 1 {
        let row = &mid[(y0 + r) * row_w..(y0 + r + 1) * row_w];
        for (s, v) in sum.iter_mut().zip(row) {
            *s = s.wrapping_add(*v);
        }
    }
    // 滑动：输出行 y 用中间行 [y, y+kh)
    for y in 0..rows {
        let sp = &mid[(y0 + y + kh - 1) * row_w..(y0 + y + kh) * row_w];
        let sm = &mid[(y0 + y) * row_w..(y0 + y + 1) * row_w];
        let dst_row = &mut dst[y * row_w..(y + 1) * row_w];
        crate::simd_kernels::box_col_step_u16(&mut sum, sp, sm, dst_row, row_w, fx, have_scale);
    }
}

/// 填充带左右边框的行（REPLICATE/REFLECT_101/CONSTANT）。
fn fill_padded_row<const C: usize>(
    padded: &mut [u8],
    src_row: &[u8],
    w: usize,
    dx1: usize,
    dx2: usize,
    border: &BorderPolicy<u8>,
) {
    // 中间复制
    padded[dx1 * C..(dx1 + w) * C].copy_from_slice(&src_row[..w * C]);
    match border {
        BorderPolicy::Clamp => {
            for i in 0..dx1 {
                let s = &src_row[0..C];
                padded[i * C..(i + 1) * C].copy_from_slice(s);
            }
            for i in 0..dx2 {
                let s = &src_row[(w - 1) * C..w * C];
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].copy_from_slice(s);
            }
        }
        BorderPolicy::Reflect => {
            // REFLECT_101: gfedcb|abcdefgh|hgfedcb
            for i in 0..dx1 {
                let sx = border_index(i as i64 - dx1 as i64, w, border);
                let s = &src_row[sx * C..(sx + 1) * C];
                padded[i * C..(i + 1) * C].copy_from_slice(s);
            }
            for i in 0..dx2 {
                let sx = border_index(w as i64 + i as i64, w, border);
                let s = &src_row[sx * C..(sx + 1) * C];
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].copy_from_slice(s);
            }
        }
        BorderPolicy::Constant(v) => {
            for i in 0..dx1 {
                padded[i * C..(i + 1) * C].fill(*v);
            }
            for i in 0..dx2 {
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].fill(*v);
            }
        }
    }
}

/// 列方向边界索引（borderInterpolate 等价）。
/// `BorderPolicy::Constant` 越界返回 `usize::MAX` 哨兵（调用方需处理）。
fn border_index(sy: i64, h: usize, border: &BorderPolicy<u8>) -> usize {
    match border {
        BorderPolicy::Clamp => sy.clamp(0, h as i64 - 1) as usize,
        BorderPolicy::Reflect => {
            // REFLECT_101
            if h == 1 {
                return 0;
            }
            let mut i = sy;
            let period = (2 * h - 2) as i64;
            i = ((i % period) + period) % period;
            if i >= h as i64 {
                i = period - i;
            }
            i as usize
        }
        BorderPolicy::Constant(_) => {
            if sy < 0 || sy >= h as i64 {
                usize::MAX // 哨兵：常量边框行（调用方用常量行和替代）
            } else {
                sy as usize
            }
        }
    }
}

/// f32 版边界索引（语义同 [`border_index`]，仅 Clamp/Reflect 分支）。
fn border_index_f32(sy: i64, h: usize, border: &BorderPolicy<f32>) -> usize {
    match border {
        BorderPolicy::Clamp => sy.clamp(0, h as i64 - 1) as usize,
        BorderPolicy::Reflect => {
            if h == 1 {
                return 0;
            }
            let mut i = sy;
            let period = (2 * h - 2) as i64;
            i = ((i % period) + period) % period;
            if i >= h as i64 {
                i = period - i;
            }
            i as usize
        }
        BorderPolicy::Constant(_) => {
            if sy < 0 || sy >= h as i64 {
                usize::MAX
            } else {
                sy as usize
            }
        }
    }
}

/// 取像素，越界时按 `border` 处理。返回该像素的 C 通道值。
fn sample_pixel<T: Pixel, const C: usize>(
    data: &[T],
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    border: &BorderPolicy<T>,
) -> [T; C] {
    let xi = border.interpolate(x as isize, w);
    let yi = border.interpolate(y as isize, h);
    match (xi, yi) {
        (Some(xi), Some(yi)) => {
            let i = (yi * w + xi) * C;
            let mut px = [T::zero(); C];
            px.copy_from_slice(&data[i..i + C]);
            px
        }
        _ => match border {
            BorderPolicy::Constant(v) => [*v; C],
            _ => unreachable!("non-constant border always interpolates"),
        },
    }
}

/// 高斯滤波（对应 OpenCV `cv::GaussianBlur`）。
///
/// 可分离卷积：先水平后垂直。`sigma <= 0` 时按 OpenCV 自动计算
/// `sigma = ((n-1)*0.5 - 1)*0.3 + 0.8`。边界用 `border` 处理。
///
/// u8 路径与 OpenCV 逐位一致（ufixedpoint16 定点，SHIFT=8，
/// `getGaussianKernelBitExact` + 误差扩散定点核）；其他类型为浮点近似。
///
/// 复杂度 O(width·height·(ksize.w + ksize.h)·C)。
pub fn gaussian_blur<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    ksize: (usize, usize),
    sigma: f64,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    let (kw, kh) = ksize;
    // u8 特派：定点两阶段（对齐 GaussianBlurFixedPoint）
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        let (w, h) = (src.width(), src.height());
        if w > 0 && h > 0 && kw % 2 == 1 && kh % 2 == 1 {
            // 此分支 T == u8（TypeId 已验证），按位转换参数
            let border_u8: BorderPolicy<u8> = match &border {
                BorderPolicy::Clamp => BorderPolicy::Clamp,
                BorderPolicy::Reflect => BorderPolicy::Reflect,
                BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
            };
            let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
            let out = gaussian_blur_u8_fixed::<C>(src_bytes, w, h, kw, kh, sigma, &border_u8);
            // 此分支 T == u8（TypeId 已验证），按位重组
            let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
            return Image::from_vec(w, h, data).expect("u8 fixed path length invariant");
        }
    }
    let kx = gaussian_kernel(kw, sigma);
    let ky = gaussian_kernel(kh, sigma);
    let (w, h) = (src.width(), src.height());

    // 水平方向：src -> tmp
    let mut tmp: Image<T, C> = Image::new(w, h);
    {
        let src_data = src.data();
        let tmp_data = tmp.data_mut();
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f64; C];
                for (i, &k) in kx.iter().enumerate() {
                    let sx = x as i32 + i as i32 - (kw as i32 / 2);
                    let px: [T; C] = sample_pixel(src_data, w, h, sx, y as i32, &border);
                    for c in 0..C {
                        acc[c] += px[c].to_f32() as f64 * k;
                    }
                }
                let oi = (y * w + x) * C;
                for c in 0..C {
                    tmp_data[oi + c] = T::from_f32(acc[c] as f32);
                }
            }
        }
    }

    // 垂直方向：tmp -> out
    let mut out: Image<T, C> = Image::new(w, h);
    {
        let tmp_data = tmp.data();
        let out_data = out.data_mut();
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f64; C];
                for (ki, &kv) in ky.iter().enumerate() {
                    let sy = y as i32 + ki as i32 - kh as i32 / 2;
                    let px: [T; C] = sample_pixel(tmp_data, w, h, x as i32, sy, &border);
                    for c in 0..C {
                        acc[c] += px[c].to_f32() as f64 * kv;
                    }
                }
                let oi = (y * w + x) * C;
                for c in 0..C {
                    out_data[oi + c] = T::from_f32(acc[c] as f32);
                }
            }
        }
    }
    out
}

/// u8 输入的 f32 高斯模糊（对齐 OpenCV `sepFilter2D` f32 路径）。
///
/// 这是 `adaptiveThreshold(GaussianC)` 的内部路径（thresh.cpp:1936）：
/// OpenCV 对 CV_32F 输入不走 u8 定点，而是
/// `src.convertTo(CV_32F)` → `sepFilter2D`（f32 核 = bitexact f64 核转 f32，
/// 水平卷积 + 对称垂直卷积，f32 累加）→ `saturate_cast<uchar>`
/// （cvRound = round-half-even + 饱和）。
///
/// 边界固定 BORDER_REPLICATE（adaptiveThreshold 语义）。
/// 仅单通道（adaptiveThreshold 仅支持灰度）。
pub(crate) fn gaussian_blur_f32_bitexact(
    src: &Image<u8, 1>,
    ksize: (usize, usize),
    sigma: f64,
) -> Image<u8, 1> {
    let (kw, kh) = ksize;
    let (w, h) = (src.width(), src.height());
    let mut out = Image::new_uninit(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    // bitexact f64 核 → f32（getGaussianKernel(ktype=CV_32F)）
    let kx: Vec<f32> = crate::simd_kernels::gaussian::gaussian_kernel_bitexact(kw, sigma)
        .iter()
        .map(|&v| v as f32)
        .collect();
    let ky: Vec<f32> = if kh == kw {
        kx.clone()
    } else {
        crate::simd_kernels::gaussian::gaussian_kernel_bitexact(kh, sigma)
            .iter()
            .map(|&v| v as f32)
            .collect()
    };
    let src_bytes: &[u8] = src.data();
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());

    // 水平：u8 → f32（convertTo 精确）→ f32 卷积 → tmp
    let mut tmp: Vec<f32> = vec![0.0; w * h];
    let ax = kw / 2;
    {
        let row_fn = |y: usize, dst_row: &mut [f32]| {
            let src_row = &src_bytes[y * w..(y + 1) * w];
            for (x, d) in dst_row.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (i, &f) in kx.iter().enumerate() {
                    let sx = x as i64 + i as i64 - ax as i64;
                    // BORDER_REPLICATE
                    let sx = sx.clamp(0, w as i64 - 1) as usize;
                    acc += f * src_row[sx] as f32;
                }
                *d = acc;
            }
        };
        use rayon::prelude::*;
        if rayon::current_num_threads() <= 1 {
            for y in 0..h {
                row_fn(y, &mut tmp[y * w..(y + 1) * w]);
            }
        } else {
            tmp.par_chunks_mut(w).enumerate().for_each(|(y, r)| row_fn(y, r));
        }
    }

    // 垂直：对称核 f*(S[k]+S[-k]) 累加 → cvRound + 饱和 → u8
    {
        let col_fn = |y: usize, dst_row: &mut [u8]| {
            let k2 = kh / 2;
            let top = &tmp[(y * w)..(y + 1) * w];
            for x in 0..w {
                let mut acc = ky[k2] * top[x];
                for k in 1..=k2 {
                    let sy_up = (y as i64 - k as i64).clamp(0, h as i64 - 1) as usize;
                    let sy_dn = (y as i64 + k as i64).clamp(0, h as i64 - 1) as usize;
                    let f = ky[k2 + k];
                    acc += f * (tmp[sy_up * w + x] + tmp[sy_dn * w + x]);
                }
                // saturate_cast<uchar>：cvRound（round-half-even）+ 饱和到 [0,255]
                let r = crate::simd_kernels::bilateral::round_ties_even_f32(acc);
                dst_row[x] = r.clamp(0.0, 255.0) as u8;
            }
        };
        use rayon::prelude::*;
        if rayon::current_num_threads() <= 1 {
            for y in 0..h {
                col_fn(y, &mut out_bytes[y * w..(y + 1) * w]);
            }
        } else {
            out_bytes
                .par_chunks_mut(w)
                .enumerate()
                .for_each(|(y, r)| col_fn(y, r));
        }
    }
    out
}

/// u8 gaussian_blur 定点实现（对齐 `fixedSmoothInvoker`，单线程流式）。
///
/// 水平：u8 → u16 定点行（SHIFT=8）进 kylen 行 ring；
/// 垂直：ring 中 kylen 行 → u8 输出。边框行按 border 映射
/// （非 CONSTANT 复用源行；CONSTANT 越界行 = 0，由 hline 跳过语义保证）。
#[allow(clippy::too_many_arguments)]
fn gaussian_blur_u8_fixed<const C: usize>(
    src: &[u8],
    w: usize,
    h: usize,
    kw: usize,
    kh: usize,
    sigma: f64,
    border: &BorderPolicy<u8>,
) -> Image<u8, C> {
    let kx = crate::simd_kernels::gaussian::gaussian_kernel_u8_fixed(kw, sigma);
    let ky = if kh == kw {
        kx.clone()
    } else {
        crate::simd_kernels::gaussian::gaussian_kernel_u8_fixed(kh, sigma)
    };
    let row_w = w * C;
    // 121/14641 核特化判定（对齐 fixedSmoothInvoker 的特化派发）
    let is_121 = kx.len() == 3 && kx == [64, 128, 64] && ky.len() == 3 && ky == [64, 128, 64];
    let is_14641 =
        kx.len() == 5 && kx == [16, 64, 96, 64, 16] && ky.len() == 5 && ky == [16, 64, 96, 64, 16];
    let mut out: Image<u8, C> = Image::new_uninit(w, h);
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());

    // 14641 融合路径（w,h ≥ 5）：主体单遍融合 + 边框带 hline/vline
    //（对齐 OpenCV doCombined5N14641：跳过全图两阶段流式）
    if is_14641 && w >= 5 && h >= 5 {
        gaussian_blur_14641_combined::<C>(src, out_bytes, w, h, border);
        return out;
    }

    // 流式路径：ST 单块 / MT 行块并行（每块独立 ring，对齐
    // fixedSmoothInvoker 的 parallel_for Range(0, dst.rows) 分块）
    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        gaussian_streaming_block::<C>(
            src, out_bytes, w, h, 0, h, kh, row_w, &kx, &ky, is_121, is_14641, border,
        );
    } else {
        out_bytes
            .par_chunks_mut(row_w * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let y1 = y0 + d.len() / row_w;
                gaussian_streaming_block::<C>(
                    src, d, w, h, y0, y1, kh, row_w, &kx, &ky, is_121, is_14641, border,
                );
            });
    }
    out
}

/// 流式块：处理输出行 [y0, y1)，独立 kh 行 ring。
///
/// 中间行 r（r ∈ [y0, y1+kh-1)）对应源行 borderInterpolate(r - kh/2, h)；
/// 输出 y 的垂直窗口 = 中间行 [y, y+kh)。
#[allow(clippy::too_many_arguments)]
fn gaussian_streaming_block<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    y0: usize,
    y1: usize,
    kh: usize,
    row_w: usize,
    kx: &[u16],
    ky: &[u16],
    is_121: bool,
    is_14641: bool,
    border: &BorderPolicy<u8>,
) {
    let mut ring: Vec<u16> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut ring_rows: Vec<&mut [u16]> = ring.chunks_exact_mut(row_w).collect();
    // 源行映射：中间行 r 对应源行 borderInterpolate(r - kh/2, h)
    //（usize::MAX = 常量边框行 → 水平结果为 0）
    let mid_lo = y0;
    let mid_hi = y1 + kh - 1;
    let src_map: Vec<usize> = (0..mid_hi)
        .map(|r| border_index(r as i64 - (kh as i64 / 2), h, border))
        .collect();

    // 水平滤波第 mid_idx 个中间行写入 slot
    let fill_mid_row = |slot: &mut [u16], mid_idx: usize| {
        match src_map[mid_idx] {
            usize::MAX => slot.fill(0), // CONSTANT 越界：全 0
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                if is_121 {
                    crate::simd_kernels::hline_smooth_3n121(src_row, slot, w, C, *border);
                } else if is_14641 {
                    crate::simd_kernels::hline_smooth_5n14641(src_row, slot, w, C, *border);
                } else {
                    crate::simd_kernels::hline_smooth_u8(src_row, slot, w, C, kx, *border);
                }
            }
        }
    };

    // 预填 [mid_lo, mid_lo+kh-1)：按绝对槽位 (mid_lo+r)%kh 写入
    for r in 0..kh - 1 {
        fill_mid_row(ring_rows[(mid_lo + r) % kh], mid_lo + r);
    }
    // 流式主循环：中间行 r 进槽 r%kh，输出行 r-(kh-1)。
    for r in (mid_lo + kh - 1)..mid_hi {
        fill_mid_row(ring_rows[r % kh], r);
        let out_y = r - (kh - 1);
        let dst_row = &mut dst[(out_y - y0) * row_w..(out_y - y0 + 1) * row_w];
        // 收集窗口行：中间行 out_y + k（k ∈ 0..kh），槽位 (out_y+k)%kh
        // 借用方案：kh=3 用 split_at_mut 取不相交借用；
        // 其余逐槽位不可变借用收集进 Vec<&[u16]>。
        if is_121 {
            let i0 = out_y % 3;
            let i1 = (out_y + 1) % 3;
            let i2 = (out_y + 2) % 3;
            let (w0, w1, w2) = {
                let (a, b) = ring_rows.split_at_mut(1);
                match (i0, i1, i2) {
                    (0, 1, 2) => (&a[0], &b[0], &b[1]),
                    (0, 2, 1) => (&a[0], &b[1], &b[0]),
                    (1, 0, 2) => (&b[0], &a[0], &b[1]),
                    (1, 2, 0) => (&b[0], &b[1], &a[0]),
                    (2, 0, 1) => (&b[1], &a[0], &b[0]),
                    (2, 1, 0) => (&b[1], &b[0], &a[0]),
                    _ => unreachable!(),
                }
            };
            crate::simd_kernels::vline_smooth_3n121(&[w0, w1, w2], dst_row);
        } else if is_14641 {
            let idx: [usize; 5] = [
                out_y % 5,
                (out_y + 1) % 5,
                (out_y + 2) % 5,
                (out_y + 3) % 5,
                (out_y + 4) % 5,
            ];
            let mut window: Vec<&[u16]> = Vec::with_capacity(5);
            for &s in &idx {
                window.push(&*ring_rows[s]);
            }
            crate::simd_kernels::vline_smooth_5n14641(
                &[window[0], window[1], window[2], window[3], window[4]],
                dst_row,
            );
        } else {
            let window: Vec<&[u16]> = (0..kh).map(|k| &*ring_rows[(out_y + k) % kh]).collect();
            crate::simd_kernels::vline_smooth_u8(&window, dst_row, ky);
        }
    }
}

/// 14641 融合路径（w,h ≥ 5）：主体区二维融合 + 边框带 hline/vline。
///
/// 对齐 OpenCV `doCombined5N14641`：主体（x ∈ [2,w-2), y ∈ [2,h-2)）由
/// `smooth5N14641` 单遍完成；边框带（上下 2 行全宽 + 中间行左右 2 列）
/// 用 hline（边框列）+ vline（边框列）两阶段。
#[allow(clippy::too_many_arguments)]
fn gaussian_blur_14641_combined<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    border: &BorderPolicy<u8>,
) {
    let row_w = w * C;
    // 1. 主体融合（行块并行，每块独立 brows 滚动缓冲）
    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        crate::simd_kernels::smooth_5n14641::<C>(src, dst, w, h, 0, h);
    } else {
        dst.par_chunks_mut(row_w * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let y1 = y0 + d.len() / row_w;
                crate::simd_kernels::smooth_5n14641::<C>(src, d, w, h, y0, y1);
            });
    }
    // 2. 边框带：
    //    上下 2 行全宽（SIMD hline+vline）+ 中间行左右 2 列（标量逐点）
    let kh = 5usize;
    let total_mid = h + kh - 1;
    let src_map: Vec<usize> = (0..total_mid)
        .map(|r| border_index(r as i64 - (kh as i64 / 2), h, border))
        .collect();
    // 上下 2 行全宽：窗口 5 行 SIMD hline（含水平边框）→ SIMD vline
    let mut hbuf: Vec<u16> = vec![0; row_w * 5];
    let top_rows: Vec<usize> = (0..h).filter(|&y| y < 2 || y + 2 >= h).collect();
    for &out_y in &top_rows {
        for k in 0..5 {
            let sy = src_map[out_y + k];
            let hb = &mut hbuf[k * row_w..(k + 1) * row_w];
            if sy == usize::MAX {
                hb.fill(0);
            } else {
                crate::simd_kernels::hline_smooth_5n14641(
                    &src[sy * row_w..(sy + 1) * row_w],
                    hb,
                    w,
                    C,
                    *border,
                );
            }
        }
        let rows: [&[u16]; 5] = [
            &hbuf[0..row_w],
            &hbuf[row_w..2 * row_w],
            &hbuf[2 * row_w..3 * row_w],
            &hbuf[3 * row_w..4 * row_w],
            &hbuf[4 * row_w..5 * row_w],
        ];
        crate::simd_kernels::vline_smooth_5n14641(
            &rows,
            &mut dst[out_y * row_w..(out_y + 1) * row_w],
        );
    }
    // 中间行左右 2 列：x ∈ {0,1,w-2,w-1}
    // 滚动缓存 5 窗口行的边框列 hline 值（slot = 窗口行号 % 5），
    // 每输出行只计算 1 个新窗口行
    let border_cols: Vec<usize> = if w < 4 {
        (0..w).collect()
    } else {
        vec![0, 1, w - 2, w - 1]
    };
    let ncols = border_cols.len();
    let hline_at = |sy: usize, x: usize, c: usize| -> u16 {
        let mut s = 0u16;
        for (k, wgt) in [1u16, 4, 6, 4, 1].iter().enumerate() {
            let sx = x as i64 + k as i64 - 2;
            let v: u16 = if sx >= 0 && (sx as usize) < w {
                src[sy * row_w + sx as usize * C + c] as u16
            } else {
                match border.interpolate(sx as isize, w) {
                    Some(sxi) => src[sy * row_w + sxi * C + c] as u16,
                    None => 0,
                }
            };
            s += wgt * v;
        }
        s << 4
    };
    let mut cache: Vec<u16> = vec![0; ncols * C * 5];
    let mid_lo = 2;
    let mid_hi = h.saturating_sub(2);
    if mid_lo < mid_hi {
        // 预填窗口行 mid_lo-2 .. mid_lo+2（绝对槽位 (r) % 5）
        for r in 0..5 {
            let wr = mid_lo + r - 2;
            let slot = r % 5;
            let sy = src_map[wr + 2];
            for (ci, &x) in border_cols.iter().enumerate() {
                for c in 0..C {
                    cache[(slot * ncols + ci) * C + c] = if sy == usize::MAX {
                        0
                    } else {
                        hline_at(sy, x, c)
                    };
                }
            }
        }
        for out_y in mid_lo..mid_hi {
            // 新窗口行 out_y+2（槽位 (out_y+2) % 5）
            let wr = out_y + 2;
            let slot = wr % 5;
            let sy = src_map[wr + 2];
            for (ci, &x) in border_cols.iter().enumerate() {
                for c in 0..C {
                    cache[(slot * ncols + ci) * C + c] = if sy == usize::MAX {
                        0
                    } else {
                        hline_at(sy, x, c)
                    };
                }
            }
            // vline：窗口行 out_y-2..out_y+2 → 槽位 (out_y+k-2) % 5
            for ci in 0..ncols {
                for c in 0..C {
                    let mut acc: u32 = 1 << 11;
                    for (k, wgt) in [1u32, 4, 6, 4, 1].iter().enumerate() {
                        let slot = (out_y + k - 2) % 5;
                        acc += wgt * cache[(slot * ncols + ci) * C + c] as u32;
                    }
                    let v = acc >> 12;
                    dst[out_y * row_w + border_cols[ci] * C + c] = v.min(0xFF) as u8;
                }
            }
        }
    }
}

/// 计算一维高斯核（OpenCV `getGaussianKernel` 的浮点近似）。
fn gaussian_kernel(n: usize, sigma: f64) -> Vec<f64> {
    let sigma = if sigma <= 0.0 {
        ((n as f64 - 1.0) * 0.5 - 1.0) * 0.3 + 0.8
    } else {
        sigma
    };
    let mut kernel = vec![0.0f64; n];
    let mut sum = 0.0;
    let center = (n - 1) as f64 / 2.0;
    for (i, k) in kernel.iter_mut().enumerate() {
        let x = i as f64 - center;
        *k = (-0.5 * (x / sigma) * (x / sigma)).exp();
        sum += *k;
    }
    for k in kernel.iter_mut() {
        *k /= sum;
    }
    kernel
}

/// 中值滤波（对应 OpenCV `cv::medianBlur`）。
///
/// 对每个像素取 `ksize x ksize` 窗口的中值。`ksize` 必须为奇数。
/// 边界用 `BORDER_REPLICATE`（Clamp），与 OpenCV `medianBlur` 一致。
///
/// 派发对齐 OpenCV5 `medianBlur`：
/// - ksize 3：SortNet（排序网络，所有类型）
/// - ksize 5：SortNet（非 u8 / 2 通道 / >4 通道）；u8 1/3/4 通道走直方图
/// - ksize > 5：直方图（u8 单通道）
///
/// u8 3x3 走 SIMD（AVX512 → AVX2 → 标量）+ rayon 行并行。
///
/// # Panics
/// `ksize` 为偶数时 panic。
///
/// 复杂度 O(width·height·ksize²·C)。
pub fn median_blur<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    ksize: usize,
) -> Image<T, C> {
    assert!(ksize % 2 == 1, "ksize must be odd");
    let (w, h) = (src.width(), src.height());
    if ksize <= 1 || w == 0 || h == 0 {
        return src.clone();
    }
    // u8 3x3 特派：SIMD + rayon 行并行（提前分支，避免多余的零填充分配）
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() && ksize == 3 {
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let mut out_u8: Image<u8, C> = Image::new_uninit(w, h);
        let dst_bytes: &mut [u8] = out_u8.data_mut();
        if rayon::current_num_threads() > 1 && h > 1 {
            use rayon::prelude::*;
            let row_bytes = w * C;
            // 行块并行（对齐库内惯例 MIN_ROWS_PER_BLOCK）：单行仅 ~0.5µs，
            // 逐行派发时 rayon 调度开销反而劣化吞吐
            const MIN_ROWS_PER_BLOCK: usize = 16;
            let chunk = MIN_ROWS_PER_BLOCK.min(h);
            dst_bytes
                .par_chunks_mut(row_bytes * chunk)
                .enumerate()
                .for_each(|(bi, d)| {
                    let y_start = bi * chunk;
                    let rows = d.len() / row_bytes;
                    for y in y_start..y_start + rows {
                        let dst_row = &mut d[(y - y_start) * row_bytes..(y - y_start + 1) * row_bytes];
                        let y0 = y.saturating_sub(1) * row_bytes;
                        let y1 = y * row_bytes;
                        let y2 = (y + 1).min(h - 1) * row_bytes;
                        crate::simd_kernels::median::median_row_u8_3x3(
                            src_bytes, dst_row, w, C, y0, y1, y2,
                        );
                    }
                });
        } else {
            crate::simd_kernels::median_blur_u8_3x3(src_bytes, dst_bytes, w, h, C);
        }
        // 此分支 T == u8（TypeId 已验证），按位重组
        let data: Vec<T> = crate::simd_kernels::vec_u8_as(out_u8.into_vec());
        return Image::from_vec(w, h, data).expect("u8 median path length invariant");
    }
    let mut out: Image<T, C> = Image::new(w, h);
    // 其他类型/ksize：标量
    crate::simd_kernels::median::median_blur_scalar::<T, C>(
        src.data(),
        out.data_mut(),
        w,
        h,
        ksize,
        &BorderPolicy::Clamp,
    );
    out
}

/// 双边滤波（对应 OpenCV `cv::bilateralFilter`）。
///
/// 同时做空间域与值域高斯加权：`w = exp(-|Δpos|²/2σs²)·exp(-|Δval|²/2σc²)`。
///
/// 对齐 OpenCV 语义：
/// - `d <= 0` 时 `radius = round(σs·1.5)`，否则 `d/2`，最小 1
/// - 空间核为圆形（`r > radius` 的点跳过）
/// - u8：color LUT（256·C 项）+ `cvRound`（round-half-even）输出
/// - f32：插值 expLUT + NaN 语义（邻域 NaN 跳过；中心 NaN 时输出邻域均值）
/// - `σc` 或 `σs` ≤ 1e-6 时直接拷贝
/// - 仅支持 C=1/3（对齐 OpenCV CV_8UC1/8UC3/32FC1/32FC3）
///
/// # Panics
/// `C` 不为 1/3 时 panic。
///
/// 复杂度 O(width·height·ksize²·C)。
pub fn bilateral_filter<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    d: i32,
    sigma_color: f64,
    sigma_space: f64,
    border: BorderPolicy<T>,
) -> Image<T, C> {
    assert!(C == 1 || C == 3, "bilateral_filter 仅支持 1/3 通道");
    let (w, h) = (src.width(), src.height());
    if w == 0 || h == 0 {
        return src.clone();
    }
    const EPS: f64 = 1e-6;
    if sigma_color <= EPS || sigma_space <= EPS {
        return src.clone();
    }
    let radius = crate::simd_kernels::bilateral::bilateral_radius(d, sigma_space);
    let mut out: Image<T, C> = Image::new_uninit(w, h);
    let is_u8 = std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>();
    let is_f32 = std::any::TypeId::of::<T>() == std::any::TypeId::of::<f32>();
    assert!(
        is_u8 || is_f32,
        "bilateral_filter 仅支持 u8/f32（对齐 OpenCV）"
    );
    if is_u8 {
        let border_u8: BorderPolicy<u8> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
        };
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let dst_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        bilateral_u8_impl::<C>(src_bytes, dst_bytes, w, h, radius, sigma_color, sigma_space, &border_u8);
    } else {
        let border_f32: BorderPolicy<f32> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, f32>(*v)),
        };
        let src_f32: &[f32] = bytemuck::cast_slice(src.data());
        let dst_f32: &mut [f32] = bytemuck::cast_slice_mut(out.data_mut());
        bilateral_f32_impl::<C>(src_f32, dst_f32, w, h, radius, sigma_color, sigma_space, &border_f32);
    }
    out
}

/// u8 双边滤波实现：padded 物化 + 参数构建 + 行并行派发。
#[allow(clippy::too_many_arguments)]
fn bilateral_u8_impl<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    radius: usize,
    sigma_color: f64,
    sigma_space: f64,
    border: &BorderPolicy<u8>,
) {
    let pw = w + 2 * radius;
    let stride = pw * C;
    // padded 物化（copyMakeBorder 等价）
    let mut padded = crate::simd_kernels::alloc_uninit(stride * (h + 2 * radius));
    for py in 0..h + 2 * radius {
        let sy = border_index(py as i64 - radius as i64, h, border);
        let src_row = &src[sy * w * C..(sy + 1) * w * C];
        fill_padded_row::<C>(
            &mut padded[py * stride..(py + 1) * stride],
            src_row,
            w,
            radius,
            radius,
            border,
        );
    }
    let params = crate::simd_kernels::bilateral::bilateral_params_8u(
        radius,
        C,
        stride,
        sigma_color,
        sigma_space,
    );
    // 行并行（对齐 OpenCV parallel_for_ 粒度 total/65536 → 16 行块）
    let row_bytes = w * C;
    let run_row = |y: usize, dst_row: &mut [u8]| {
        crate::simd_kernels::bilateral::bilateral_row_u8(&padded, dst_row, w, C, y, &params);
    };
    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        for y in 0..h {
            run_row(y, &mut dst[y * row_bytes..(y + 1) * row_bytes]);
        }
    } else {
        dst.par_chunks_mut(row_bytes * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let rows = d.len() / row_bytes;
                for y in y0..y0 + rows {
                    let dst_row = &mut d[(y - y0) * row_bytes..(y - y0 + 1) * row_bytes];
                    run_row(y, dst_row);
                }
            });
    }
}

/// f32 双边滤波实现（标量内核 + 行并行；对齐 OpenCV 32f 路径）。
#[allow(clippy::too_many_arguments)]
fn bilateral_f32_impl<const C: usize>(
    src: &[f32],
    dst: &mut [f32],
    w: usize,
    h: usize,
    radius: usize,
    sigma_color: f64,
    sigma_space: f64,
    border: &BorderPolicy<f32>,
) {
    // 值域（对齐 OpenCV minMaxLoc）
    let mut min_val = f32::INFINITY;
    let mut max_val = f32::NEG_INFINITY;
    for &v in src {
        if v.is_nan() {
            continue;
        }
        if v < min_val {
            min_val = v;
        }
        if v > max_val {
            max_val = v;
        }
    }
    if (max_val - min_val).abs() < f32::EPSILON {
        // 常量图：直接拷贝（对齐 OpenCV）
        dst.copy_from_slice(src);
        return;
    }
    let pw = w + 2 * radius;
    let stride = pw * C;
    // padded 物化
    let mut padded = crate::simd_kernels::alloc_uninit(stride * (h + 2 * radius));
    for py in 0..h + 2 * radius {
        let sy = border_index_f32(py as i64 - radius as i64, h, border);
        let src_row = &src[sy * w * C..(sy + 1) * w * C];
        fill_padded_row_f32::<C>(
            &mut padded[py * stride..(py + 1) * stride],
            src_row,
            w,
            radius,
            radius,
            border,
        );
    }
    let params = crate::simd_kernels::bilateral::bilateral_params_32f(
        radius,
        C,
        stride,
        sigma_color,
        sigma_space,
        min_val,
        max_val,
    );
    let row_elems = w * C;
    let run_row = |y: usize, dst_row: &mut [f32]| {
        crate::simd_kernels::bilateral::bilateral_row_f32_scalar(
            &padded, dst_row, w, C, y, &params,
        )
    };
    use rayon::prelude::*;
    const MIN_ROWS_PER_BLOCK: usize = 16;
    let chunk = MIN_ROWS_PER_BLOCK.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        for y in 0..h {
            run_row(y, &mut dst[y * row_elems..(y + 1) * row_elems]);
        }
    } else {
        dst.par_chunks_mut(row_elems * chunk)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * chunk;
                let rows = d.len() / row_elems;
                for y in y0..y0 + rows {
                    let dst_row = &mut d[(y - y0) * row_elems..(y - y0 + 1) * row_elems];
                    run_row(y, dst_row);
                }
            });
    }
}

/// f32 带左右边框的行填充。
#[allow(clippy::too_many_arguments)]
fn fill_padded_row_f32<const C: usize>(
    padded: &mut [f32],
    src_row: &[f32],
    w: usize,
    dx1: usize,
    dx2: usize,
    border: &BorderPolicy<f32>,
) {
    padded[dx1 * C..(dx1 + w) * C].copy_from_slice(&src_row[..w * C]);
    match border {
        BorderPolicy::Clamp => {
            for i in 0..dx1 {
                let s = &src_row[0..C];
                padded[i * C..(i + 1) * C].copy_from_slice(s);
            }
            for i in 0..dx2 {
                let s = &src_row[(w - 1) * C..w * C];
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].copy_from_slice(s);
            }
        }
        BorderPolicy::Reflect => {
            for i in 0..dx1 {
                let sx = border_index_f32(i as i64 - dx1 as i64, w, border);
                let s = &src_row[sx * C..(sx + 1) * C];
                padded[i * C..(i + 1) * C].copy_from_slice(s);
            }
            for i in 0..dx2 {
                let sx = border_index_f32(w as i64 + i as i64, w, border);
                let s = &src_row[sx * C..(sx + 1) * C];
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].copy_from_slice(s);
            }
        }
        BorderPolicy::Constant(v) => {
            for i in 0..dx1 {
                padded[i * C..(i + 1) * C].fill(*v);
            }
            for i in 0..dx2 {
                padded[(dx1 + w + i) * C..(dx1 + w + i + 1) * C].fill(*v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_filter_3x3_normalize_center() {
        // 3x3 全 9 的图，3x3 窗口均值 = 9
        let img = Image::<u8, 1>::from_pixel(3, 3, [9]);
        let out = box_filter(&img, (3, 3), (-1, -1), true, BorderPolicy::Clamp);
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[9]);
            }
        }
    }

    #[test]
    fn box_filter_identity_1x1() {
        let img = Image::<u8, 1>::from_fn(3, 2, |x, y| [(x + y * 3) as u8]);
        let out = box_filter(&img, (1, 1), (-1, -1), true, BorderPolicy::Clamp);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[(x + y * 3) as u8]);
            }
        }
    }

    #[test]
    fn box_filter_manual() {
        // 2x2 图 [1 2; 3 4]，3x3 窗口中心，Clamp 边界
        // 中心 (0,0)：窗口覆盖 [0,0],[0,1],[1,0],[1,1] 各多次
        // sum = 1+2+3+4 + 边界复制
        let img = Image::<u8, 1>::from_fn(2, 2, |x, y| [(x + y * 2 + 1) as u8]);
        // 1 2
        // 3 4
        let out = box_filter(&img, (3, 3), (-1, -1), true, BorderPolicy::Clamp);
        // 中心 (0,0)：窗口 x∈[-1,1], y∈[-1,1]，Clamp 到 [0,1]
        // 像素：[(0,0),(0,1),(0,0),(1,0),(1,1),(1,0),(0,0),(0,1),(0,0)]
        // = 1,2,1,3,4,3,1,2,1 = 18, /9 = 2
        assert_eq!(out.pixel(0, 0).unwrap(), &[2]);
    }

    #[test]
    fn gaussian_blur_identity_1x1() {
        let img = Image::<u8, 1>::from_pixel(3, 3, [50]);
        let out = gaussian_blur(&img, (1, 1), 0.0, BorderPolicy::Clamp);
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[50]);
            }
        }
    }

    #[test]
    fn gaussian_blur_3x3_constant() {
        // 全 100 的图，3x3 高斯核，输出仍为 100（核归一化）
        let img = Image::<u8, 1>::from_pixel(3, 3, [100]);
        let out = gaussian_blur(&img, (3, 3), 0.0, BorderPolicy::Clamp);
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[100]);
            }
        }
    }

    #[test]
    fn gaussian_kernel_sums_to_one() {
        let k = gaussian_kernel(5, 0.0);
        let sum: f64 = k.iter().sum();
        assert!((sum - 1.0).abs() < 1e-12);
        // 对称
        assert!((k[0] - k[4]).abs() < 1e-12);
        assert!((k[1] - k[3]).abs() < 1e-12);
        // 中心最大
        assert!(k[2] > k[1] && k[1] > k[0]);
    }

    #[test]
    fn median_blur_3x3_known() {
        // 3x3 图，中心像素的 3x3 窗口 = 全图，中值 = 排序后第 5 个
        let img = Image::<u8, 1>::from_fn(3, 3, |x, y| [x as u8 + y as u8 * 3]);
        // 值: 0 1 2 / 3 4 5 / 6 7 8，排序后中值 = 4
        let out = median_blur(&img, 3);
        assert_eq!(out.pixel(1, 1).unwrap(), &[4]);
    }

    #[test]
    fn median_blur_3x3_impulse() {
        // 中心一个亮点，3x3 中值应被周围暗像素淹没
        let mut img = Image::<u8, 1>::from_pixel(3, 3, [0]);
        img.pixel_mut(1, 1).unwrap().copy_from_slice(&[255]);
        let out = median_blur(&img, 3);
        // 中心 3x3 窗口含 8 个 0 和 1 个 255，中值 = 0
        assert_eq!(out.pixel(1, 1).unwrap(), &[0]);
    }

    #[test]
    fn median_blur_3x3_border_clamp() {
        // 边界用 Clamp（BORDER_REPLICATE）：左上角窗口 = 左上 2x2 复制
        let img = Image::<u8, 1>::from_fn(3, 3, |x, y| [x as u8 + y as u8 * 3]);
        // 值: 0 1 2 / 3 4 5 / 6 7 8
        // 左上角 (0,0) 窗口（Clamp）: 0 0 1 / 0 0 1 / 3 3 4 → 排序 [0,0,0,0,1,1,3,3,4] 中值=1
        let out = median_blur(&img, 3);
        assert_eq!(out.pixel(0, 0).unwrap(), &[1]);
    }

    #[test]
    fn median_blur_3x3_rgb() {
        // 3 通道，逐通道独立中值
        let img = Image::<u8, 3>::from_fn(3, 3, |x, y| {
            [x as u8, y as u8, (x + y) as u8]
        });
        let out = median_blur(&img, 3);
        // 中心 (1,1)：R 通道窗口 [0,1,2,0,1,2,0,1,2] 中值=1；G 通道 [0,0,0,1,1,1,2,2,2] 中值=1；
        // B 通道 (x+y) 窗口 [0,1,2,1,2,3,2,3,4] 排序 [0,1,1,2,2,2,3,3,4] 中值=2
        assert_eq!(out.pixel(1, 1).unwrap(), &[1, 1, 2]);
    }

    #[test]
    fn median_blur_ksize1_copy() {
        let img = Image::<u8, 1>::from_fn(3, 2, |x, y| [(x + y * 3) as u8]);
        let out = median_blur(&img, 1);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(out.pixel(x, y).unwrap(), &[(x + y * 3) as u8]);
            }
        }
    }

    #[test]
    fn median_blur_5x5_known() {
        // 5x5 图，中心 5x5 窗口 = 全图，中值 = 排序后第 13 个
        let img = Image::<u8, 1>::from_fn(5, 5, |x, y| [x as u8 + y as u8 * 5]);
        // 值 0..24，中值 = 12
        let out = median_blur(&img, 5);
        assert_eq!(out.pixel(2, 2).unwrap(), &[12]);
    }

    #[test]
    fn median_blur_f32() {
        // f32 类型走 SortNet
        let img = Image::<f32, 1>::from_fn(3, 3, |x, y| [x as f32 + y as f32 * 3.0]);
        let out = median_blur(&img, 3);
        assert_eq!(out.pixel(1, 1).unwrap(), &[4.0]);
    }

    #[test]
    #[should_panic]
    fn median_blur_even_ksize_panics() {
        let img = Image::<u8, 1>::from_pixel(3, 3, [0]);
        let _ = median_blur(&img, 4);
    }
}
