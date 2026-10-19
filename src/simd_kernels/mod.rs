//! 手写 SIMD 内循环（唯一允许 `unsafe` 的模块）。
//!
//! 结构（DESIGN.md §6）：
//! - `x86/avx2.rs`：AVX2 内核（主力路径）
//! - `x86/avx512.rs`：AVX512 内核（与 AVX2 同时落地，运行时派发）
//! - `scalar.rs`：标量 fallback（所有平台的正确性基准）
//!
//! 每处 `unsafe` 必须附 `// SAFETY:` 注释说明前置条件（对齐、长度、边界）。
#![allow(unsafe_code)]

use std::mem::MaybeUninit;

pub mod bilateral;
pub mod box_filter;
pub mod contours;
pub mod cvt_color;
pub mod drawing;
pub mod edges;
pub mod fft;
pub mod filter2d;
pub mod match_template;
pub mod gaussian;
pub mod gradient;
pub mod histogram;
pub mod integral;
pub mod median;
pub mod morphology;
pub mod scalar;
pub mod spin_pool;
pub mod x86;

/// 把 `Vec<u8>` 按位重解释为 `Vec<T>`（`T == u8` 场景，TypeId 已由调用方验证）。
///
/// # Safety（调用方契约）
/// 调用方必须保证 `T` 与 `u8` 布局一致（如已用 `TypeId::of::<T>() == TypeId::of::<u8>()` 验证）。
pub fn vec_u8_as<T: 'static>(v: Vec<u8>) -> Vec<T> {
    // SAFETY: 调用方契约保证 T == u8；Vec 分配不变，仅元素类型重解释
    unsafe { std::mem::transmute(v) }
}

/// 分配 `len` 个 `T` 的未初始化缓冲（SIMD 输出整块覆写场景）。
///
/// # Safety（调用方契约）
/// 返回的 Vec 内容未初始化，调用方必须完全覆写后才能读取。
pub fn alloc_uninit<T: Copy + 'static>(len: usize) -> Vec<T> {
    // SAFETY: MaybeUninit<T> 与 T 布局一致（repr(transparent)），
    // Box<[MaybeUninit<T>]> -> Box<[T]> -> Vec<T> 转换保持分配不变；
    // T: Copy + 'static 且 u8/u16/f32 无无效位模式，未初始化内存按位解释总是合法。
    unsafe {
        let boxed: Box<[MaybeUninit<T>]> = Box::new_uninit_slice(len);
        let ptr: *mut [T] = Box::into_raw(boxed) as *mut [T];
        Vec::from(Box::from_raw(ptr))
    }
}

/// 分配 64 字节对齐的未初始化缓冲（非临时存储 `_mm512_stream_*` 需要）。
///
/// 返回 `(Vec<T>, 对齐起始偏移)`：`vec[offset..offset+len]` 起始地址 64 字节对齐。
/// 调用方只使用 `vec[offset..offset+len]`，其余为填充。
///
/// # Safety（调用方契约）
/// 返回的 Vec 内容未初始化，调用方必须完全覆写后才能读取。
pub fn alloc_uninit_aligned64<T: Copy + 'static>(len: usize) -> (Vec<T>, usize) {
    // 64 字节对齐需要最多 64/T 个元素填充
    let pad = 64 / std::mem::size_of::<T>();
    let total = len + pad;
    let mut v = alloc_uninit::<T>(total);
    let base = v.as_ptr() as usize;
    let offset = (64 - (base & 63)) & 63;
    let offset_elems = offset / std::mem::size_of::<T>();
    (v, offset_elems)
}

/// RGB u8 → 灰度 u8（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为 RGB 交错（每像素 3 字节），`dst` 为灰度（每像素 1 字节），`n` 为像素数。
pub fn to_grayscale_u8(src: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::to_grayscale_u8(src, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::to_grayscale_u8(src, dst, n) },
        x86::Isa::Scalar => scalar::to_grayscale_u8(src, dst, n),
    }
}

/// u8 阈值化（运行时派发：AVX512 → AVX2 → 标量查找表）。
///
/// `type_`: 0=Binary 1=BinaryInv 2=Trunc 3=ToZero 4=ToZeroInv。
/// `t`/`mv` 为整数化后的阈值/最大值，`n` 为元素数。
pub fn threshold_u8(src: &[u8], dst: &mut [u8], n: usize, t: u8, mv: u8, type_: u8) {
    match x86::isa() {
        x86::Isa::Avx512 => {
            if type_ == 0 {
                unsafe { x86::avx512::threshold_binary_u8(src, dst, n, t, mv) }
            } else {
                unsafe { x86::avx512::threshold_u8(src, dst, n, t, mv, type_) }
            }
        }
        x86::Isa::Avx2 => {
            if type_ == 0 {
                unsafe { x86::avx2::threshold_binary_u8(src, dst, n, t, mv) }
            } else {
                unsafe { x86::avx2::threshold_u8(src, dst, n, t, mv, type_) }
            }
        }
        x86::Isa::Scalar => scalar::threshold_u8(src, dst, n, t, mv, type_),
    }
}

/// 自适应阈值化（运行时派发：AVX512 → AVX2 → 标量）。
///
/// `d = src - mean`（i32）；`inv=false` 时 `d > -idelta ? imax : 0`，
/// `inv=true` 时 `d <= -idelta ? imax : 0`。`n` 为像素数。
pub fn adaptive_thresh_u8(
    src: &[u8],
    mean: &[u8],
    dst: &mut [u8],
    n: usize,
    imax: u8,
    idelta: i32,
    inv: bool,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::adaptive_thresh_u8(src, mean, dst, n, imax, idelta, inv) },
        x86::Isa::Avx2 => unsafe { x86::avx2::adaptive_thresh_u8(src, mean, dst, n, imax, idelta, inv) },
        x86::Isa::Scalar => scalar::adaptive_thresh_u8(src, mean, dst, n, imax, idelta, inv),
    }
}

