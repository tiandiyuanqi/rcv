//! TRU 并行轮廓提取，对齐 OpenCV 5 `contours_truco.cpp` 的
//! `findTRUContoursImpl`（RETR_LIST 无 hierarchy 的快路径）。
//!
//! 算法：行条带并行（rayon），每条带内 Moore 邻域跟踪外/内轮廓
//!（内核在 `simd_kernels/contours.rs`），条带边界行的碎片按
//! Suzuki-Abe 顺序两指针合并。
//!
//! 像素标记：FOREGROUND=255（pad 后二值化）、VISITED_OUTER_RIGHT=100、
//! VISITED=200、BACKGROUND=0。

use crate::core::image::Image;
use crate::core::point::Point;

use super::ContourApproxMethod;

/// 条带累加器（对齐 AccumulatorT）。
#[derive(Default)]
pub(crate) struct Accumulator {
    pub(crate) contours: Vec<Vec<Point>>,
    /// 本条带最后一行的内轮廓索引（尾部 run）。
    pub(crate) idx_internal_lastline: Vec<usize>,
    /// 本条带第一行的外轮廓索引（头部 run）。
    pub(crate) idx_external_firstline: Vec<usize>,
}

/// TRU 主入口（对齐 findTRUContoursImpl）。
///
/// 输入为原图（未 pad）；内部 pad 1 圈 + 二值化（非零 → 255）。
/// 返回轮廓（RETR_LIST 语义：全部轮廓，Suzuki-Abe 顺序）。
pub fn tru_find_contours(
    src: &Image<u8, 1>,
    method: ContourApproxMethod,
    offset: Point,
) -> Vec<Vec<Point>> {
    let (w, h) = (src.width(), src.height());
    let pw = w + 2;
    let ph = h + 2;

    // pad + 二值化（对齐 copyMakeBorder + threshold(0,255)）：
    // 行级 copy + SIMD 阈值化（threshold Binary t=0 mv=255 等价非零→255）
    let mut img = vec![0u8; pw * ph];
    for y in 0..h {
        let (dst_row, src_row) = {
            let d = &mut img[(y + 1) * pw + 1..(y + 1) * pw + 1 + w];
            let s = &src.data()[y * w..y * w + w];
            (d, s)
        };
        dst_row.copy_from_slice(src_row);
        crate::simd_kernels::threshold_u8(src_row, dst_row, w, 0, 255, 0);
    }

    // 条带数（对齐 nstripes = getNumThreads()）。共享机器上
    // 过多条带（每条带仅 16 行）导致争抢 + 合并开销，钳制上限
    let nstripes = rayon::current_num_threads().clamp(1, 8);
    let mut out = tru_impl(&mut img, pw, ph, nstripes, method);

    // offset 平移
    if offset.x != 0 || offset.y != 0 {
        for c in &mut out {
            for p in c.iter_mut() {
                p.x += offset.x;
                p.y += offset.y;
            }
        }
    }
    out
}

/// 核心实现（padded 图 + 条带数）。
fn tru_impl(
    img: &mut [u8],
    pw: usize,
    ph: usize,
    nstripes: usize,
    method: ContourApproxMethod,
) -> Vec<Vec<Point>> {
    // 条带边界（对齐 truco:508-523：行 [1, ph-2] 均分）
    let total_rows = ph - 2;
    let mut ranges: Vec<(i32, i32)> = Vec::new();
    if nstripes > 1 {
        let rows_per = total_rows / nstripes;
        let rem = total_rows % nstripes;
        let mut cur = 1i32;
        for t in 0..nstripes {
            let start = cur;
            let end = start + rows_per as i32 + if t < rem { 1 } else { 0 };
            ranges.push((start, end));
            cur = end;
        }
    } else {
        ranges.push((1, ph as i32 - 1));
    }

    // 并行处理条带（对齐 parallel_for_）
    let accs: Vec<crate::simd_kernels::contours::TruAccumulator> =
        crate::simd_kernels::contours::tru_process_stripes(img, pw, ph, &ranges);

    // 条带边界合并（对齐 truco:536-575：两指针按首点 x 归并）
    let mut accs: Vec<Accumulator> = accs
        .into_iter()
        .map(|a| Accumulator {
            contours: a.contours,
            idx_internal_lastline: a.idx_internal_lastline,
            idx_external_firstline: a.idx_external_firstline,
        })
        .collect();
    for t in 0..accs.len().saturating_sub(1) {
        let (head, tail) = accs.split_at_mut(t + 1);
        let acc_t = &mut head[t];
        let acc_t1 = &mut tail[0];

        let ki = acc_t.idx_internal_lastline.len();
        let ke = acc_t1.idx_external_firstline.len();
        if ki == 0 && ke == 0 {
            continue;
        }

        let tail_start = acc_t.contours.len() - ki;
        // 归并（按首点 x 升序）
        let mut merged: Vec<Vec<Point>> = Vec::with_capacity(ki + ke);
        let mut i = tail_start;
        let mut j = 0usize;
        while i < acc_t.contours.len() && j < ke {
            let ext_idx = acc_t1.idx_external_firstline[j];
            if acc_t.contours[i][0].x <= acc_t1.contours[ext_idx][0].x {
                merged.push(std::mem::take(&mut acc_t.contours[i]));
                i += 1;
            } else {
                merged.push(std::mem::take(&mut acc_t1.contours[ext_idx]));
                j += 1;
            }
        }
        while i < acc_t.contours.len() {
            merged.push(std::mem::take(&mut acc_t.contours[i]));
            i += 1;
        }
        while j < ke {
            let ext_idx = acc_t1.idx_external_firstline[j];
            merged.push(std::mem::take(&mut acc_t1.contours[ext_idx]));
            j += 1;
        }

        acc_t.contours.truncate(tail_start);
        acc_t.contours.extend(merged);
        // 删除 acc_t1 头部的 ke 个外轮廓（对齐 erase(begin, begin+kE)）。
        // 首行只产生外轮廓（内轮廓要求 r > row_start），故它们是
        // acc_t1 的前 ke 个（索引 0..ke 连续），drain 即可
        acc_t1.contours.drain(0..ke);
        for e in acc_t1.idx_internal_lastline.iter_mut() {
            *e -= ke;
        }
        acc_t1.idx_external_firstline.clear();
        acc_t.idx_internal_lastline.clear();
    }

    // 拼接 + 逆序（对齐 truco:577-593）
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut total = 0;
    for a in &accs {
        total += a.contours.len();
    }
    out.reserve(total);
    for mut a in accs {
        out.append(&mut a.contours);
    }
    out.reverse();

    // 近似（对齐 truco:596-603：并行 approxContour）
    if method != ContourApproxMethod::None {
        use rayon::prelude::*;
        out.par_iter_mut().for_each(|c| {
            *c = super::approx_contour(c, method);
        });
    }

    out
}
