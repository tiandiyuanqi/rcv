//! Canny 边缘检测（对齐 OpenCV 5.0 `cv::Canny`）。
//!
//! 数据流：Sobel/Scharr 16S 梯度（BORDER_REPLICATE）→ 幅度（L1/L2）→
//! NMS + 双阈值（map: 0=候选 1=非边缘 2=边缘）→ 滞后洪泛 → finalPass。
//!
//! 对齐细节：
//! - 阈值：`low > high` 先交换；aperture=5 时阈值 /16；L2 时
//!   `min(32767, t)²`（先 clamp 再平方）；`cvFloor` 取整
//! - NMS 角度比较用 TG22=13573 定点（`y<<15` vs `x*TG22`、
//!   `tg67x = tg22x + (x<<16)`），前向邻居 `>`、后向邻居 `>=`
//! - map 为 `(h+2) × mapstep`，行 0/h+1 与每行首尾哨兵为 1，
//!   滞后洪泛只沿 map==0 扩展
//! - 多线程：行块并行（块内 Sobel + NMS + 局部洪泛），块边界行
//!   （首尾输出行）的峰值像素收集后串行全局洪泛（对齐 parallelCanny
//!   的 borderPeaks 机制）

use crate::core::border::BorderPolicy;
use crate::core::image::Image;



/// Canny 线程局部缓冲池（map + mag ring + 洪泛栈）。
///
/// MT 下每块 1MB map + 12KB mag + 栈分配是热路径；池化后跨迭代复用，
/// 避免 64 线程 × 每迭代 64 块的反复 malloc/mmap 缺页
///（对齐 OpenCV parallelCanny 的 AutoBuffer 栈分配语义）。
struct CannyPool {
    map: Vec<u8>,
    mag: Vec<i32>,
    stack: Vec<usize>,
}

thread_local! {
    static CANNY_TLS: std::cell::RefCell<CannyPool> = std::cell::RefCell::new(CannyPool {
        map: Vec::new(),
        mag: Vec::new(),
        stack: Vec::new(),
    });
}

/// Sobel 核阶数（aperture_size）：3 = Sobel 3×3（默认），5 = Sobel 5×5，
/// -1 = Scharr 3×3。
///
/// OpenCV 另有 aperture=7（7-tap 核 + 阈值/16），极罕见，暂不支持。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApertureSize {
    Sobel3,
    Sobel5,
    Scharr,
}

