//! 梯度（对应 OpenCV `modules/imgproc` 的 `Sobel`/`Scharr`/`Laplacian` 与
//! `modules/core` 的 `magnitude`/`phase`/`cartToPolar`）。
//!
//! 公开 API 一律 RGB（铁律 6）。
//!
//! Sobel/Scharr 8U→16S 对齐 OpenCV bit-exact 整数路径：
//! 行 u8→i32（对称 [1,2,1]/[3,10,3] 或反对称 [-1,0,1]），
//! 列 i32→i16 饱和（对称 [1,2,1]/[3,10,3] 或反对称 [-1,0,1]）。

use crate::core::border::BorderPolicy;
use crate::core::image::{Image, Pixel};
use crate::imgproc::morphology::border_index;

/// Sobel 导数核（ksize=3 特化，对齐 `getSobelKernels`）。
///
/// order=0 → [1,2,1]，order=1 → [-1,0,1]。
const fn sobel_k3(order: u32) -> [i32; 3] {
    match order {
        0 => [1, 2, 1],
        _ => [-1, 0, 1],
    }
}

/// Scharr 导数核（对齐 `getScharrKernels`）。
///
/// order=0 → [3,10,3]，order=1 → [-1,0,1]。
const fn scharr_k(order: u32) -> [i32; 3] {
    match order {
        0 => [3, 10, 3],
        _ => [-1, 0, 1],
    }
}

/// 可分离导数滤波主入口（u8 → i16，ksize=3 核，cn=1）。
///
/// `kx`/`ky` 为 3-tap 核（中心化存储 [k[-1], k[0], k[1]]）。
/// 行阶段 u8 → i32，列阶段 i32 → i16 饱和（对齐 OpenCV bit-exact 路径）。
fn deriv_sep_u8_to_i16(
    src: &[u8],
    dst: &mut [i16],
    w: usize,
    h: usize,
    kx: [i32; 3],
    ky: [i32; 3],
    border: &BorderPolicy<u8>,
) {
    if w == 0 || h == 0 {
        return;
    }
    let row_w = w;
    // 核对称性：k[0] == k[2] 为对称（k[-1]=k[1]=kx[0]，k[0]=kx[1]）；
    // k[0] == -k[2] 且 k[1] == 0 为反对称（k[1]=kx[0]）
    let symm_x = kx[0] == kx[2];
    let symm_y = ky[0] == ky[2];
    // 中心化参数：对称核 (k0, k1) = (k[0], k[±1])；反对称核 k1 = k[+1]（kx[2]）
    let (xk0, xk1) = if symm_x { (kx[1], kx[0]) } else { (0, kx[2]) };
    let (yk0, yk1) = if symm_y { (ky[1], ky[0]) } else { (0, ky[2]) };

    // 流式两段：mid 滚动 ring（3 行，L1/L2 常驻），行阶段领先列阶段 1 行。
    // mid 行 v ∈ [-1, h]；输出行 y 消费 mid 行 [y-1, y+1]。
    // 行阶段单线程（列阶段依赖顺序），列阶段 16 行块 rayon 并行时改用分块全量 mid。
    use rayon::prelude::*;
    const MIN_ROWS: usize = 256;
    let chunk = MIN_ROWS.min(h);
    let nblocks = h.div_ceil(chunk);
    let parallel = nblocks > 1 && rayon::current_num_threads() > 1;

    // 行滤波单行（v ∈ [-1, h]）→ mid 行
    let row_filter = |v: i64, mrow: &mut [i32]| {
        match border_index(v, h, border) {
            usize::MAX => {
                // Constant 边框：行值 = k0*c + k1*(c + c)（对称）或 k1*(c - c) = 0（反对称）
                if let BorderPolicy::Constant(v) = border {
                    let c = *v as i32;
                    let val = if symm_x { xk0 * c + xk1 * 2 * c } else { 0 };
                    mrow.fill(val);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                if symm_x {
                    crate::simd_kernels::deriv_row_symm_k3_cn1(
                        src_row, mrow, w, xk0, xk1, *border,
                    );
                } else {
                    crate::simd_kernels::deriv_row_asymm_k3_cn1(src_row, mrow, w, xk1, *border);
                }
            }
        }
    };
    // 列滤波单行：mid 行 [y-1, y+1] → dst 行
    let col_filter = |rows: [&[i32]; 3], dst_row: &mut [i16]| {
        if symm_y {
            crate::simd_kernels::deriv_col_symm_k3(rows, dst_row, w, yk0, yk1, 0);
        } else {
            crate::simd_kernels::deriv_col_asymm_k3(rows, dst_row, w, yk1, 0);
        }
    };

    if parallel {
        // 并行路径：行块独立流式 ring（块内 3 行 mid ring，L1 常驻，
        // 不物化全块 mid——64 核下全块 mid 物化引发 DRAM/NUMA 带宽争抢，
        // 实测 4 线程 14.8K → 64 线程 7.6K 退化；流式后 mid 流量进 L1）
        dst.par_chunks_mut(row_w * chunk).enumerate().for_each(|(bi, d)| {
            let y0 = bi * chunk;
            let rows = d.len() / row_w;
            let mut ring: Vec<i32> = crate::simd_kernels::alloc_uninit(row_w * 3);
            // 预填 mid 行 y0-1 和 y0
            {
                let (r0, rest) = ring.split_at_mut(row_w);
                let (r1, _) = rest.split_at_mut(row_w);
                row_filter(y0 as i64 - 1, r0);
                row_filter(y0 as i64, r1);
            }
            for y in 0..rows {
                // 滤 mid 行 y0+y+1 → ring 槽 (y+2) % 3
                let slot = ((y + 2) % 3) * row_w;
                let mrow = &mut ring[slot..slot + row_w];
                row_filter(y0 as i64 + y as i64 + 1, mrow);
                // 输出行 y：mid 行 [y-1, y+1] = ring 槽 [y%3, (y+1)%3, (y+2)%3]
                let m0 = &ring[(y % 3) * row_w..(y % 3) * row_w + row_w];
                let m1 = &ring[((y + 1) % 3) * row_w..((y + 1) % 3) * row_w + row_w];
                let m2 = &ring[((y + 2) % 3) * row_w..((y + 2) % 3) * row_w + row_w];
                let dst_row = &mut d[y * row_w..(y + 1) * row_w];
                col_filter([m0, m1, m2], dst_row);
            }
        });
        return;
    }

    // 串行流式路径：3 行 ring，行阶段领先 1 行
    let mut ring: Vec<i32> = crate::simd_kernels::alloc_uninit(row_w * 3);
    // 预填 mid 行 -1 和 0
    {
        let (r0, rest) = ring.split_at_mut(row_w);
        let (r1, _) = rest.split_at_mut(row_w);
        row_filter(-1, r0);
        row_filter(0, r1);
    }
    for y in 0..h {
        // 滤 mid 行 y+1 → ring 槽 (y+2) % 3
        if y + 1 < h + 1 {
            let slot = ((y + 2) % 3) * row_w;
            let mrow = &mut ring[slot..slot + row_w];
            row_filter(y as i64 + 1, mrow);
        }
        // 输出行 y：mid 行 [y-1, y+1] = ring 槽 [(y)%3, (y+1)%3, (y+2)%3]
        let m0 = &ring[(y % 3) * row_w..(y % 3) * row_w + row_w];
        let m1 = &ring[((y + 1) % 3) * row_w..((y + 1) % 3) * row_w + row_w];
        let m2 = &ring[((y + 2) % 3) * row_w..((y + 2) % 3) * row_w + row_w];
        let dst_row = &mut dst[y * row_w..(y + 1) * row_w];
        col_filter([m0, m1, m2], dst_row);
    }
}

/// Sobel 算子（对应 OpenCV `cv::Sobel`，ksize=3，8U → 16S）。
///
/// `dx`/`dy` 为导数阶数（dx+dy == 1）。`scale` 作用于平滑核（对齐 OpenCV：
/// dx==0 时缩 kx，否则缩 ky）。
///
/// # Panics
/// - `dx + dy != 1`
/// - 空图像
pub fn sobel<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    dx: u32,
    dy: u32,
    scale: f64,
    border: BorderPolicy<T>,
) -> Image<i16, C> {
    assert!(dx + dy == 1, "dx + dy must be 1, got dx={dx} dy={dy}");
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "sobel: empty image");
    let mut kx = sobel_k3(dx);
    let mut ky = sobel_k3(dy);
    // 对齐 OpenCV：scale 作用于平滑部分（dx==0 → kx 是平滑核）
    if scale != 1.0 {
        if dx == 0 {
            for k in &mut kx {
                *k = (*k as f64 * scale).round() as i32;
            }
        } else {
            for k in &mut ky {
                *k = (*k as f64 * scale).round() as i32;
            }
        }
    }
    deriv_dispatch(src, kx, ky, border)
}

