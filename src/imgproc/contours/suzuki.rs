//! Suzuki-Abe 边界跟踪扫描器，对齐 OpenCV 5 `contours_new.cpp` 的
//! `ContourScanner_`（RETR_EXTERNAL/LIST/CCOMP/TREE + hierarchy）。
//!
//! 数据布局对齐 OpenCV：
//! - 图像加 1 圈 0 边框，非零 → 1（schar）
//! - 像素标记：`MASK8_RIGHT`(0x80) 已访问右邻、`MASK8_NEW`(0x02) 新标记、
//!   低 7 位为 NBD 序号（`MASK8_LVAL` = 0x7F）
//! - 轮廓树：`Vec<Node>`，节点含 parent/first_child/prev/next/ctable_next

use crate::core::image::Image;
use crate::core::point::Point;

use super::{ContourApproxMethod, ContourRetrievalMode, Hierarchy};

const MASK8_RIGHT: i8 = -128; // 0x80
const MASK8_NEW: i8 = 0x02;
const MASK8_FLAGS: i8 = -2; // 0xFE
const MASK8_BLACK: i8 = 0x01;
const MASK8_LVAL: i8 = 0x7F;

/// 链码方向增量（对齐 chainCodeDeltas，0=E 顺时针）。
const CHAIN_CODE_DELTAS: [(i32, i32); 8] = [
    (1, 0), (1, -1), (0, -1), (-1, -1), (-1, 0), (-1, 1), (0, 1), (1, 1),
];

/// 轮廓树节点（对齐 TreeNode<Contour>）。
struct Node {
    parent: i32,
    first_child: i32,
    prev: i32,
    next: i32,
    ctable_next: i32,
    is_hole: bool,
    /// 轮廓点（CHAIN_APPROX_NONE/SIMPLE）。
    pts: Vec<Point>,
    /// 链码（TC89 跟踪阶段收集）。
    codes: Vec<i8>,
    is_chain: bool,
    /// 轮廓外接矩形（brect，padded 坐标，用于 findFirstBoundingContour）。
    brect: (i32, i32, i32, i32), // x, y, w, h
    /// 轮廓起点（padded 坐标，未加 offset）。
    origin: Point,
}

struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    fn new_elem(&mut self, is_hole: bool) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent: -1,
            first_child: -1,
            prev: -1,
            next: -1,
            ctable_next: -1,
            is_hole,
            pts: Vec::new(),
            codes: Vec::new(),
            is_chain: false,
            brect: (0, 0, 0, 0),
            origin: Point::new(0, 0),
        });
        idx
    }

    /// 对齐 addChild：头插子节点链表（后加的成为 first_child）。
    fn add_child(&mut self, parent: usize, child: usize) {
        if self.nodes[parent].first_child != -1 {
            let fc = self.nodes[parent].first_child as usize;
            self.nodes[fc].prev = child as i32;
            self.nodes[child].next = self.nodes[parent].first_child;
        }
        self.nodes[parent].first_child = child as i32;
        self.nodes[child].parent = parent as i32;
        self.nodes[child].prev = -1;
    }
}

/// 邻居偏移（对齐 getDelta：s → ±1 ±stride）。
#[inline(always)]
fn delta(s: i32, stride: isize) -> isize {
    let (dx, dy) = CHAIN_CODE_DELTAS[(s & 7) as usize];
    dx as isize + dy as isize * stride
}