/// Canny 边缘检测（对应 `cv::Canny`，8U 灰度 → 8U 二值边缘图）。
///
/// `low`/`high` 为滞后阈值（low > high 时自动交换）。`l2_gradient=true`
/// 用 L2 幅度（阈值先 clamp 32767 再平方），否则 L1（|dx|+|dy|）。
///
/// 复杂度：O(w·h)（Sobel + NMS + 洪泛线性扫描）。
///
/// # Panics
/// - 空图像
pub fn canny(
    src: &Image<u8, 1>,
    low_thresh: f64,
    high_thresh: f64,
    aperture: ApertureSize,
    l2_gradient: bool,
) -> Image<u8, 1> {
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "canny: empty image");

    // 阈值预处理（对齐 canny.cpp:788-817）
    let (mut low_t, mut high_t) = (low_thresh, high_thresh);
    if low_t > high_t {
        std::mem::swap(&mut low_t, &mut high_t);
    }
    if l2_gradient {
        low_t = low_t.min(32767.0);
        high_t = high_t.min(32767.0);
        if low_t > 0.0 {
            low_t *= low_t;
        }
        if high_t > 0.0 {
            high_t *= high_t;
        }
    }
    let low = low_t.floor() as i32;
    let high = high_t.floor() as i32;

    // map 布局：(h+2) × mapstep；pmap = 行起点 + 32 偏移（对齐 OpenCV
    // CV_SIMD_WIDTH 偏移），pmap[0]/pmap[w+1] 哨兵 = 1；行 0/h+1 全 1
    let mapstep = (w + 32 + 1 + 31) & !31;
    // map 走线程局部池（ST 主线程复用，跨迭代免 1MB mmap 缺页）；
    // NMS 内核全量覆写每行 [1, w] 并写首尾哨兵（pmap[0]/pmap[w+1] = 1），
    // 此处只预填行 0/h+1（全 1 哨兵行）；32 偏移前缀与行尾 padding 不被读写
    let mut map: Vec<u8> = CANNY_TLS.with(|p| {
        let mut pool = p.borrow_mut();
        pool.map.clear();
        pool.map.resize((h + 2) * mapstep, 0);
        std::mem::take(&mut pool.map)
    });
    map[..mapstep].fill(1); // 行 0（哨兵行）
    map[(h + 1) * mapstep..].fill(1); // 行 h+1（哨兵行）
    let isa = crate::simd_kernels::x86::isa();

    // NMS + 双阈值 + 滞后洪泛（块内 Sobel，对齐 OpenCV parallelCanny）
    let mut border_peaks: Vec<usize> = Vec::new();
    nms_and_hysteresis(
        src, &mut map, w, h, mapstep, low, high, l2_gradient, aperture, isa, &mut border_peaks,
    );

    // 全局洪泛（块边界峰值）
    if !border_peaks.is_empty() {
        let mut stack = std::mem::take(&mut border_peaks);
        crate::simd_kernels::canny_hysteresis(&mut map, mapstep, &mut stack);
    }

    // finalPass：dst[y][x] = (map[(y+1)][x+32偏移+1] == 2) ? 255 : 0
    let mut out: Image<u8, 1> =
        Image::from_vec(w, h, crate::simd_kernels::alloc_uninit(w * h)).expect("len");
    {
        use rayon::prelude::*;
        let out_data = out.data_mut();
        // 行块并行（每块 ≥ 16 行，避免逐行任务的调度开销）
        const FP_ROWS: usize = 16;
        let fp_chunk = w * FP_ROWS;
        out_data.par_chunks_mut(fp_chunk).enumerate().for_each(|(bi, dst_blk)| {
            let y0 = bi * FP_ROWS;
            for (r, dst_row) in dst_blk.chunks_exact_mut(w).enumerate() {
                let y = y0 + r;
                let pmap = &map[(y + 1) * mapstep + 32..];
                crate::simd_kernels::canny_final_pass_row(pmap, dst_row, w);
            }
        });
    }
    // 归还 map 到线程局部池（容量保留，跨迭代复用）
    CANNY_TLS.with(|p| {
        let mut pool = p.borrow_mut();
        pool.map = map;
    });
    out
}