/// Scharr 算子（对应 OpenCV `cv::Scharr`，8U → 16S）。
///
/// `dx`/`dy` 为导数方向（dx+dy == 1）。
///
/// # Panics
/// - `dx + dy != 1`
/// - 空图像
pub fn scharr<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    dx: u32,
    dy: u32,
    scale: f64,
    border: BorderPolicy<T>,
) -> Image<i16, C> {
    assert!(dx + dy == 1, "dx + dy must be 1, got dx={dx} dy={dy}");
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "scharr: empty image");
    let mut kx = scharr_k(dx);
    let mut ky = scharr_k(dy);
    if scale != 1.0 {
        if dx == 0 {
            for k in &mut kx {
                *k = (*k as f64 * scale).round() as i32;
            }
        } else {
            for k in &mut ky {
                *k = (*k as f64 * scale).round() as i32;
            }
        }
    }
    deriv_dispatch(src, kx, ky, border)
}

/// Sobel 5×5 算子（对应 `cv::Sobel` ksize=5，8U → 16S，标量整数路径）。
///
/// 核（对齐 `getSobelKernels` ksize=5）：order=0 → [1,4,6,4,1]，
/// order=1 → [-1,-2,0,2,1]。`scale` 作用于平滑核（对齐 3×3 版本）。
/// 行阶段 u8 → i32，列阶段 i32 → i16 饱和（与 3×3 bit-exact 路径一致）。
///
/// # Panics
/// - `dx + dy != 1`
/// - 空图像
pub fn sobel5<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    dx: u32,
    dy: u32,
    scale: f64,
    border: BorderPolicy<T>,
) -> Image<i16, C> {
    assert!(dx + dy == 1, "dx + dy must be 1, got dx={dx} dy={dy}");
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "sobel5: empty image");
    // 5-tap 核（中心化 [k[-2], k[-1], k[0], k[1], k[2]]）
    let smooth = [1i32, 4, 6, 4, 1];
    let deriv = [-1i32, -2, 0, 2, 1];
    let (mut kx, mut ky) = if dx == 1 {
        (deriv, smooth)
    } else {
        (smooth, deriv)
    };
    // scale 作用于平滑核（对齐 OpenCV：dx==1 时 ky 是平滑核）
    if scale != 1.0 {
        let k = if dx == 1 { &mut ky } else { &mut kx };
        for v in k.iter_mut() {
            *v = (*v as f64 * scale).round() as i32;
        }
    }
    // 标量 5-tap 分离滤波（u8 → i32 → i16 饱和）
    let mut out: Image<i16, C> = Image::new(w, h);
    let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
    let out_data = out.data_mut();
    let row_w = w * C;
    let ph = h + 4;
    // 行阶段：src 行 → mid 行（i32）
    let mut mid: Vec<i32> = vec![0; row_w * ph];
    for r in 0..ph {
        let sy = border_index(r as i64 - 2, h, &border);
        if sy == usize::MAX {
            // 常量边框行：行值 = Σkx[i]*c（平滑核）或 0（反对称导数核）
            if let BorderPolicy::Constant(v) = &border {
                let cv = bytemuck::cast::<T, u8>(*v) as i32;
                let val: i32 = kx.iter().map(|&k| k * cv).sum();
                mid[r * row_w..(r + 1) * row_w].fill(val);
            }
            continue;
        }
        for x in 0..w {
            for c in 0..C {
                let px = |d: i64| -> i32 {
                    let xx = x as i64 + d;
                    if xx >= 0 && (xx as usize) < w {
                        src_bytes[sy * row_w + (xx as usize) * C + c] as i32
                    } else {
                        // 越界：按边框策略映射绝对坐标 xx
                        match border.interpolate(xx as isize, w) {
                            Some(sx) => src_bytes[sy * row_w + sx * C + c] as i32,
                            None => match &border {
                                BorderPolicy::Constant(v) => bytemuck::cast::<T, u8>(*v) as i32,
                                _ => unreachable!(),
                            },
                        }
                    }
                };
                let acc = kx[0] * px(-2) + kx[1] * px(-1) + kx[2] * px(0) + kx[3] * px(1) + kx[4] * px(2);
                mid[r * row_w + x * C + c] = acc;
            }
        }
    }
    // 列阶段：mid 5 行 → dst 行（i16 饱和）
    for y in 0..h {
        for x in 0..row_w {
            let r0 = &mid[y * row_w + x..];
            let r1 = &mid[(y + 1) * row_w + x..];
            let r2 = &mid[(y + 2) * row_w + x..];
            let r3 = &mid[(y + 3) * row_w + x..];
            let r4 = &mid[(y + 4) * row_w + x..];
            let acc = ky[0] * r0[0] + ky[1] * r1[0] + ky[2] * r2[0] + ky[3] * r3[0] + ky[4] * r4[0];
            out_data[y * row_w + x] = acc.clamp(-32768, 32767) as i16;
        }
    }
    out
}
fn deriv_dispatch<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    kx: [i32; 3],
    ky: [i32; 3],
    border: BorderPolicy<T>,
) -> Image<i16, C> {
    let (w, h) = (src.width(), src.height());
    if w == 0 || h == 0 {
        return Image::new(w, h);
    }
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        // 输出缓冲未初始化（内核全量覆写）
        let out: Image<i16, C> =
            Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(w * h * C)).expect("len");
        let border_u8: BorderPolicy<u8> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
        };
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let mut out = out;
        if C == 1 {
            // cn=1 快路径：直接平面处理，无通道拷贝
            let out_bytes: &mut [i16] = bytemuck::cast_slice_mut(out.data_mut());
            deriv_sep_u8_to_i16(src_bytes, out_bytes, w, h, kx, ky, &border_u8);
            return out;
        }
        // 逐通道：cn>1 时按通道平面处理（行/列内核 cn=1），通道间顺序（行内已并行）
        let cn = C;
        let row_w = w * cn;
        let mut ch_src: Vec<u8> = crate::simd_kernels::alloc_uninit(w * h);
        let mut ch_dst: Vec<i16> = crate::simd_kernels::alloc_uninit(w * h);
        for c in 0..cn {
            for y in 0..h {
                for x in 0..w {
                    ch_src[y * w + x] = src_bytes[y * row_w + x * cn + c];
                }
            }
            deriv_sep_u8_to_i16(&ch_src, &mut ch_dst, w, h, kx, ky, &border_u8);
            let out_data = out.data_mut();
            for y in 0..h {
                for x in 0..w {
                    out_data[y * row_w + x * cn + c] = ch_dst[y * w + x];
                }
            }
        }
        return out;
    }
    let mut out: Image<i16, C> = Image::new(w, h);
    // 通用类型：f32 标量两段
    let src_f: Vec<f32> = src.data().iter().map(|p| p.to_f32()).collect();
    let out_data = out.data_mut();
    let symm_x = kx[0] == kx[2];
    let symm_y = ky[0] == ky[2];
    let row_w = w * C;
    let ph = h + 2;
    let mut mid: Vec<f32> = vec![0.0; row_w * ph];
    for r in 0..ph {
        let sy = border_index(r as i64 - 1, h, &border);
        match sy {
            usize::MAX => {
                if let BorderPolicy::Constant(v) = border {
                    let c = v.to_f32();
                    let val = if symm_x {
                        kx[1] as f32 * c + kx[0] as f32 * 2.0 * c
                    } else {
                        0.0
                    };
                    mid[r * row_w..(r + 1) * row_w].fill(val);
                }
            }
            sy => {
                for x in 0..w {
                    for c in 0..C {
                        let sample = |dx: i64| -> f32 {
                            match border.interpolate(dx as isize, w) {
                                Some(sx) => src_f[sy * row_w + sx * C + c],
                                None => match &border {
                                    BorderPolicy::Constant(v) => v.to_f32(),
                                    _ => unreachable!(),
                                },
                            }
                        };
                        let s = src_f[sy * row_w + x * C + c];
                        let l = if x >= 1 { src_f[sy * row_w + (x - 1) * C + c] } else { sample(-1) };
                        let rv = if x + 1 < w { src_f[sy * row_w + (x + 1) * C + c] } else { sample(w as i64) };
                        mid[r * row_w + x * C + c] = if symm_x {
                            kx[1] as f32 * s + kx[0] as f32 * (l + rv)
                        } else {
                            kx[2] as f32 * (rv - l)
                        };
                    }
                }
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            for c in 0..C {
                let s0 = mid[y * row_w + x * C + c];
                let s1 = mid[(y + 1) * row_w + x * C + c];
                let s2 = mid[(y + 2) * row_w + x * C + c];
                let v = if symm_y {
                    ky[1] as f32 * s1 + ky[0] as f32 * (s0 + s2)
                } else {
                    ky[2] as f32 * (s2 - s0)
                };
                out_data[y * row_w + x * C + c] = v as i16;
            }
        }
    }
    out
}

