//! 轮廓查找 SIMD 内核：行扫描跳转（找非零/找零）+ TC89 链码近似 +
//! Douglas-Peucker。
//!
//! 对齐 OpenCV 5 `contours_link.cpp` 的 `findStartContourPoint`/
//! `findEndContourPoint`（v_uint8 比较 + scan_forward）与
//! `contours_approx.cpp` 的 `approximateChainTC89`、`approx.cpp` 的
//! `approxPolyDP_`。
//!
//! 三路径：标量（本文件）/ AVX2 / AVX512（x86/），运行时派发。

use crate::core::point::Point;

/// 行内找第一个非零像素（对齐 findStartContourPoint）。
///
/// `row` 为整行（含 pad 哨兵），从 `j` 开始扫描到 `width`（不含）。
/// 返回第一个非零下标；无则返回 `width`。
pub fn find_start_contour_point(row: &[u8], j: usize) -> usize {
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => unsafe {
            crate::simd_kernels::x86::avx512::find_start_contour_point(row, j)
        },
        crate::simd_kernels::x86::Isa::Avx2 => unsafe {
            crate::simd_kernels::x86::avx2::find_start_contour_point(row, j)
        },
        crate::simd_kernels::x86::Isa::Scalar => scalar_find_start(row, j),
    }
}

/// 行内找第一个零像素（对齐 findEndContourPoint）。
pub fn find_end_contour_point(row: &[u8], j: usize) -> usize {
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => unsafe {
            crate::simd_kernels::x86::avx512::find_end_contour_point(row, j)
        },
        crate::simd_kernels::x86::Isa::Avx2 => unsafe {
            crate::simd_kernels::x86::avx2::find_end_contour_point(row, j)
        },
        crate::simd_kernels::x86::Isa::Scalar => scalar_find_end(row, j),
    }
}

/// 单趟扫描一行，返回所有 run 边界（对齐 OpenCV TRU 的 findStart/findEnd
/// 交替扫描，但消除逐 run 分支预测抖动）。
pub fn scan_row_runs(row: &[u8], starts: &mut Vec<u32>, ends: &mut Vec<u32>) {
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => unsafe {
            crate::simd_kernels::x86::avx512::scan_row_runs(row, starts, ends)
        },
        crate::simd_kernels::x86::Isa::Avx2 => unsafe {
            crate::simd_kernels::x86::avx2::scan_row_runs(row, starts, ends)
        },
        _ => {
            // 标量/AVX2 回退：逐 run 扫描
            starts.clear();
            ends.clear();
            let width = row.len();
            let mut c = 1usize;
            while c < width - 1 {
                c = find_start_contour_point(row, c);
                if c >= width - 1 {
                    break;
                }
                let s = c as u32;
                c = find_end_contour_point(row, c + 1);
                if c >= width {
                    break;
                }
                starts.push(s);
                ends.push(c as u32);
            }
        }
    }
}

/// Suzuki findNextX：行内找第一个与 `prev` 不同的像素（对齐
/// contours_new.cpp:586 标量路径）。
///
/// `row` 为 padded 行；扫描 `[x, width)`；`p` 输出该像素值。
pub fn find_next_diff(row: &[i8], x: usize, width: usize, prev: i8, p: &mut i8) -> usize {
    let mut j = x;
    // 对齐 OpenCV：先查首元素（SIMD 路径的前置快速检查）
    if j < width {
        let v = row[j];
        if v != prev {
            *p = v;
            return j;
        }
        j += 1;
    }
    // SIMD 批量跳过 == prev 的段
    match crate::simd_kernels::x86::isa() {
        crate::simd_kernels::x86::Isa::Avx512 => unsafe {
            crate::simd_kernels::x86::avx512::find_next_diff(row, j, width, prev, p)
        },
        crate::simd_kernels::x86::Isa::Avx2 => unsafe {
            crate::simd_kernels::x86::avx2::find_next_diff(row, j, width, prev, p)
        },
        crate::simd_kernels::x86::Isa::Scalar => {
            while j < width && row[j] == prev {
                j += 1;
            }
            if j < width {
                *p = row[j];
            }
            j
        }
    }
}

fn scalar_find_start(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    while k < width && row[k] == 0 {
        k += 1;
    }
    k
}

fn scalar_find_end(row: &[u8], j: usize) -> usize {
    let width = row.len();
    let mut k = j;
    while k < width && row[k] != 0 {
        k += 1;
    }
    k
}

// ---------------------------------------------------------------------------
// TRU 并行轮廓跟踪内核（对齐 contours_truco.cpp TRUCOntourTracer）
// ---------------------------------------------------------------------------

const TRU_FOREGROUND: u8 = 255;
const TRU_BACKGROUND: u8 = 0;
const TRU_VISITED_OUTER_RIGHT: u8 = 100;
const TRU_VISITED: u8 = 200;

// 0=E, 1=NE, 2=N, 3=NW, 4=W, 5=SW, 6=S, 7=SE
const TRU_DX: [i32; 8] = [1, 1, 0, -1, -1, -1, 0, 1];
const TRU_DY: [i32; 8] = [0, -1, -1, -1, 0, 1, 1, 1];

