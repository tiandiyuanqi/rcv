//! 形态学手写内核（对齐 OpenCV `morph.simd.hpp`）。
//!
//! 可分离两段（矩形核）：
//! - 行阶段 `MorphRowVec`：u8 → u8 水平滑动窗 min/max
//! - 列阶段 `MorphColumnVec`：u8 → u8 垂直 min/max
//!
//! 中间缓冲布局：`w × (h + kh - 1)` 的 u8（上下含边框行），
//! 列阶段顺序访问 kh 行，无需索引映射。内核不做边界（调用方预填充）。

/// 行阶段：u8 水平滑动窗 min/max（标量，通道感知，正确性基准）。
///
/// `src` 为带左右边框的行（长度 ≥ `(w + kw - 1)*cn`），`dst` 长度 ≥ `w*cn`。
/// `op=true` 为 min（erode），`op=false` 为 max（dilate）。
pub fn row_morph_u8_cn(src: &[u8], dst: &mut [u8], w: usize, kw: usize, cn: usize, erode: bool) {
    let total = w * cn;
    for i in 0..total {
        let mut acc = src[i];
        for k in 1..kw {
            let v = src[i + k * cn];
            acc = if erode { acc.min(v) } else { acc.max(v) };
        }
        dst[i] = acc;
    }
}

/// 行阶段首尾特判：`kw-1` 像素逐点按 border 采样 min/max（供各 ISA 版复用）。
#[allow(clippy::too_many_arguments)]
pub fn row_morph_edges(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    kw: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
    ax: usize,
) {
    let sample = |x: i64, c: usize| -> u8 {
        match border.interpolate(x as isize, w) {
            Some(sx) => src[sx * cn + c],
            None => match border {
                crate::core::border::BorderPolicy::Constant(v) => v,
                _ => unreachable!("non-constant border always interpolates"),
            },
        }
    };
    let ax = ax as i64;
    for x in 0..ax.min(w as i64) as usize {
        for c in 0..cn {
            let mut acc = if x as i64 - ax >= 0 {
                src[(x as i64 - ax) as usize * cn + c]
            } else {
                sample(x as i64 - ax, c)
            };
            for k in 1..kw {
                let sx = x as i64 + k as i64 - ax;
                let v = if sx >= 0 && (sx as usize) < w {
                    src[sx as usize * cn + c]
                } else {
                    sample(sx, c)
                };
                acc = if erode { acc.min(v) } else { acc.max(v) };
            }
            dst[x * cn + c] = acc;
        }
    }
    // 右边界：x 从 w-(kw-1-ax) 开始（窗口右端越界）
    let right_start = w.saturating_sub(kw - 1 - ax as usize);
    for x in right_start..w {
        for c in 0..cn {
            let mut acc = if x as i64 - ax >= 0 {
                src[(x as i64 - ax) as usize * cn + c]
            } else {
                sample(x as i64 - ax, c)
            };
            for k in 1..kw {
                let sx = x as i64 + k as i64 - ax;
                let v = if sx >= 0 && (sx as usize) < w {
                    src[sx as usize * cn + c]
                } else {
                    sample(sx, c)
                };
                acc = if erode { acc.min(v) } else { acc.max(v) };
            }
            dst[x * cn + c] = acc;
        }
    }
}

/// 行阶段（无边框源）：u8 水平滑动窗 min/max，内核内部处理首尾边框。
///
/// `src` 为原始行（长度 ≥ `w*cn`，无左右边框），`dst` 长度 ≥ `w*cn`。
/// 首尾 `kw-1` 像素的越界采样由 `border` 策略给出（Clamp/Reflect/Constant）。
/// 省去调用方构造 padded 行的 memcpy 中转。
pub fn row_morph_u8_direct_cn(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    kw: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let ax = kw / 2;
    // 主体：像素 [ax, w-kw+ax] 无越界（窗口 [x-ax, x-ax+kw)）
    let lo = ax;
    let hi = w.saturating_sub(kw - 1 - ax);
    for x in lo..hi {
        for c in 0..cn {
            let mut acc = src[(x - ax) * cn + c];
            for k in 1..kw {
                let v = src[(x - ax + k) * cn + c];
                acc = if erode { acc.min(v) } else { acc.max(v) };
            }
            dst[x * cn + c] = acc;
        }
    }
    // 首尾特判：逐像素按 border 采样
    row_morph_edges(src, dst, w, kw, cn, erode, border, ax);
}