/// icvFetchContourEx：跟踪一条轮廓并标记访问（对齐 contours_new.cpp:173）。
///
/// `start` 为 padded 坐标；`nbd` 为当前轮廓标记值；`is_direct` =
/// CHAIN_APPROX_NONE（每个边界点都记录）。
fn fetch_contour(
    img: &mut [i8],
    stride: usize,
    start: Point,
    nbd: i8,
    node: &mut Node,
    is_direct: bool,
) {
    let stride_i = stride as isize;
    let i0 = (start.y as isize * stride_i + start.x as isize) as usize;
    let is_hole = node.is_hole;
    let is_chain = node.is_chain;

    let mut pt = Point::new(node.origin.x - 1, node.origin.y - 1);
    // rect 用 (xmin, ymin, xmax, ymax) 累积，最后转 w/h
    let (mut rxmin, mut rymin, mut rxmax, mut rymax) = (pt.x, pt.y, pt.x, pt.y);

    let s_end: i32 = if is_hole { 0 } else { 4 };
    let mut s = s_end;

    // 找第一个非零邻居（从 s_end 逆时针）
    let mut i1 = 0usize;
    loop {
        s = (s - 1) & 7;
        let i1c = (i0 as isize + delta(s, stride_i)) as usize;
        if img[i1c] != 0 {
            i1 = i1c;
            break;
        }
        if s == s_end {
            break;
        }
    }

    if s == s_end {
        // 单像素域
        img[i0] = nbd | MASK8_RIGHT;
        if !is_chain {
            node.pts.push(pt);
        }
    } else {
        let mut i3 = i0;
        let mut prev_s = s ^ 4;

        loop {
            let s_end_cur = s;
            // 从 s+1 开始顺时针找非零邻居（clamp_direction + 递增到 15）
            let mut i4 = i3;
            let mut ss = s;
            while ss < 15 {
                ss += 1;
                let i4c = (i3 as isize + delta(ss, stride_i)) as usize;
                if img[i4c] != 0 {
                    i4 = i4c;
                    break;
                }
            }
            s = ss & 7;

            // 检查右边界标记（对齐 (unsigned)(s - 1) < (unsigned)s_end）
            if (s as u32).wrapping_sub(1) < s_end_cur as u32 {
                // schar 路径 setRightFlag：*elem = nbd | MASK8_RIGHT
                img[i3] = nbd | MASK8_RIGHT;
            } else if img[i3] == MASK8_BLACK {
                // schar 路径 setNewFlag：isVal = (*elem == 1)，与 i0 无关
                img[i3] = nbd;
            }

            if is_chain {
                node.codes.push(s as i8);
            } else if s != prev_s || is_direct {
                node.pts.push(pt);
            }

            if s != prev_s {
                if pt.x < rxmin {
                    rxmin = pt.x;
                } else if pt.x > rxmax {
                    rxmax = pt.x;
                }
                if pt.y < rymin {
                    rymin = pt.y;
                } else if pt.y > rymax {
                    rymax = pt.y;
                }
            }

            prev_s = s;
            let (dx, dy) = CHAIN_CODE_DELTAS[s as usize];
            pt.x += dx;
            pt.y += dy;

            if i4 == i0 && i3 == i1 {
                break;
            }
            i3 = i4;
            s = (s + 4) & 7;
        }
    }

    // brect 回 padded 坐标系（对齐 res.body.brect.x -= offset.x，offset=-1）
    node.brect = (rxmin + 1, rymin + 1, rxmax - rxmin + 1, rymax - rymin + 1);
}

/// icvTraceContour（对齐 contours_new.cpp:100）：从 origin 跟踪轮廓，
/// 判断 end 点是否属于该轮廓的边界（用于 findFirstBoundingContour）。
fn trace_contour(img: &[i8], stride: usize, start: Point, end: (i32, i32), is_hole: bool) -> bool {
    let stride_i = stride as isize;
    let stop = (end.1 as isize * stride_i + end.0 as isize) as usize;
    let i0 = (start.y as isize * stride_i + start.x as isize) as usize;
    let s_end: i32 = if is_hole { 0 } else { 4 };
    let mut s = s_end;

    // 找第一个非零邻居
    let mut i1 = 0usize;
    let mut has_neighbor = false;
    loop {
        s = (s - 1) & 7;
        let i1c = (i0 as isize + delta(s, stride_i)) as usize;
        if img[i1c] != 0 {
            i1 = i1c;
            has_neighbor = true;
            break;
        }
        if s == s_end {
            break;
        }
    }

    let mut i3 = i0;
    if has_neighbor {
        // follow border
        loop {
            let mut i4 = i3;
            let mut ss = s;
            while ss < 15 {
                ss += 1;
                let i4c = (i3 as isize + delta(ss, stride_i)) as usize;
                if img[i4c] != 0 {
                    i4 = i4c;
                    break;
                }
            }
            // 对齐 OpenCV：s 即 while 循环变量（找到的方向，可 >7），
            // 后续 (s+4)&7 与 stop 检查的 t=s 都用它
            s = ss;

            if i3 == stop {
                if img[i3] & MASK8_RIGHT == 0 {
                    // 唯一轮廓
                    return true;
                }
                // 检查是否为光栅扫描中遇到的最后一条轮廓
                let mut t = s;
                loop {
                    t = (t - 1) & 7;
                    let i5 = (i3 as isize + delta(t, stride_i)) as usize;
                    if img[i5] != 0 {
                        break;
                    }
                    if t == 0 {
                        return true;
                    }
                }
            }

            if i4 == i0 && i3 == i1 {
                break;
            }

            i3 = i4;
            s = (s + 4) & 7;
        }
        false
    } else {
        // 单像素域
        i3 == stop
    }
}