/// box_filter 行阶段：u8 → u16 滑动窗行和（运行时派发，通道感知）。
///
/// `src` 为带左右边框的行（长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`。
/// cn=1 走 SIMD（kw=3/5 直接求和），cn>1 走通道感知标量（与 OpenCV 一致）。
pub fn box_row_sum_u8(src: &[u8], dst: &mut [u16], w: usize, kw: usize, cn: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::box_row_sum_u8(src, dst, w, kw, cn) },
        x86::Isa::Avx2 => unsafe { x86::avx2::box_row_sum_u8(src, dst, w, kw, cn) },
        x86::Isa::Scalar => box_filter::box_row_sum_u8_cn(src, dst, w, kw, cn),
    }
}

/// box_filter 行阶段（无边框源）：u8 → u16 行和，kw=3/5，内核内部处理首尾边框。
///
/// `src` 为原始行（长度 ≥ `w*cn`），省去 padded 中转。运行时派发。
pub fn box_row_sum_u8_direct_cn(
    src: &[u8],
    dst: &mut [u16],
    w: usize,
    kw: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::box_row_sum_u8_direct_cn(src, dst, w, kw, cn, border)
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::box_row_sum_u8_direct_cn(src, dst, w, kw, cn, border)
        },
        x86::Isa::Scalar => box_filter::box_row_sum_u8_direct_cn(src, dst, w, kw, cn, border),
    }
}

/// 3×3 距离变换前向扫描（AVX512 向量化 top 行 min + 标量 left 修复）。
///
/// 见 `avx512::dist_forward_3x3` 契约。运行时派发（AVX512 → 标量）。
pub fn dist_forward_3x3(
    temp: &mut [u32],
    s: &[u8],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    dist_max: u32,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::dist_forward_3x3(temp, s, w, step, row, border, hv_dist, diag_dist, dist_max)
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::dist_forward_3x3(temp, s, w, step, row, border, hv_dist, diag_dist, dist_max)
        },
        _ => {
            // 标量 fallback（与 distance.rs 内联逻辑一致）
            let mut left = dist_max;
            for j in 0..w {
                let idx = row + border + j;
                if s[j] == 0 {
                    temp[idx] = 0;
                    left = 0;
                } else {
                    let t0 = temp[idx - step - 1].wrapping_add(diag_dist);
                    let t1 = temp[idx - step].wrapping_add(hv_dist);
                    let t2 = temp[idx - step + 1].wrapping_add(diag_dist);
                    let t3 = left.wrapping_add(hv_dist);
                    let mut m = t0.min(t1);
                    m = m.min(t2);
                    m = m.min(t3);
                    temp[idx] = if m > dist_max { dist_max } else { m };
                    left = temp[idx];
                }
            }
        }
    }
}

/// 5×5 距离变换前向扫描（AVX512 向量化 top1/top2 行 min + 标量 left1/left2 修复）。
///
/// 见 `avx512::dist_forward_5x5` 契约。运行时派发（AVX512 → 标量）。
pub fn dist_forward_5x5(
    temp: &mut [u32],
    s: &[u8],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    long_dist: u32,
    dist_max: u32,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::dist_forward_5x5(
                temp, s, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max,
            )
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::dist_forward_5x5(
                temp, s, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max,
            )
        },
        _ => {
            let mut left1 = dist_max;
            let mut left2 = dist_max;
            for j in 0..w {
                let idx = row + border + j;
                if s[j] == 0 {
                    temp[idx] = 0;
                    left1 = 0;
                    left2 = 0;
                } else {
                    let mut t0 = temp[idx - 2 * step - 1].wrapping_add(long_dist);
                    let mut t = temp[idx - 2 * step + 1].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx - step - 2].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx - step - 1].wrapping_add(diag_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx - step].wrapping_add(hv_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx - step + 1].wrapping_add(diag_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx - step + 2].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = left1.wrapping_add(hv_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    temp[idx] = if t0 > dist_max { dist_max } else { t0 };
                    left2 = left1;
                    left1 = temp[idx];
                }
            }
        }
    }
}

/// 5×5 距离变换后向扫描（AVX512 向量化 bot1/bot2 行 min + 标量 right 修复 + 定点→浮点）。
///
/// 见 `avx512::dist_backward_5x5` 契约。运行时派发（AVX512 → 标量）。
pub fn dist_backward_5x5(
    temp: &mut [u32],
    drow: &mut [f32],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    long_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::dist_backward_5x5(
                temp, drow, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max, scale,
            )
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::dist_backward_5x5(
                temp, drow, w, step, row, border, hv_dist, diag_dist, long_dist, dist_max, scale,
            )
        },
        _ => {
            let mut right = dist_max;
            for j in (0..w).rev() {
                let idx = row + border + j;
                let mut t0 = temp[idx];
                if t0 > hv_dist {
                    let mut t = temp[idx + 2 * step + 1].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + 2 * step - 1].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + step + 2].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + step + 1].wrapping_add(diag_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + step].wrapping_add(hv_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + step - 1].wrapping_add(diag_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = temp[idx + step - 2].wrapping_add(long_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    t = right.wrapping_add(hv_dist);
                    if t < t0 {
                        t0 = t;
                    }
                    temp[idx] = t0;
                }
                right = temp[idx];
            }
            for j in 0..w {
                drow[j] = temp[row + border + j] as f32 * scale;
            }
        }
    }
}