/// NMS + 双阈值 + 滞后洪泛（ST 顺序 / MT 行块并行）。
///
/// 块内 Sobel（对齐 OpenCV parallelCanny）：每块只算自己行 + 2 halo 行，
/// 不物化全图 dx/dy。`border_peaks` 收集块边界行的峰值像素（map 线性偏移），
/// 供全局洪泛。
#[allow(clippy::too_many_arguments)]
fn nms_and_hysteresis(
    src: &Image<u8, 1>,
    map: &mut [u8],
    w: usize,
    h: usize,
    mapstep: usize,
    low: i32,
    high: i32,
    l2: bool,
    aperture: ApertureSize,
    isa: crate::simd_kernels::x86::Isa,
    border_peaks: &mut Vec<usize>,
) {
    use rayon::prelude::*;

    // 行块划分（对齐 OpenCV grainSize：块 ≥ 2*(ksize2+1) 行）
    let threads = rayon::current_num_threads().max(1);
    let min_grain = 4usize;
    let chunk = (h / threads).max(min_grain);
    let nblocks = h.div_ceil(chunk);
    let parallel = nblocks > 1 && threads > 1;

    // 块处理：输出行 [y0, y1)，块内 3 行 mag ring + NMS + 局部洪泛。
    // `map` 为块内切片，块内偏移 0 = 全图 map 行 y0+1（输出行 y ↔ 块内
    // 行 y-y0）。返回块边界行（map 行 y0+1 与 y1）上的峰值像素偏移
    // （全图坐标）。
    // mag/stack 走线程局部缓冲池（MT 下每块 12KB+ 栈分配是热路径，
    // 池化后跨迭代复用，避免反复 malloc + 缺页）
    let process_block = |y0: usize, y1: usize, map: &mut [u8]| -> Vec<usize> {
        let mag_w = w + 2;
        // 从线程局部池 take 缓冲（块尾归还），避免借用交叉
        let (mut mag, mut stack) = CANNY_TLS.with(|p| {
            let mut pool = p.borrow_mut();
            (
                std::mem::take(&mut pool.mag),
                std::mem::take(&mut pool.stack),
            )
        });
        mag.clear();
        mag.resize(4 * mag_w, 0);
        stack.clear();
        // 块内 Sobel：行阶段 u8 → i32（mid ring 3 行），列阶段 i32 → i16。
        // 块内行 r ∈ [y0-1, y1+1]（越界 clamp 到图像边界，对齐 BORDER_REPLICATE）。
        // dx/dy 各 3 行 ring（槽 k 持有行 k-1 mod 3）。
        let border = BorderPolicy::Clamp;
        // Sobel 3×3 / Scharr 3×3 核（中心化 [k[-1], k[0], k[1]]）
        // dx: kx 反对称 [-1,0,1], ky 对称 [1,2,1]（Scharr: [3,10,3]）
        // dy: kx 对称 [1,2,1]（Scharr: [3,10,3]）, ky 反对称 [-1,0,1]
        let (dx_row_k1, dy_row_k0, dy_row_k1, dx_col_f0, dx_col_f1, dy_col_f1) = match aperture {
            ApertureSize::Sobel3 => (1, 2, 1, 2, 1, 1),
            ApertureSize::Scharr => (1, 10, 3, 10, 3, 1),
            ApertureSize::Sobel5 => {
                // Sobel 5×5 走标量路径（sobel5 无 SIMD 内核），块内直接调用
                // sobel5 逐行（每行 5-tap 需 5 行源，块内 halo 2 行足够）
                // —— 此处用 3×3 核占位，实际走下方 sobel5 分支
                (1, 2, 1, 2, 1, 1)
            }
        };

        // 块内 dx/dy ring（槽 k 持有行 k-1 mod 3）
        let mut dx_ring: Vec<i16> = crate::simd_kernels::alloc_uninit(3 * w);
        let mut dy_ring: Vec<i16> = crate::simd_kernels::alloc_uninit(3 * w);
        // mag ring（槽 k 持有行 k-1 mod 4）—— 4 槽：NMS 行 y 需 mag [y-1, y, y+1]，
        // 且 col_stage(y+2) 写 mag 行 y+2，需与 NMS 三行不重叠（3 槽会覆盖前一行）
        // mid ring（行阶段输出，槽 k 持有行 k-1 mod 5）—— dx/dy 各一份
        // 5 槽：col_stage(r) 需 mid 行 [r-1, r+1]，预填 5 行避免覆盖
        let mut mid_dx: Vec<i32> = crate::simd_kernels::alloc_uninit(5 * w);
        let mut mid_dy: Vec<i32> = crate::simd_kernels::alloc_uninit(5 * w);

        // 计算块内行 r 的 dx/dy + mag（r ∈ [y0-1, y1+1]，越界 clamp）
        // 行阶段：src 行 r → mid 行 r（i32）
        // 列阶段：mid 行 [r-1, r+1] → dx/dy 行 r（i16）
        // 行阶段：src 行 r → mid 行 r（i32）。r 越界 clamp 到图像边界。
        // mid ring 槽 = r mod 5（5 槽，col_stage 需 [r-1, r+1] 3 行，预填 5 行不覆盖）
        let row_stage = |r: i64, mid_dx: &mut [i32], mid_dy: &mut [i32]| {
            let sy = r.clamp(0, h as i64 - 1) as usize;
            let src_row = &src.data()[sy * w..(sy + 1) * w];
            let mslot = (r.rem_euclid(5)) as usize;
            let mrow = &mut mid_dx[mslot * w..mslot * w + w];
            crate::simd_kernels::deriv_row_asymm_k3_cn1(src_row, mrow, w, dx_row_k1, border);
            let mrow_dy = &mut mid_dy[mslot * w..mslot * w + w];
            crate::simd_kernels::deriv_row_symm_k3_cn1(src_row, mrow_dy, w, dy_row_k0, dy_row_k1, border);
        };
        // 列阶段 + mag：mid 行 [r-1, r+1] → dx/dy 行 r → mag 行 r
        let col_stage = |r: i64, dx_ring: &mut [i16], dy_ring: &mut [i16], mid_dx: &[i32], mid_dy: &[i32], mag: &mut [i32]| {
            let dslot = (r.rem_euclid(3)) as usize;
            let mslot = (r.rem_euclid(4)) as usize;
            let m0 = &mid_dx[((r - 1).rem_euclid(5)) as usize * w..((r - 1).rem_euclid(5)) as usize * w + w];
            let m1 = &mid_dx[((r).rem_euclid(5)) as usize * w..((r).rem_euclid(5)) as usize * w + w];
            let m2 = &mid_dx[((r + 1).rem_euclid(5)) as usize * w..((r + 1).rem_euclid(5)) as usize * w + w];
            let drow = &mut dx_ring[dslot * w..dslot * w + w];
            crate::simd_kernels::deriv_col_symm_k3([m0, m1, m2], drow, w, dx_col_f0, dx_col_f1, 0);
            let n0 = &mid_dy[((r - 1).rem_euclid(5)) as usize * w..((r - 1).rem_euclid(5)) as usize * w + w];
            let n1 = &mid_dy[((r).rem_euclid(5)) as usize * w..((r).rem_euclid(5)) as usize * w + w];
            let n2 = &mid_dy[((r + 1).rem_euclid(5)) as usize * w..((r + 1).rem_euclid(5)) as usize * w + w];
            let drow_dy = &mut dy_ring[dslot * w..dslot * w + w];
            crate::simd_kernels::deriv_col_asymm_k3([n0, n1, n2], drow_dy, w, dy_col_f1, 0);
            mag[mslot * mag_w] = 0;
            mag[mslot * mag_w + w + 1] = 0;
            let mag_row = &mut mag[mslot * mag_w + 1..mslot * mag_w + 1 + w];
            mag_row_dispatch(drow, drow_dy, mag_row, w, l2, isa);
        };

        // 预填 mid 行 y0-2, y0-1, y0, y0+1, y0+2（行阶段，5 槽）
        row_stage(y0 as i64 - 2, &mut mid_dx, &mut mid_dy);
        row_stage(y0 as i64 - 1, &mut mid_dx, &mut mid_dy);
        row_stage(y0 as i64, &mut mid_dx, &mut mid_dy);
        row_stage(y0 as i64 + 1, &mut mid_dx, &mut mid_dy);
        row_stage(y0 as i64 + 2, &mut mid_dx, &mut mid_dy);
        // 预填 mag 行 y0-1, y0, y0+1（列阶段）
        col_stage(y0 as i64 - 1, &mut dx_ring, &mut dy_ring, &mid_dx, &mid_dy, &mut mag);
        col_stage(y0 as i64, &mut dx_ring, &mut dy_ring, &mid_dx, &mid_dy, &mut mag);
        col_stage(y0 as i64 + 1, &mut dx_ring, &mut dy_ring, &mid_dx, &mid_dy, &mut mag);
        // 首块：mag 行 -1 应为全 0（对齐 OpenCV 首块 ring 初始化为 0，
        // 而非用 clamp 源行算出的 mag 行 -1）
        if y0 == 0 {
            let slot = (y0 as i64 - 1).rem_euclid(4) as usize;
            mag[slot * mag_w..slot * mag_w + mag_w].fill(0);
        }
        // 处理输出行 y ∈ [y0, y1)：先算 mid 行 y+3（行阶段），再算 dx/dy/mag 行 y+2
        //（列阶段，用 mid 行 [y+1, y+3]），最后 NMS（用 mag 行 [y-1, y+1]）。
        for y in y0..y1 {
            row_stage(y as i64 + 3, &mut mid_dx, &mut mid_dy);
            col_stage(y as i64 + 2, &mut dx_ring, &mut dy_ring, &mid_dx, &mid_dy, &mut mag);
            // 末块：mag 行 h 应为全 0（对齐 OpenCV 末块 ring 清零，
            // 而非用 clamp 源行算出的 mag 行 h）
            if y1 == h && y as i64 + 2 == h as i64 {
                let slot = (h as i64).rem_euclid(4) as usize;
                mag[slot * mag_w..slot * mag_w + mag_w].fill(0);
            }
            let sp = (y as i64 - 1).rem_euclid(4) as usize;
            let sa = (y as i64).rem_euclid(4) as usize;
            let sn = ((y + 1) as i64).rem_euclid(4) as usize;
            let p_row = &mag[sp * mag_w..sp * mag_w + mag_w];
            let a_row = &mag[sa * mag_w..sa * mag_w + mag_w];
            let n_row = &mag[sn * mag_w..sn * mag_w + mag_w];
            let dsa = (y as i64).rem_euclid(3) as usize;
            let dx_row = &dx_ring[dsa * w..dsa * w + w];
            let dy_row = &dy_ring[dsa * w..dsa * w + w];
            let pmap = &mut map[(y - y0) * mapstep + 32..(y - y0) * mapstep + 32 + w + 2];
            let stack_len = stack.len();
            crate::simd_kernels::canny_nms_row(
                p_row, a_row, n_row, dx_row, dy_row, pmap, w, low, high, &mut stack,
            );
            // 栈内新元素是 pmap 相对偏移（列 1..w+1）→ 块内 map 线性偏移
            let base = (y - y0) * mapstep + 32;
            for off in &mut stack[stack_len..] {
                *off += base;
            }
        }
        // 局部洪泛：块内行 [0, y1-y0)（块内行 r ↔ 全图 map 行 y0+1+r）。
        // 边界行（块内行 0 与 y1-y0-1）上的像素交全局洪泛。
        let lo = 0usize;
        let hi = (y1 - y0) * mapstep;
        let mut peaks: Vec<usize> = Vec::new();
        while let Some(off) = stack.pop() {
            let on_border = off < lo + mapstep || off >= hi - mapstep;
            if on_border {
                peaks.push(off);
                continue;
            }
            flood_within(map, mapstep, off, lo, hi, &mut stack, &mut peaks);
        }
        // 归还缓冲到线程局部池（容量保留，跨迭代复用）
        CANNY_TLS.with(|p| {
            let mut pool = p.borrow_mut();
            pool.mag = mag;
            pool.stack = stack;
        });
        // 块内坐标 → 全图坐标（块内偏移 0 = 全图 map 行 y0+1）
        for off in &mut peaks {
            *off += (y0 + 1) * mapstep;
        }
        peaks
    };

    if !parallel {
        // ST：单块覆盖全图（map 切掉行 0 哨兵，块内行 0 = map 行 1）
        let (_, rest) = map.split_at_mut(mapstep);
        let mut stack = process_block(0, h, rest);
        crate::simd_kernels::canny_hysteresis(map, mapstep, &mut stack);
        return;
    }

    // MT：行块并行。块 bi = map 行 [y0+1, y1+1)（写区间恰好不重叠），
    // 串行 split 后并行处理
    let (_, rest) = map.split_at_mut(mapstep);
    let mut blocks: Vec<&mut [u8]> = Vec::with_capacity(nblocks);
    let mut rest = rest;
    for bi in 0..nblocks {
        let y0 = bi * chunk;
        let y1 = (y0 + chunk).min(h);
        let rows = y1 - y0;
        let (blk, r) = rest.split_at_mut(rows * mapstep);
        blocks.push(blk);
        rest = r;
    }
    let peaks_per_block: Vec<Vec<usize>> = blocks
        .into_par_iter()
        .enumerate()
        .map(|(bi, mblock)| {
            let y0 = bi * chunk;
            let y1 = (y0 + chunk).min(h);
            process_block(y0, y1, mblock)
        })
        .collect();
    for peaks in peaks_per_block {
        border_peaks.extend(peaks);
    }
}