/// Laplacian 算子（对应 OpenCV `cv::Laplacian`，ksize=1/3，8U → 16S）。
///
/// ksize=1 核 [0,1,0; 1,-4,1; 0,1,0]；ksize=3 核 [2,0,2; 0,-8,0; 2,0,2]。
///
/// # Panics
/// - `ksize` 不为 1 或 3
/// - 空图像
pub fn laplacian<T: Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    ksize: usize,
    scale: f64,
    border: BorderPolicy<T>,
) -> Image<i16, C> {
    assert!(ksize == 1 || ksize == 3, "laplacian ksize must be 1 or 3, got {ksize}");
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "laplacian: empty image");
    let mut out: Image<i16, C> = Image::new(w, h);
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        let border_u8: BorderPolicy<u8> = match &border {
            BorderPolicy::Clamp => BorderPolicy::Clamp,
            BorderPolicy::Reflect => BorderPolicy::Reflect,
            BorderPolicy::Constant(v) => BorderPolicy::Constant(bytemuck::cast::<T, u8>(*v)),
        };
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let out_bytes: &mut [i16] = bytemuck::cast_slice_mut(out.data_mut());
        laplacian_impl_u8(src_bytes, out_bytes, w, h, C, ksize == 1, scale, &border_u8);
        return out;
    }
    // 通用类型：f32 标量
    let src_f: Vec<f32> = src.data().iter().map(|p| p.to_f32()).collect();
    let (cf, cc) = if ksize == 1 { (1.0f32, -4.0) } else { (2.0, -8.0) };
    let (cf, cc) = (cf * scale as f32, cc * scale as f32);
    let row_w = w * C;
    let out_data = out.data_mut();
    for y in 0..h {
        let sy0 = border_index(y as i64 - 1, h, &border);
        let sy2 = border_index(y as i64 + 1, h, &border);
        for x in 0..w {
            for c in 0..C {
                let sample = |sy: usize, dx: i64| -> f32 {
                    let sx = border_index(dx, w, &border);
                    match sx {
                        usize::MAX => match &border {
                            BorderPolicy::Constant(v) => v.to_f32(),
                            _ => unreachable!(),
                        },
                        sx => src_f[sy * row_w + sx * C + c],
                    }
                };
                let ctr = src_f[y * row_w + x * C + c];
                let v = if ksize == 1 {
                    // 十字：u + d + l + r - 4c
                    let u = match sy0 {
                        usize::MAX => match &border {
                            BorderPolicy::Constant(v) => v.to_f32(),
                            _ => unreachable!(),
                        },
                        sy => src_f[sy * row_w + x * C + c],
                    };
                    let d = match sy2 {
                        usize::MAX => match &border {
                            BorderPolicy::Constant(v) => v.to_f32(),
                            _ => unreachable!(),
                        },
                        sy => src_f[sy * row_w + x * C + c],
                    };
                    let l = if x >= 1 { src_f[y * row_w + (x - 1) * C + c] } else { sample(y, -1) };
                    let r = if x + 1 < w { src_f[y * row_w + (x + 1) * C + c] } else { sample(y, w as i64) };
                    cf * (u + d + l + r) + cc * ctr
                } else {
                    // 对角：2*(r0[x±1] + r2[x±1]) - 8c
                    let mut acc = 0.0f32;
                    for sy in [sy0, sy2] {
                        match sy {
                            // 垂直越界行：对角邻居 y 也越界 → 常量
                            usize::MAX => match &border {
                                BorderPolicy::Constant(v) => acc += 2.0 * v.to_f32(),
                                _ => unreachable!(),
                            },
                            sy => {
                                let l = if x >= 1 {
                                    src_f[sy * row_w + (x - 1) * C + c]
                                } else {
                                    sample(sy, -1)
                                };
                                let r = if x + 1 < w {
                                    src_f[sy * row_w + (x + 1) * C + c]
                                } else {
                                    sample(sy, w as i64)
                                };
                                acc += l + r;
                            }
                        }
                    }
                    cf * acc + cc * ctr
                };
                out_data[y * row_w + x * C + c] = v as i16;
            }
        }
    }
    out
}