/// 3×3 距离变换后向扫描（AVX512 向量化 bottom 行 min + 标量 right 修复 + 定点→浮点）。
///
/// 见 `avx512::dist_backward_3x3` 契约。运行时派发（AVX512 → 标量）。
pub fn dist_backward_3x3(
    temp: &mut [u32],
    drow: &mut [f32],
    w: usize,
    step: usize,
    row: usize,
    border: usize,
    hv_dist: u32,
    diag_dist: u32,
    dist_max: u32,
    scale: f32,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::dist_backward_3x3(
                temp, drow, w, step, row, border, hv_dist, diag_dist, dist_max, scale,
            )
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::dist_backward_3x3(
                temp, drow, w, step, row, border, hv_dist, diag_dist, dist_max, scale,
            )
        },
        _ => {
            let mut right = temp[row + border + w];
            for j in (0..w).rev() {
                let idx = row + border + j;
                let t0 = temp[idx];
                if t0 > hv_dist {
                    let b1 = temp[idx + step + 1].wrapping_add(diag_dist);
                    let b2 = temp[idx + step].wrapping_add(hv_dist);
                    let b3 = temp[idx + step - 1].wrapping_add(diag_dist);
                    let r = right.wrapping_add(hv_dist);
                    let mut t = b1.min(b2);
                    t = t.min(b3);
                    t = t.min(r);
                    if t < t0 {
                        temp[idx] = t;
                    }
                }
                right = temp[idx];
            }
            for j in 0..w {
                drow[j] = temp[row + border + j] as f32 * scale;
            }
        }
    }
}

#[inline(never)]
/// gaussian 水平阶段：u8 → u16 定点（SHIFT=8，运行时派发）。
pub fn hline_smooth_u8(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    m: &[u16],
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::hline_smooth_u8(src, dst, len, cn, m, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::hline_smooth_u8(src, dst, len, cn, m, border) },
        x86::Isa::Scalar => gaussian::hline_smooth_u8(src, dst, len, cn, m, border),
    }
}

#[inline(never)]
/// gaussian 垂直阶段：kylen 行 u16 定点 → u8（运行时派发）。
pub fn vline_smooth_u8(rows: &[&[u16]], dst: &mut [u8], m: &[u16]) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::vline_smooth_u8(rows, dst, m) },
        x86::Isa::Avx2 => unsafe { x86::avx2::vline_smooth_u8(rows, dst, m) },
        x86::Isa::Scalar => gaussian::vline_smooth_u8(rows, dst, m),
    }
}

#[inline(never)]
/// gaussian 水平阶段 121 核特化（[64,128,64]）：`dst = (a+2b+c)<<6`（运行时派发）。
pub fn hline_smooth_3n121(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::hline_smooth_3n121(src, dst, len, cn, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::hline_smooth_3n121(src, dst, len, cn, border) },
        x86::Isa::Scalar => gaussian::hline_smooth_3n121(src, dst, len, cn, border),
    }
}

#[inline(never)]
/// gaussian 垂直阶段 121 核特化：`(r0+2r1+r2+512)>>10`（运行时派发）。
pub fn vline_smooth_3n121(rows: &[&[u16]; 3], dst: &mut [u8]) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::vline_smooth_3n121(rows, dst) },
        x86::Isa::Avx2 => unsafe { x86::avx2::vline_smooth_3n121(rows, dst) },
        x86::Isa::Scalar => gaussian::vline_smooth_3n121(rows, dst),
    }
}

#[inline(never)]
/// gaussian 水平阶段 14641 核特化（[16,64,96,64,16]）：`(a+4b+6c+4d+e)<<4`（运行时派发）。
pub fn hline_smooth_5n14641(
    src: &[u8],
    dst: &mut [u16],
    len: usize,
    cn: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::hline_smooth_5n14641(src, dst, len, cn, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::hline_smooth_5n14641(src, dst, len, cn, border) },
        x86::Isa::Scalar => gaussian::hline_smooth_5n14641(src, dst, len, cn, border),
    }
}

#[inline(never)]
/// 14641 二维融合主体区（运行时派发）。
#[allow(clippy::too_many_arguments)]
pub fn smooth_5n14641<const C: usize>(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    y_lo: usize,
    y_hi: usize,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::smooth_5n14641::<C>(src, dst, w, h, y_lo, y_hi) },
        x86::Isa::Avx2 => unsafe { x86::avx2::smooth_5n14641::<C>(src, dst, w, h, y_lo, y_hi) },
        x86::Isa::Scalar => gaussian::smooth_5n14641::<C>(src, dst, w, h, y_lo, y_hi),
    }
}

#[inline(never)]
/// gaussian 垂直阶段 14641 核特化：`(6r2+4(r1+r3)+r0+r4+2^11)>>12`（运行时派发）。
pub fn vline_smooth_5n14641(rows: &[&[u16]; 5], dst: &mut [u8]) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::vline_smooth_5n14641(rows, dst) },
        x86::Isa::Avx2 => unsafe { x86::avx2::vline_smooth_5n14641(rows, dst) },
        x86::Isa::Scalar => gaussian::vline_smooth_5n14641(rows, dst),
    }
}

/// 3x3 中值滤波（u8，运行时派发：AVX512 → AVX2 → 标量）。
///
/// `src` 为连续像素（每像素 `cn` 字节），`dst` 同尺寸。Clamp 边界。
pub fn median_blur_u8_3x3(src: &[u8], dst: &mut [u8], w: usize, h: usize, cn: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::median_blur_u8_3x3(src, dst, w, h, cn) },
        x86::Isa::Avx2 => unsafe { x86::avx2::median_blur_u8_3x3(src, dst, w, h, cn) },
        x86::Isa::Scalar => median::median_blur_u8_sortnet(src, dst, w, h, cn, 3),
    }
}

