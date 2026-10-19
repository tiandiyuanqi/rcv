//! 图像几何变换（对应 OpenCV `cv::resize` / `cv::warpAffine` / `cv::warpPerspective`）。
//!
//! - `resize`：对齐 resize.cpp（INTER_NEAREST/LINEAR/CUBIC/AREA，
//!   u8 走 11 位定点系数，`fx = (dx+0.5)*scale - 0.5` 半像素中心约定）
//! - `warp_affine` / `warp_perspective`：对齐 imgwarp.cpp
//!   （AB_BITS=10 定点前向映射 + INTER_BITS=5 子像素表 + remap 采样）
//! - `get_rotation_matrix_2d`：对齐 imgwarp.cpp:2887（getRotationMatrix2D_）

use crate::core::image::Image;

/// 插值方法（对应 `cv::InterpolationFlags` 的常用子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    /// 最近邻（`INTER_NEAREST`，floor 语义）。
    Nearest,
    /// 双线性（`INTER_LINEAR`，u8 走 11 位定点）。
    Linear,
    /// 双三次（`INTER_CUBIC`，A=-0.75，u8 走 11 位定点）。
    Cubic,
    /// 区域平均（`INTER_AREA`；缩小走真 area，放大退化为 linear）。
    Area,
}

/// resize 的目标尺寸描述：显式 `(w, h)` 或缩放因子。
#[derive(Debug, Clone, Copy)]
pub enum DSize {
    /// 显式目标尺寸。
    Exact(usize, usize),
    /// 缩放因子（fx, fy），目标尺寸 = `(w*fx).round(), (h*fy).round()`。
    Scale(f64, f64),
}

/// 11 位定点系数（`INTER_RESIZE_COEF_BITS`）。
const COEF_BITS: u32 = 11;
const COEF_SCALE: i32 = 1 << COEF_BITS;

/// cubic 插值系数（对齐 resize.cpp:964 `interpolateCubic`，A=-0.75，f32 计算）。
fn interpolate_cubic(x: f32, coeffs: &mut [f32; 4]) {
    const A: f32 = -0.75;
    coeffs[0] = ((A * (x + 1.0) - 5.0 * A) * (x + 1.0) + 8.0 * A) * (x + 1.0) - 4.0 * A;
    coeffs[1] = ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0;
    coeffs[2] = ((A + 2.0) * (1.0 - x) - (A + 3.0)) * (1.0 - x) * (1.0 - x) + 1.0;
    coeffs[3] = 1.0 - coeffs[0] - coeffs[1] - coeffs[2];
}

/// cvFloor 语义（f64 → i32，向负无穷）。
fn cv_floor(v: f64) -> i32 {
    v.floor() as i32
}

/// cvRound 语义（round-half-even，对齐 cvRound SSE）。
fn cv_round(v: f64) -> i32 {
    v.round_ties_even() as i32
}

/// saturate_cast<short>（cvRound + clamp）。
fn sat_short(v: f32) -> i16 {
    v.round_ties_even().clamp(-32768.0, 32767.0) as i16
}

/// 图像缩放（对应 `cv::resize`）。
///
/// u8 单/三通道与 OpenCV 逐位一致：
/// - Nearest：`sx = floor(dx/fx)`，clamp 到 `[0, w-1]`
/// - Linear/Cubic：`f = (d+0.5)*scale - 0.5`（scale = 1/fx），
///   系数 11 位定点（`saturate_cast<short>(cbuf*2048)`），
///   水平 i32 中间缓冲 + 垂直 `(Σ + 2^21) >> 22` 饱和
/// - Area：整数倍缩小走 fast 路径（`saturate(sum/area)`），
///   非整数倍走 DecimateAlpha f32 表；放大退化为 Linear
///
/// # Panics
/// 目标尺寸为 0。
pub fn resize<T: crate::core::image::Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    dsize: DSize,
    interpolation: Interpolation,
) -> Image<T, C> {
    let (sw, sh) = (src.width(), src.height());
    assert!(sw > 0 && sh > 0, "resize: empty src");
    let (dw, dh) = match dsize {
        DSize::Exact(w, h) => (w, h),
        DSize::Scale(fx, fy) => (
            cv_round(sw as f64 * fx) as usize,
            cv_round(sh as f64 * fy) as usize,
        ),
    };
    assert!(dw > 0 && dh > 0, "resize: empty dst");
    if dw == sw && dh == sh {
        return src.clone();
    }
    let fx = dw as f64 / sw as f64; // inv_scale_x
    let fy = dh as f64 / sh as f64;
    let scale_x = 1.0 / fx;
    let scale_y = 1.0 / fy;

    // u8 特派（对齐 OpenCV CV_8U 定点路径）
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() {
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let mut out: Image<u8, C> = Image::new_uninit(dw, dh);
        let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        // INTER_AREA 真路径仅缩小（scale >= 1，resize.cpp:4040）；
        // 放大退化为 INTER_LINEAR 但保留 area_mode 的 fx 公式（resize.cpp:4080
        // 走 linear_tab 且 area_mode = true）
        let area_upscale = matches!(interpolation, Interpolation::Area)
            && !(scale_x >= 1.0 && scale_y >= 1.0);
        // OpenCV resize.cpp:4020：INTER_LINEAR && is_area_fast && iscale_x==2 && iscale_y==2
        // 切换到 INTER_AREA（area-fast 2x2，比 generic linear 快且结果相同）
        let iscale_x = cv_round(scale_x);
        let iscale_y = cv_round(scale_y);
        let is_area_fast = (scale_x - iscale_x as f64).abs() < f64::EPSILON
            && (scale_y - iscale_y as f64).abs() < f64::EPSILON;
        let linear_to_area_fast = matches!(interpolation, Interpolation::Linear)
            && is_area_fast
            && iscale_x == 2
            && iscale_y == 2;
        match interpolation {
            Interpolation::Nearest => resize_nn_u8::<C>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy),
            Interpolation::Linear if linear_to_area_fast => {
                resize_area_u8::<C>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy, scale_x, scale_y)
            }
            Interpolation::Linear => {
                resize_generic_u8::<C, 2>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy, scale_x, scale_y, false)
            }
            Interpolation::Cubic => {
                resize_generic_u8::<C, 4>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy, scale_x, scale_y, false)
            }
            Interpolation::Area if area_upscale => {
                resize_generic_u8::<C, 2>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy, scale_x, scale_y, true)
            }
            Interpolation::Area => resize_area_u8::<C>(src_bytes, sw, sh, out_bytes, dw, dh, fx, fy, scale_x, scale_y),
        }
        let data: Vec<T> = crate::simd_kernels::vec_u8_as(out.into_vec());
        return Image::from_vec(dw, dh, data).expect("u8 resize path length invariant");
    }

    // 通用浮点路径（f32 中间，非 bitexact——对齐 OpenCV CV_32F 行为）
    let ksize = match interpolation {
        Interpolation::Nearest => {
            let mut out: Image<T, C> = Image::new_uninit(dw, dh);
            let xofs: Vec<usize> = (0..dw)
                .map(|dx| ((dx as f64 / fx).floor() as usize).min(sw - 1))
                .collect();
            for dy in 0..dh {
                let sy = ((dy as f64 / fy).floor() as usize).min(sh - 1);
                for dx in 0..dw {
                    let sx = xofs[dx];
                    for c in 0..C {
                        let v = src.data()[(sy * sw + sx) * C + c];
                        out.data_mut()[(dy * dw + dx) * C + c] = v;
                    }
                }
            }
            return out;
        }
        Interpolation::Linear | Interpolation::Area => 2,
        Interpolation::Cubic => 4,
    };
    let ksize2 = ksize / 2;
    let width = dw * C;

    // xofs/alpha（对齐 resize.cpp:4093-4155）
    let mut xofs = vec![0i32; width];
    let mut alpha = vec![0.0f32; width * ksize];
    // OpenCV resizeGeneric_ 的 xmin/xmax（area 路径不更新 xmin，保留结构）
    #[allow(unused_assignments, unused_mut, unused_variables)]
    let mut xmin = 0usize;
    let mut xmax = dw;
    for dx in 0..dw {
        let f = (dx as f64 + 0.5) * scale_x - 0.5;
        let mut sx = cv_floor(f);
        let mut fxp = (f - sx as f64) as f32;
        if sx < ksize2 as i32 - 1 {
            #[allow(unused_assignments)]
            {
                xmin = dx + 1;
            }
            if sx < 0 && interpolation != Interpolation::Cubic {
                fxp = 0.0;
                sx = 0;
            }
        }
        if sx + ksize2 as i32 >= sw as i32 {
            xmax = xmax.min(dx);
            if sx >= sw as i32 - 1 && interpolation != Interpolation::Cubic {
                fxp = 0.0;
                sx = sw as i32 - 1;
            }
        }
        let sx = sx as usize * C;
        for k in 0..C {
            xofs[dx * C + k] = (sx + k) as i32;
        }
        let mut cbuf = [0.0f32; 4];
        match ksize {
            2 => {
                cbuf[0] = 1.0 - fxp;
                cbuf[1] = fxp;
            }
            _ => interpolate_cubic(fxp, &mut cbuf),
        }
        for k in 0..ksize {
            alpha[dx * C * ksize + k] = cbuf[k];
        }
        for k in ksize..C * ksize {
            alpha[dx * C * ksize + k] = alpha[dx * C * ksize + k - ksize];
        }
    }

    let mut out: Image<T, C> = Image::new_uninit(dw, dh);
    let bufstep = (dw + 15) & !15;
    let mut buffer: Vec<f32> = vec![0.0; bufstep * ksize];
    let src_data = src.data();
    let dst_data = out.data_mut();
    for dy in 0..dh {
        let f = (dy as f64 + 0.5) * scale_y - 0.5;
        let sy0 = cv_floor(f);
        let fyp = (f - sy0 as f64) as f32;
        let mut beta = [0.0f32; 4];
        match ksize {
            2 => {
                beta[0] = 1.0 - fyp;
                beta[1] = fyp;
            }
            _ => interpolate_cubic(fyp, &mut beta),
        }
        // 水平：ksize 行 → buffer
        for k in 0..ksize {
            let sy = (sy0 - ksize2 as i32 + 1 + k as i32).clamp(0, sh as i32 - 1) as usize;
            let srow = &src_data[sy * sw * C..(sy + 1) * sw * C];
            let drow = &mut buffer[k * bufstep..(k + 1) * bufstep];
            for dx in 0..dw {
                let mut acc = 0.0f32;
                if dx < xmax {
                    let sx = xofs[dx * C] as usize;
                    for c in 0..C {
                        acc += srow[sx + c].to_f32() * alpha[dx * C * ksize + c * ksize];
                    }
                } else {
                    acc = srow[xofs[dx * C] as usize].to_f32();
                }
                drow[dx] = acc;
            }
        }
        // 垂直：buffer → dst
        for dx in 0..dw {
            for c in 0..C {
                let mut acc = 0.0f32;
                for k in 0..ksize {
                    acc += buffer[k * bufstep + dx] * beta[k];
                }
                dst_data[(dy * dw + dx) * C + c] = T::from_f32(acc);
            }
        }
    }
    out
}