/// 扫描器状态（对齐 ContourScanner_）。
struct Scanner {
    /// padded 图像（(w+2) × (h+2)，schar 标记），行步长 = w+2。
    img: Vec<i8>,
    stride: usize,
    width: usize,  // padded 宽（w+2）
    height: usize, // padded 高（h+2）
    pt: (usize, usize), // 当前扫描位置（padded 坐标）
    lnbd: (i32, i32),   // 上一个遇到的轮廓位置
    nbd: i8,
    /// 跟踪阶段方法：None = CHAIN_CODE（收集链码），Some(m) = 直接点输出
    approx_method1: Option<ContourApproxMethod>,
    approx_method2: ContourApproxMethod,
    mode: ContourRetrievalMode,
    tree: Tree,
    /// ctable：NBD 值 → 最近使用该值的轮廓节点（对齐 ctable[128]）。
    ctable: [i32; 128],
}

impl Scanner {
    fn is_simple(&self) -> bool {
        matches!(self.mode, ContourRetrievalMode::External | ContourRetrievalMode::List)
    }

    /// makeContour（对齐 contours_new.cpp:372）。
    fn make_contour(&mut self, is_hole: bool, x: usize, y: usize) -> usize {
        // TC89：跟踪阶段收集链码（对齐 approx_method1 = CHAIN_CODE）
        let is_chain = self.approx_method1.is_none();
        let is_direct = self.approx_method1 == Some(ContourApproxMethod::None);

        let start_pt = Point::new(x as i32 - if is_hole { 1 } else { 0 }, y as i32);

        let idx = self.tree.new_elem(is_hole);
        self.tree.nodes[idx].is_chain = is_chain;
        self.tree.nodes[idx].origin = start_pt;

        if self.is_simple() {
            let mut node = std::mem::replace(&mut self.tree.nodes[idx], dummy_node());
            fetch_contour(&mut self.img, self.stride, start_pt, MASK8_NEW, &mut node, is_direct);
            self.tree.nodes[idx] = node;
        } else {
            // change nbd
            let lval = self.nbd;
            self.nbd = (self.nbd + 1) & MASK8_LVAL;
            if self.nbd == 0 {
                self.nbd = MASK8_BLACK | MASK8_NEW;
            }
            let mut node = std::mem::replace(&mut self.tree.nodes[idx], dummy_node());
            fetch_contour(&mut self.img, self.stride, start_pt, lval, &mut node, is_direct);
            self.tree.nodes[idx] = node;
            // ctable 登记（对齐 res.ctable_next = ctable[lval]; ctable[lval] = res.self()）
            let prev_head = self.ctable[lval as usize];
            self.tree.nodes[idx].ctable_next = prev_head;
            self.ctable[lval as usize] = idx as i32;
        }

        // TC89：链码 → 点（对齐 approximateChainTC89；prev_origin 为
        // origin + offset，offset 含 -1,-1，即原图坐标系）
        if self.approx_method1.is_none() {
            let node = &mut self.tree.nodes[idx];
            debug_assert!(node.is_chain);
            let codes = std::mem::take(&mut node.codes);
            let origin = Point::new(node.origin.x - 1, node.origin.y - 1);
            let method = self.approx_method2;
            node.pts = crate::simd_kernels::contours::approximate_chain_tc89(&codes, origin, method);
            node.is_chain = false;
        }

        idx
    }