/// 条带累加器（对齐 AccumulatorT）。
#[derive(Default)]
pub struct TruAccumulator {
    pub contours: Vec<Vec<Point>>,
    /// 本条带最后一行的内轮廓索引（尾部 run）。
    pub idx_internal_lastline: Vec<usize>,
    /// 本条带第一行的外轮廓索引（头部 run）。
    pub idx_external_firstline: Vec<usize>,
}

/// 分块点缓冲（对齐 OpenCV `TRUCOPagedContour`）。
///
/// 热循环 `push_back` 仅做指针比较 + 存储（无计数器、无容量检查），
/// 块满时分配新块。`to_vec` 用块级 memcpy 输出，避免逐点 push。
struct PagedBuffer {
    blocks: Vec<Box<[Point; BLOCK_SIZE]>>,
    /// 当前块索引。
    block_idx: usize,
    /// 当前块内已用点数。
    count: usize,
}

const BLOCK_SIZE: usize = 4096;

impl PagedBuffer {
    #[inline]
    fn new() -> Self {
        let mut blocks = Vec::with_capacity(4);
        blocks.push(Box::new([Point::new(0, 0); BLOCK_SIZE]));
        Self { blocks, block_idx: 0, count: 0 }
    }

    #[inline]
    fn clear(&mut self) {
        self.block_idx = 0;
        self.count = 0;
    }

    #[inline]
    fn push(&mut self, p: Point) {
        if self.count == BLOCK_SIZE {
            self.block_idx += 1;
            if self.block_idx == self.blocks.len() {
                self.blocks.push(Box::new([Point::new(0, 0); BLOCK_SIZE]));
            }
            self.count = 0;
        }
        self.blocks[self.block_idx][self.count] = p;
        self.count += 1;
    }

    #[inline]
    fn pop(&mut self) {
        if self.count == 0 {
            if self.block_idx == 0 {
                return;
            }
            self.block_idx -= 1;
            self.count = BLOCK_SIZE;
        }
        self.count -= 1;
    }

    #[inline]
    fn len(&self) -> usize {
        self.block_idx * BLOCK_SIZE + self.count
    }

    #[inline]
    fn first(&self) -> Point {
        self.blocks[0][0]
    }

    #[inline]
    fn last(&self) -> Point {
        if self.count == 0 {
            self.blocks[self.block_idx - 1][BLOCK_SIZE - 1]
        } else {
            self.blocks[self.block_idx][self.count - 1]
        }
    }

    /// 块级 memcpy 输出到 `Vec<Point>`（对齐 `copyTo`）。
    fn to_vec(&self) -> Vec<Point> {
        let total = self.len();
        let mut out = Vec::with_capacity(total);
        // 完整块
        for b in &self.blocks[..self.block_idx] {
            out.extend_from_slice(&b[..]);
        }
        // 最后部分块
        if self.count > 0 {
            out.extend_from_slice(&self.blocks[self.block_idx][..self.count]);
        }
        out
    }
}

/// 邻居偏移（对齐 offsets_[16]：idx ∈ [0, 16)）。
#[inline(always)]
fn tru_offset(idx: i32, stride: isize) -> isize {
    const DXI: [isize; 8] = [1, 1, 0, -1, -1, -1, 0, 1];
    const DYI: [isize; 8] = [0, -1, -1, -1, 0, 1, 1, 1];
    let i = (idx & 7) as usize;
    DXI[i] + DYI[i] * stride
}