/// NEAREST：`sx = floor(dx*ifx)`（对齐 resizeNN，resize.cpp:1122）。
fn resize_nn_u8<const C: usize>(
    src: &[u8],
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    fx: f64,
    fy: f64,
) {
    let ifx = 1.0 / fx;
    let ify = 1.0 / fy;
    let xofs: Vec<usize> = (0..dw)
        .map(|dx| (((dx as f64 * ifx).floor() as usize).min(sw - 1)) * C)
        .collect();
    use rayon::prelude::*;
    fn run<const C: usize>(
        src: &[u8],
        sw: usize,
        sh: usize,
        dst: &mut [u8],
        dw: usize,
        xofs: &[usize],
        ify: f64,
        y0: usize,
        _y1: usize,
    ) {
        for (li, drow) in dst.chunks_mut(dw * C).enumerate() {
            let dy = y0 + li;
            let sy = ((dy as f64 * ify).floor() as usize).min(sh - 1);
            let srow = &src[sy * sw * C..(sy + 1) * sw * C];
            for dx in 0..dw {
                let sx = xofs[dx];
                for c in 0..C {
                    drow[dx * C + c] = srow[sx + c];
                }
            }
        }
    }
    const MIN_ROWS: usize = 16;
    if dh >= MIN_ROWS * 2 && rayon::current_num_threads() > 1 {
        dst.par_chunks_mut(dw * C * MIN_ROWS)
            .enumerate()
            .for_each(|(bi, d)| {
                run::<C>(src, sw, sh, d, dw, &xofs, ify, bi * MIN_ROWS, bi * MIN_ROWS + d.len() / (dw * C))
            });
    } else {
        run::<C>(src, sw, sh, dst, dw, &xofs, ify, 0, dh);
    }
}