/// 行阶段（无边框源）kw=3 特化：u8 水平滑动窗 min/max。
///
/// 与 [`row_morph_u8_direct_cn`] 等价，但 kw=3 编译期展开（无运行时循环）。
pub fn row_morph_u8_direct_k3_cn(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    cn: usize,
    erode: bool,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let ax = 1;
    let lo = 1;
    let hi = w.saturating_sub(1);
    for x in lo..hi {
        for c in 0..cn {
            let mut acc = src[(x - 1) * cn + c];
            let v1 = src[x * cn + c];
            let v2 = src[(x + 1) * cn + c];
            acc = if erode {
                acc.min(v1).min(v2)
            } else {
                acc.max(v1).max(v2)
            };
            dst[x * cn + c] = acc;
        }
    }
    row_morph_edges(src, dst, w, 3, cn, erode, border, ax);
}

/// 行阶段（无边框源）kw=3 融合：一次扫描同时产出 min（erode）与 max（dilate）。
///
/// 供 morph_gradient 融合路径使用（src 只读一遍）。cn=1。
pub fn row_morph_minmax_k3_cn(
    src: &[u8],
    dst_min: &mut [u8],
    dst_max: &mut [u8],
    w: usize,
    border: crate::core::border::BorderPolicy<u8>,
) {
    let lo = 1;
    let hi = w.saturating_sub(1);
    for x in lo..hi {
        let v0 = src[x - 1];
        let v1 = src[x];
        let v2 = src[x + 1];
        dst_min[x] = v0.min(v1).min(v2);
        dst_max[x] = v0.max(v1).max(v2);
    }
    row_morph_edges(src, dst_min, w, 3, 1, true, border, 1);
    row_morph_edges(src, dst_max, w, 3, 1, false, border, 1);
}

/// 列阶段：垂直 min/max（标量，正确性基准）。
///
/// `rows` 为 kh 行 u8（每行长度 ≥ w，stride 相同），`dst` 长度 ≥ w。
pub fn col_morph_u8_scalar(rows: &[&[u8]], dst: &mut [u8], w: usize, erode: bool) {
    for i in 0..w {
        let mut acc = rows[0][i];
        for row in rows.iter().skip(1) {
            let v = row[i];
            acc = if erode { acc.min(v) } else { acc.max(v) };
        }
        dst[i] = acc;
    }
}

/// 形态学梯度：`dst = d - e`（u8 饱和减，标量基准）。
pub fn gradient_u8_scalar(d: &[u8], e: &[u8], dst: &mut [u8], n: usize) {
    for i in 0..n {
        dst[i] = d[i].saturating_sub(e[i]);
    }
}

/// 边框索引：`p` 越界时按 `border` 策略映射；Constant 返回 `usize::MAX`。
fn border_index_u8(p: i64, len: usize, border: &crate::core::border::BorderPolicy<u8>) -> usize {
    match border.interpolate(p as isize, len) {
        Some(i) => i,
        None => usize::MAX,
    }
}

/// 填充带左右边框的行（u8 版，`padded` 长度 ≥ `(w + kw - 1)*cn`）。
fn fill_padded_row_u8(
    padded: &mut [u8],
    src_row: &[u8],
    w: usize,
    dx1: usize,
    dx2: usize,
    cn: usize,
    border: &crate::core::border::BorderPolicy<u8>,
) {
    let pw = w + dx1 + dx2;
    for x in 0..pw {
        let sx = x as i64 - dx1 as i64;
        for c in 0..cn {
            padded[x * cn + c] = match border.interpolate(sx as isize, w) {
                Some(sx) => src_row[sx * cn + c],
                None => match border {
                    crate::core::border::BorderPolicy::Constant(v) => *v,
                    _ => unreachable!("non-constant border always interpolates"),
                },
            };
        }
    }
}