/// 块内洪泛：从 `off` 扩展，8 邻域限制在 map 行 [map_lo, map_hi)。
///
/// 越界邻居（块边界行外）跳过；边界行上的 0 像素若被触及也收集到 peaks
/// （对齐 OpenCV borderPeaks 语义：边界行像素交全局洪泛处理）。
#[allow(clippy::too_many_arguments)]
fn flood_within(
    map: &mut [u8],
    mapstep: usize,
    off: usize,
    map_lo: usize,
    map_hi: usize,
    stack: &mut Vec<usize>,
    peaks: &mut Vec<usize>,
) {
    map[off] = 2;
    let neighbors = [
        off.wrapping_sub(mapstep + 1),
        off.wrapping_sub(mapstep),
        off.wrapping_sub(mapstep - 1),
        off - 1,
        off + 1,
        off + mapstep - 1,
        off + mapstep,
        off + mapstep + 1,
    ];
    for nb in neighbors {
        if nb < map_lo || nb >= map_hi {
            continue;
        }
        if map[nb] == 0 {
            let on_border = nb < map_lo + mapstep || nb >= map_hi - mapstep;
            if on_border {
                // 边界行上的候选：置 2 并收集（全局洪泛会继续向外扩展）
                map[nb] = 2;
                peaks.push(nb);
            } else {
                map[nb] = 2;
                stack.push(nb);
            }
        }
    }
}