/// LINEAR/CUBIC 通用定点路径（对齐 resizeGeneric_ + HResize/VResize u8）。
///
/// 水平：`D[dx] = Σ S[xofs[dx]+j·cn]·alpha[dx·cn·ksize+j]`（i32）；
/// 垂直：`dst = sat_u8((Σ βk·Dk + 2^21) >> 22)`（FixedPtCast bits=22）。
fn resize_generic_u8<const C: usize, const K: usize>(
    src: &[u8],
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    fx: f64,
    fy: f64,
    scale_x: f64,
    scale_y: f64,
    area_mode: bool,
) {
    let ksize2 = K / 2;
    let width = dw * C;

    // xofs/ialpha（对齐 resize.cpp:4093-4155，fixpt 分支）
    let mut xofs = vec![0i32; width];
    let mut ialpha = vec![0i16; width * K];
    // OpenCV resizeGeneric_ 的 xmin/xmax（area 路径不更新 xmin，保留结构）
    #[allow(unused_assignments, unused_mut, unused_variables)]
    let mut xmin = 0usize;
    let mut xmax = dw;
    for dx in 0..dw {
        // 对齐 OpenCV：fx = (float)((dx+0.5)*scale_x - 0.5)——f64 算后先转 f32，
        // 再 sx = cvFloor(fx)、fx -= sx（f32 域减法）。
        // area_mode（INTER_AREA 放大退化）：sx = floor(dx*scale_x)，
        // fx = (dx+1) - (sx+1)*inv_scale_x，<=0 归 0，去小数部分
        let (mut sx, mut fxp) = if area_mode {
            // sx = floor(dx*scale_x)；fx = (dx+1)-(sx+1)*inv_scale_x（<=0 归 0，
            // 已是最终权重，不再减 sx）
            let s = (dx as f64 * scale_x).floor() as i32;
            let v = (dx as f64 + 1.0) - (s as f64 + 1.0) * fx;
            let v = if v <= 0.0 { 0.0 } else { v - v.floor() };
            (s, v as f32)
        } else {
            let f = ((dx as f64 + 0.5) * scale_x - 0.5) as f32;
            let sx = cv_floor(f as f64);
            (sx, f - sx as f32)
        };
        if !area_mode && sx < ksize2 as i32 - 1 {
            xmin = dx + 1;
            if sx < 0 && K == 2 {
                fxp = 0.0;
                sx = 0;
            }
        }
        if !area_mode && sx + ksize2 as i32 >= sw as i32 {
            xmax = xmax.min(dx);
            if sx >= sw as i32 - 1 && K == 2 {
                fxp = 0.0;
                sx = sw as i32 - 1;
            }
        }
        let sx = sx as usize * C;
        for k in 0..C {
            xofs[dx * C + k] = (sx + k) as i32;
        }
        let mut cbuf = [0.0f32; 4];
        match K {
            2 => {
                cbuf[0] = 1.0 - fxp;
                cbuf[1] = fxp;
            }
            _ => interpolate_cubic(fxp, &mut cbuf),
        }
        for k in 0..K {
            ialpha[dx * C * K + k] = sat_short(cbuf[k] * COEF_SCALE as f32);
        }
        for k in K..C * K {
            ialpha[dx * C * K + k] = ialpha[dx * C * K + k - K];
        }
    }
    // K=4 时预构造交错布局（SIMD madd 用）：扁平域 [a0,a1] 对 / [a2,a3] 对
    let ialpha01;
    let ialpha23;
    if K == 4 {
        let mut a01 = vec![0i16; width * 2];
        let mut a23 = vec![0i16; width * 2];
        for f in 0..width {
            let base = f * 4;
            a01[f * 2] = ialpha[base];
            a01[f * 2 + 1] = ialpha[base + 1];
            a23[f * 2] = ialpha[base + 2];
            a23[f * 2 + 1] = ialpha[base + 3];
        }
        ialpha01 = Some(a01);
        ialpha23 = Some(a23);
    } else {
        ialpha01 = None;
        ialpha23 = None;
    }

    // yofs/ibeta
    let mut yofs = vec![0i32; dh];
    let mut ibeta = vec![0i16; dh * K];
    for dy in 0..dh {
        // 对齐 OpenCV：fy = (float)(...)，f32 域减法；area_mode 同 x
        let (sy, fyp) = if area_mode {
            let s = (dy as f64 * scale_y).floor() as i32;
            let v = (dy as f64 + 1.0) - (s as f64 + 1.0) * fy;
            let v = if v <= 0.0 { 0.0 } else { v - v.floor() };
            (s, v as f32)
        } else {
            let f = ((dy as f64 + 0.5) * scale_y - 0.5) as f32;
            let sy = cv_floor(f as f64);
            (sy, f - sy as f32)
        };
        yofs[dy] = sy;
        let mut cbuf = [0.0f32; 4];
        match K {
            2 => {
                cbuf[0] = 1.0 - fyp;
                cbuf[1] = fyp;
            }
            _ => interpolate_cubic(fyp, &mut cbuf),
        }
        for k in 0..K {
            ibeta[dy * K + k] = sat_short(cbuf[k] * COEF_SCALE as f32);
        }
    }

    // xmin/xmax 转扁平域（对齐 resizeGeneric_: xmin *= cn; xmax *= cn）
    let xmin = xmin * C;
    let xmax = xmax * C;
    // VBMI 快路径帧级检测（xofs 帧内不变，避免逐行重复检测）
    let vbmi = K == 4
        && C == 3
        && crate::simd_kernels::hresize_cubic_vbmi_pattern(&xofs, C, xmax);
    // 主体（对齐 resizeGeneric_Invoker：行缓存复用 + HResize + VResize）。
    // bufstep = alignSize(dsize.width*cn, 16)（dsize.width 已 *= cn）
    let bufstep = (dw * C + 15) & !15;
    let mut buffer: Vec<i32> = crate::simd_kernels::alloc_uninit(bufstep * K);
    let mut prev_sy = [-1i32; 4];
    use rayon::prelude::*;
    #[allow(clippy::too_many_arguments)]
    fn run<const C: usize, const K: usize>(
        src: &[u8],
        sw: usize,
        sh: usize,
        dst: &mut [u8],
        dw: usize,
        xofs: &[i32],
        ialpha: &[i16],
        ialpha01: &[i16],
        ialpha23: &[i16],
        yofs: &[i32],
        ibeta: &[i16],
        xmin: usize,
        xmax: usize,
        bufstep: usize,
        y0: usize,
        _y1: usize,
        buffer: &mut [i32],
        prev_sy: &mut [i32; 4],
        vbmi: bool,
    ) {
        let ksize2 = K / 2;
        for (li, drow_all) in dst.chunks_mut(dw * C).enumerate() {
            let dy = y0 + li;
            let sy0 = yofs[dy];
            // 行缓存：与 OpenCV 相同的复用策略（sy 命中则 memcpy）
            let mut k0 = K;
            let mut k1 = 0usize;
            for k in 0..K {
                let sy = (sy0 - ksize2 as i32 + 1 + k as i32).clamp(0, sh as i32 - 1);
                k1 = k1.max(k);
                while k1 < K {
                    if sy == prev_sy[k1] {
                        if k1 > k {
                            let (a, b) = buffer.split_at_mut(k1 * bufstep);
                            a[k * bufstep..(k + 1) * bufstep]
                                .copy_from_slice(&b[..bufstep]);
                        }
                        break;
                    }
                    k1 += 1;
                }
                if k1 == K {
                    k0 = k0.min(k);
                }
                prev_sy[k] = sy;
            }
            // HResize：k0..K 行
            for k in k0..K {
                let _ = k;
                let sy = prev_sy[k] as usize;
                let srow = &src[sy * sw * C..(sy + 1) * sw * C];
                let drow = &mut buffer[k * bufstep..(k + 1) * bufstep];
                // [0, xmin)：边框区（sx 越界回绕，对齐 HResizeLinear/Cubic 边框分支）
                for dx in 0..xmin {
                    let mut v: i32 = 0;
                    let sx = xofs[dx] - C as i32;
                    for j in 0..K {
                        let mut sxj = sx + j as i32 * C as i32;
                        if sxj < 0 || sxj >= (sw * C) as i32 {
                            while sxj < 0 {
                                sxj += C as i32;
                            }
                            while sxj >= (sw * C) as i32 {
                                sxj -= C as i32;
                            }
                        }
                        v += srow[sxj as usize] as i32 * ialpha[dx * K + j] as i32;
                    }
                    drow[dx] = v;
                }
                // [xmin, xmax)：主体（扁平域）。
                // linear（K=2）：xofs[dx] = 左像素，采样 [sx, sx+C]
                // cubic（K=4）：对齐 HResizeCubic 的 sx = xofs[dx] - cn，
                //   采样 [sx-C, sx+2C]（xofs 存中心像素）
                let srow_len = srow.len();
                if K == 2 {
                    // SIMD 主段（对齐 HResizeLinearVecU8_X4）+ 标量尾段
                    crate::simd_kernels::hresize_linear_u8(
                        srow, xofs, ialpha, drow, xmin, xmax, C, srow_len,
                    );
                } else if vbmi {
                    // AVX512VBMI 快路径（帧级已检测 stride=6 等距模式）
                    crate::simd_kernels::hresize_cubic_vbmi(
                        srow, xofs, ialpha, ialpha01, ialpha23,
                        drow, xmin, xmax, C, srow_len,
                    );
                } else {
                    // SIMD 主段（对齐 HResizeCubic 标量语义）+ 标量尾段
                    crate::simd_kernels::hresize_cubic_u8(
                        srow, xofs, ialpha, ialpha01, ialpha23,
                        drow, xmin, xmax, C, srow_len,
                    );
                }
                // [xmax, dw*C)：尾段。
                // linear（HResizeLinear）：ONE 拷贝（xofs 已 clamp）
                // cubic（HResizeCubic）：与首段相同的回绕逻辑（limit=dwidth 二次边框段）
                if K == 2 {
                    for dx in xmax..dw * C {
                        drow[dx] = srow[xofs[dx] as usize] as i32 * COEF_SCALE;
                    }
                } else {
                    for dx in xmax..dw * C {
                        let mut v: i32 = 0;
                        let sx = xofs[dx] - C as i32;
                        for j in 0..K {
                            let mut sxj = sx + j as i32 * C as i32;
                            if sxj < 0 || sxj >= (sw * C) as i32 {
                                while sxj < 0 {
                                    sxj += C as i32;
                                }
                                while sxj >= (sw * C) as i32 {
                                    sxj -= C as i32;
                                }
                            }
                            v += srow[sxj as usize] as i32 * ialpha[dx * K + j] as i32;
                        }
                        drow[dx] = v;
                    }
                }
            }
            // VResize（u8 定点）：
            // linear（K=2）对齐 VResizeLinear<uchar,int,short> 的压缩公式：
            //   dst = (((b0*(S0>>4))>>16) + ((b1*(S1>>4))>>16) + 2) >> 2
            // cubic（K=4）对齐 VResizeCubic + FixedPtCast<int,uchar,22>：
            //   dst = (Σ βk·Sk + 2^21) >> 22
            let b: [i32; 4] = [
                ibeta[dy * K] as i32,
                ibeta[dy * K + 1] as i32,
                if K > 2 { ibeta[dy * K + 2] as i32 } else { 0 },
                if K > 3 { ibeta[dy * K + 3] as i32 } else { 0 },
            ];
            if K == 2 {
                // SIMD 主段（对齐 VResizeLinearVec_32s8u）+ 标量尾段
                let width = dw * C;
                let s0 = &buffer[..width];
                let s1 = &buffer[bufstep..bufstep + width];
                crate::simd_kernels::vresize_linear_u8(
                    s0, s1, &mut drow_all[..width], b[0] as i16, b[1] as i16, width,
                );
            } else {
                // cubic：f32 muladd（对齐 VResizeCubicVec_32s8u SIMD 主段：
                // b*k = beta[k]·(1/2048²) 转 f32，acc = Σ S·bf，v_round）
                // SIMD 主段 + 标量尾段（对齐 VResizeCubicVec_32s8u：
                // OpenCV CV_SIMD baseline = SSE2，v_int16 = 8 lanes，
                // v_muladd = 分离乘加（无 FMA，每步舍入）；
                // 尾段走 VResizeCubic 标量 FixedPtCast (val+2^21)>>22）
                let width = dw * C;
                let scale = 1.0f32 / (COEF_SCALE as f32 * COEF_SCALE as f32);
                let s0 = &buffer[..width];
                let s1 = &buffer[bufstep..bufstep + width];
                let s2 = &buffer[2 * bufstep..2 * bufstep + width];
                let s3 = &buffer[3 * bufstep..3 * bufstep + width];
                crate::simd_kernels::vresize_cubic_u8(
                    s0, s1, s2, s3, &mut drow_all[..width], b, scale, width,
                );
            }
        }
    }
    const MIN_ROWS: usize = 16;
    if dh >= MIN_ROWS * 2 && rayon::current_num_threads() > 1 {
        // 每线程独立 buffer/prev_sy（对齐 parallel_for_ 每 range 一份 AutoBuffer）
        dst.par_chunks_mut(dw * C * MIN_ROWS)
            .enumerate()
            .for_each(|(bi, d)| {
                let y0 = bi * MIN_ROWS;
                let y1 = y0 + d.len() / (dw * C);
                let mut buf: Vec<i32> = crate::simd_kernels::alloc_uninit(bufstep * K);
                let mut ps = [-1i32; 4];
                run::<C, K>(src, sw, sh, d, dw, &xofs, &ialpha, ialpha01.as_deref().unwrap_or(&[]), ialpha23.as_deref().unwrap_or(&[]), &yofs, &ibeta, xmin, xmax, bufstep, y0, y1, &mut buf, &mut ps, vbmi);
            });
    } else {
        run::<C, K>(src, sw, sh, dst, dw, &xofs, &ialpha, ialpha01.as_deref().unwrap_or(&[]), ialpha23.as_deref().unwrap_or(&[]), &yofs, &ibeta, xmin, xmax, bufstep, 0, dh, &mut buffer, &mut prev_sy, vbmi);
    }
}