    /// contourScan（对齐 contours_new.cpp:421）。
    fn contour_scan(&mut self, prev: i8, p: i8, last_pos: &mut (i32, i32), x: usize, y: usize) -> bool {
        let mut is_hole = false;

        // 外轮廓判定：prev == 0 && p == 1
        if !(prev == 0 && p == 1) {
            if p != 0 || prev < 1 {
                return false;
            }
            // 孔轮廓：p == 0 && prev >= 1
            if prev & MASK8_FLAGS != 0 {
                last_pos.0 = x as i32 - 1;
            }
            is_hole = true;
        }

        if self.mode == ContourRetrievalMode::External
            && (is_hole || self.img[y * self.stride + last_pos.0 as usize] > 0)
        {
            return false;
        }

        // 找父轮廓
        let mut main_parent: i32 = -1;
        if self.is_simple()
            || (!is_hole && matches!(self.mode, ContourRetrievalMode::CComp))
            || last_pos.0 <= 0
        {
            main_parent = 0;
        } else {
            let lval = self.img[last_pos.1 as usize * self.stride + last_pos.0 as usize] & MASK8_LVAL;
            main_parent = self.find_first_bounding_contour(*last_pos, y, lval, main_parent);

            // 孔-孔 / 外-外：父取上一条轮廓的父（对齐 contours_new.cpp:481-494）
            let mp = main_parent as usize;
            if self.tree.nodes[mp].is_hole == is_hole {
                if self.tree.nodes[mp].parent != -1 {
                    main_parent = self.tree.nodes[mp].parent;
                } else {
                    main_parent = 0;
                }
            }
            // 父的 isHole 必须与当前相反
            debug_assert!(self.tree.nodes[main_parent as usize].is_hole != is_hole);
        }

        last_pos.0 = x as i32 - if is_hole { 1 } else { 0 };

        let new_contour = self.make_contour(is_hole, x, y);
        if self.tree.nodes[new_contour].parent == -1 {
            self.tree.add_child(main_parent as usize, new_contour);
        }
        self.pt.0 = x + 1;
        self.pt.1 = y;
        true
    }

    /// findFirstBoundingContour（对齐 contours_new.cpp:517）。
    fn find_first_bounding_contour(&self, last_pos: (i32, i32), y: usize, lval: i8, par: i32) -> i32 {
        let end_point = (last_pos.0, y as i32);
        let mut res = par;
        let mut cur = self.ctable[lval as usize];
        while cur != -1 {
            let cur_us = cur as usize;
            let (bx, by, bw, bh) = self.tree.nodes[cur_us].brect;
            if (last_pos.0 - bx) < bw && (last_pos.1 - by) < bh {
                if res != -1 {
                    let origin = self.tree.nodes[res as usize].origin;
                    let is_hole = self.tree.nodes[res as usize].is_hole;
                    if trace_contour(&self.img, self.stride, origin, end_point, is_hole) {
                        break;
                    }
                }
                res = cur;
            }
            cur = self.tree.nodes[cur_us].ctable_next;
        }
        res
    }

    /// findNextX：行内跳到下一个与 prev 不同的像素（SIMD 加速）。
    fn find_next_x(&self, x: usize, y: usize, prev: i8, p: &mut i8) -> usize {
        let row = &self.img[y * self.stride..y * self.stride + self.width];
        crate::simd_kernels::contours::find_next_diff(row, x, self.width - 1, prev, p)
    }

    /// findNext（对齐 contours_new.cpp:592）。
    fn find_next(&mut self) -> bool {
        let mut x = self.pt.0;
        let mut y = self.pt.1;
        let width = self.width - 1;
        let height = self.height - 1;
        let mut last_pos = self.lnbd;
        let mut prev = self.img[y * self.stride + x - 1];

        while y < height {
            let mut p: i8 = 0;
            while x < width {
                x = self.find_next_x(x, y, prev, &mut p);
                if x >= width {
                    break;
                }
                if self.contour_scan(prev, p, &mut last_pos, x, y) {
                    self.lnbd = last_pos;
                    self.pt = (x + 1, y);
                    return true;
                } else {
                    prev = p;
                    if prev & MASK8_FLAGS != 0 {
                        last_pos.0 = x as i32;
                    }
                }
            }
            last_pos = (0, y as i32 + 1);
            x = 1;
            prev = 0;
            y += 1;
        }
        false
    }
}