/// Laplacian u8 实现（5-tap，行块并行）。
#[allow(clippy::too_many_arguments)]
fn laplacian_impl_u8(
    src: &[u8],
    dst: &mut [i16],
    w: usize,
    h: usize,
    cn: usize,
    ksize1: bool,
    scale: f64,
    border: &BorderPolicy<u8>,
) {
    let row_w = w * cn;
    // scale ≠ 1：走标量（OpenCV 同样把 scale 并入核系数）
    if scale != 1.0 {
        let (cf, cc) = if ksize1 { (1.0f32, -4.0) } else { (2.0, -8.0) };
        let (cf, cc) = (cf * scale as f32, cc * scale as f32);
        for y in 0..h {
            let sy0 = border_index(y as i64 - 1, h, border);
            let sy2 = border_index(y as i64 + 1, h, border);
            for x in 0..w {
                for c in 0..cn {
                    let sample = |sy: usize, dx: i64| -> f32 {
                        match border_index(dx, w, border) {
                            usize::MAX => match border {
                                BorderPolicy::Constant(v) => *v as f32,
                                _ => unreachable!(),
                            },
                            sx => src[sy * row_w + sx * cn + c] as f32,
                        }
                    };
                    let row = |sy: usize, dx: i64| -> f32 {
                        match border_index(dx, w, border) {
                            usize::MAX => match border {
                                BorderPolicy::Constant(v) => *v as f32,
                                _ => unreachable!(),
                            },
                            sx => src[sy * row_w + sx * cn + c] as f32,
                        }
                    };
                    let ctr = src[y * row_w + x * cn + c] as f32;
                    let v = if ksize1 {
                        // 十字：u + d + l + r - 4c
                        let u = match sy0 {
                            usize::MAX => match border {
                                BorderPolicy::Constant(v) => *v as f32,
                                _ => unreachable!(),
                            },
                            sy => src[sy * row_w + x * cn + c] as f32,
                        };
                        let d = match sy2 {
                            usize::MAX => match border {
                                BorderPolicy::Constant(v) => *v as f32,
                                _ => unreachable!(),
                            },
                            sy => src[sy * row_w + x * cn + c] as f32,
                        };
                        let l = if x >= 1 {
                            src[y * row_w + (x - 1) * cn + c] as f32
                        } else {
                            sample(y, -1)
                        };
                        let r = if x + 1 < w {
                            src[y * row_w + (x + 1) * cn + c] as f32
                        } else {
                            sample(y, w as i64)
                        };
                        cf * (u + d + l + r) + cc * ctr
                    } else {
                        // 对角：2*(r0[x±1] + r2[x±1]) - 8c
                        let mut acc = 0.0f32;
                        for sy in [sy0, sy2] {
                            let rv = match sy {
                                // 垂直越界行：对角邻居 y 也越界 → 常量
                                usize::MAX => match border {
                                    BorderPolicy::Constant(v) => 2.0 * *v as f32,
                                    _ => unreachable!(),
                                },
                                sy => {
                                    let l = if x >= 1 {
                                        src[sy * row_w + (x - 1) * cn + c] as f32
                                    } else {
                                        row(sy, -1)
                                    };
                                    let r = if x + 1 < w {
                                        src[sy * row_w + (x + 1) * cn + c] as f32
                                    } else {
                                        row(sy, w as i64)
                                    };
                                    l + r
                                }
                            };
                            acc += rv;
                        }
                        cf * acc + cc * ctr
                    };
                    dst[y * row_w + x * cn + c] = v as i16;
                }
            }
        }
        return;
    }
    // scale == 1：整数 5-tap SIMD
    // 256 行块：带宽型内核块太小会引发总线争抢（64 线程 16/64 行块实测
    // 1.1-2.2 GB/s 级退化）；1024 行图切 4 块，每块 256KB src + 512KB dst
    use rayon::prelude::*;
    const MIN_ROWS: usize = 256;
    let chunk = MIN_ROWS.min(h);
    let nblocks = h.div_ceil(chunk);
    if nblocks <= 1 || rayon::current_num_threads() <= 1 {
        laplacian_rows_range(src, dst, w, h, cn, ksize1, border, 0, h);
    } else {
        dst.par_chunks_mut(row_w * chunk).enumerate().for_each(|(bi, d)| {
            let y0 = bi * chunk;
            let rows = d.len() / row_w;
            laplacian_rows_range(src, d, w, h, cn, ksize1, border, y0, rows);
        });
    }
}