/// INTER_AREA（对齐 resize.cpp:4010-4060：整数倍 fast / 非整数 DecimateAlpha）。
fn resize_area_u8<const C: usize>(
    src: &[u8],
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    fx: f64,
    fy: f64,
    scale_x: f64,
    scale_y: f64,
) {
    let iscale_x = cv_round(scale_x);
    let iscale_y = cv_round(scale_y);
    let is_area_fast =
        (scale_x - iscale_x as f64).abs() < f64::EPSILON && (scale_y - iscale_y as f64).abs() < f64::EPSILON;

    if is_area_fast {
        // 干净 2×2 缩小（sw/sh 偶数，无边框）：SIMD 快路径
        if iscale_x == 2 && iscale_y == 2 && sw % 2 == 0 && sh % 2 == 0 && (C == 1 || C == 3) {
            match C {
                1 => crate::simd_kernels::area_fast_2x2_cn1(src, dst, sw, dw, dh),
                _ => crate::simd_kernels::area_fast_2x2_cn3(src, dst, sw, dw, dh),
            }
            return;
        }
        // resizeAreaFast_：整数倍缩小（对齐 resizeAreaFast_Invoker）
        let area = (iscale_x * iscale_y) as usize;
        let scale = 1.0f32 / area as f32;
        let dwidth1 = (sw / iscale_x as usize) * C;
        // ofs：sy*srcstep + sx*cn（相对块首）
        let mut ofs = vec![0usize; area];
        let mut k = 0;
        for sy in 0..iscale_y as usize {
            for sx in 0..iscale_x as usize {
                ofs[k] = sy * sw * C + sx * C;
                k += 1;
            }
        }
        let mut xofs = vec![0usize; dw * C];
        for dx in 0..dw {
            let j = dx * C;
            let sx = iscale_x as usize * j;
            for c in 0..C {
                xofs[j + c] = sx + c;
            }
        }
        use rayon::prelude::*;
        #[allow(clippy::too_many_arguments)]
        fn run<const C: usize>(
            src: &[u8],
            sw: usize,
            sh: usize,
            dst: &mut [u8],
            dw: usize,
            iscale_x: usize,
            iscale_y: usize,
            ofs: &[usize],
            xofs: &[usize],
            dwidth1: usize,
            scale: f32,
            y0: usize,
            _y1: usize,
        ) {
            for (li, drow) in dst.chunks_mut(dw * C).enumerate() {
                let dy = y0 + li;
                let sy0 = dy * iscale_y;
                if sy0 >= sh {
                    drow.fill(0);
                    continue;
                }
                let srow0 = &src[sy0 * sw * C..];
                let w = if sy0 + iscale_y <= sh { dwidth1 } else { 0 };
                // 对齐 OpenCV ResizeAreaFastVec：
                // - 2x2（fast_mode）：整行用整数 (sum+2)>>2（SIMD + 标量尾段一致）
                // - 其他整数倍：整行用 f32 saturate_cast<uchar>(sum*scale)（cvRound ties-even）
                let fast2x2 = iscale_x == 2 && iscale_y == 2 && (C == 1 || C == 3 || C == 4);
                if fast2x2 {
                    for dx in 0..w {
                        let s = &srow0[xofs[dx]..];
                        let mut sum = 0u32;
                        for &o in ofs {
                            sum += s[o] as u32;
                        }
                        drow[dx] = ((sum + 2) >> 2) as u8;
                    }
                } else {
                    for dx in 0..w {
                        let s = &srow0[xofs[dx]..];
                        let mut sum = 0u32;
                        for &o in ofs {
                            sum += s[o] as u32;
                        }
                        drow[dx] = ((sum as f32 * scale).round_ties_even().clamp(0.0, 255.0)) as u8;
                    }
                }
                // 右/下边框：按实际覆盖像素数平均（对齐 resizeAreaFast_Invoker 尾段：
                // for(sx=0; sx<scale_x*cn; sx+=cn) if(sx0+sx >= ssize.width*cn) break）
                for dx in w..dw * C {
                    let sx0 = xofs[dx];
                    if sx0 >= sw * C {
                        drow[dx] = 0;
                        continue;
                    }
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for sy in 0..iscale_y {
                        if sy0 + sy >= sh {
                            break;
                        }
                        let srow = &src[(sy0 + sy) * sw * C..];
                        let mut rel = 0usize;
                        while rel < iscale_x * C && sx0 + rel < sw * C {
                            sum += srow[sx0 + rel] as u32;
                            count += 1;
                            rel += C;
                        }
                    }
                    drow[dx] = if count > 0 {
                        ((sum as f32 / count as f32).round_ties_even().clamp(0.0, 255.0)) as u8
                    } else {
                        0
                    };
                }
            }
        }
        const MIN_ROWS: usize = 16;
        if dh >= MIN_ROWS * 2 && rayon::current_num_threads() > 1 {
            dst.par_chunks_mut(dw * C * MIN_ROWS)
                .enumerate()
                .for_each(|(bi, d)| {
                    run::<C>(src, sw, sh, d, dw, iscale_x as usize, iscale_y as usize, &ofs, &xofs, dwidth1, scale, bi * MIN_ROWS, bi * MIN_ROWS + d.len() / (dw * C))
                });
        } else {
            run::<C>(src, sw, sh, dst, dw, iscale_x as usize, iscale_y as usize, &ofs, &xofs, dwidth1, scale, 0, dh);
        }
        return;
    }

    // 非整数倍：DecimateAlpha f32 表（对齐 computeResizeAreaTab + ResizeArea_Invoker）
    #[derive(Clone, Copy)]
    struct DecimateAlpha {
        di: i32,
        si: i32,
        alpha: f32,
    }
    fn compute_tab(ssize: usize, dsize: usize, cn: usize, scale: f64) -> Vec<DecimateAlpha> {
        let mut tab = Vec::new();
        for dx in 0..dsize {
            let fsx1 = dx as f64 * scale;
            let fsx2 = fsx1 + scale;
            let cell_width = (scale).min(ssize as f64 - fsx1);
            let mut sx1 = fsx1.ceil() as i32;
            let mut sx2 = fsx2.floor() as i32;
            sx2 = sx2.min(ssize as i32 - 1);
            sx1 = sx1.min(sx2);
            if sx1 as f64 - fsx1 > 1e-3 {
                tab.push(DecimateAlpha {
                    di: (dx * cn) as i32,
                    si: (sx1 - 1) * (cn as i32),
                    alpha: ((sx1 as f64 - fsx1) / cell_width) as f32,
                });
            }
            for sx in sx1..sx2 {
                tab.push(DecimateAlpha {
                    di: (dx * cn) as i32,
                    si: sx * cn as i32,
                    alpha: (1.0 / cell_width) as f32,
                });
            }
            if fsx2 - sx2 as f64 > 1e-3 {
                tab.push(DecimateAlpha {
                    di: (dx * cn) as i32,
                    si: sx2 * cn as i32,
                    alpha: ((fsx2 - sx2 as f64).min(1.0).min(cell_width) / cell_width) as f32,
                });
            }
        }
        tab
    }
    let xtab = compute_tab(sw, dw, C, scale_x);
    let ytab = compute_tab(sh, dh, 1, scale_y);
    // tabofs：每个 dy 的 ytab 起始
    let mut tabofs = vec![0usize; dh + 1];
    {
        let mut dy = 0;
        for (k, t) in ytab.iter().enumerate() {
            if k == 0 || t.di != ytab[k - 1].di {
                tabofs[dy] = k;
                dy += 1;
            }
        }
        tabofs[dy] = ytab.len();
    }
    let dwidth = dw * C;
    use rayon::prelude::*;
    #[allow(clippy::too_many_arguments)]
    fn run<const C: usize>(
        src: &[u8],
        sw: usize,
        dst: &mut [u8],
        xtab: &[DecimateAlpha],
        ytab: &[DecimateAlpha],
        tabofs: &[usize],
        dwidth: usize,
        y0: usize,
        y1: usize,
    ) {
        let mut buf = vec![0.0f32; dwidth];
        let mut sum = vec![0.0f32; dwidth];
        let j_start = tabofs[y0];
        let j_end = tabofs[y1];
        let mut prev_dy = ytab[j_start].di as usize;
        // dst 是 [y0, y1) 的块切片，局部行号 = 全局 di - y0
        let store = |dst: &mut [u8], gdy: usize, sum: &[f32]| {
            let lrow = gdy - y0;
            let drow = &mut dst[lrow * dwidth..(lrow + 1) * dwidth];
            for (d, &s) in drow.iter_mut().zip(sum) {
                *d = (s.round_ties_even().clamp(0.0, 255.0)) as u8;
            }
        };
        for j in j_start..j_end {
            let beta = ytab[j].alpha;
            let dy = ytab[j].di as usize;
            let sy = ytab[j].si as usize;
            let srow = &src[sy * sw * C..(sy + 1) * sw * C];
            buf.fill(0.0);
            for t in xtab {
                let dxn = t.di as usize;
                let alpha = t.alpha;
                for c in 0..C {
                    buf[dxn + c] += srow[t.si as usize + c] as f32 * alpha;
                }
            }
            if dy != prev_dy {
                store(dst, prev_dy, &sum);
                for dx in 0..dwidth {
                    sum[dx] = beta * buf[dx];
                }
                prev_dy = dy;
            } else {
                for dx in 0..dwidth {
                    sum[dx] += beta * buf[dx];
                }
            }
        }
        store(dst, prev_dy, &sum);
    }
    const MIN_ROWS: usize = 16;
    if dh >= MIN_ROWS * 2 && rayon::current_num_threads() > 1 {
        dst.par_chunks_mut(dwidth * MIN_ROWS)
            .enumerate()
            .for_each(|(bi, d)| {
                run::<C>(src, sw, d, &xtab, &ytab, &tabofs, dwidth, bi * MIN_ROWS, bi * MIN_ROWS + d.len() / dwidth)
            });
    } else {
        run::<C>(src, sw, dst, &xtab, &ytab, &tabofs, dwidth, 0, dh);
    }
    let _ = (fx, fy);
}