/// 流式形态学（矩形核，u8）：kh 行环形缓冲（L1 常驻），行滤波结果立即被列滤波消费。
///
/// 对齐 OpenCV FilterEngine 数据流：避免物化整个 mid（1MB）导致列阶段从 L3 读冷数据。
/// 行滤波按 kw 派发（kw=3 特化 / kw=5 直接边框 / 其余 padded 中转）。
///
/// 处理输出行范围 `[y0, y0+h_out)`（全图传 y0=0, h_out=h）。
/// `src` 长度 ≥ `w*h*cn`；`dst` 为该输出行范围的连续切片（长度 ≥ `w*h_out*cn`）。
#[allow(clippy::too_many_arguments)]
pub fn morph_streaming_u8(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    cn: usize,
    kw: usize,
    kh: usize,
    dx1: usize,
    dx2: usize,
    dy1: usize,
    y0: usize,
    h_out: usize,
    border: &crate::core::border::BorderPolicy<u8>,
    erode: bool,
) {
    let row_w = w * cn;
    let pw = w + kw - 1;
    let mut ring: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut padded: Vec<u8> = crate::simd_kernels::alloc_uninit(pw * cn);
    // 行滤波：mid 行 r → ring[r % kh]
    // SAFETY: ring/padded 为本函数独有 Vec，指针有效；r < h+kh-1 保证环内偏移 < row_w*kh
    let row_filter = |r: usize, ring: *mut u8, padded: *mut u8| unsafe {
        let dst_row = std::slice::from_raw_parts_mut(ring.add((r % kh) * row_w), row_w);
        let sy = border_index_u8(r as i64 - dy1 as i64, h, border);
        match sy {
            usize::MAX => {
                if let crate::core::border::BorderPolicy::Constant(v) = border {
                    dst_row.fill(*v);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                if kw == 3 {
                    crate::simd_kernels::morph_row_u8_direct_k3(src_row, dst_row, w, cn, erode, *border);
                } else if kw == 5 {
                    crate::simd_kernels::morph_row_u8_direct(src_row, dst_row, w, kw, cn, erode, *border);
                } else {
                    let padded = std::slice::from_raw_parts_mut(padded, pw * cn);
                    fill_padded_row_u8(padded, src_row, w, dx1, dx2, cn, border);
                    crate::simd_kernels::morph_row_u8(padded, dst_row, w, kw, cn, erode);
                }
            }
        }
    };
    // 预填 kh-1 行（mid 行 y0..y0+kh-1）
    for r in y0..y0 + kh - 1 {
        row_filter(r, ring.as_mut_ptr(), padded.as_mut_ptr());
    }
    // 主循环：mid 行 r = y0+kh-1 起，每行滤波后立即产出输出行 y = r - (kh-1)。
    // SAFETY: 裸指针绕开 refs 与 row_filter 的借用冲突；
    // ring/padded/dst 均为本函数独有缓冲，偏移由 y < y0+h_out、k < kh 界定。
    // dst 为输出行范围 [y0, y0+h_out) 的连续切片，块内偏移 = (y - y0) * row_w。
    unsafe {
        let rp = ring.as_mut_ptr();
        let dp = dst.as_mut_ptr();
        for y in y0..y0 + h_out {
            let r = y + kh - 1;
            row_filter(r, rp, padded.as_mut_ptr());
            let mut refs: [&[u8]; 16] = [&[]; 16];
            for (k, refk) in refs.iter_mut().enumerate().take(kh) {
                let rk = (y + k) % kh;
                *refk = std::slice::from_raw_parts(rp.add(rk * row_w), row_w);
            }
            let dst_row = std::slice::from_raw_parts_mut(dp.add((y - y0) * row_w), row_w);
            crate::simd_kernels::morph_col_u8(&refs[..kh], dst_row, row_w, erode);
        }
    }
}

/// 流式形态学梯度（3x3 矩形核，u8，cn=1）：
/// 一次行滤波同时算 min/max（两个环形缓冲），一次列阶段，立即饱和减。
///
/// `src` 长度 ≥ `w*h`，`dst` 长度 ≥ `w*h`。
pub fn morph_gradient_streaming_u8(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    border: &crate::core::border::BorderPolicy<u8>,
) {
    let kh = 3;
    let row_w = w;
    let mut ring_min: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut ring_max: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut col_min: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w);
    // 行滤波：mid 行 r → min/max 环
    // SAFETY: ring 为本函数独有 Vec，指针有效；r < h+kh-1 保证环内偏移 < row_w*kh
    let row_filter = |r: usize, rmin: *mut u8, rmax: *mut u8| unsafe {
        let dmin = std::slice::from_raw_parts_mut(rmin.add((r % kh) * row_w), row_w);
        let dmax = std::slice::from_raw_parts_mut(rmax.add((r % kh) * row_w), row_w);
        let sy = border_index_u8(r as i64 - 1, h, border);
        match sy {
            usize::MAX => {
                if let crate::core::border::BorderPolicy::Constant(v) = border {
                    dmin.fill(*v);
                    dmax.fill(*v);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                crate::simd_kernels::morph_row_minmax_k3(src_row, dmin, dmax, w, *border);
            }
        }
    };
    // 预填 kh-1 行
    for r in 0..kh - 1 {
        row_filter(r, ring_min.as_mut_ptr(), ring_max.as_mut_ptr());
    }
    // 主循环：每行滤波后立即列阶段 + 饱和减。
    // SAFETY: 裸指针绕开 refs 与 row_filter 的借用冲突；
    // ring_min/ring_max/col_min/dst 均为本函数独有缓冲，偏移由 y < h、k < kh 界定。
    unsafe {
        let rmin = ring_min.as_mut_ptr();
        let rmax = ring_max.as_mut_ptr();
        let cmin = col_min.as_mut_ptr();
        let dp = dst.as_mut_ptr();
        for y in 0..h {
            let r = y + kh - 1;
            row_filter(r, rmin, rmax);
            let mut refs_min: [&[u8]; 16] = [&[]; 16];
            let mut refs_max: [&[u8]; 16] = [&[]; 16];
            for k in 0..kh {
                let rk = (y + k) % kh;
                refs_min[k] = std::slice::from_raw_parts(rmin.add(rk * row_w), row_w);
                refs_max[k] = std::slice::from_raw_parts(rmax.add(rk * row_w), row_w);
            }
            let col_min_slice = std::slice::from_raw_parts_mut(cmin, row_w);
            let dst_row = std::slice::from_raw_parts_mut(dp.add(y * row_w), row_w);
            crate::simd_kernels::morph_col_u8(&refs_min[..kh], col_min_slice, row_w, true);
            crate::simd_kernels::morph_col_u8(&refs_max[..kh], dst_row, row_w, false);
            // dst = max - min（饱和减）；dst_row 同时是输入与输出，用裸指针绕开别名借用
            crate::simd_kernels::gradient_u8(
                std::slice::from_raw_parts(dp.add(y * row_w), row_w),
                std::slice::from_raw_parts(cmin, row_w),
                dst_row,
                row_w,
            );
        }
    }
}

/// 流式形态学 open/close（3x3 矩形核，u8，cn=1）：三阶段流水线融合。
///
/// open = dilate(erode(x))，close = erode(dilate(x))。
/// 对齐 OpenCV `morphologyEx` 语义：两级各自独立做边框处理——
/// 第一级边框作用于 src，第二级边框作用于第一级输出 E（中间图）：
/// - mid1(v) = h-op1(src 行 v)，v 越界按 border 映射（Constant 填常量行）；
/// - E(e) = col-op1 over mid1 行 [e-1, e+1]，e ∈ [0, h)；
/// - m2(u) = h-op2(E 行 u)，u 越界按 border 映射 E 行号
///   （min/max 幂等，故 m2(越界) = m2(映射行)；Constant 用常量行）；
/// - O(y) = col-op2 over m2 行 [y-1, y+1]。
///
/// 中间结果不物化，两级环形缓冲均 L1 常驻。
///
/// `src` 长度 ≥ `w*h`，`dst` 长度 ≥ `w*h`。
pub fn morph_open_close_streaming_u8(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    border: &crate::core::border::BorderPolicy<u8>,
    op1_erode: bool,
) {
    if w == 0 || h == 0 {
        return;
    }
    let kh = 3;
    let row_w = w;
    let mut ring1: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut ring2: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w * kh);
    let mut mid_row: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w);
    let mut const_row: Vec<u8> = crate::simd_kernels::alloc_uninit(row_w);
    if let crate::core::border::BorderPolicy::Constant(v) = border {
        const_row.fill(*v);
    }
    // 第一级行滤波：mid1 行 v（v ∈ [-1, h]）→ ring1[v.rem_euclid(kh)]
    // SAFETY: ring1 为本函数独有 Vec，指针有效；rem_euclid 保证环内偏移 < row_w*kh
    let row_filter1 = |v: i64, ring1: *mut u8| unsafe {
        let slot = v.rem_euclid(kh as i64) as usize;
        let dst_row = std::slice::from_raw_parts_mut(ring1.add(slot * row_w), row_w);
        match border_index_u8(v, h, border) {
            usize::MAX => {
                if let crate::core::border::BorderPolicy::Constant(cv) = border {
                    dst_row.fill(*cv);
                }
            }
            sy => {
                let src_row = &src[sy * row_w..(sy + 1) * row_w];
                crate::simd_kernels::morph_row_u8_direct_k3(
                    src_row, dst_row, w, 1, op1_erode, *border,
                );
            }
        }
    };
    // 预填 mid1 行 {-1, 0}（E(0) 窗口 [e-1, e+1] = [-1, 1] 的前两行）
    for v in [-1i64, 0] {
        row_filter1(v, ring1.as_mut_ptr());
    }
    // 产出输出行 y：col-op2 over m2 行 [y-1, y+1]（越界行按 border 映射 E 行号）
    // SAFETY: 裸指针绕开环形缓冲借用冲突；ring2/const_row/dst 为本函数独有缓冲，
    // 偏移由 y < h、k < kh、映射行 ∈ [0, h) 界定。
    let emit_row = |y: usize, r2p: *const u8, cp: *const u8, dp: *mut u8| unsafe {
        let mut refs2: [&[u8]; 16] = [&[]; 16];
        for (k, refk) in refs2.iter_mut().enumerate().take(kh) {
            let r = y as i64 - 1 + k as i64;
            let ri = if r >= 0 && (r as usize) < h {
                r as usize
            } else {
                border_index_u8(r, h, border)
            };
            *refk = if ri == usize::MAX {
                std::slice::from_raw_parts(cp, row_w)
            } else {
                std::slice::from_raw_parts(r2p.add((ri % kh) * row_w), row_w)
            };
        }
        let dst_row = std::slice::from_raw_parts_mut(dp.add(y * row_w), row_w);
        crate::simd_kernels::morph_col_u8(&refs2[..kh], dst_row, row_w, !op1_erode);
    };
    // 主循环：每轮先滤波 mid1 行 e+1，E(e) 列归约后立即做第二级行滤波 m2(e)；
    // m2 行 [e-2, e] 攒齐（e ≥ 1）即产出输出行 y = e-1。
    // SAFETY: ring1 行 e-1, e, e+1 与 ring2 行 e-2, e-1, e 模 kh 互异；
    // ring1/ring2/mid_row/const_row/dst 均为本函数独有缓冲。
    unsafe {
        let r1p = ring1.as_mut_ptr();
        let r2p_mut = ring2.as_mut_ptr();
        let r2p = r2p_mut as *const u8;
        let mp = mid_row.as_mut_ptr();
        let cp = const_row.as_ptr();
        let dp = dst.as_mut_ptr();
        for e in 0..h {
            row_filter1(e as i64 + 1, r1p);
            let mut refs1: [&[u8]; 16] = [&[]; 16];
            for (k, refk) in refs1.iter_mut().enumerate().take(kh) {
                let rk = (e as i64 - 1 + k as i64).rem_euclid(kh as i64) as usize;
                *refk = std::slice::from_raw_parts(r1p.add(rk * row_w), row_w);
            }
            let mid_slice = std::slice::from_raw_parts_mut(mp, row_w);
            crate::simd_kernels::morph_col_u8(&refs1[..kh], mid_slice, row_w, op1_erode);
            // 第二级：E(e) 水平 op2 → ring2[e % kh]
            let dst_row2 =
                std::slice::from_raw_parts_mut(r2p_mut.add((e % kh) * row_w), row_w);
            crate::simd_kernels::morph_row_u8_direct_k3(
                mid_slice, dst_row2, w, 1, !op1_erode, *border,
            );
            if e >= 1 {
                emit_row(e - 1, r2p, cp, dp);
            }
        }
        // 末行 y = h-1：窗口 m2 行 [h-2, h]，行 h 越界按 border 映射
        emit_row(h - 1, r2p, cp, dp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_morph_basic() {
        // kw=3, padded src=[1,2,3,4,5,6,7]（w=5，左右各 1 边框）
        let src = [1u8, 2, 3, 4, 5, 6, 7];
        let mut dst = [0u8; 5];
        row_morph_u8_cn(&src, &mut dst, 5, 3, 1, true);
        // 窗口 [1,2,3]=1, [2,3,4]=2, [3,4,5]=3, [4,5,6]=4, [5,6,7]=5
        assert_eq!(dst, [1, 2, 3, 4, 5]);
        let mut dst2 = [0u8; 5];
        row_morph_u8_cn(&src, &mut dst2, 5, 3, 1, false);
        // max: [1,2,3]=3, [2,3,4]=4, [3,4,5]=5, [4,5,6]=6, [5,6,7]=7
        assert_eq!(dst2, [3, 4, 5, 6, 7]);
    }

    #[test]
    fn col_morph_basic() {
        let rows: Vec<Vec<u8>> = vec![vec![1, 5], vec![3, 2], vec![2, 4]];
        let rrefs: Vec<&[u8]> = rows.iter().map(|v| v.as_slice()).collect();
        let mut dst = [0u8; 2];
        col_morph_u8_scalar(&rrefs, &mut dst, 2, true);
        assert_eq!(dst, [1, 2]); // min over rows
        let mut dst2 = [0u8; 2];
        col_morph_u8_scalar(&rrefs, &mut dst2, 2, false);
        assert_eq!(dst2, [3, 5]); // max over rows
    }
}
#[cfg(target_arch = "x86_64")]
#[cfg(test)]
mod isa_cross_tests {
    use super::*;

    fn has_avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }
    fn has_avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl")
    }

    /// 三路径交叉验证：行 min/max。
    #[test]
    fn morph_row_cross_isa() {
        for cn in [1usize, 3] {
            for w in [1usize, 5, 16, 17, 32, 33, 64, 100] {
                for kw in [1usize, 3, 5, 7, 9] {
                    for erode in [true, false] {
                        let padded = w + kw - 1;
                        let src: Vec<u8> = (0..padded * cn).map(|i| (i * 37 % 256) as u8).collect();
                        let mut dst_scalar = vec![0u8; w * cn];
                        row_morph_u8_cn(&src, &mut dst_scalar, w, kw, cn, erode);
                        if has_avx2() {
                            let mut dst = vec![0u8; w * cn];
                            // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx2::morph_row_u8(
                                    &src, &mut dst, w, kw, cn, erode,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx2 w={w} cn={cn} kw={kw} erode={erode}");
                        }
                        if has_avx512() {
                            let mut dst = vec![0u8; w * cn];
                            // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                            unsafe {
                                crate::simd_kernels::x86::avx512::morph_row_u8(
                                    &src, &mut dst, w, kw, cn, erode,
                                )
                            };
                            assert_eq!(dst_scalar, dst, "avx512 w={w} cn={cn} kw={kw} erode={erode}");
                        }
                    }
                }
            }
        }
    }

    /// 三路径交叉验证：列 min/max。
    #[test]
    fn morph_col_cross_isa() {
        for w in [1usize, 16, 17, 32, 33, 64, 100] {
            for kh in [1usize, 3, 5, 7] {
                for erode in [true, false] {
                    let rows: Vec<Vec<u8>> = (0..kh)
                        .map(|r| (0..w).map(|i| ((i * 131 + r * 17) % 256) as u8).collect())
                        .collect();
                    let rrefs: Vec<&[u8]> = rows.iter().map(|v| v.as_slice()).collect();
                    let mut dst_scalar = vec![0u8; w];
                    col_morph_u8_scalar(&rrefs, &mut dst_scalar, w, erode);
                    if has_avx2() {
                        let mut dst = vec![0u8; w];
                        // SAFETY: 测试环境已探测 AVX2；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx2::morph_col_u8(
                                &rrefs, &mut dst, w, erode,
                            )
                        };
                        assert_eq!(dst_scalar, dst, "avx2 w={w} kh={kh} erode={erode}");
                    }
                    if has_avx512() {
                        let mut dst = vec![0u8; w];
                        // SAFETY: 测试环境已探测 AVX512F/BW/VL；切片长度与内核契约一致
                        unsafe {
                            crate::simd_kernels::x86::avx512::morph_col_u8(
                                &rrefs, &mut dst, w, erode,
                            )
                        };
                        assert_eq!(dst_scalar, dst, "avx512 w={w} kh={kh} erode={erode}");
                    }
                }
            }
        }
    }
}