/// Laplacian 行范围处理（cn=1 走 SIMD，cn>1 逐通道标量）。
#[allow(clippy::too_many_arguments)]
fn laplacian_rows_range(
    src: &[u8],
    dst: &mut [i16],
    w: usize,
    h: usize,
    cn: usize,
    ksize1: bool,
    border: &BorderPolicy<u8>,
    y0: usize,
    rows: usize,
) {
    if cn == 1 {
        for y in y0..y0 + rows {
            let sy0 = border_index(y as i64 - 1, h, border);
            let sy2 = border_index(y as i64 + 1, h, border);
            let (r0, r2) = match (sy0, sy2) {
                (usize::MAX, usize::MAX) => (None, None),
                (usize::MAX, sy2) => (None, Some(&src[sy2 * w..(sy2 + 1) * w])),
                (sy0, usize::MAX) => (Some(&src[sy0 * w..(sy0 + 1) * w]), None),
                (sy0, sy2) => (
                    Some(&src[sy0 * w..(sy0 + 1) * w]),
                    Some(&src[sy2 * w..(sy2 + 1) * w]),
                ),
            };
            let r1 = &src[y * w..(y + 1) * w];
            let dst_row = &mut dst[(y - y0) * w..(y - y0 + 1) * w];
            laplacian_row_dispatch(r0, r1, r2, dst_row, w, ksize1, border);
        }
        return;
    }
    // cn>1：逐通道标量
    let row_w = w * cn;
    for y in y0..y0 + rows {
        let sy0 = border_index(y as i64 - 1, h, border);
        let sy2 = border_index(y as i64 + 1, h, border);
        for x in 0..w {
            for c in 0..cn {
                let sample = |sy: usize, dx: i64| -> u8 {
                    match border_index(dx, w, border) {
                        usize::MAX => match border {
                            BorderPolicy::Constant(v) => *v,
                            _ => unreachable!(),
                        },
                        sx => src[sy * row_w + sx * cn + c],
                    }
                };
                let ctr = src[y * row_w + x * cn + c];
                let s = if ksize1 {
                    // 十字：u + d + l + r - 4c
                    let u = match sy0 {
                        usize::MAX => match border {
                            BorderPolicy::Constant(v) => *v,
                            _ => unreachable!(),
                        },
                        sy => src[sy * row_w + x * cn + c],
                    };
                    let d = match sy2 {
                        usize::MAX => match border {
                            BorderPolicy::Constant(v) => *v,
                            _ => unreachable!(),
                        },
                        sy => src[sy * row_w + x * cn + c],
                    };
                    let l = if x >= 1 { src[y * row_w + (x - 1) * cn + c] } else { sample(y, -1) };
                    let r = if x + 1 < w { src[y * row_w + (x + 1) * cn + c] } else { sample(y, w as i64) };
                    u as i32 + d as i32 + l as i32 + r as i32 - 4 * ctr as i32
                } else {
                    // 对角：2*(r0[x±1] + r2[x±1]) - 8c
                    let mut acc = 0i32;
                    for sy in [sy0, sy2] {
                        match sy {
                            usize::MAX => match border {
                                // 垂直越界行：对角邻居 y 也越界 → 常量
                                BorderPolicy::Constant(v) => acc += 2 * *v as i32,
                                _ => unreachable!(),
                            },
                            sy => {
                                let l = if x >= 1 {
                                    src[sy * row_w + (x - 1) * cn + c] as i32
                                } else {
                                    sample(sy, -1) as i32
                                };
                                let r = if x + 1 < w {
                                    src[sy * row_w + (x + 1) * cn + c] as i32
                                } else {
                                    sample(sy, w as i64) as i32
                                };
                                acc += l + r;
                            }
                        }
                    }
                    2 * acc - 8 * ctr as i32
                };
                dst[(y - y0) * row_w + x * cn + c] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
    }
}

/// 单行 Laplacian 派发：正常 3 行走 SIMD；Constant 边框行（None）走标量。
fn laplacian_row_dispatch(
    r0: Option<&[u8]>,
    r1: &[u8],
    r2: Option<&[u8]>,
    dst: &mut [i16],
    w: usize,
    ksize1: bool,
    border: &BorderPolicy<u8>,
) {
    match (r0, r2) {
        (Some(a), Some(b)) => {
            crate::simd_kernels::laplacian3_rows_u8([a, r1, b], dst, w, ksize1, *border);
        }
        _ => {
            // Constant 边框：越界行为常量
            let cv = match border {
                BorderPolicy::Constant(v) => *v as i32,
                _ => unreachable!(),
            };
            for x in 0..w {
                // 行内水平越界同样取常量
                let l1 = if x >= 1 { r1[x - 1] as i32 } else { cv };
                let r1v = if x + 1 < w { r1[x + 1] as i32 } else { cv };
                let s = if ksize1 {
                    // 十字：u + d + l + r - 4c
                    let u = r0.map(|r| r[x] as i32).unwrap_or(cv);
                    let d = r2.map(|r| r[x] as i32).unwrap_or(cv);
                    u + d + l1 + r1v - 4 * r1[x] as i32
                } else {
                    // 对角：2*(r0[x±1] + r2[x±1]) - 8c
                    let mut acc = 0i32;
                    for rr in [r0, r2] {
                        match rr {
                            Some(r) => {
                                let l = if x >= 1 { r[x - 1] as i32 } else { cv };
                                let rv = if x + 1 < w { r[x + 1] as i32 } else { cv };
                                acc += l + rv;
                            }
                            None => acc += 2 * cv,
                        }
                    }
                    2 * acc - 8 * r1[x] as i32
                };
                dst[x] = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
    }
}

/// 梯度幅值（对应 OpenCV `cv::magnitude`，f32）。
///
/// `dst = sqrt(x² + y²)`，逐通道。
///
/// # Panics
/// - 两图尺寸/通道不一致
pub fn magnitude<T: Pixel + bytemuck::Pod, const C: usize>(
    x: &Image<T, C>,
    y: &Image<T, C>,
) -> Image<f32, C> {
    assert!(
        x.width() == y.width() && x.height() == y.height(),
        "magnitude: size mismatch"
    );
    let (w, h) = (x.width(), x.height());
    let n = w * h * C;
    // 输出缓冲未初始化（内核全量覆写）
    let mut out: Image<f32, C> =
        Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(n)).expect("len");
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<f32>() {
        // f32 快路径：零转换 + 256K 元素块并行（带宽型）
        let xf: &[f32] = bytemuck::cast_slice(x.data());
        let yf: &[f32] = bytemuck::cast_slice(y.data());
        par_chunks3(xf, yf, out.data_mut(), n, |a, b, c, len| {
            crate::simd_kernels::magnitude32f(a, b, c, len)
        });
        return out;
    }
    let xf: Vec<f32> = x.data().iter().map(|p| p.to_f32()).collect();
    let yf: Vec<f32> = y.data().iter().map(|p| p.to_f32()).collect();
    crate::simd_kernels::magnitude32f(&xf, &yf, out.data_mut(), n);
    out
}

/// 相位角（对应 OpenCV `cv::phase`，f32）。
///
/// `dst = atan2(y, x)`，`angle_in_degrees=false` 为弧度（[0, 2π)），true 为度数 [0, 360)。
///
/// # Panics
/// - 两图尺寸/通道不一致
pub fn phase<T: Pixel + bytemuck::Pod, const C: usize>(
    x: &Image<T, C>,
    y: &Image<T, C>,
    angle_in_degrees: bool,
) -> Image<f32, C> {
    assert!(
        x.width() == y.width() && x.height() == y.height(),
        "phase: size mismatch"
    );
    let (w, h) = (x.width(), y.height());
    let n = w * h * C;
    // 输出缓冲未初始化（内核全量覆写）
    let mut out: Image<f32, C> =
        Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(n)).expect("len");
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<f32>() {
        // f32 快路径：零转换 + 256K 元素块并行（带宽型）
        let xf: &[f32] = bytemuck::cast_slice(x.data());
        let yf: &[f32] = bytemuck::cast_slice(y.data());
        let deg = angle_in_degrees;
        par_chunks3(yf, xf, out.data_mut(), n, |a, b, c, len| {
            crate::simd_kernels::fast_atan2_32f(a, b, c, len, deg)
        });
        return out;
    }
    let xf: Vec<f32> = x.data().iter().map(|p| p.to_f32()).collect();
    let yf: Vec<f32> = y.data().iter().map(|p| p.to_f32()).collect();
    crate::simd_kernels::fast_atan2_32f(&yf, &xf, out.data_mut(), n, angle_in_degrees);
    out
}

/// 直角坐标转极坐标（对应 OpenCV `cv::cartToPolar`，f32）。
///
/// 返回 (magnitude, phase)。
///
/// # Panics
/// - 两图尺寸/通道不一致
pub fn cart_to_polar<T: Pixel + bytemuck::Pod, const C: usize>(
    x: &Image<T, C>,
    y: &Image<T, C>,
    angle_in_degrees: bool,
) -> (Image<f32, C>, Image<f32, C>) {
    assert!(
        x.width() == y.width() && x.height() == y.height(),
        "cart_to_polar: size mismatch"
    );
    let (w, h) = (x.width(), x.height());
    let n = w * h * C;
    // 输出缓冲未初始化（内核全量覆写）
    let mut mag: Image<f32, C> =
        Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(n)).expect("len");
    let mut angle: Image<f32, C> =
        Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(n)).expect("len");
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<f32>() {
        // f32 快路径：零转换 + 256K 元素块并行（带宽型）
        let xf: &[f32] = bytemuck::cast_slice(x.data());
        let yf: &[f32] = bytemuck::cast_slice(y.data());
        let deg = angle_in_degrees;
        par_chunks4(
            xf,
            yf,
            mag.data_mut(),
            angle.data_mut(),
            n,
            |a, b, c, d, len| crate::simd_kernels::cart_to_polar32f(a, b, c, d, len, deg),
        );
        return (mag, angle);
    }
    let xf: Vec<f32> = x.data().iter().map(|p| p.to_f32()).collect();
    let yf: Vec<f32> = y.data().iter().map(|p| p.to_f32()).collect();
    crate::simd_kernels::cart_to_polar32f(
        &xf,
        &yf,
        mag.data_mut(),
        angle.data_mut(),
        n,
        angle_in_degrees,
    );
    (mag, angle)
}