/// box_filter 列阶段单步：u16 行和 → u8（运行时派发，SHIFT=23 定点）。
///
/// `sum` 为跨行滑动累加缓冲（长度 ≥ `w`，调用方初始化为前 kh-1 行之和），
/// `sp` 为当前加行，`sm` 为当前减行，`dst` 长度 ≥ `w`。
pub fn box_col_step_u16(
    sum: &mut [u16],
    sp: &[u16],
    sm: &[u16],
    dst: &mut [u8],
    w: usize,
    fx: box_filter::BoxFixed,
    have_scale: bool,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::box_col_step_u16(sum, sp, sm, dst, w, fx, have_scale)
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::box_col_step_u16(sum, sp, sm, dst, w, fx, have_scale)
        },
        x86::Isa::Scalar => {
            let s0v: Vec<u16> = (0..w).map(|i| sum[i].wrapping_add(sp[i])).collect();
            for i in 0..w {
                let s0 = s0v[i];
                dst[i] = if have_scale {
                    (((s0 as u32 + fx.dd as u32) * fx.ds) >> 23) as u8
                } else {
                    s0.min(255) as u8
                };
                sum[i] = s0.wrapping_sub(sm[i]);
            }
        }
    }
}

/// [`spin_pool::run_blocks`] 的 threshold 桥接：ctx = t | mv<<8 | type<<16。
///
/// # Safety
/// 仅由 spin_pool 内部调用，指针契约由 run_blocks 保证。
pub(crate) unsafe fn threshold_block(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
    let t = ctx as u8;
    let mv = (ctx >> 8) as u8;
    let ty = (ctx >> 16) as u8;
    // SAFETY: 指针与长度契约由 run_blocks 保证
    unsafe {
        let s = std::slice::from_raw_parts(src, len);
        let d = std::slice::from_raw_parts_mut(dst, len);
        threshold_u8(s, d, len, t, mv, ty);
    }
}

/// [`spin_pool::run_blocks`] 的 to_grayscale 桥接：ctx = 像素数/块。
///
/// # Safety
/// 仅由 spin_pool 内部调用，指针契约由 run_blocks 保证。
pub(crate) unsafe fn to_grayscale_block(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
    // SAFETY: 指针与长度契约由 run_blocks 保证；ctx 为该块像素数（len）
    unsafe {
        let s = std::slice::from_raw_parts(src, len * 3);
        let d = std::slice::from_raw_parts_mut(dst, len);
        to_grayscale_u8(s, d, ctx);
    }
}

/// 形态学行阶段：u8 水平滑动窗 min/max（运行时派发，通道感知）。
///
/// `src` 为带左右边框的行（长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`。
/// `erode=true` 为 min，`erode=false` 为 max。
pub fn morph_row_u8(src: &[u8], dst: &mut [u8], w: usize, kw: usize, cn: usize, erode: bool) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::morph_row_u8(src, dst, w, kw, cn, erode) },
        x86::Isa::Avx2 => unsafe { x86::avx2::morph_row_u8(src, dst, w, kw, cn, erode) },
        x86::Isa::Scalar => morphology::row_morph_u8_cn(src, dst, w, kw, cn, erode),
    }
}

/// 形态学列阶段：u8 垂直 min/max（运行时派发）。
///
/// `rows` 为 kh 行 u8（每行长度 ≥ w，stride 相同），`dst` 长度 ≥ w。
pub fn morph_col_u8(rows: &[&[u8]], dst: &mut [u8], w: usize, erode: bool) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::morph_col_u8(rows, dst, w, erode) },
        x86::Isa::Avx2 => unsafe { x86::avx2::morph_col_u8(rows, dst, w, erode) },
        x86::Isa::Scalar => morphology::col_morph_u8_scalar(rows, dst, w, erode),
    }
}

/// 形态学行阶段（无边框源）：u8 水平滑动窗 min/max（运行时派发，通道感知）。
///
/// `src` 为原始行（长度 ≥ `w*cn`，无左右边框），`dst` 长度 ≥ `w*cn`。
/// 首尾 `kw-1` 像素的越界采样由 `border` 策略给出。
pub fn morph_row_u8_direct(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    kw: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    if kw == 3 {
        return morph_row_u8_direct_k3(src, dst, w, cn, erode, border);
    }
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::morph_row_u8_direct(src, dst, w, kw, cn, erode, border)
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::morph_row_u8_direct(src, dst, w, kw, cn, erode, border)
        },
        x86::Isa::Scalar => morphology::row_morph_u8_direct_cn(src, dst, w, kw, cn, erode, border),
    }
}

/// 形态学行阶段（无边框源）kw=3 特化：u8 水平滑动窗 min/max（运行时派发）。
pub fn morph_row_u8_direct_k3(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::morph_row_u8_direct_k3(src, dst, w, cn, erode, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::morph_row_u8_direct_k3(src, dst, w, cn, erode, border) },
        x86::Isa::Scalar => morphology::row_morph_u8_direct_k3_cn(src, dst, w, cn, erode, border),
    }
}

/// 形态学行阶段 kw=3 融合：一次扫描同时产出 min 与 max（运行时派发，cn=1）。
///
/// 供 morph_gradient 融合路径使用（src 只读一遍）。
pub fn morph_row_minmax_k3(
    src: &[u8],
    dst_min: &mut [u8],
    dst_max: &mut [u8],
    w: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::morph_row_minmax_k3(src, dst_min, dst_max, w, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::morph_row_minmax_k3(src, dst_min, dst_max, w, border) },
        x86::Isa::Scalar => morphology::row_morph_minmax_k3_cn(src, dst_min, dst_max, w, border),
    }
}

