//! 多边形扫描线填充（AET 活动边表）内核。
//!
//! 对齐 OpenCV `FillEdgeCollection`（drawing.cpp:1347）。热循环用裸指针
//! 访问边表（对齐 OpenCV 的 `PolyEdge*` 链表），消除下标边界检查。
//! 因 `unsafe` 须限定在 `simd_kernels/` 内（见 scripts/check_unsafe.sh），
//! 故将 AET 扫描线循环放在本模块，`drawing.rs` 仅做安全的前置处理
//! （排序、哨兵、边界裁剪）。

use crate::imgproc::drawing::{PolyEdge, XY_ONE, XY_SHIFT};

/// 水平填充 [xl, xr]（对齐 OpenCV `ICV_HLINE`，drawing.cpp:1063）。
///
/// 调用方保证 0 ≤ xl ≤ xr < width、0 ≤ y < height，故
/// `off = y*step + xl*pix_size` 与 `end = y*step + (xr+1)*pix_size` 均在
/// `data` 界内。用 `get_unchecked_mut` 消除切片边界检查。
pub fn hline(
    data: &mut [u8],
    step: usize,
    pix_size: usize,
    color: &[u8; 4],
    y: i32,
    xl: i32,
    xr: i32,
) {
    let row = y as usize * step;
    let start = row + xl as usize * pix_size;
    let end = row + (xr as usize + 1) * pix_size;
    // SAFETY: 调用方保证 0 ≤ xl ≤ xr < width、0 ≤ y < height，
    // data 长度 = height*step，故 [start, end) 在界内
    let seg = unsafe { data.get_unchecked_mut(start..end) };
    if pix_size == 1 {
        seg.fill(color[0]);
    } else {
        let mut i = 0;
        while i < seg.len() {
            seg[i..i + pix_size].copy_from_slice(&color[..pix_size]);
            i += pix_size;
        }
    }
}

/// 写单像素（对齐 OpenCV `ICV_PUT_POINT`）。
///
/// 调用方保证 0 ≤ x < width、0 ≤ y < height，故
/// `off = y*step + x*pix_size` 在 `data` 界内。用 `get_unchecked_mut`
/// 消除切片边界检查。
pub fn put_point(
    data: &mut [u8],
    step: usize,
    pix_size: usize,
    color: &[u8; 4],
    x: i32,
    y: i32,
) {
    let off = y as usize * step + x as usize * pix_size;
    // SAFETY: 调用方保证 0 ≤ x < width、0 ≤ y < height，
    // data 长度 = height*step，故 [off, off+pix_size) 在界内
    let p = unsafe { data.get_unchecked_mut(off..off + pix_size) };
    p.copy_from_slice(&color[..pix_size]);
}

/// AET 扫描线填充（对齐 OpenCV `FillEdgeCollection` 的扫描线循环）。
///
/// `edges` 为已排序的边表（含哨兵，长度 = total+1），`total` 为真实边数。
/// 每扫描线对活动边表做插入/删除/冒泡排序，并对每对边调用 `hline` 填充。
///
/// 内部用 `get_unchecked` 消除下标边界检查（对齐 OpenCV 的 `PolyEdge*`
/// 链表）。`edges` 长度 ≥ total+1，链表 `next` 指针维护正确，所有访问
/// 下标 < total+1，故 `get_unchecked` 安全。
pub fn fill_edges_aet(
    edges: &mut [PolyEdge],
    total: usize,
    size_w: i32,
    size_h: i32,
    mut hline: impl FnMut(i32, i32, i32),
) {
    // SAFETY: edges 长度 ≥ total+1（含哨兵），所有下标 < total+1
    macro_rules! e {
        ($i:expr) => {
            unsafe { edges.get_unchecked($i) }
        };
    }
    macro_rules! em {
        ($i:expr) => {
            unsafe { edges.get_unchecked_mut($i) }
        };
    }
    let tmp = total as i32;
    let mut i = 0usize;
    let mut e_cur = 0usize;
    let delta = XY_ONE - 1;
    let mut y_max = i32::MIN;
    let mut y_min = i32::MAX;
    let mut x_max = i64::MIN;
    let mut x_min = i64::MAX;

    for k in 0..total {
        let e1 = e!(k);
        debug_assert!(e1.y0 < e1.y1);
        let x1 = e1.x + (e1.y1 as i64 - e1.y0 as i64) * e1.dx;
        y_min = y_min.min(e1.y0);
        y_max = y_max.max(e1.y1);
        x_min = x_min.min(e1.x);
        x_max = x_max.max(e1.x);
        x_min = x_min.min(x1);
        x_max = x_max.max(x1);
    }

    if y_max < 0 || y_min >= size_h || x_max < 0 || x_min >= (size_w as i64) << XY_SHIFT {
        return;
    }

    y_max = y_max.min(size_h);

    let mut y = e!(0).y0;
    while y < y_max {
        let mut draw = 0;
        let clipline = y < 0;
        let mut prelast = tmp;
        let mut last = e!(tmp as usize).next;
        let mut keep_prelast;

        loop {
            let last_valid = last >= 0;
            let e_y0 = e!(e_cur).y0;
            if !(last_valid || e_y0 == y) {
                break;
            }
            if last_valid && e!(last as usize).y1 == y {
                // 排除到达下端点的边
                let ln = e!(last as usize).next;
                em!(prelast as usize).next = ln;
                last = ln;
                continue;
            }
            keep_prelast = prelast;
            if last_valid && (e!(e_cur).y0 > y || e!(last as usize).x < e!(e_cur).x) {
                // 活动链表前进
                prelast = last;
                last = e!(last as usize).next;
            } else if i < total {
                // 插入新边到活动链表
                em!(prelast as usize).next = e_cur as i32;
                em!(e_cur).next = last;
                prelast = e_cur as i32;
                e_cur += 1;
                i += 1;
            } else {
                break;
            }

            if draw != 0 {
                if !clipline {
                    let (x1, x2);
                    let kx = e!(keep_prelast as usize).x;
                    let px = e!(prelast as usize).x;
                    if kx > px {
                        x1 = ((px + delta) >> XY_SHIFT) as i32;
                        x2 = (kx >> XY_SHIFT) as i32;
                    } else {
                        x1 = ((kx + delta) >> XY_SHIFT) as i32;
                        x2 = (px >> XY_SHIFT) as i32;
                    }
                    if x1 < size_w && x2 >= 0 {
                        let x1 = x1.max(0);
                        let x2 = x2.min(size_w - 1);
                        hline(y, x1, x2);
                    }
                }
                em!(keep_prelast as usize).x += em!(keep_prelast as usize).dx;
                em!(prelast as usize).x += em!(prelast as usize).dx;
            }
            draw ^= 1;
        }

        // 冒泡排序活动链表（按 x）
        let mut keep_prelast_loop: i32 = -1;
        loop {
            let mut prelast = tmp;
            let mut last = e!(tmp as usize).next;
            let mut last_exchange: i32 = -1;

            while last != keep_prelast_loop && e!(last as usize).next != -1 {
                let te = e!(last as usize).next;
                if e!(last as usize).x > e!(te as usize).x {
                    // swap
                    em!(prelast as usize).next = te;
                    em!(last as usize).next = e!(te as usize).next;
                    em!(te as usize).next = last;
                    prelast = te;
                    last_exchange = prelast;
                } else {
                    prelast = last;
                    last = te;
                }
            }
            if last_exchange == -1 {
                break;
            }
            keep_prelast_loop = last_exchange;
            if keep_prelast_loop == e!(tmp as usize).next || keep_prelast_loop == tmp {
                break;
            }
        }

        y += 1;
    }
}