// ---------------------------------------------------------------------------
// warp
// ---------------------------------------------------------------------------

/// 仿射变换矩阵（2×3，行主序）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AffineMatrix {
    /// 6 元素：`[m00, m01, m02, m10, m11, m12]`。
    pub m: [f64; 6],
}

impl AffineMatrix {
    pub const fn new(m: [f64; 6]) -> Self {
        Self { m }
    }

    /// 逆映射矩阵（对齐 warpAffine 的 M 求逆段，imgwarp.cpp:2395-2408）。
    ///
    /// 返回 None 当行列式为 0（OpenCV 语义 D=0 → 全 0 矩阵，此处显式报错）。
    pub fn invert(&self) -> Option<Self> {
        let m = &self.m;
        let mut d = m[0] * m[4] - m[1] * m[3];
        if d == 0.0 {
            return None;
        }
        d = 1.0 / d;
        let a11 = m[4] * d;
        let a22 = m[0] * d;
        let m01 = m[1] * -d;
        let m10 = m[3] * -d;
        let b1 = -a11 * m[2] - m01 * m[5];
        let b2 = -m10 * m[2] - a22 * m[5];
        Some(Self {
            m: [a11, m01, b1, m10, a22, b2],
        })
    }
}

/// 2D 旋转矩阵（对应 `cv::getRotationMatrix2D`，imgwarp.cpp:2887）。
///
/// `angle` 单位度，顺时针（图像坐标系 y 向下）。
pub fn get_rotation_matrix_2d(center: (f64, f64), angle: f64, scale: f64) -> AffineMatrix {
    let angle = angle * std::f64::consts::PI / 180.0;
    let alpha = angle.cos() * scale;
    let beta = angle.sin() * scale;
    AffineMatrix::new([
        alpha,
        beta,
        (1.0 - alpha) * center.0 - beta * center.1,
        -beta,
        alpha,
        beta * center.0 + (1.0 - alpha) * center.1,
    ])
}

/// 透视变换矩阵（3×3，行主序）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerspectiveMatrix {
    /// 9 元素：`[m00..m22]`。
    pub m: [f64; 9],
}

impl PerspectiveMatrix {
    pub const fn new(m: [f64; 9]) -> Self {
        Self { m }
    }

    /// 3×3 求逆（伴随法；奇异返回 None）。
    pub fn invert(&self) -> Option<Self> {
        let m = &self.m;
        let det = m[0] * (m[4] * m[8] - m[5] * m[7])
            - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        if det == 0.0 {
            return None;
        }
        let inv = 1.0 / det;
        let r = [
            (m[4] * m[8] - m[5] * m[7]) * inv,
            (m[2] * m[7] - m[1] * m[8]) * inv,
            (m[1] * m[5] - m[2] * m[4]) * inv,
            (m[5] * m[6] - m[3] * m[8]) * inv,
            (m[0] * m[8] - m[2] * m[6]) * inv,
            (m[2] * m[3] - m[0] * m[5]) * inv,
            (m[3] * m[7] - m[4] * m[6]) * inv,
            (m[1] * m[6] - m[0] * m[7]) * inv,
            (m[0] * m[4] - m[1] * m[3]) * inv,
        ];
        Some(Self { m: r })
    }
}

/// warp 边框模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpBorder {
    /// `BORDER_CONSTANT`：越界填常量。
    Constant([u8; 4]),
    /// `BORDER_REPLICATE`：越界复制边缘。
    Replicate,
}

/// 仿射变换（对应 `cv::warpAffine`）。
///
/// `m` 为**前向**映射（dst → src 的逆映射在内部求逆，对齐 OpenCV
/// 无 `WARP_INVERSE_MAP` 时的行为）。u8 单/三通道与 OpenCV 逐位一致
/// （AB_BITS=10 定点 + INTER_BITS=5 子像素表 + 15 位定点双线性）。
///
/// # Panics
/// `m` 奇异（行列式 0）。
pub fn warp_affine<T: crate::core::image::Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    m: &AffineMatrix,
    dw: usize,
    dh: usize,
    interpolation: Interpolation,
    border: WarpBorder,
) -> Image<T, C> {
    let inv = m.invert().expect("warp_affine: singular matrix");
    // OpenCV 5.0 warp_kernels.simd.hpp：M 先转 f32
    let m: [f32; 6] = core::array::from_fn(|i| inv.m[i] as f32);
    let (sw, sh) = (src.width(), src.height());
    let mut out: Image<T, C> = Image::new_uninit(dw, dh);
    let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());

    warp_run::<C>(
        src_bytes, sw, sh, out_bytes, dw, dh, &m, None, interpolation, border,
    );
    out
}