/// 形态学梯度：`dst = d - e`（u8 饱和减，运行时派发）。
pub fn gradient_u8(d: &[u8], e: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::gradient_u8(d, e, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::gradient_u8(d, e, dst, n) },
        x86::Isa::Scalar => morphology::gradient_u8_scalar(d, e, dst, n),
    }
}

// ==================== 梯度（gradient） ====================

/// 梯度行阶段（u8 → i32，对称核 ksize=3，cn=1，运行时派发）。
///
/// `D[i] = k0*S[i] + k1*(S[i-1] + S[i+1])`，首尾像素按 border 采样。
pub fn deriv_row_symm_k3_cn1(
    src: &[u8],
    dst: &mut [i32],
    w: usize,
    k0: i32,
    k1: i32,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::deriv_row_symm_k3_cn1(src, dst, w, k0, k1, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::deriv_row_symm_k3_cn1(src, dst, w, k0, k1, border) },
        x86::Isa::Scalar => gradient::deriv_row_symm_k3_cn1(src, dst, w, k0, k1, border),
    }
}

/// 梯度行阶段（u8 → i32，反对称核 ksize=3，cn=1，运行时派发）。
///
/// `D[i] = k1*(S[i+1] - S[i-1])`，首尾像素按 border 采样。
pub fn deriv_row_asymm_k3_cn1(
    src: &[u8],
    dst: &mut [i32],
    w: usize,
    k1: i32,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::deriv_row_asymm_k3_cn1(src, dst, w, k1, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::deriv_row_asymm_k3_cn1(src, dst, w, k1, border) },
        x86::Isa::Scalar => gradient::deriv_row_asymm_k3_cn1(src, dst, w, k1, border),
    }
}

/// 梯度列阶段（i32 → i16，对称核 ksize=3，运行时派发）。
///
/// `D = f0*S1 + f1*(S0 + S2) + delta`，饱和 i16。
pub fn deriv_col_symm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f0: i32, f1: i32, delta: i32) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::deriv_col_symm_k3(rows, dst, w, f0, f1, delta) },
        x86::Isa::Avx2 => unsafe { x86::avx2::deriv_col_symm_k3(rows, dst, w, f0, f1, delta) },
        x86::Isa::Scalar => gradient::deriv_col_symm_k3(rows, dst, w, f0, f1, delta),
    }
}

/// 梯度列阶段（i32 → i16，反对称核 ksize=3，运行时派发）。
///
/// `D = f1*(S2 - S0) + delta`，饱和 i16。
pub fn deriv_col_asymm_k3(rows: [&[i32]; 3], dst: &mut [i16], w: usize, f1: i32, delta: i32) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::deriv_col_asymm_k3(rows, dst, w, f1, delta) },
        x86::Isa::Avx2 => unsafe { x86::avx2::deriv_col_asymm_k3(rows, dst, w, f1, delta) },
        x86::Isa::Scalar => gradient::deriv_col_asymm_k3(rows, dst, w, f1, delta),
    }
}

/// Laplacian 3x3（u8 → i16，运行时派发）。
///
/// ksize1=true 核 [0,1,0;1,-4,1;0,1,0]，否则 [2,0,2;0,-8,0;2,0,2]。
pub fn laplacian3_rows_u8(
    rows: [&[u8]; 3],
    dst: &mut [i16],
    w: usize,
    ksize1: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::laplacian3_rows_u8(rows, dst, w, ksize1, border) },
        x86::Isa::Avx2 => unsafe { x86::avx2::laplacian3_rows_u8(rows, dst, w, ksize1, border) },
        x86::Isa::Scalar => gradient::laplacian3_rows_u8(rows, dst, w, ksize1, border),
    }
}

/// magnitude（f32，运行时派发）：`dst = sqrt(x*x + y*y)`。
pub fn magnitude32f(x: &[f32], y: &[f32], dst: &mut [f32], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::magnitude32f(x, y, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::magnitude32f(x, y, dst, n) },
        x86::Isa::Scalar => gradient::magnitude32f_scalar(x, y, dst, n),
    }
}

/// phase（f32，运行时派发）：`dst = atan2(y, x) * scale`（度或弧度）。
pub fn fast_atan2_32f(y: &[f32], x: &[f32], dst: &mut [f32], n: usize, angle_in_degrees: bool) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::fast_atan2_32f(y, x, dst, n, angle_in_degrees) },
        x86::Isa::Avx2 => unsafe { x86::avx2::fast_atan2_32f(y, x, dst, n, angle_in_degrees) },
        x86::Isa::Scalar => gradient::fast_atan2_32f_scalar(y, x, dst, n, angle_in_degrees),
    }
}

/// cart_to_polar（f32，运行时派发）：一次扫描同时产出 mag 与 angle。
pub fn cart_to_polar32f(
    x: &[f32],
    y: &[f32],
    mag: &mut [f32],
    angle: &mut [f32],
    n: usize,
    angle_in_degrees: bool,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::cart_to_polar32f(x, y, mag, angle, n, angle_in_degrees) },
        x86::Isa::Avx2 => unsafe { x86::avx2::cart_to_polar32f(x, y, mag, angle, n, angle_in_degrees) },
        x86::Isa::Scalar => gradient::cart_to_polar32f_scalar(x, y, mag, angle, n, angle_in_degrees),
    }
}

/// Canny 幅度行（i16 dx/dy → i32 mag，运行时派发）：L1 `|dx|+|dy|` 或 L2 `dx²+dy²`。
pub fn canny_mag_row(dx: &[i16], dy: &[i16], mag: &mut [i32], n: usize, l2: bool) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::canny_mag_row(dx, dy, mag, n, l2) },
        x86::Isa::Avx2 => unsafe { x86::avx2::canny_mag_row(dx, dy, mag, n, l2) },
        x86::Isa::Scalar => edges::mag_row(dx, dy, mag, n, l2),
    }
}