/// Moore 邻域轮廓跟踪（对齐 TRUCOntourTracer::traceContour）。
///
/// 返回 false 表示越出条带上界（碎片）或孤立内点。
/// 跟踪的点写入 `buffer`（padded 坐标 - 1，即原图坐标）。
///
/// # Safety
/// `img` 长度 = 行数 × stride；`r`/`c` 为 padded 坐标且在图内；
/// 跟踪路径由 pad 哨兵圈（全 0）保证不越出 `img`。
unsafe fn tru_trace_contour(
    img: &mut [u8],
    stride: usize,
    r: i32,
    c: i32,
    row_start: i32,
    is_external: bool,
    buffer: &mut PagedBuffer,
) -> bool {
    // SAFETY: pad 哨兵圈 + 调用方契约保证所有偏移在 img 内
    unsafe {
    buffer.clear();

    let stride_i = stride as isize;
    // 预计算 16 项邻居偏移（对齐 TRUCOntourTracer::offsets_[16]，
    // 消除热循环内的乘法）
    let mut offsets = [0isize; 16];
    for (i, off) in offsets.iter_mut().enumerate() {
        *off = tru_offset(i as i32, stride_i);
    }
    let mut curr_x = c;
    let mut curr_y = r;
    let mut start_dir: i32 = -1;
    let mut search_idx: i32 = if is_external { 5 } else { 1 };
    let mut curr = (r as usize) * stride + c as usize;
    let start = curr;
    let mut is_first_move = true;

    // 内轮廓快速检查：第一个非背景邻居必须是 NE（对齐 truco:236-246）
    if !is_external {
        let mut n = 0;
        while n < 8 {
            let idx = search_idx + n;
            // SAFETY: pad 哨兵圈保证邻居偏移在 img 内
            let nb = *img.get_unchecked((curr as isize + offsets[idx as usize]) as usize);
            if nb == TRU_BACKGROUND {
                n += 1;
                continue;
            }
            if curr_x + TRU_DX[(idx & 7) as usize] != c + 1
                || curr_y + TRU_DY[(idx & 7) as usize] != r - 1
            {
                return false;
            }
            break;
        }
        if n == 8 {
            return false; // 孤立像素不算内轮廓
        }
    }

    loop {
        buffer.push(Point::new(curr_x - 1, curr_y - 1));
        let mut dir = -1;
        for n in 0..8 {
            let idx = search_idx + n;
            let nb_off = (curr as isize + offsets[idx as usize]) as usize;
            // SAFETY: pad 哨兵圈保证偏移在 img 内
            let neighbor = *img.get_unchecked(nb_off);
            if neighbor == TRU_BACKGROUND {
                continue;
            }
            dir = idx & 7;

            // 移动
            curr_y += TRU_DY[dir as usize];
            curr_x += TRU_DX[dir as usize];

            // 越出条带上界 → 碎片
            if curr_y < row_start {
                return false;
            }

            if (search_idx <= 1) || (dir <= search_idx - 2) {
                // SAFETY: curr 为有效像素偏移
                *img.get_unchecked_mut(curr) = TRU_VISITED_OUTER_RIGHT;
            } else if *img.get_unchecked(curr) == TRU_FOREGROUND {
                // SAFETY: curr 为有效像素偏移
                *img.get_unchecked_mut(curr) = TRU_VISITED;
            }

            // Jacob's 停止检查
            if curr == start && !is_first_move && dir == start_dir {
                return true;
            }

            curr = nb_off;
            search_idx = (dir + 6) & 7;
            break;
        }

        if is_first_move {
            if dir == -1 {
                // 单像素
                // SAFETY: curr 为有效像素偏移
                *img.get_unchecked_mut(curr) = TRU_VISITED_OUTER_RIGHT;
                break;
            }
            start_dir = dir;
            is_first_move = false;
        }
    }
    true

    } // unsafe
}

/// 外轮廓标记（对齐 traceExternalContourMock：只标 EAST，不收集点）。
///
/// # Safety
/// 同 `tru_trace_contour`。
unsafe fn tru_trace_external_mock(img: &mut [u8], stride: usize, r: i32, c: i32, row_start: i32) {
    // SAFETY: pad 哨兵圈 + 调用方契约保证所有偏移在 img 内
    unsafe {
    let stride_i = stride as isize;
    let mut offsets = [0isize; 16];
    for (i, off) in offsets.iter_mut().enumerate() {
        *off = tru_offset(i as i32, stride_i);
    }
    let mut curr_x = c;
    let mut curr_y = r;
    let mut start_dir: i32 = -1;
    let mut search_idx: i32 = 5;
    let mut curr = (r as usize) * stride + c as usize;
    let start = curr;
    let mut is_first_move = true;

    loop {
        let mut dir = -1;
        for n in 0..8 {
            let idx = search_idx + n;
            let nb_off = (curr as isize + offsets[idx as usize]) as usize;
            // SAFETY: pad 哨兵圈保证偏移在 img 内
            let neighbor = *img.get_unchecked(nb_off);
            if neighbor == TRU_BACKGROUND {
                continue;
            }
            dir = idx & 7;
            if ((search_idx <= 1) || (dir <= search_idx - 2)) && (curr_x != c && curr_y != r) {
                // SAFETY: curr 为有效像素偏移
                *img.get_unchecked_mut(curr) = TRU_VISITED_OUTER_RIGHT;
            }
            curr_y += TRU_DY[dir as usize];
            curr_x += TRU_DX[dir as usize];
            if curr_y < row_start {
                return;
            }
            #[allow(clippy::collapsible_if)]
            if curr == start {
                if !is_first_move && dir == start_dir {
                    return;
                }
            }
            curr = nb_off;
            search_idx = (dir + 6) & 7;
            break;
        }
        if is_first_move {
            if dir == -1 {
                break;
            }
            start_dir = dir;
            is_first_move = false;
        }
    }

    } // unsafe
}