fn dummy_node() -> Node {
    Node {
        parent: -1,
        first_child: -1,
        prev: -1,
        next: -1,
        ctable_next: -1,
        is_hole: false,
        pts: Vec::new(),
        codes: Vec::new(),
        is_chain: false,
        brect: (0, 0, 0, 0),
        origin: Point::new(0, 0),
    }
}

/// 入口：预处理（pad + 二值化）→ 扫描 → 树序列化。
pub fn find_contours_impl(
    src: &Image<u8, 1>,
    mode: ContourRetrievalMode,
    method: ContourApproxMethod,
    offset: Point,
) -> (Vec<Vec<Point>>, Vec<Hierarchy>) {
    let (w, h) = (src.width(), src.height());
    let pw = w + 2;
    let ph = h + 2;

    // pad 1 圈 0 + 非零 → 1（对齐 copyMakeBorder + threshold(0,1)）
    let mut img = vec![0i8; pw * ph];
    for y in 0..h {
        for x in 0..w {
            img[(y + 1) * pw + x + 1] = if src.data()[y * w + x] != 0 { 1 } else { 0 };
        }
    }

    let mut scanner = Scanner {
        img,
        stride: pw,
        width: pw,
        height: ph,
        pt: (1, 1),
        lnbd: (0, 1),
        nbd: 2,
        approx_method1: Some(method),
        approx_method2: method,
        mode,
        tree: Tree::new(),
        ctable: [-1; 128],
    };
    // TC89：跟踪阶段先收集链码（对齐 approx_method1 = CHAIN_CODE）
    if matches!(method, ContourApproxMethod::Tc89L1 | ContourApproxMethod::Tc89Kcos) {
        scanner.approx_method1 = None;
    }

    // 根节点（虚拟，isHole=true）
    let root = scanner.tree.new_elem(true);
    debug_assert_eq!(root, 0);

    while scanner.find_next() {}

    serialize(scanner.tree, offset)
}

/// 树序列化（对齐 contourTreeToResults：TreeIterator 先序遍历，跳过根）。
fn serialize(tree: Tree, offset: Point) -> (Vec<Vec<Point>>, Vec<Hierarchy>) {
    let tree = &tree;
    let mut contours: Vec<Vec<Point>> = Vec::new();
    let mut hierarchy: Vec<Hierarchy> = Vec::new();

    let empty_root = tree.nodes[0].pts.is_empty()
        && tree.nodes[0].codes.is_empty()
        && tree.nodes[0].first_child == -1;
    if tree.nodes.is_empty() || empty_root {
        return (contours, hierarchy);
    }

    let mut index_mapping = vec![-1i32; tree.nodes.len() + 1];

    // 先序遍历（对齐 TreeIterator：弹栈 → 子链表尾到头压栈 → 弹出即 next 正序）
    let mut order: Vec<usize> = Vec::new();
    let mut stack = vec![0usize];
    while let Some(idx) = stack.pop() {
        order.push(idx);
        let mut child = tree.nodes[idx].first_child;
        let mut children = Vec::new();
        while child != -1 {
            children.push(child as usize);
            child = tree.nodes[child as usize].next;
        }
        for &c in children.iter().rev() {
            stack.push(c);
        }
    }

    for &idx in &order {
        if idx == 0 {
            continue;
        }
        index_mapping[idx + 1] = (contours.len()) as i32;
        let node = &tree.nodes[idx];
        let mut pts = node.pts.clone();
        if offset.x != 0 || offset.y != 0 {
            for p in pts.iter_mut() {
                p.x += offset.x;
                p.y += offset.y;
            }
        }
        contours.push(pts);
        hierarchy.push(Hierarchy::none());
    }

    for &idx in &order {
        if idx == 0 {
            continue;
        }
        let node = &tree.nodes[idx];
        let hi = index_mapping[idx + 1] as usize;
        hierarchy[hi].next = index_mapping[(node.next + 1) as usize];
        hierarchy[hi].prev = index_mapping[(node.prev + 1) as usize];
        hierarchy[hi].first_child = index_mapping[(node.first_child + 1) as usize];
        hierarchy[hi].parent = index_mapping[(node.parent + 1) as usize];
    }

    (contours, hierarchy)
}