/// Canny NMS 批预筛（运行时派发）：`mag_a[0..32] > low` 的 32 位掩码。
///
/// # Safety（调用方契约）
/// `mag_a` 长度 ≥ 32。
pub fn canny_low_mask32(mag_a: &[i32], low: i32) -> u32 {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::canny_low_mask32(mag_a, low) },
        x86::Isa::Avx2 => unsafe { x86::avx2::canny_low_mask32(mag_a, low) },
        x86::Isa::Scalar => {
            let mut m = 0u32;
            for (k, &v) in mag_a.iter().take(32).enumerate() {
                if v > low {
                    m |= 1 << k;
                }
            }
            m
        }
    }
}

/// Canny finalPass 单行（运行时派发）：`dst = (map==2) ? 255 : 0`。
pub fn canny_final_pass_row(pmap: &[u8], dst: &mut [u8], w: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::canny_final_pass_row(pmap, dst, w) },
        x86::Isa::Avx2 => unsafe { x86::avx2::canny_final_pass_row(pmap, dst, w) },
        x86::Isa::Scalar => edges::final_pass_row(pmap, dst, w),
    }
}

/// Canny NMS 批预写 1（运行时派发）：`pmap[0..n] = 1`（n = 32 或 64）。
pub fn canny_fill_one(pmap: &mut [u8], n: usize) {
    match (x86::isa(), n) {
        (x86::Isa::Avx512, 64) => unsafe { x86::avx512::canny_fill_one64(pmap) },
        (x86::Isa::Avx2, 32) => unsafe { x86::avx2::canny_fill_one32(pmap) },
        _ => edges::fill_one_row(pmap, n),
    }
}

/// Canny NMS 批大小（运行时派发）：AVX512 = 64，其余 32。
pub fn canny_batch() -> usize {
    match x86::isa() {
        x86::Isa::Avx512 => 64,
        _ => 32,
    }
}

/// Canny NMS 批预筛（运行时派发）：`mag_a[0..64] > low` 的 64 位掩码
///（AVX512；AVX2/标量用 32 位批）。
///
/// # Safety（调用方契约）
/// `mag_a` 长度 ≥ 64。
pub fn canny_low_mask64(mag_a: &[i32], low: i32) -> u64 {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::canny_low_mask64(mag_a, low) },
        _ => unreachable!("canny_low_mask64 only on AVX512"),
    }
}

/// Canny 滞后洪泛（安全包装：裸指针内核 + 切片接口）。
///
/// 栈元素为 map 内偏移；哨兵行/列保证邻居不越界（栈元素列 ∈ [1, w]，
/// 邻居列 ∈ [0, w+1]，行 ∈ [1, h]，邻居行 ∈ [0, h+1]）。
pub fn canny_hysteresis(map: &mut [u8], mapstep: usize, stack: &mut Vec<usize>) {
    // SAFETY: 栈元素由 NMS/块内洪泛产出，均为有效像素偏移（列 ∈ [1, w]），
    // 8 邻域由 map 哨兵行（行 0/h+1 全 1）与列哨兵（pmap[0]/pmap[w+1]=1）
    // 保护在 map 范围内
    unsafe { edges::hysteresis_unchecked(map.as_mut_ptr(), mapstep, stack) }
}

/// Canny NMS + 双阈值整行（运行时派发）。
///
/// `mag_*`/`pmap` 长度 w+2（哨兵），有效区 [1, w]；`dx`/`dy` 长度 w。
/// 栈元素为 pmap 相对偏移（列 1..w+1）。
#[allow(clippy::too_many_arguments)]
pub fn canny_nms_row(
    mag_p: &[i32],
    mag_a: &[i32],
    mag_n: &[i32],
    dx: &[i16],
    dy: &[i16],
    pmap: &mut [u8],
    w: usize,
    low: i32,
    high: i32,
    stack: &mut Vec<usize>,
) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe {
            x86::avx512::canny_nms_row(mag_p, mag_a, mag_n, dx, dy, pmap, w, low, high, stack)
        },
        x86::Isa::Avx2 => unsafe {
            x86::avx2::canny_nms_row(mag_p, mag_a, mag_n, dx, dy, pmap, w, low, high, stack)
        },
        x86::Isa::Scalar => edges::nms_full_row(mag_p, mag_a, mag_n, dx, dy, pmap, w, low, high, stack),
    }
}

/// 按位与（字节域，运行时派发：AVX512 → AVX2 → 标量）。
pub fn bitwise_and_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::bitwise_and_bytes(a, b, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::bitwise_and_bytes(a, b, dst, n) },
        x86::Isa::Scalar => scalar::bitwise_and_bytes(a, b, dst, n),
    }
}

/// 按位或（字节域，运行时派发）。
pub fn bitwise_or_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::bitwise_or_bytes(a, b, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::bitwise_or_bytes(a, b, dst, n) },
        x86::Isa::Scalar => scalar::bitwise_or_bytes(a, b, dst, n),
    }
}

/// 按位异或（字节域，运行时派发）。
pub fn bitwise_xor_bytes(a: &[u8], b: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::bitwise_xor_bytes(a, b, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::bitwise_xor_bytes(a, b, dst, n) },
        x86::Isa::Scalar => scalar::bitwise_xor_bytes(a, b, dst, n),
    }
}

/// 按位取反（字节域，运行时派发）。
pub fn bitwise_not_bytes(a: &[u8], dst: &mut [u8], n: usize) {
    match x86::isa() {
        x86::Isa::Avx512 => unsafe { x86::avx512::bitwise_not_bytes(a, dst, n) },
        x86::Isa::Avx2 => unsafe { x86::avx2::bitwise_not_bytes(a, dst, n) },
        x86::Isa::Scalar => scalar::bitwise_not_bytes(a, dst, n),
    }
}