/// 单条带处理（对齐 TRUCOntourTracer::operator()）。
///
/// # Safety
/// `img` 长度 = 行数 × stride；条带行区间 [row_start, row_end] 在
/// padded 图内（[1, ph-2]）；跟踪由 pad 哨兵圈保护。
unsafe fn tru_process_stripe(
    img: &mut [u8],
    stride: usize,
    row_start: i32,
    row_end: i32,
) -> TruAccumulator {
    // SAFETY: pad 哨兵圈 + 调用方契约保证所有偏移在 img 内
    unsafe {
    let mut acc = TruAccumulator::default();
    // 分块缓冲（对齐 TRUCOPagedContour）：热循环 push 仅指针比较 + 存储
    let mut buffer = PagedBuffer::new();
    let cols = stride;
    // 单趟扫描缓冲（复用，避免逐 run 分支预测抖动）
    let mut run_starts: Vec<u32> = Vec::with_capacity(512);
    let mut run_ends: Vec<u32> = Vec::with_capacity(512);

    for r in row_start..=row_end {
        let row_off = r as usize * stride;
        // 单趟扫描：一次 AVX512 块扫描找出所有 run 边界
        scan_row_runs(&img[row_off..row_off + cols], &mut run_starts, &mut run_ends);

        for i in 0..run_starts.len() {
            let c = run_starts[i] as i32;
            let c_end = run_ends[i] as i32;

            // 外轮廓（FOREGROUND 且非末行）
            if img[row_off + c as usize] == TRU_FOREGROUND && r < row_end {
                if tru_trace_contour(img, stride, r, c, row_start, true, &mut buffer) {
                    if buffer.len() > 1 && buffer.last() == buffer.first() {
                        buffer.pop();
                    }
                    if r == row_start {
                        acc.idx_external_firstline.push(acc.contours.len());
                    }
                    acc.contours.push(buffer.to_vec());
                }
            } else if img[row_off + c as usize] == TRU_FOREGROUND && r == row_end {
                // 末行：只标 EAST，让内轮廓提取正确
                tru_trace_external_mock(img, stride, r, c, row_start);
            }

            // 内轮廓（对齐 truco:356：row_ptr[c-1] > VISITED_OUTER_RIGHT 且非首行）
            #[allow(clippy::collapsible_if)]
            if img[row_off + c_end as usize - 1] > TRU_VISITED_OUTER_RIGHT && r > row_start {
                if tru_trace_contour(img, stride, r, c_end - 1, row_start, false, &mut buffer) {
                    if buffer.len() > 1 && buffer.last() == buffer.first() {
                        buffer.pop();
                    }
                    if r == row_end {
                        acc.idx_internal_lastline.push(acc.contours.len());
                    }
                    acc.contours.push(buffer.to_vec());
                }
            }
        }
    }
    acc

    } // unsafe
}