/// 幅度行派发（SIMD/标量）。
fn mag_row_dispatch(
    dx: &[i16],
    dy: &[i16],
    mag: &mut [i32],
    n: usize,
    l2: bool,
    _isa: crate::simd_kernels::x86::Isa,
) {
    crate::simd_kernels::canny_mag_row(dx, dy, mag, n, l2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::image::Image;

    fn img(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Image<u8, 1> {
        Image::from_fn(w, h, |x, y| [f(x, y)])
    }

    #[test]
    fn canny_vertical_edge() {
        // 左暗右亮 → x=边界处垂直边缘
        let src = img(16, 16, |x, _| if x < 8 { 0 } else { 255 });
        let dst = canny(&src, 50.0, 100.0, ApertureSize::Sobel3, false);
        // x=7/8 附近应有边缘
        let mut edge_cols = 0;
        for y in 0..16 {
            for x in 0..16 {
                if dst.data()[y * 16 + x] == 255 {
                    assert!(x == 7 || x == 8, "edge at ({x},{y})");
                    edge_cols += 1;
                }
            }
        }
        assert!(edge_cols > 0, "should detect vertical edge");
    }

    #[test]
    fn canny_blank_image() {
        let src = img(32, 32, |_, _| 128);
        let dst = canny(&src, 50.0, 100.0, ApertureSize::Sobel3, false);
        assert!(dst.data().iter().all(|&v| v == 0), "blank → no edges");
    }

    #[test]
    fn canny_low_gt_high_swapped() {
        let src = img(16, 16, |x, _| if x < 8 { 0 } else { 255 });
        let a = canny(&src, 50.0, 100.0, ApertureSize::Sobel3, false);
        let b = canny(&src, 100.0, 50.0, ApertureSize::Sobel3, false);
        assert_eq!(a.data(), b.data(), "low>high must swap");
    }

    #[test]
    fn canny_l2_gradient() {
        let src = img(16, 16, |x, _| if x < 8 { 0 } else { 255 });
        let dst = canny(&src, 50.0, 100.0, ApertureSize::Sobel3, true);
        // L2 阈值平方后 2500/10000，梯度 1020² > 10000 → 仍有边缘
        assert!(dst.data().contains(&255));
    }

    #[test]
    fn canny_scharr_aperture() {
        let src = img(16, 16, |x, _| if x < 8 { 0 } else { 255 });
        for ap in [ApertureSize::Sobel3, ApertureSize::Scharr] {
            let dst = canny(&src, 50.0, 100.0, ap, false);
            assert!(dst.data().contains(&255), "{ap:?}");
        }
    }

    #[test]
    fn canny_tiny_images() {
        // 1×1、1×N、N×1 不 panic
        let _ = canny(&img(1, 1, |_, _| 0), 10.0, 20.0, ApertureSize::Sobel3, false);
        let _ = canny(&img(5, 1, |x, _| x as u8 * 50), 10.0, 20.0, ApertureSize::Sobel3, false);
        let _ = canny(&img(1, 5, |_, y| y as u8 * 50), 10.0, 20.0, ApertureSize::Sobel3, false);
    }

    #[test]
    fn canny_st_mt_equal() {
        // ST 与 MT 结果一致（全图 Sobel 保证）
        let src = img(200, 200, |x, y| ((x * 7 + y * 13) % 256) as u8);
        let st = {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
            pool.install(|| canny(&src, 30.0, 60.0, ApertureSize::Sobel3, false))
        };
        let mt = {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap();
            pool.install(|| canny(&src, 30.0, 60.0, ApertureSize::Sobel3, false))
        };
        assert_eq!(st.data(), mt.data());
    }
}