/// Bresenham 光栅线（对齐 OpenCV `Line` + `LineIterator`，drawing.cpp:264）。
///
/// 调用方已 clip 到图像边界，故所有像素坐标在界内。用 `get_unchecked_mut`
/// 写像素，消除 `put_point` 的切片边界检查（对齐 OpenCV 的 `uchar* ptr` 写）。
///
/// `data` 为整幅图像数据（长度 = height*step），`step` 为行字节数，
/// `pix_size` 为像素字节数。所有 (x, y) 满足 0 ≤ x < width、0 ≤ y < height
/// （调用方 clip 保证），故 `off = y*step + x*pix_size < height*step`。
pub fn raster_line(
    data: &mut [u8],
    step: usize,
    pix_size: usize,
    color: &[u8; 4],
    pt1: (i32, i32),
    pt2: (i32, i32),
    connectivity: i32,
) {
    let mut pt1 = pt1;
    let mut pt2 = pt2;
    let mut delta_x = 1i32;
    let mut delta_y = 1i32;
    let mut dx = pt2.0 - pt1.0;
    let mut dy = pt2.1 - pt1.1;

    if dx < 0 {
        dx = -dx;
        dy = -dy;
        std::mem::swap(&mut pt1, &mut pt2);
    }
    if dy < 0 {
        dy = -dy;
        delta_y = -1;
    }

    let vert = dy > dx;
    if vert {
        std::mem::swap(&mut dx, &mut dy);
        std::mem::swap(&mut delta_x, &mut delta_y);
    }

    let (err, plus_delta, minus_delta, mut minus_shift, mut plus_shift, mut minus_step, mut plus_step, count);
    if connectivity == 8 {
        err = dx - (dy + dy);
        plus_delta = dx + dx;
        minus_delta = -(dy + dy);
        minus_shift = delta_x;
        plus_shift = 0;
        minus_step = 0;
        plus_step = delta_y;
        count = dx + 1;
    } else {
        err = 0;
        plus_delta = (dx + dx) + (dy + dy);
        minus_delta = -(dy + dy);
        minus_shift = delta_x;
        plus_shift = -delta_x;
        minus_step = 0;
        plus_step = delta_y;
        count = dx + dy + 1;
    }
    let mut err = err;

    if vert {
        std::mem::swap(&mut plus_step, &mut plus_shift);
        std::mem::swap(&mut minus_step, &mut minus_shift);
    }

    let mut x = pt1.0;
    let mut y = pt1.1;
    // SAFETY: 调用方契约保证所有 (x, y) 在界内，data 长度 = height*step
    for _ in 0..count {
        let off = y as usize * step + x as usize * pix_size;
        let p = unsafe { data.get_unchecked_mut(off..off + pix_size) };
        p.copy_from_slice(&color[..pix_size]);
        let mask = if err < 0 { -1i32 } else { 0 };
        err += minus_delta + (plus_delta & mask);
        x += minus_shift + (plus_shift & mask);
        y += minus_step + (plus_step & mask);
    }
}