/// 连通域第二遍重写：`labels[i] = p[labels[i]]`（原地，标量 8 展开）。
///
/// # Safety（调用方契约）
/// `labels` 的每个元素必须是 `p` 的合法下标（`0 <= v < p.len()`）。
pub fn cc_relabel_inplace(labels: &mut [i32], p: &[i32]) {
    scalar::cc_relabel_inplace(labels, p);
}

/// HResize LINEAR（u8 → i32 缓冲，扁平域 `[start, end)`，运行时派发）。
///
/// 对齐 OpenCV `HResizeLinear<uchar,int,short>`：`D[dx] = S[xofs[dx]]*a0 + S[xofs[dx]+cn]*a1`。
pub fn hresize_linear_u8(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::hresize_linear_u8(src, xofs, ialpha, dst, start, end, cn, srow_len)
        },
        x86::Isa::Scalar => {
            for dx in start..end {
                let sx = xofs[dx] as usize;
                dst[dx] = src[sx] as i32 * ialpha[dx * 2] as i32
                    + src[sx + cn] as i32 * ialpha[dx * 2 + 1] as i32;
            }
        }
    }
}

/// VResize LINEAR（i32 缓冲 → u8，运行时派发）。
///
/// 对齐 OpenCV `VResizeLinear<uchar,int,short,FixedPtCast>`：
/// `dst[x] = sat_u8(((b0*(S0>>4))>>16 + (b1*(S1>>4))>>16 + 2)>>2)`。
pub fn vresize_linear_u8(s0: &[i32], s1: &[i32], dst: &mut [u8], b0: i16, b1: i16, width: usize) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::vresize_linear_u8(s0, s1, dst, b0, b1, width)
        },
        x86::Isa::Scalar => {
            for x in 0..width {
                let v = ((b0 as i32 * (s0[x] >> 4)) >> 16) + ((b1 as i32 * (s1[x] >> 4)) >> 16) + 2;
                dst[x] = ((v >> 2).clamp(0, 255)) as u8;
            }
        }
    }
}

/// 2×2 area-fast 缩小（cn=1，运行时派发）。
///
/// 对齐 OpenCV `ResizeAreaFastVec_SIMD_8u`：`(S0[2dx]+S0[2dx+1]+S1[2dx]+S1[2dx+1]+2)>>2`。
pub fn area_fast_2x2_cn1(src: &[u8], dst: &mut [u8], sw: usize, dw: usize, dh: usize) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::area_fast_2x2_cn1(src, dst, sw, dw, dh)
        },
        x86::Isa::Scalar => {
            for dy in 0..dh {
                let s0 = &src[dy * 2 * sw..];
                let s1 = &src[(dy * 2 + 1) * sw..];
                let drow = &mut dst[dy * dw..];
                for dx in 0..dw {
                    let i = dx * 2;
                    drow[dx] = ((s0[i] as u32 + s0[i + 1] as u32 + s1[i] as u32 + s1[i + 1] as u32 + 2) >> 2) as u8;
                }
            }
        }
    }
}

/// 2×2 area-fast 缩小（cn=3，运行时派发）。
///
/// 对齐 OpenCV `ResizeAreaFastVec_SIMD_8u`：`(S0[2dx*3+k]+S0[2dx*3+3+k]+S1[2dx*3+k]+S1[2dx*3+3+k]+2)>>2`。
pub fn area_fast_2x2_cn3(src: &[u8], dst: &mut [u8], sw: usize, dw: usize, dh: usize) {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::area_fast_2x2_cn3(src, dst, sw, dw, dh)
        },
        x86::Isa::Scalar => {
            for dy in 0..dh {
                let s0 = &src[dy * 2 * sw * 3..];
                let s1 = &src[(dy * 2 + 1) * sw * 3..];
                let drow = &mut dst[dy * dw * 3..];
                for dx in 0..dw {
                    let i = dx * 6;
                    for k in 0..3 {
                        drow[dx * 3 + k] = ((s0[i + k] as u32 + s0[i + 3 + k] as u32 + s1[i + k] as u32
                            + s1[i + 3 + k] as u32
                            + 2)
                            >> 2) as u8;
                    }
                }
            }
        }
    }
}

/// HResize CUBIC（u8 → i32 缓冲，扁平域 `[start, end)`，运行时派发）。
///
/// 对齐 OpenCV `HResizeCubic<uchar,int,short>`：`D[dx] = Σ S[sx+j*C] * ialpha[dx*4+j]`。
/// `ialpha01`/`ialpha23` 为预交错布局（扁平域 [a0,a1]/[a2,a3] 对，SIMD madd 用）。
/// 帧级检测：xofs 是否为 cn=3、stride=6 等距模式（scale=2 下采样）。
/// VBMI HResize 快路径前置条件；每帧调用一次（xofs 帧内不变），
/// 避免逐行重复检测。
pub fn hresize_cubic_vbmi_pattern(xofs: &[i32], cn: usize, end: usize) -> bool {
    if cn != 3 || end < 16 {
        return false;
    }
    let npix = end / 3;
    npix >= 2
        && xofs[3] - xofs[0] == 6
        && (1..npix).all(|p| xofs[p * 3] - xofs[(p - 1) * 3] == 6)
        && (0..npix).all(|p| xofs[p * 3 + 2] - xofs[p * 3] == 2)
}