/// 3 切片分块并行辅助（带宽型内核，256K 元素块）。
fn par_chunks3<F>(a: &[f32], b: &[f32], c: &mut [f32], n: usize, f: F)
where
    F: Fn(&[f32], &[f32], &mut [f32], usize) + Sync + Send,
{
    use rayon::prelude::*;
    const CHUNK: usize = 256 * 1024;
    if n <= CHUNK || rayon::current_num_threads() <= 1 {
        f(a, b, c, n);
        return;
    }
    let nblocks = n.div_ceil(CHUNK);
    let last = nblocks - 1;
    // 预切可变块（避免闭包内可变借用）
    let c_chunks: Vec<&mut [f32]> = c.chunks_mut(CHUNK).take(nblocks).collect();
    (0..nblocks).into_par_iter().zip(c_chunks).for_each(|(bi, cc)| {
        let s = bi * CHUNK;
        let e = if bi == last { n } else { s + CHUNK };
        f(&a[s..e], &b[s..e], cc, e - s);
    });
}

/// 4 切片分块并行辅助（带宽型内核，256K 元素块）。
fn par_chunks4<F>(a: &[f32], b: &[f32], c: &mut [f32], d: &mut [f32], n: usize, f: F)
where
    F: Fn(&[f32], &[f32], &mut [f32], &mut [f32], usize) + Sync + Send,
{
    use rayon::prelude::*;
    const CHUNK: usize = 256 * 1024;
    if n <= CHUNK || rayon::current_num_threads() <= 1 {
        f(a, b, c, d, n);
        return;
    }
    let nblocks = n.div_ceil(CHUNK);
    let last = nblocks - 1;
    // 预切可变块（避免闭包内可变借用）
    let c_chunks: Vec<&mut [f32]> = c.chunks_mut(CHUNK).take(nblocks).collect();
    let d_chunks: Vec<&mut [f32]> = d.chunks_mut(CHUNK).take(nblocks).collect();
    (0..nblocks)
        .into_par_iter()
        .zip(c_chunks)
        .zip(d_chunks)
        .for_each(|((bi, cc), dd)| {
            let s = bi * CHUNK;
            let e = if bi == last { n } else { s + CHUNK };
            f(&a[s..e], &b[s..e], cc, dd, e - s);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img3x3(v: &[u8]) -> Image<u8, 1> {
        Image::from_vec(3, 3, v.to_vec()).unwrap()
    }

    #[test]
    fn sobel_dx_basic() {
        // Sobel dx（kx=[-1,0,1], ky=[1,2,1]），Clamp 边框
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let out = sobel(&src, 1, 0, 1.0, BorderPolicy::Clamp);
        // 手算：行阶段 dx（反对称 [-1,0,1]）：
        // 行 [1,2,3] → [2-1, 3-1, 3-2] = [1,2,1]
        // 行 [4,5,6] → [1,2,1]；行 [7,8,9] → [1,2,1]
        // 列阶段 dy（对称 [1,2,1]）：
        // y=0: 1*[1,2,1] + 2*[1,2,1] + 1*[1,2,1] = [4,8,4]
        // y=1: 同 [4,8,4]；y=2: [4,8,4]
        assert_eq!(out.data(), &[4, 8, 4, 4, 8, 4, 4, 8, 4]);
    }

    #[test]
    fn sobel_dy_basic() {
        // Sobel dy（kx=[1,2,1], ky=[-1,0,1]），Clamp 边框（对齐 OpenCV 实测）
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let out = sobel(&src, 0, 1, 1.0, BorderPolicy::Clamp);
        // 行阶段（对称 [1,2,1]，Clamp）：
        // 行 [1,2,3] → [5,8,11]；行 [4,5,6] → [17,20,23]；行 [7,8,9] → [29,32,35]
        // 列阶段（反对称 [-1,0,1]）：
        // y=0: [17,20,23]-[5,8,11] = [12,12,12]
        // y=1: [29,32,35]-[5,8,11] = [24,24,24]
        // y=2: [29,32,35]-[17,20,23] = [12,12,12]
        assert_eq!(out.data(), &[12, 12, 12, 24, 24, 24, 12, 12, 12]);
    }

    #[test]
    fn scharr_dx_basic() {
        // Scharr dx（kx=[-1,0,1], ky=[3,10,3]），Clamp 边框
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let out = scharr(&src, 1, 0, 1.0, BorderPolicy::Clamp);
        // 行阶段同 sobel dx：全 [1,2,1]
        // 列阶段 [3,10,3]：y=0: 3*[1,2,1]+10*[1,2,1]+3*[1,2,1] = [16,32,16]
        assert_eq!(out.data(), &[16, 32, 16, 16, 32, 16, 16, 32, 16]);
    }

    #[test]
    fn laplacian_k1_basic() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let out = laplacian(&src, 1, 1.0, BorderPolicy::Clamp);
        // 对齐 OpenCV 实测：[4,3,2; 1,0,-1; -2,-3,-4]
        assert_eq!(out.data(), &[4, 3, 2, 1, 0, -1, -2, -3, -4]);
    }

    #[test]
    fn laplacian_k3_basic() {
        let src = img3x3(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let out = laplacian(&src, 3, 1.0, BorderPolicy::Clamp);
        // 对齐 OpenCV 实测：[16,12,8; 4,0,-4; -8,-12,-16]
        assert_eq!(out.data(), &[16, 12, 8, 4, 0, -4, -8, -12, -16]);
    }

    #[test]
    fn magnitude_phase_basic() {
        let x = Image::<f32, 1>::from_vec(2, 1, vec![3.0, 1.0]).unwrap();
        let y = Image::<f32, 1>::from_vec(2, 1, vec![4.0, 1.0]).unwrap();
        let m = magnitude(&x, &y);
        assert_eq!(m.data()[0], 5.0);
        assert!((m.data()[1] - 2.0f32.sqrt()).abs() < 1e-6);
        let p = phase(&x, &y, false);
        // atan2(4, 3) ≈ 53.13° ≈ 0.9273 rad；atan2(1,1) = 45°
        assert!((p.data()[0] - 0.9273).abs() < 1e-2);
        assert!((p.data()[1] - std::f32::consts::FRAC_PI_4).abs() < 1e-2);
    }

    #[test]
    fn cart_to_polar_basic() {
        let x = Image::<f32, 1>::from_vec(2, 1, vec![3.0, -1.0]).unwrap();
        let y = Image::<f32, 1>::from_vec(2, 1, vec![4.0, -1.0]).unwrap();
        let (m, p) = cart_to_polar(&x, &y, true);
        assert_eq!(m.data()[0], 5.0);
        assert!((p.data()[0] - 53.13).abs() < 0.05);
        assert!((p.data()[1] - 225.0).abs() < 0.05);
    }

    #[test]
    fn sobel_saturates() {
        // 极值图触发 i16 饱和（Scharr dy 最大 32760+ 不溢出，构造大值图验证不 panic）
        let data: Vec<u8> = vec![255u8; 16 * 16];
        let img = Image::<u8, 1>::from_vec(16, 16, data).unwrap();
        let _ = sobel(&img, 0, 1, 1.0, BorderPolicy::Clamp);
        let _ = scharr(&img, 0, 1, 1.0, BorderPolicy::Clamp);
    }
}