/// 并行处理所有条带（对齐 parallel_for_ + threadAccumulators）。
///
/// # Safety（调用方契约）
/// `img` 长度 = ph × pw；条带行区间互不重叠且在 [1, ph-2] 内。
/// TRU 跟踪的写操作只发生在本条带行区间内（外轮廓跟踪越出条带上界
/// 即返回 false，不写越界行），无数据竞争。
pub fn tru_process_stripes(
    img: &mut [u8],
    pw: usize,
    ph: usize,
    ranges: &[(i32, i32)],
) -> Vec<TruAccumulator> {
    use rayon::prelude::*;
    // 裸指针包 Copy wrapper 绕过 rayon 的 Send 检查（写区间不重叠，
    // 见函数级 SAFETY 注释）
    #[derive(Clone, Copy)]
    struct SyncPtr(*mut u8);
    // SAFETY: TRU 条带写区间互不重叠（见 tru_process_stripes 文档），
    // 裸指针跨线程共享无数据竞争
    unsafe impl Sync for SyncPtr {}
    // SAFETY: 同上
    unsafe impl Send for SyncPtr {}

    let ptr = SyncPtr(img.as_mut_ptr());
    ranges
        .par_iter()
        .map(move |&(rs, re)| {
            let ptr = ptr;
            // SAFETY: img 长度 = ph * pw（调用方契约）；条带行区间
            // [rs, re] 在 padded 图内，跟踪由 pad 哨兵圈保护
            let sub = unsafe { std::slice::from_raw_parts_mut(ptr.0, ph * pw) };
            // SAFETY: 同上
            unsafe { tru_process_stripe(sub, pw, rs, re) }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// TC89 链码近似（对齐 contours_approx.cpp，标量——OpenCV 也是标量）
// ---------------------------------------------------------------------------

const ABS_DIFF: [i8; 16] = [1, 2, 3, 4, 3, 2, 1, 0, 1, 2, 3, 4, 3, 2, 1, 0];
const CHAIN_CODE_DELTAS: [(i32, i32); 8] = [
    (1, 0), (1, -1), (0, -1), (-1, -1), (-1, 0), (-1, 1), (0, 1), (1, 1),
];

#[derive(Clone)]
struct ApproxItem {
    pt: Point,
    k: usize,
    s: i32,
    removed: bool,
}

/// approximateChainTC89（对齐 contours_approx.cpp:273）。
pub fn approximate_chain_tc89(
    chain: &[i8],
    origin: Point,
    method: crate::imgproc::contours::ContourApproxMethod,
) -> Vec<Point> {
    use crate::imgproc::contours::ContourApproxMethod;
    if chain.is_empty() {
        return vec![origin];
    }

    let is_approx = matches!(method, ContourApproxMethod::Tc89L1 | ContourApproxMethod::Tc89Kcos);

    let mut ares = pass_0(chain, origin, is_approx, method == ContourApproxMethod::None);

    if is_approx {
        // Pass 1：支撑域
        for i in 0..ares.len() {
            if ares[i].removed {
                continue;
            }
            ares[i].k = calc_support(&ares, i);
            if method == ContourApproxMethod::Tc89Kcos {
                ares[i].s = calc_cosine(&ares, i);
            }
        }

        // Pass 2：非极大值抑制
        for i in 0..ares.len() {
            if calc_nms_cleanup(&ares, i) {
                ares[i].s = 0;
                ares[i].removed = true;
            }
        }

        // Pass 3：支撑域 1 的非支配点
        for i in 0..ares.len() {
            if ares[i].removed {
                continue;
            }
            if ares[i].k == 1 && calc_dominance(&ares, i) {
                ares[i].s = 0;
                ares[i].removed = true;
            }
        }

        if method == ContourApproxMethod::Tc89L1 {
            // Pass 4：清理残留点对
            let mut skip = false;
            let mut new_start_idx = 0usize;
            let len = ares.len();
            #[allow(clippy::collapsible_if)]
            if ares[0].s != 0 && ares[len - 1].s != 0 {
                if !calc_new_start(&mut ares, &mut new_start_idx) {
                    skip = true;
                }
            }
            if !skip {
                pass_cleanup(&mut ares, new_start_idx);
            }
        }
    }

    ares.iter().filter(|a| !a.removed).map(|a| a.pt).collect()
}

/// Pass 0（对齐 contours_approx.cpp:34）。
fn pass_0(chain: &[i8], mut pt: Point, is_approx: bool, is_full: bool) -> Vec<ApproxItem> {
    let len = chain.len();
    let mut res = Vec::with_capacity(len / 2);
    for i in 0..len {
        let prev = if i == 0 { chain[len - 1] } else { chain[i - 1] };
        let cur = chain[i];
        let s = ABS_DIFF[(cur - prev + 7) as usize] as i32;
        if (!is_approx && (is_full || s != 0)) || is_approx {
            let mut item = ApproxItem { pt, k: 0, s, removed: false };
            if s == 0 {
                item.removed = true;
            }
            res.push(item);
        }
        let (dx, dy) = CHAIN_CODE_DELTAS[cur as usize];
        pt.x += dx;
        pt.y += dy;
    }
    res
}

/// calc_support（对齐 contours_approx.cpp:65）。
fn calc_support(ares: &[ApproxItem], i: usize) -> usize {
    let len = ares.len();
    let mut d_num: i32 = 0;
    let mut l: i32 = 0;
    let mut k = 1usize;
    loop {
        debug_assert!(k <= len);
        let i1 = if i >= k { i - k } else { len - k + i };
        let i2 = if i + k < len { i + k } else { i + k - len };

        let dx = ares[i2].pt.x - ares[i1].pt.x;
        let dy = ares[i2].pt.y - ares[i1].pt.y;
        let lk = dx * dx + dy * dy;
        let dk_num = (ares[i].pt.x - ares[i1].pt.x) * dy - (ares[i].pt.y - ares[i1].pt.y) * dx;

        // 对齐 union { int i; float f; } d; d.f = (float)(d_num*lk - dk_num*l)
        let d_f = ((d_num as f64) * (lk as f64) - (dk_num as f64) * (l as f64)) as f32;
        let d_i = d_f.to_bits() as i32;

        if k > 1 && (l >= lk || (d_num > 0 && d_i <= 0) || (d_num < 0 && d_i >= 0)) {
            break;
        }
        d_num = dk_num;
        l = lk;
        k += 1;
    }
    k - 1
}

/// calc_cosine（对齐 contours_approx.cpp:105）。
fn calc_cosine(ares: &[ApproxItem], i: usize) -> i32 {
    let k = ares[i].k;
    let len = ares.len();
    let mut s: i32 = 0;
    let mut j = k;
    while j > 0 {
        let i1 = if i >= j { i - j } else { len - j + i };
        let i2 = if i + j < len { i + j } else { i + j - len };

        let dx1 = ares[i1].pt.x - ares[i].pt.x;
        let dy1 = ares[i1].pt.y - ares[i].pt.y;
        let dx2 = ares[i2].pt.x - ares[i].pt.x;
        let dy2 = ares[i2].pt.y - ares[i].pt.y;

        if (dx1 | dy1) == 0 || (dx2 | dy2) == 0 {
            break;
        }

        let temp_num = (dx1 * dx2 + dy1 * dy2) as f64;
        let temp_num = (temp_num
            / (((dx1 * dx1 + dy1 * dy1) as f64) * ((dx2 * dx2 + dy2 * dy2) as f64)).sqrt())
            as f32;
        let sk_f = temp_num + 1.1f32;
        let sk = sk_f.to_bits() as i32;

        debug_assert!((0.0..=2.2).contains(&sk_f));
        if j < k && sk <= s {
            break;
        }
        s = sk;
        j -= 1;
    }
    s
}

/// calc_nms_cleanup（对齐 contours_approx.cpp:140）。
fn calc_nms_cleanup(ares: &[ApproxItem], i: usize) -> bool {
    let k2 = ares[i].k >> 1;
    let s = ares[i].s;
    let len = ares.len();
    let mut j = 1usize;
    while j <= k2 {
        let i1 = if i >= j { i - j } else { len - j + i };
        let i2 = if i + j < len { i + j } else { i + j - len };
        if ares[i1].s > s || ares[i2].s > s {
            break;
        }
        j += 1;
    }
    j <= k2
}

/// calc_dominance（对齐 contours_approx.cpp:156）。
fn calc_dominance(ares: &[ApproxItem], i: usize) -> bool {
    let len = ares.len();
    let i1 = if i >= 1 { i - 1 } else { len - 1 + i };
    let i2 = if i + 1 < len { i + 1 } else { i + 1 - len };
    ares[i].s <= ares[i1].s || ares[i].s <= ares[i2].s
}

fn get_next_idx(ares: &[ApproxItem], start: usize) -> usize {
    let len = ares.len();
    let mut res = start + 1;
    while res < len {
        if !ares[res].removed {
            break;
        }
        res += 1;
    }
    res
}

fn clear_until(ares: &mut [ApproxItem], start: usize, finish: usize) {
    let len = ares.len();
    let end = finish.min(len);
    let mut i = start + 1;
    while i < end {
        ares[i].removed = true;
        i += 1;
    }
}

/// calc_new_start（对齐 contours_approx.cpp:186）。
fn calc_new_start(ares: &mut Vec<ApproxItem>, res: &mut usize) -> bool {
    let len = ares.len();
    debug_assert!(len > 0);
    // 从头移除 s != 0 的项
    let mut i1 = 1usize;
    while i1 < len && ares[i1].s != 0 {
        ares[i1 - 1].s = 0;
        i1 += 1;
    }
    if i1 == len {
        return false;
    }
    i1 -= 1;

    // 从尾移除 s != 0 的项
    let mut i2 = len as isize - 2;
    while i2 > 0 && ares[i2 as usize].s != 0 {
        clear_until(ares, i2 as usize, len);
        ares[i2 as usize + 1].s = 0;
        i2 -= 1;
    }
    i2 += 1;

    if i1 == 0 && i2 as usize == len - 1 {
        // 只剩两点：从头找第一个未移除项，把首项追加到尾
        i1 = get_next_idx(ares, 0);
        let first = ares[0].clone();
        ares.push(first);
        let last = ares.len() - 1;
        ares[last].removed = false;
    }
    *res = i1;
    true
}

/// pass_cleanup（对齐 contours_approx.cpp:225）。
fn pass_cleanup(ares: &mut [ApproxItem], start_idx: usize) {
    let len = ares.len();
    let mut count = 1;
    let mut first = start_idx;
    let mut prev = start_idx;
    for i in start_idx..len {
        if ares[i].removed {
            continue;
        }
        let next_idx = get_next_idx(ares, i);
        if next_idx == len || next_idx - i != 1 {
            if count >= 2 {
                if count == 2 {
                    let s1 = ares[prev].s;
                    let s2 = ares[i].s;
                    if s1 > s2 || (s1 == s2 && ares[prev].k <= ares[i].k) {
                        ares[i].removed = true;
                    } else {
                        ares[prev].removed = true;
                    }
                } else {
                    first = get_next_idx(ares, first);
                    clear_until(ares, first, i);
                }
            }
            first = i;
            count = 1;
        } else {
            count += 1;
        }
        prev = i;
    }
}

// ---------------------------------------------------------------------------
// approxPolyDP（对齐 geometry/src/approx.cpp approxPolyDP_，标量）
// ---------------------------------------------------------------------------

/// Douglas-Peucker 多边形近似（对齐 cv::approxPolyDP，含最终直线清理）。
pub fn approx_poly_dp_impl(curve: &[Point], epsilon: f64, closed: bool) -> Vec<Point> {
    let count0 = curve.len();
    if count0 == 0 {
        return Vec::new();
    }

    let mut eps = epsilon * epsilon;
    let mut is_closed = closed;
    let mut init_iters = 3;

    // 切片栈（对齐 AutoBuffer<Range>）
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut dst: Vec<Point> = Vec::with_capacity(count0);

    if !is_closed {
        let start_pt = curve[count0 - 1];
        let end_pt = curve[0];
        if start_pt.x != end_pt.x || start_pt.y != end_pt.y {
            stack.push((0, count0 - 1));
        } else {
            is_closed = true;
            init_iters = 1;
        }
    }

    if is_closed {
        // 1. 找近似最远两点（对齐 init_iters 轮）
        let mut right_slice_start = 0usize;
        let mut pos = 0usize;
        let mut start_pt = Point::new(0, 0);
        let mut le_eps = false;
        for _ in 0..init_iters {
            let mut max_dist: f64 = 0.0;
            pos = (pos + right_slice_start) % count0;
            start_pt = curve[pos];
            for j in 1..count0 {
                // READ_PT 语义：先读后自增（绕回）
                pos = (pos + 1) % count0;
                let pt = curve[pos];
                let dx = (pt.x - start_pt.x) as f64;
                let dy = (pt.y - start_pt.y) as f64;
                let dist = dx * dx + dy * dy;
                if dist > max_dist {
                    max_dist = dist;
                    right_slice_start = j;
                }
            }
            // 内层读完 count-1 个点后 pos 绕回本轮起点（对齐 READ_PT 归零）
            pos = (pos + 1) % count0;
            le_eps = max_dist <= eps;
        }

        // 2. 初始化栈
        if !le_eps {
            // 对齐：right_slice.end = slice.start = pos % count；
            // slice.end = right_slice.start = (right_slice.start + slice.start) % count
            //（注意 right_slice.start 被重新赋值为后者）
            let slice_start = pos % count0;
            let new_right_start = (right_slice_start + slice_start) % count0;
            stack.push((new_right_start, slice_start));
            stack.push((slice_start, new_right_start));
        } else {
            dst.push(start_pt);
        }
    }

    // 3. 递归过程（栈循环）
    while let Some(slice) = stack.pop() {
        let end_pt = curve[slice.1];
        let mut pos = slice.0;
        let start_pt = curve[pos];
        pos = (pos + 1) % count0;

        let le_eps;
        if pos != slice.1 {
            let mut max_dist_2_mul_seg_len_2: f64 = 0.0;
            let dx = (end_pt.x - start_pt.x) as f64;
            let dy = (end_pt.y - start_pt.y) as f64;
            let segment_len_2 = dx * dx + dy * dy;
            // farthest 追踪（对齐 right_slice.start = (pos+count-1)%count，
            // pos 为 READ_PT 自增后的下标，即 farthest 点本身）
            let mut right_start = slice.0;

            while pos != slice.1 {
                let pt = curve[pos];
                let projection = (pt.x - start_pt.x) as f64 * dx + (pt.y - start_pt.y) as f64 * dy;
                let dist_2_mul;
                if projection < 0.0 {
                    let ex = (pt.x - start_pt.x) as f64;
                    let ey = (pt.y - start_pt.y) as f64;
                    dist_2_mul = (ex * ex + ey * ey) * segment_len_2;
                } else if projection > segment_len_2 {
                    let ex = (pt.x - end_pt.x) as f64;
                    let ey = (pt.y - end_pt.y) as f64;
                    dist_2_mul = (ex * ex + ey * ey) * segment_len_2;
                } else {
                    let dist = (pt.y - start_pt.y) as f64 * dx - (pt.x - start_pt.x) as f64 * dy;
                    dist_2_mul = dist * dist;
                }
                if dist_2_mul > max_dist_2_mul_seg_len_2 {
                    max_dist_2_mul_seg_len_2 = dist_2_mul;
                    // OpenCV 的 pos 在 READ_PT 后已自增，(pos-1) 即当前点；
                    // 本循环自增在尾部，故 right_start = pos（当前点下标）
                    right_start = pos;
                }
                pos = (pos + 1) % count0;
            }
            le_eps = max_dist_2_mul_seg_len_2 <= eps * segment_len_2;
            if !le_eps {
                stack.push((right_start, slice.1));
                stack.push((slice.0, right_start));
            }
        } else {
            le_eps = true;
        }

        if le_eps {
            dst.push(start_pt);
        }
    }

    if !is_closed {
        dst.push(curve[count0 - 1]);
    }

    // 最后阶段：直线段清理（对齐 approx.cpp:225-260）
    let mut new_count = dst.len();
    if new_count > 2 {
        let count = new_count;
        let mut pos = if is_closed { count - 1 } else { 0 };
        let mut start_pt = dst[pos];
        pos = (pos + 1) % count;
        let mut wpos = pos;
        let mut pt = dst[pos];

        let mut i = if is_closed { 0 } else { 1 };
        while i < count - (if is_closed { 0 } else { 1 }) && new_count > 2 {
            pos = (pos + 1) % count;
            let end_pt = dst[pos];

            let dx = (end_pt.x - start_pt.x) as f64;
            let dy = (end_pt.y - start_pt.y) as f64;
            let dist = ((pt.x - start_pt.x) as f64 * dy - (pt.y - start_pt.y) as f64 * dx).abs();
            let successive_inner = (pt.x - start_pt.x) as f64 * (end_pt.x - pt.x) as f64
                + (pt.y - start_pt.y) as f64 * (end_pt.y - pt.y) as f64;

            if dist * dist <= 0.5 * eps * (dx * dx + dy * dy)
                && dx != 0.0
                && dy != 0.0
                && successive_inner >= 0.0
            {
                new_count -= 1;
                dst[wpos] = end_pt;
                start_pt = end_pt;
                wpos = (wpos + 1) % count;
                pos = (pos + 1) % count;
                pt = dst[pos];
                i += 1;
                i += 1;
                continue;
            }
            dst[wpos] = pt;
            start_pt = pt;
            wpos = (wpos + 1) % count;
            pt = end_pt;
            i += 1;
        }

        if !is_closed {
            dst[wpos] = pt;
        }
        dst.truncate(new_count);
    }

    let _ = &mut eps;
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_start_end_roundtrip() {
        let row = [0u8, 0, 5, 5, 0, 7, 0, 0];
        assert_eq!(find_start_contour_point(&row, 0), 2);
        assert_eq!(find_start_contour_point(&row, 3), 3);
        assert_eq!(find_start_contour_point(&row, 5), 5);
        assert_eq!(find_start_contour_point(&row, 6), 8);
        assert_eq!(find_end_contour_point(&row, 2), 4);
        assert_eq!(find_end_contour_point(&row, 5), 6);
        assert_eq!(find_end_contour_point(&row, 0), 0);
    }

    #[test]
    fn find_next_diff_basic() {
        let row = [0i8, 0, 1, 1, 2, 2, 0, 1];
        let mut p = 0i8;
        assert_eq!(find_next_diff(&row, 1, 7, 0, &mut p), 2);
        assert_eq!(p, 1);
        assert_eq!(find_next_diff(&row, 2, 7, 1, &mut p), 4);
        assert_eq!(p, 2);
        assert_eq!(find_next_diff(&row, 4, 7, 2, &mut p), 6);
        assert_eq!(p, 0);
    }

    #[test]
    fn approx_poly_dp_collinear_dropped() {
        // 共线中间点应被移除
        let curve = vec![
            Point::new(0, 0), Point::new(2, 0), Point::new(4, 0),
            Point::new(4, 4), Point::new(2, 4), Point::new(0, 4),
        ];
        let out = approx_poly_dp_impl(&curve, 0.1, true);
        assert_eq!(out.len(), 4, "{out:?}");
    }
}

#[cfg(test)]
mod isa_cross_tests {
    use super::*;

    /// 三路径交叉验证：标量 vs AVX2 vs AVX512。
    #[test]
    fn find_start_end_cross_isa() {
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        for len in [1usize, 8, 16, 31, 32, 33, 64, 100, 257] {
            let row: Vec<u8> = (0..len).map(|i| ((i * 37) % 5 == 0) as u8 * 200).collect();
            for j in [0usize, 1, len / 2, len.saturating_sub(1)] {
                let s_scalar = scalar_find_start(&row, j);
                let e_scalar = scalar_find_end(&row, j);
                if has_avx2 {
                    let s = unsafe { crate::simd_kernels::x86::avx2::find_start_contour_point(&row, j) };
                    let e = unsafe { crate::simd_kernels::x86::avx2::find_end_contour_point(&row, j) };
                    assert_eq!(s, s_scalar, "avx2 start len={len} j={j}");
                    assert_eq!(e, e_scalar, "avx2 end len={len} j={j}");
                }
                if has_avx512 {
                    let s = unsafe { crate::simd_kernels::x86::avx512::find_start_contour_point(&row, j) };
                    let e = unsafe { crate::simd_kernels::x86::avx512::find_end_contour_point(&row, j) };
                    assert_eq!(s, s_scalar, "avx512 start len={len} j={j}");
                    assert_eq!(e, e_scalar, "avx512 end len={len} j={j}");
                }
            }
        }
    }

    #[test]
    fn find_next_diff_cross_isa() {
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        for len in [1usize, 8, 16, 31, 32, 33, 64, 100, 257] {
            let row: Vec<i8> = (0..len).map(|i| ((i * 37) % 5) as i8 - 2).collect();
            for prev in [-2i8, 0, 1] {
                for j in [0usize, 1, len / 2, len.saturating_sub(1)] {
                    let width = len;
                    let mut p_scalar = 0i8;
                    let mut p_simd = 0i8;
                    // 标量路径（含首元素检查）
                    let s_scalar = {
                        let mut k = j;
                        if k < width {
                            let v = row[k];
                            if v != prev {
                                p_scalar = v;
                                k = find_next_diff(&row, k, width, prev, &mut p_scalar);
                                // find_next_diff 已处理首元素
                            } else {
                                k = find_next_diff(&row, k, width, prev, &mut p_scalar);
                            }
                        }
                        k
                    };
                    if has_avx2 {
                        let r = unsafe {
                            crate::simd_kernels::x86::avx2::find_next_diff(&row, j, width, prev, &mut p_simd)
                        };
                        assert_eq!(r, s_scalar, "avx2 len={len} prev={prev} j={j}");
                        assert_eq!(p_simd, p_scalar);
                    }
                    if has_avx512 {
                        let r = unsafe {
                            crate::simd_kernels::x86::avx512::find_next_diff(&row, j, width, prev, &mut p_simd)
                        };
                        assert_eq!(r, s_scalar, "avx512 len={len} prev={prev} j={j}");
                        assert_eq!(p_simd, p_scalar);
                    }
                }
            }
        }
    }
}