/// HResize CUBIC VBMI 快路径（无检测版，调用方已用
/// [`hresize_cubic_vbmi_pattern`] 确认模式）。
///
/// # Safety（内部已校验）
/// - xofs 满足 stride=6 等距模式（调用方保证）；CPU 支持探测在内部完成
pub fn hresize_cubic_vbmi(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    ialpha01: &[i16],
    ialpha23: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    static VBMI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *VBMI.get_or_init(|| std::arch::is_x86_feature_detected!("avx512vbmi")) {
        unsafe {
            x86::warp::hresize_cubic_vbmi_regular(
                src, xofs, ialpha, ialpha01, ialpha23, dst, start, end, cn, srow_len,
            )
        }
    } else {
        // 无 VBMI 时回退 AVX2/标量路径
        hresize_cubic_u8(src, xofs, ialpha, ialpha01, ialpha23, dst, start, end, cn, srow_len);
    }
}

pub fn hresize_cubic_u8(
    src: &[u8],
    xofs: &[i32],
    ialpha: &[i16],
    ialpha01: &[i16],
    ialpha23: &[i16],
    dst: &mut [i32],
    start: usize,
    end: usize,
    cn: usize,
    srow_len: usize,
) {
    // AVX512VBMI 快路径（cn=3、stride=6 等距模式；探测一次缓存）
    static VBMI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *VBMI.get_or_init(|| std::arch::is_x86_feature_detected!("avx512vbmi")) {
        let handled = unsafe {
            x86::warp::hresize_cubic_vbmi_u8(
                src, xofs, ialpha, ialpha01, ialpha23, dst, start, end, cn, srow_len,
            )
        };
        if handled {
            return;
        }
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::hresize_cubic_u8(
                src, xofs, ialpha, ialpha01, ialpha23, dst, start, end, cn, srow_len,
            )
        },
        x86::Isa::Scalar => {
            for dx in start..end {
                let sx = xofs[dx] as usize - cn;
                let mut v: i32 = 0;
                for j in 0..4 {
                    let idx = sx + j * cn;
                    let sval = if idx < srow_len { src[idx] as i32 } else { 0 };
                    v += sval * ialpha[dx * 4 + j] as i32;
                }
                dst[dx] = v;
            }
        }
    }
}

/// VResize CUBIC（i32 缓冲 → u8，运行时派发）。
///
/// 对齐 OpenCV `VResizeCubicVec_32s8u`：f32 分离乘加链 + v_round + 饱和 pack。
pub fn vresize_cubic_u8(
    s0: &[i32],
    s1: &[i32],
    s2: &[i32],
    s3: &[i32],
    dst: &mut [u8],
    b: [i32; 4],
    scale: f32,
    width: usize,
) {
    // AVX512 版（与 VBMI HResize 配对，避免 AVX512/AVX2 混用切换 penalty）
    static AVX512: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *AVX512.get_or_init(|| {
        std::arch::is_x86_feature_detected!("avx512f") && std::arch::is_x86_feature_detected!("avx512bw")
    }) {
        return unsafe { x86::warp::vresize_cubic_avx512_u8(s0, s1, s2, s3, dst, b, scale, width) };
    }
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::vresize_cubic_u8(s0, s1, s2, s3, dst, b, scale, width)
        },
        x86::Isa::Scalar => {
            let simd_end = if width >= 8 { (width - 8) / 8 * 8 + 8 } else { 0 };
            for dx in 0..simd_end {
                let acc = s0[dx] as f32 * (b[0] as f32 * scale)
                    + s1[dx] as f32 * (b[1] as f32 * scale)
                    + s2[dx] as f32 * (b[2] as f32 * scale)
                    + s3[dx] as f32 * (b[3] as f32 * scale);
                dst[dx] = acc.round_ties_even().clamp(0.0, 255.0) as u8;
            }
            for dx in simd_end..width {
                let acc = s0[dx] * b[0] + s1[dx] * b[1] + s2[dx] * b[2] + s3[dx] * b[3];
                dst[dx] = (((acc + (1 << 21)) >> 22).clamp(0, 255)) as u8;
            }
        }
    }
}

/// Warp LINEAR 快路径（AVX2，每批 8 像素）。`replicate=true` 时 clamp 坐标处理
/// Replicate 边界（覆盖全部像素）；否则仅处理 all-inlier 批，不满足返回 `false`。
///
/// 返回 `true` 表示已处理 `[x, x+8)` 的 8 像素（写入 `dst`），`false` 表示需回退标量。
pub fn warp_linear_fast_u8(
    src: &[u8],
    sw: usize,
    sh: usize,
    step: usize,
    dst: &mut [u8],
    m: &[f32],
    pm: Option<&[f32; 9]>,
    y: usize,
    x: usize,
    cn: usize,
    replicate: bool,
) -> bool {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::warp_linear_fast_u8(src, sw, sh, step, dst, m, pm, y, x, cn, replicate)
        },
        x86::Isa::Scalar => false,
    }
}

/// Warp CUBIC 快路径（AVX2，每批 8 像素，affine/perspective + replicate 边界）。
/// `replicate=true` 时 clamp 坐标处理 Replicate 边界（覆盖全部像素）；否则仅处理
/// all-inlier 批，不满足返回 `false`。
///
/// 返回 `true` 表示已处理 `[x, x+8)` 的 8 像素（写入 `dst`），`false` 表示需回退标量。
pub fn warp_cubic_fast_u8(
    src: &[u8],
    sw: usize,
    sh: usize,
    step: usize,
    dst: &mut [u8],
    m: &[f32],
    pm: Option<&[f32; 9]>,
    y: usize,
    x: usize,
    cn: usize,
    replicate: bool,
) -> bool {
    match x86::isa() {
        x86::Isa::Avx512 | x86::Isa::Avx2 => unsafe {
            x86::warp::warp_cubic_fast_u8(src, sw, sh, step, dst, m, pm, y, x, cn, replicate)
        },
        x86::Isa::Scalar => false,
    }
}