/// OpenCV 5.0 warp 内核公共实现（warp_kernels.simd.hpp）。
///
/// - affine：`m` 为 6 元素（已求逆、已转 f32），`pm` 为 None
/// - perspective：`m` 为 9 元素，`pm` 为 Some
/// - 坐标：SIMD 主段 `sx = fma(M0, x, M_x)`（M_x = f32(y*M1+M2)）；
///   标量尾段 `sx = x*M0 + y*M1 + M2`（分离乘加）——两者数值不同，
///   以 uf=32（AVX512 v_float32=16 lanes）为界
/// - NN：`ix = round_ties_even(sx)`；Linear：`ix = floor(sx)`，`s -= ix`
/// - 边界：inlier `ix < sw-1 && iy < sh-1`（unsigned）；完全出图
///   `ix+1 >= sw+1 || iy+1 >= sh+1`（unsigned）→ CONSTANT cval；
///   否则 FETCH_PIXEL（界内原值 / CONSTANT cval / borderInterpolate_fast）
#[allow(clippy::too_many_arguments)]
fn warp_run<const C: usize>(
    src: &[u8],
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    m: &[f32],
    pm: Option<&[f32; 9]>,
    interpolation: Interpolation,
    border: WarpBorder,
) {
    use rayon::prelude::*;
    // AVX2：v_float32 = 8 lanes，uf = 16（warp_kernels 只编译到 AVX2）
    const UF: usize = 16;
    let nearest = matches!(interpolation, Interpolation::Nearest);
    let linear = matches!(interpolation, Interpolation::Linear);
    let cubic = matches!(interpolation, Interpolation::Cubic);
    let row = dw * C;
    let step = sw * C;

    #[inline(always)]
    fn fetch<const C: usize>(
        src: &[u8], sw: usize, sh: usize, _step: usize,
        ix: i32, iy: i32, border: WarpBorder,
    ) -> [u8; 4] {
        // 界内直接取
        if ix >= 0 && (ix as usize) < sw && iy >= 0 && (iy as usize) < sh {
            let s = (iy as usize * sw + ix as usize) * C;
            let mut v = [0u8; 4];
            v[..C].copy_from_slice(&src[s..s + C]);
            return v;
        }
        match border {
            WarpBorder::Constant(v) => v,
            WarpBorder::Replicate => {
                let x = if ix < 0 { 0 } else if ix as usize >= sw { sw as i32 - 1 } else { ix };
                let y = if iy < 0 { 0 } else if iy as usize >= sh { sh as i32 - 1 } else { iy };
                let s = (y as usize * sw + x as usize) * C;
                let mut v = [0u8; 4];
                v[..C].copy_from_slice(&src[s..s + C]);
                v
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run<const C: usize>(
        src: &[u8], sw: usize, sh: usize, step: usize,
        dst: &mut [u8], dw: usize, row: usize,
        m: &[f32], pm: Option<&[f32; 9]>,
        nearest: bool, linear: bool, cubic: bool,
        border: WarpBorder, y0: usize, _y1: usize,
    ) {
        let simd_end = if dw >= UF { (dw - UF) / UF * UF + UF } else { 0 };
        for (li, drow) in dst.chunks_mut(row).enumerate() {
            let y = y0 + li;
            let yf = y as f32;
            // LINEAR 快路径：SIMD 主段按 8 像素批处理（all-inlier 才走，否则回退标量）
            if linear && C <= 3 {
                #[allow(clippy::manual_range_contains)]
                let avx_floor = |v: f32| -> i32 {
                    let f = v.floor();
                    if f >= 2147483648.0 || f < -2147483648.0 || f.is_nan() {
                        i32::MIN
                    } else {
                        f as i32
                    }
                };
                let mut x = 0;
                let replicate = matches!(border, WarpBorder::Replicate);
                while x + 8 <= simd_end {
                    if crate::simd_kernels::warp_linear_fast_u8(
                        src, sw, sh, step, drow, m, pm, y, x, C, replicate,
                    ) {
                        x += 8;
                    } else {
                        // 回退标量：处理这 8 像素
                        for xx in x..x + 8 {
                            let (sxf, syf) = match pm {
                                None => {
                                    let m_x = yf * m[1] + m[2];
                                    let m_y = yf * m[4] + m[5];
                                    (
                                        m[0].mul_add(xx as f32, m_x),
                                        m[3].mul_add(xx as f32, m_y),
                                    )
                                }
                                Some(p) => {
                                    let m_x = yf * p[1] + p[2];
                                    let m_y = yf * p[4] + p[5];
                                    let m_w = yf * p[7] + p[8];
                                    let w = p[6].mul_add(xx as f32, m_w);
                                    let xf = p[0].mul_add(xx as f32, m_x);
                                    let yf2 = p[3].mul_add(xx as f32, m_y);
                                    (xf / w, yf2 / w)
                                }
                            };
                            let ix = avx_floor(sxf);
                            let iy = avx_floor(syf);
                            let sx = sxf - ix as f32;
                            let sy = syf - iy as f32;
                            if ix >= 0
                                && (ix as u32) < (sw as u32 - 1)
                                && iy >= 0
                                && (iy as u32) < (sh as u32 - 1)
                            {
                                let s = (iy as usize * sw + ix as usize) * C;
                                for c in 0..C {
                                    let p00 = src[s + c] as f32;
                                    let p01 = src[s + C + c] as f32;
                                    let p10 = src[s + step + c] as f32;
                                    let p11 = src[s + step + C + c] as f32;
                                    let v0 = sx.mul_add(p01 - p00, p00);
                                    let v1 = sx.mul_add(p11 - p10, p10);
                                    let v = sy.mul_add(v1 - v0, v0);
                                    drow[xx * C + c] =
                                        v.round_ties_even().clamp(0.0, 255.0) as u8;
                                }
                            } else if matches!(border, WarpBorder::Constant(_)) {
                                let out_x = (ix as u32).wrapping_add(1) >= (sw as u32).wrapping_add(1);
                                let out_y = (iy as u32).wrapping_add(1) >= (sh as u32).wrapping_add(1);
                                if out_x || out_y {
                                    if let WarpBorder::Constant(v) = border {
                                        for c in 0..C {
                                            drow[xx * C + c] = v[c & 3];
                                        }
                                    }
                                    continue;
                                }
                                let p00 = fetch::<C>(src, sw, sh, step, ix, iy, border);
                                let p01 = fetch::<C>(src, sw, sh, step, ix + 1, iy, border);
                                let p10 = fetch::<C>(src, sw, sh, step, ix, iy + 1, border);
                                let p11 = fetch::<C>(src, sw, sh, step, ix + 1, iy + 1, border);
                                for c in 0..C {
                                    let a0 = p00[c & 3] as f32;
                                    let a1 = p01[c & 3] as f32;
                                    let b0 = p10[c & 3] as f32;
                                    let b1 = p11[c & 3] as f32;
                                    let v0 = sx.mul_add(a1 - a0, a0);
                                    let v1 = sx.mul_add(b1 - b0, b0);
                                    let v = sy.mul_add(v1 - v0, v0);
                                    drow[xx * C + c] =
                                        v.round_ties_even().clamp(0.0, 255.0) as u8;
                                }
                            } else {
                                let p00 = fetch::<C>(src, sw, sh, step, ix, iy, border);
                                let p01 = fetch::<C>(src, sw, sh, step, ix + 1, iy, border);
                                let p10 = fetch::<C>(src, sw, sh, step, ix, iy + 1, border);
                                let p11 = fetch::<C>(src, sw, sh, step, ix + 1, iy + 1, border);
                                for c in 0..C {
                                    let a0 = p00[c & 3] as f32;
                                    let a1 = p01[c & 3] as f32;
                                    let b0 = p10[c & 3] as f32;
                                    let b1 = p11[c & 3] as f32;
                                    let v0 = sx.mul_add(a1 - a0, a0);
                                    let v1 = sx.mul_add(b1 - b0, b0);
                                    let v = sy.mul_add(v1 - v0, v0);
                                    drow[xx * C + c] =
                                        v.round_ties_even().clamp(0.0, 255.0) as u8;
                                }
                            }
                        }
                        x += 8;
                    }
                }
            }
            // CUBIC 快路径：SIMD 主段按 8 像素批处理（affine/perspective + replicate 边界）
            if cubic && C <= 3 {
                let replicate = matches!(border, WarpBorder::Replicate);
                let mut x = 0;
                while x + 8 <= simd_end {
                    if crate::simd_kernels::warp_cubic_fast_u8(
                        src, sw, sh, step, drow, m, pm, y, x, C, replicate,
                    ) {
                        x += 8;
                    } else {
                        for xx in x..x + 8 {
                            let (cx, cy) = match pm {
                                None => {
                                    let m_x = yf * m[1] + m[2];
                                    let m_y = yf * m[4] + m[5];
                                    (m_x + m[0] * xx as f32, m_y + m[3] * xx as f32)
                                }
                                Some(p) => {
                                    let xf = xx as f64;
                                    let m_x = (yf * p[1] + p[2]) as f64;
                                    let m_y = (yf * p[4] + p[5]) as f64;
                                    let m_z = (yf * p[7] + p[8]) as f64;
                                    let invz = 1.0 / (m_z + p[6] as f64 * xf);
                                    (
                                        ((m_x + p[0] as f64 * xf) * invz) as f32,
                                        ((m_y + p[3] as f64 * xf) * invz) as f32,
                                    )
                                }
                            };
                            cubic_sample::<C>(src, sw, sh, step, cx, cy, xx, drow, border, false);
                        }
                        x += 8;
                    }
                }
            }
            let main_start = if (linear || cubic) && C <= 3 { simd_end } else { 0 };
            for x in main_start..dw {
                // 坐标（f32 域）
                let (sxf, syf) = if x < simd_end {
                    // SIMD 主段：M_x = f32(y*M1 + M2)（分离），sx = fma(M0, x, M_x)
                    match pm {
                        None => {
                            let m_x = yf * m[1] + m[2];
                            let m_y = yf * m[4] + m[5];
                            (m[0].mul_add(x as f32, m_x), m[3].mul_add(x as f32, m_y))
                        }
                        Some(p) => {
                            let m_x = yf * p[1] + p[2];
                            let m_y = yf * p[4] + p[5];
                            let m_w = yf * p[7] + p[8];
                            let w = p[6].mul_add(x as f32, m_w);
                            let xf = p[0].mul_add(x as f32, m_x);
                            let yf2 = p[3].mul_add(x as f32, m_y);
                            (xf / w, yf2 / w)
                        }
                    }
                } else {
                    // 标量尾段：全分离乘加（GCC 无 contract 的标量路径）
                    match pm {
                        None => (
                            x as f32 * m[0] + yf * m[1] + m[2],
                            x as f32 * m[3] + yf * m[4] + m[5],
                        ),
                        Some(p) => {
                            let w = x as f32 * p[6] + yf * p[7] + p[8];
                            (
                                (x as f32 * p[0] + yf * p[1] + p[2]) / w,
                                (x as f32 * p[3] + yf * p[4] + p[5]) / w,
                            )
                        }
                    }
                };
                if nearest {
                    // AVX cvtps2dq 溢出语义：越界 → i32::MIN（0x80000000）
                    #[allow(clippy::manual_range_contains)]
                    let avx_round = |v: f32| -> i32 {
                        let r = v.round_ties_even();
                        if r >= 2147483648.0 || r < -2147483648.0 {
                            i32::MIN
                        } else {
                            r as i32
                        }
                    };
                    let ix = avx_round(sxf);
                    let iy = avx_round(syf);
                    // inlier（unsigned 语义）
                    if ix >= 0 && (ix as u32) < (sw as u32 - 1) && iy >= 0 && (iy as u32) < (sh as u32 - 1) {
                        let s = (iy as usize * sw + ix as usize) * C;
                        for c in 0..C { drow[x * C + c] = src[s + c]; }
                    } else if matches!(border, WarpBorder::Constant(_)) {
                        // 完全出图（unsigned：ix+1 >= sw+1 || iy+1 >= sh+1）
                        let out_x = (ix as u32).wrapping_add(1) >= (sw as u32).wrapping_add(1);
                        let out_y = (iy as u32).wrapping_add(1) >= (sh as u32).wrapping_add(1);
                        if out_x || out_y {
                            if let WarpBorder::Constant(v) = border {
                                for c in 0..C { drow[x * C + c] = v[c & 3]; }
                            }
                            continue;
                        }
                        let v = fetch::<C>(src, sw, sh, step, ix, iy, border);
                        for c in 0..C { drow[x * C + c] = v[c & 3]; }
                    } else {
                        let v = fetch::<C>(src, sw, sh, step, ix, iy, border);
                        for c in 0..C { drow[x * C + c] = v[c & 3]; }
                    }
                } else if linear {
                    // AVX v_floor = roundps(floor) + cvtps2dq（溢出 → i32::MIN）
                    #[allow(clippy::manual_range_contains)]
                    let avx_floor = |v: f32| -> i32 {
                        let f = v.floor();
                        if f >= 2147483648.0 || f < -2147483648.0 || f.is_nan() {
                            i32::MIN
                        } else {
                            f as i32
                        }
                    };
                    let ix = avx_floor(sxf);
                    let iy = avx_floor(syf);
                    let sx = sxf - ix as f32;
                    let sy = syf - iy as f32;
                    // inlier（unsigned 语义）
                    if ix >= 0 && (ix as u32) < (sw as u32 - 1) && iy >= 0 && (iy as u32) < (sh as u32 - 1) {
                        let s = (iy as usize * sw + ix as usize) * C;
                        for c in 0..C {
                            let p00 = src[s + c] as f32;
                            let p01 = src[s + C + c] as f32;
                            let p10 = src[s + step + c] as f32;
                            let p11 = src[s + step + C + c] as f32;
                            // SIMD 语义：fma(alpha, p1-p0, p0)
                            let v0 = sx.mul_add(p01 - p00, p00);
                            let v1 = sx.mul_add(p11 - p10, p10);
                            let v = sy.mul_add(v1 - v0, v0);
                            drow[x * C + c] = v.round_ties_even().clamp(0.0, 255.0) as u8;
                        }
                    } else if matches!(border, WarpBorder::Constant(_)) {
                        let out_x = (ix as u32).wrapping_add(1) >= (sw as u32).wrapping_add(1);
                        let out_y = (iy as u32).wrapping_add(1) >= (sh as u32).wrapping_add(1);
                        if out_x || out_y {
                            if let WarpBorder::Constant(v) = border {
                                for c in 0..C { drow[x * C + c] = v[c & 3]; }
                            }
                            continue;
                        }
                        let p00 = fetch::<C>(src, sw, sh, step, ix, iy, border);
                        let p01 = fetch::<C>(src, sw, sh, step, ix + 1, iy, border);
                        let p10 = fetch::<C>(src, sw, sh, step, ix, iy + 1, border);
                        let p11 = fetch::<C>(src, sw, sh, step, ix + 1, iy + 1, border);
                        for c in 0..C {
                            let a0 = p00[c & 3] as f32;
                            let a1 = p01[c & 3] as f32;
                            let b0 = p10[c & 3] as f32;
                            let b1 = p11[c & 3] as f32;
                            let v0 = sx.mul_add(a1 - a0, a0);
                            let v1 = sx.mul_add(b1 - b0, b0);
                            let v = sy.mul_add(v1 - v0, v0);
                            drow[x * C + c] = v.round_ties_even().clamp(0.0, 255.0) as u8;
                        }
                    } else {
                        let p00 = fetch::<C>(src, sw, sh, step, ix, iy, border);
                        let p01 = fetch::<C>(src, sw, sh, step, ix + 1, iy, border);
                        let p10 = fetch::<C>(src, sw, sh, step, ix, iy + 1, border);
                        let p11 = fetch::<C>(src, sw, sh, step, ix + 1, iy + 1, border);
                        for c in 0..C {
                            let a0 = p00[c & 3] as f32;
                            let a1 = p01[c & 3] as f32;
                            let b0 = p10[c & 3] as f32;
                            let b1 = p11[c & 3] as f32;
                            let v0 = sx.mul_add(a1 - a0, a0);
                            let v1 = sx.mul_add(b1 - b0, b0);
                            let v = sy.mul_add(v1 - v0, v0);
                            drow[x * C + c] = v.round_ties_even().clamp(0.0, 255.0) as u8;
                        }
                    }
                } else if cubic {
                    // cubic：genericWarp bicubic8u 内核。
                    // 坐标来自 genericWarp xbuf：M_x + M_xx*xf（分离乘加，
                    // 与 NN/Linear 的 fma 不同！）
                    // genericWarp xbuf：M_x + M_xx*xf（分离乘加）
                    let (cx, cy) = match pm {
                        None => {
                            let m_x = yf * m[1] + m[2];
                            let m_y = yf * m[4] + m[5];
                            (m_x + m[0] * x as f32, m_y + m[3] * x as f32)
                        }
                        Some(p) => {
                            // genericWarp warpPerspective：M_x/M_y/M_z 先 f32
                            // 舍入（float(y)*M(0,1)+M(0,2)），再提升 f64
                            let xf = x as f64;
                            let m_x = (yf * p[1] + p[2]) as f64;
                            let m_y = (yf * p[4] + p[5]) as f64;
                            let m_z = (yf * p[7] + p[8]) as f64;
                            let invz = 1.0 / (m_z + p[6] as f64 * xf);
                            (
                                ((m_x + p[0] as f64 * xf) * invz) as f32,
                                ((m_y + p[3] as f64 * xf) * invz) as f32,
                            )
                        }
                    };
                    cubic_sample::<C>(src, sw, sh, step, cx, cy, x, drow, border, x >= simd_end);
                }
            }
            let _ = (cubic,);
        }
    }

    const MIN_ROWS: usize = 16;
    if dh >= MIN_ROWS * 2 && rayon::current_num_threads() > 1 {
        dst.par_chunks_mut(row * MIN_ROWS).enumerate().for_each(|(bi, d)| {
            let y0 = bi * MIN_ROWS;
            let y1 = y0 + d.len() / row;
            run::<C>(src, sw, sh, step, d, dw, row, m, pm, nearest, linear, cubic, border, y0, y1);
        });
    } else {
        run::<C>(src, sw, sh, step, dst, dw, row, m, pm, nearest, linear, cubic, border, 0, dh);
    }
}

/// cubic 采样（对齐 warp_kernels.simd.hpp bicubicVec 的 CV_SIMD_FP16 路径，
/// AVX512_FP16：权重/累加全 f16 域，BATCH=32）。
///
/// - 坐标 clamp（f32）→ floor → alpha/beta 转 f16
/// - inlier：`ix0 < max(w-3,0) && iy0 < max(h-5,0)`（f16 版高度更严）
/// - 权重（f16）：`w0 = A*α*b²; w3 = A*α²*b; w1 = α²((A+2)α-(A+3))+1;
///   w2 = 1-w0-w1-w3`（每步 f16 舍入）
/// - 累加（f16 FMA）：`sumwx = fma(v1,wx1, v0*wx0); sumwx = fma(v2,wx2,sumwx);
///   sumwx = fma(v3,wx3,sumwx); acc = fma(sumwx, wy, acc)`
/// - 输出 v_round（half-even）+ saturate
#[allow(clippy::too_many_arguments)]
fn cubic_sample<const C: usize>(
    src: &[u8], sw: usize, sh: usize, _step: usize,
    sxf: f32, syf: f32,
    dx: usize, drow: &mut [u8], border: WarpBorder,
    scalar_tail: bool,
) {
    const A: f32 = -0.75;
    // 坐标 clamp（f32 域，bicubicCoeffs）
    let bigw = (sw.max(16)) as f32;
    let bigh = (sh.max(16)) as f32;
    let vx = sxf.clamp(-bigw, bigw * 2.0);
    let vy = syf.clamp(-bigh, bigh * 2.0);
    let ix0 = vx.floor() as i32;
    let iy0 = vy.floor() as i32;
    let tl_x = ix0 - 1;
    let tl_y = iy0 - 1;

    // all_outliers（unsigned）
    let out = (tl_x as u32).wrapping_add(4) >= (sw as u32).wrapping_add(4)
        || (tl_y as u32).wrapping_add(4) >= (sh as u32).wrapping_add(4);
    #[allow(clippy::collapsible_if)]
    if out {
        if let WarpBorder::Constant(v) = border {
            for c in 0..C {
                drow[dx * C + c] = v[c & 3];
            }
            return;
        }
    }
    // all_inliers（f16 版：iy0 < max(h-5, 0)）
    let inlier = (tl_x as u32) < (sw.saturating_sub(3) as u32)
        && (tl_y as u32) < (sh.saturating_sub(3) as u32);

    // 权重（f16 域，bicubicWeights v_float16 版）
    #[inline(always)]
    fn weights_f32(alpha: f32) -> [f32; 4] {
        // 对齐 bicubicWeights v_float32 版（AVX2 汇编顺序）：
        // w0 = A*(alpha*b2)、w3 = A*(b*a2)（乘法结合顺序影响 1ulp）
        let a2 = alpha * alpha;
        let b = 1.0 - alpha;
        let b2 = b * b;
        let w0 = A * (alpha * b2);
        let w3 = A * (b * a2);
        let w1 = f32::mul_add(a2, f32::mul_add(A + 2.0, alpha, -(A + 3.0)), 1.0);
        let w2 = 1.0 - w0 - w1 - w3;
        [w0, w1, w2, w3]
    }
    let wx = weights_f32(vx - ix0 as f32);
    let wy = weights_f32(vy - iy0 as f32);

    if inlier {
        // 快速路径：f16 FMA 链
        for c in 0..C {
            let mut acc = 0f32;
            for row in 0..4 {
                let s = ((tl_y + row) as usize * sw + tl_x as usize) * C + c;
                let v0 = src[s] as f32;
                let v1 = src[s + C] as f32;
                let v2 = src[s + 2 * C] as f32;
                let v3 = src[s + 3 * C] as f32;
                let sumwx = f32::mul_add(v1, wx[1], v0 * wx[0]);
                let sumwx = f32::mul_add(v2, wx[2], sumwx);
                let sumwx = f32::mul_add(v3, wx[3], sumwx);
                acc = f32::mul_add(sumwx, wy[row as usize], acc);
            }
            drow[dx * C + c] = acc.round_ties_even().clamp(0.0, 255.0) as u8;
        }
        return;
    }
    // 边界路径（bicubicFetchPixels + f16 FMA）
    for c in 0..C {
        let mut acc = 0f32;
        for row in 0..4 {
            let y0 = tl_y + row;
            let mut v = [0u8; 4];
            match border {
                WarpBorder::Constant(bv) => {
                    let my = (y0 as u32) < (sh as u32);
                    let ycl = y0.clamp(0, sh as i32 - 1) as usize;
                    for j in 0..4 {
                        let xj = tl_x + j;
                        let mx = (xj as u32) < (sw as u32) && my;
                        let xcl = xj.clamp(0, sw as i32 - 1) as usize;
                        let s = (ycl * sw + xcl) * C + c;
                        let pix = src[s] as i32 * mx as i32 + bv[c & 3] as i32 * (1 - mx as i32);
                        v[j as usize] = pix as u8;
                    }
                }
                WarpBorder::Replicate => {
                    let yb = if (y0 as u32) < (sh as u32) {
                        y0
                    } else if y0 < 0 {
                        0
                    } else {
                        sh as i32 - 1
                    };
                    for j in 0..4 {
                        let xj = tl_x + j;
                        let xb = if (xj as u32) < (sw as u32) {
                            xj
                        } else if xj < 0 {
                            0
                        } else {
                            sw as i32 - 1
                        };
                        let s = (yb as usize * sw + xb as usize) * C + c;
                        v[j as usize] = src[s];
                    }
                }
            }
            if scalar_tail {
                // 标量尾段（bicubicRef）：全分离
                let t = v[0] as f32 * wx[0] + v[1] as f32 * wx[1] + v[2] as f32 * wx[2] + v[3] as f32 * wx[3];
                acc += t * wy[row as usize];
            } else {
                // SIMD 段（向量 CALC）：fma 链
                let sumwx = f32::mul_add(v[1] as f32, wx[1], v[0] as f32 * wx[0]);
                let sumwx = f32::mul_add(v[2] as f32, wx[2], sumwx);
                let sumwx = f32::mul_add(v[3] as f32, wx[3], sumwx);
                acc = f32::mul_add(sumwx, wy[row as usize], acc);
            }
        }
        drow[dx * C + c] = acc.round_ties_even().clamp(0.0, 255.0) as u8;
    }
}

pub fn warp_perspective<T: crate::core::image::Pixel + bytemuck::Pod, const C: usize>(
    src: &Image<T, C>,
    m: &PerspectiveMatrix,
    dw: usize,
    dh: usize,
    interpolation: Interpolation,
    border: WarpBorder,
) -> Image<T, C> {
    let inv = m.invert().expect("warp_perspective: singular matrix");
    // OpenCV 5.0：M 转 f32（genericWarp 的 Matx33f）
    let m: [f32; 9] = core::array::from_fn(|i| inv.m[i] as f32);
    let (sw, sh) = (src.width(), src.height());
    let mut out: Image<T, C> = Image::new_uninit(dw, dh);
    let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
    let out_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());

    warp_run::<C>(
        src_bytes, sw, sh, out_bytes, dw, dh, &m, Some(&m), interpolation, border,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_img(w: usize, h: usize, seed: u32) -> Image<u8, 3> {
        let mut img = Image::new(w, h);
        let mut s = seed;
        for p in img.data_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *p = (s >> 24) as u8;
        }
        img
    }

    #[test]
    fn resize_nn_up_down() {
        let img = mk_img(16, 16, 1);
        let up = resize(&img, DSize::Exact(32, 32), Interpolation::Nearest);
        assert_eq!(up.width(), 32);
        // 2× 放大：每个像素重复 2×2
        for y in 0..32 {
            for x in 0..32 {
                for c in 0..3 {
                    assert_eq!(
                        up.data()[(y * 32 + x) * 3 + c],
                        img.data()[((y / 2) * 16 + x / 2) * 3 + c]
                    );
                }
            }
        }
        let down = resize(&up, DSize::Exact(16, 16), Interpolation::Nearest);
        assert_eq!(down.width(), 16);
    }

    #[test]
    fn resize_linear_identity_half() {
        let img = mk_img(32, 32, 2);
        // 同尺寸 = copy
        let same = resize(&img, DSize::Exact(32, 32), Interpolation::Linear);
        assert_eq!(same.data(), img.data());
        // 2× 整数缩小（LINEAR 不切 AREA——OpenCV 只在 is_area_fast 时切，
        // LINEAR 的 2× 缩小也走 AREA fast，此处验证不 panic 且尺寸对）
        let half = resize(&img, DSize::Exact(16, 16), Interpolation::Linear);
        assert_eq!(half.width(), 16);
    }

    #[test]
    fn resize_area_fast_2x() {
        let img = mk_img(32, 32, 3);
        let half = resize(&img, DSize::Exact(16, 16), Interpolation::Area);
        // 2×2 均值（OpenCV area-fast：saturate_cast<uchar>((sum+2)>>2)）
        for y in 0..16 {
            for x in 0..16 {
                for c in 0..3 {
                    let s = img.data()[((2 * y) * 32 + 2 * x) * 3 + c] as u32
                        + img.data()[((2 * y) * 32 + 2 * x + 1) * 3 + c] as u32
                        + img.data()[((2 * y + 1) * 32 + 2 * x) * 3 + c] as u32
                        + img.data()[((2 * y + 1) * 32 + 2 * x + 1) * 3 + c] as u32;
                    let expect = ((s + 2) >> 2) as u8;
                    assert_eq!(half.data()[(y * 16 + x) * 3 + c], expect);
                }
            }
        }
    }

    #[test]
    fn rotation_matrix_roundtrip() {
        let m = get_rotation_matrix_2d((8.0, 8.0), 30.0, 1.0);
        let inv = m.invert().unwrap();
        // M·M⁻¹ ≈ I（仿射 2×3，验证线性部分）
        let a = m.m;
        let b = inv.m;
        let i00 = a[0] * b[0] + a[1] * b[3];
        let i01 = a[0] * b[1] + a[1] * b[4];
        assert!((i00 - 1.0).abs() < 1e-12 && i01.abs() < 1e-12);
    }

    #[test]
    fn warp_affine_identity() {
        let img = mk_img(16, 16, 5);
        let id = AffineMatrix::new([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        let out = warp_affine(&img, &id, 16, 16, Interpolation::Linear, WarpBorder::Constant([9; 4]));
        assert_eq!(out.data(), img.data());
    }

    #[test]
    fn warp_affine_translate_constant() {
        let img = mk_img(16, 16, 6);
        // 前向平移 (2,1)：dst(x,y) = src(x-2, y-1)
        let m = AffineMatrix::new([1.0, 0.0, 2.0, 0.0, 1.0, 1.0]);
        let out = warp_affine(&img, &m, 16, 16, Interpolation::Nearest, WarpBorder::Constant([7; 4]));
        for y in 0..16 {
            for x in 0..16 {
                for c in 0..3 {
                    let expect = if x >= 2 && y >= 1 {
                        img.data()[((y - 1) * 16 + (x - 2)) * 3 + c]
                    } else {
                        7
                    };
                    assert_eq!(out.data()[(y * 16 + x) * 3 + c], expect, "at ({x},{y},{c})");
                }
            }
        }
    }

    #[test]
    fn warp_perspective_identity() {
        let img = mk_img(12, 12, 7);
        let id = PerspectiveMatrix::new([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
        let out = warp_perspective(&img, &id, 12, 12, Interpolation::Linear, WarpBorder::Constant([3; 4]));
        assert_eq!(out.data(), img.data());
    }
}
