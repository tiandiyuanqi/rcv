//! 轮廓几何分析（对应 OpenCV `modules/geometry`）。
//!
//! - `arc_length`：对齐 geometry.cpp:471（f32 累加路径长）
//! - `convex_hull`：对齐 convhull.cpp:139（Sklansky 扫描，指针排序含稳定序）
//! - `is_contour_convex`：对齐 convhull.cpp:444（方向位累积）
//! - `min_area_rect`：对齐 rotcalipers.cpp:361（rotating calipers）
//! - `min_enclosing_circle`：对齐 shapedescr.cpp:133（Welzl + cv::RNG 复刻）

use crate::core::point::Point;
use crate::imgproc::contours::contour_area;

/// 旋转矩形，对应 `cv::RotatedRect`（angle 单位度，OpenCV 约定 [-90, 0)）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RotatedRect {
    pub center: (f32, f32),
    pub size: (f32, f32),
    pub angle: f32,
}

/// 计算轮廓周长（对应 `cv::arcLength`）。
///
/// `is_closed=true` 时首尾相连。点转 f32 后累加欧氏距离（对齐 OpenCV 的
/// float 中间精度）。空/单点返回 0。
pub fn arc_length(curve: &[Point], is_closed: bool) -> f64 {
    let count = curve.len();
    if count <= 1 {
        return 0.0;
    }
    let last = if is_closed { count - 1 } else { 0 };
    let mut prev = (curve[last].x as f32, curve[last].y as f32);
    let mut perimeter = 0f64;
    for p in curve {
        let cur = (p.x as f32, p.y as f32);
        let dx = cur.0 - prev.0;
        let dy = cur.1 - prev.1;
        perimeter += (dx * dx + dy * dy).sqrt() as f64;
        prev = cur;
    }
    perimeter
}

/// Sklansky 扫描（对齐 convhull.cpp:47 Sklansky_）。
///
/// `array` 为按 x 排序的指针数组；`nsign`/`sign2` 控制方向。
/// 返回栈中元素数（栈内容为指向 array 的下标）。
fn sklansky(array: &[&Point], start: usize, end: usize, stack: &mut Vec<usize>, nsign: i32, sign2: i32) -> usize {
    let incr: i32 = if end > start { 1 } else { -1 };
    let mut pprev = start as i32;
    let mut pcur = pprev + incr;
    let mut pnext = pcur + incr;
    let mut stacksize = 3usize;

    let end_i = end as i32;
    if start == end
        || (array[start].x == array[end].x && array[start].y == array[end].y)
    {
        stack.push(start);
        return 1;
    }

    stack.push(pprev as usize);
    stack.push(pcur as usize);
    stack.push(pnext as usize);

    let end_after = end_i + incr; // afterend

    while pnext != end_after {
        // check the angle p1,p2,p3
        let cury = array[pcur as usize].y;
        let nexty = array[pnext as usize].y;
        let by = nexty - cury;

        if sign_of(by) != nsign {
            let a = (
                array[pcur as usize].x - array[pprev as usize].x,
                cury - array[pprev as usize].y,
            );
            let b = (array[pnext as usize].x - array[pcur as usize].x, by);
            // 整数路径无 normalize（对齐 is_float=false 分支）
            let convexity = a.1 as i64 * b.0 as i64 - a.0 as i64 * b.1 as i64;

            if sign_of64(convexity) == sign2 && (a.0 != 0 || a.1 != 0) {
                pprev = pcur;
                pcur = pnext;
                pnext += incr;
                stack.push(pnext as usize);
                stacksize += 1;
            } else if pprev as usize == start {
                pcur = pnext;
                stack[1] = pcur as usize;
                pnext += incr;
                stack[2] = pnext as usize;
            } else {
                stack[stacksize - 2] = pnext as usize;
                pcur = pprev;
                pprev = stack[stacksize - 4] as i32;
                stacksize -= 1;
                stack.truncate(stacksize);
            }
        } else {
            pnext += incr;
            stack[stacksize - 1] = pnext as usize;
        }
    }

    stacksize - 1
}

#[inline]
fn sign_of(v: i32) -> i32 {
    (v > 0) as i32 - (v < 0) as i32
}

#[inline]
fn sign_of64(v: i64) -> i32 {
    (v > 0) as i32 - (v < 0) as i32
}

/// 计算凸包（对应 `cv::convexHull`，整数点输入）。
///
/// 返回**输入下标**（对齐 `returnPoints=false`）。`clockwise=false` 为
/// OpenCV 默认逆时针。空输入返回空。
pub fn convex_hull(points: &[Point], clockwise: bool) -> Vec<usize> {
    let total = points.len();
    if total == 0 {
        return Vec::new();
    }

    // 指针排序：x 升序，x 相同 y 升序，再按指针序（稳定）——
    // Rust 用 (x, y, 原下标) 元组排序等价（原数组顺序即指针序）
    let mut pointer: Vec<usize> = (0..total).collect();
    pointer.sort_by(|&a, &b| {
        let (pa, pb) = (&points[a], &points[b]);
        pa.x.cmp(&pb.x).then(pa.y.cmp(&pb.y)).then(a.cmp(&b))
    });

    let mut miny_ind = 0usize;
    let mut maxy_ind = 0usize;
    for i in 1..total {
        let y = points[pointer[i]].y;
        if points[pointer[miny_ind]].y > y {
            miny_ind = i;
        }
        if points[pointer[maxy_ind]].y < y {
            maxy_ind = i;
        }
    }

    let mut hullbuf: Vec<usize> = Vec::with_capacity(total);
    let p = |i: usize| &points[pointer[i]];

    if p(0).x == p(total - 1).x && p(0).y == p(total - 1).y {
        hullbuf.push(0);
    } else {
        // upper half
        let mut tl_stack: Vec<usize> = Vec::new();
        let tl_count = sklansky(&pointer.iter().map(|&i| &points[i]).collect::<Vec<_>>(), 0, maxy_ind, &mut tl_stack, -1, 1);
        let mut tr_stack: Vec<usize> = Vec::new();
        let tr_count = sklansky(&pointer.iter().map(|&i| &points[i]).collect::<Vec<_>>(), total - 1, maxy_ind, &mut tr_stack, -1, -1);

        let (tl_stack, tl_count, tr_stack, tr_count) = if !clockwise {
            (&tr_stack, tr_count, &tl_stack, tl_count)
        } else {
            (&tl_stack, tl_count, &tr_stack, tr_count)
        };

        for i in 0..tl_count.saturating_sub(1) {
            hullbuf.push(tl_stack[i]);
        }
        let mut i = tr_count as i64 - 1;
        while i > 0 {
            hullbuf.push(tr_stack[i as usize]);
            i -= 1;
        }
        let stop_idx = if tr_count > 2 {
            tr_stack[1]
        } else if tl_count > 2 {
            tl_stack[tl_count - 2]
        } else {
            usize::MAX
        };

        // lower half
        let pts_ref: Vec<&Point> = pointer.iter().map(|&i| &points[i]).collect();
        let mut bl_stack: Vec<usize> = Vec::new();
        let bl_count = sklansky(&pts_ref, 0, miny_ind, &mut bl_stack, 1, -1);
        let mut br_stack: Vec<usize> = Vec::new();
        let br_count = sklansky(&pts_ref, total - 1, miny_ind, &mut br_stack, 1, 1);

        let (bl_stack, bl_count, br_stack, br_count) = if clockwise {
            (&br_stack, br_count, &bl_stack, bl_count)
        } else {
            (&bl_stack, bl_count, &br_stack, br_count)
        };

        if stop_idx != usize::MAX {
            let check_idx = if bl_count > 2 {
                bl_stack[1]
            } else if bl_count + br_count > 2 {
                br_stack[2 - bl_count]
            } else {
                usize::MAX
            };
            if check_idx == stop_idx
                || (check_idx != usize::MAX
                    && p(check_idx).x == p(stop_idx).x
                    && p(check_idx).y == p(stop_idx).y)
            {
                // all points on the same line: bottom part mirrors top
                let bl_count = bl_count.min(2);
                let br_count = br_count.min(2);
                for i in 0..bl_count.saturating_sub(1) {
                    hullbuf.push(bl_stack[i]);
                }
                let mut i = br_count as i64 - 1;
                while i > 0 {
                    hullbuf.push(br_stack[i as usize]);
                    i -= 1;
                }
                return hullbuf.iter().map(|&i| pointer[i]).collect();
            }
        }

        for i in 0..bl_count.saturating_sub(1) {
            hullbuf.push(bl_stack[i]);
        }
        let mut i = br_count as i64 - 1;
        while i > 0 {
            hullbuf.push(br_stack[i as usize]);
            i -= 1;
        }
    }

    // 位置 → 原下标
    let mut hullbuf: Vec<i32> = hullbuf.iter().map(|&i| pointer[i] as i32).collect();

    // 循环移位规范化（对齐 convhull.cpp:296-335）：把下标序列旋转成
    // 升序或降序（min_idx/max_idx 相邻且单调时）
    let nout = hullbuf.len();
    if nout >= 3 {
        let mut min_idx = 0usize;
        let mut max_idx = 0usize;
        let mut lt = 0i32;
        let mut break_early = false;
        for i in 1..nout {
            let idx = hullbuf[i];
            lt += (hullbuf[i - 1] < idx) as i32;
            if lt > 1 && lt <= (i as i32) - 2 {
                break_early = true;
                break;
            }
            if idx < hullbuf[min_idx] {
                min_idx = i;
            }
            if idx > hullbuf[max_idx] {
                max_idx = i;
            }
        }
        if !break_early {
            let mmdist = (max_idx as i32 - min_idx as i32).abs() as usize;
            if (mmdist == 1 || mmdist == nout - 1) && (lt <= 1 || lt >= nout as i32 - 2) {
                let ascending = (max_idx + 1) % nout == min_idx;
                let i0 = if ascending { min_idx } else { max_idx };
                let mut j = i0;
                if i0 > 0 {
                    let mut rotated = Vec::with_capacity(nout);
                    let mut ok = true;
                    for i in 0..nout {
                        let curr_idx = hullbuf[j];
                        let next_j = if j + 1 < nout { j + 1 } else { 0 };
                        let next_idx = hullbuf[next_j];
                        if i < nout - 1 && (ascending != (curr_idx < next_idx)) {
                            ok = false;
                            break;
                        }
                        rotated.push(curr_idx);
                        j = next_j;
                    }
                    if ok && rotated.len() == nout {
                        hullbuf = rotated;
                    }
                }
            }
        }
    }

    hullbuf.iter().map(|&i| i as usize).collect()
}

/// 判断轮廓是否凸（对应 `cv::isContourConvex`）。
///
/// 方向位累积：出现混合方向（orientation==3）即非凸。空输入返回 false。
pub fn is_contour_convex(contour: &[Point]) -> bool {
    let n = contour.len();
    if n == 0 {
        return false;
    }
    let mut prev_pt = contour[(n - 2 + n) % n];
    let mut cur_pt = contour[n - 1];

    let mut dx0 = cur_pt.x - prev_pt.x;
    let mut dy0 = cur_pt.y - prev_pt.y;
    let mut orientation = 0u32;

    for p in contour {
        prev_pt = cur_pt;
        cur_pt = *p;

        let dx = cur_pt.x - prev_pt.x;
        let dy = cur_pt.y - prev_pt.y;
        let dxdy0 = dx as i64 * dy0 as i64;
        let dydx0 = dy as i64 * dx0 as i64;

        orientation |= if dydx0 > dxdy0 {
            1
        } else if dydx0 < dxdy0 {
            2
        } else {
            3
        };
        if orientation == 3 {
            return false;
        }

        dx0 = dx;
        dy0 = dy;
    }

    true
}

/// rotating calipers 内部（对齐 rotcalipers.cpp:119，仅 MINAREARECT 模式）。
fn rotating_calipers(points: &[(f32, f32)], n: usize, orientation: f32, out: &mut [f32; 6]) {
    let mut minarea = f32::MAX;
    let mut inv_vect_length = vec![0f32; n];
    let mut vect = vec![(0f32, 0f32); n];
    let mut left = 0usize;
    let mut bottom = 0usize;
    let mut right = 0usize;
    let mut top = 0usize;
    let mut seq = [0usize; 4];

    let mut base_a;
    let mut base_b = 0f32;

    let mut pt0 = points[0];
    let (mut left_x, mut right_x) = (pt0.0, pt0.0);
    let (mut top_y, mut bottom_y) = (pt0.1, pt0.1);

    for i in 0..n {
        if pt0.0 < left_x {
            left_x = pt0.0;
            left = i;
        }
        if pt0.0 > right_x {
            right_x = pt0.0;
            right = i;
        }
        if pt0.1 > top_y {
            top_y = pt0.1;
            top = i;
        }
        if pt0.1 < bottom_y {
            bottom_y = pt0.1;
            bottom = i;
        }

        let pt = points[(i + 1) & if i + 1 < n { !0 } else { 0 }];
        let dx = pt.0 - pt0.0;
        let dy = pt.1 - pt0.1;
        vect[i] = (dx, dy);
        inv_vect_length[i] = (1.0f64 / ((dx * dx + dy * dy) as f64).sqrt()) as f32;
        pt0 = pt;
    }

    // find convex hull orientation
    let mut orientation = orientation;
    if orientation == 0.0 {
        let mut ax = vect[n - 1].0 as f64;
        let mut ay = vect[n - 1].1 as f64;
        for i in 0..n {
            let bx = vect[i].0 as f64;
            let by = vect[i].1 as f64;
            let convexity = ax * by - ay * bx;
            if convexity != 0.0 {
                orientation = if convexity > 0.0 { 1.0 } else { -1.0 };
                break;
            }
            ax = bx;
            ay = by;
        }
    }
    base_a = orientation;

    seq[0] = bottom;
    seq[1] = right;
    seq[2] = top;
    seq[3] = left;

    let mut buffer = [0f32; 8]; // [0]=leftist idx(int bits), 1=base_a, 2=width, 3=base_b, 4=height, 5=bottom idx(int bits), 6=area

    for _k in 0..n {
        let mut main_element = 0usize;

        let rot = |v: (f32, f32)| v;
        let rot_vect = [
            vect[seq[0]],
            (vect[seq[1]].1, -vect[seq[1]].0),          // rotate90CW
            (-vect[seq[2]].0, -vect[seq[2]].1),         // rotate180
            (-vect[seq[3]].1, vect[seq[3]].0),          // rotate90CCW
        ];
        let _ = rot;
        for i in 1..4 {
            // firstVecIsRight: rotate90CW(vec1)·vec2 < 0
            let tmp = (rot_vect[i].1, -rot_vect[i].0);
            if tmp.0 * rot_vect[main_element].0 + tmp.1 * rot_vect[main_element].1 < 0.0 {
                main_element = i;
            }
        }

        // rotate calipers
        {
            let pindex = seq[main_element];
            let lead_x = vect[pindex].0 * inv_vect_length[pindex];
            let lead_y = vect[pindex].1 * inv_vect_length[pindex];
            match main_element {
                0 => {
                    base_a = lead_x;
                    base_b = lead_y;
                }
                1 => {
                    base_a = lead_y;
                    base_b = -lead_x;
                }
                2 => {
                    base_a = -lead_x;
                    base_b = -lead_y;
                }
                _ => {
                    base_a = -lead_y;
                    base_b = lead_x;
                }
            }
        }
        // change base point of main edge
        seq[main_element] += 1;
        if seq[main_element] == n {
            seq[main_element] = 0;
        }

        // CALIPERS_MINAREARECT
        {
            let dx = points[seq[1]].0 - points[seq[3]].0;
            let dy = points[seq[1]].1 - points[seq[3]].1;
            let width = dx * base_a + dy * base_b;

            let dx = points[seq[2]].0 - points[seq[0]].0;
            let dy = points[seq[2]].1 - points[seq[0]].1;
            let height = -dx * base_b + dy * base_a;

            let area = width * height;
            if area <= minarea {
                minarea = area;
                buffer[0] = seq[3] as i32 as f32; // leftist point（int 位模式存 float 槽）
                buffer[1] = base_a;
                buffer[2] = width;
                buffer[3] = base_b;
                buffer[4] = height;
                buffer[5] = seq[0] as i32 as f32; // bottom point
                buffer[6] = area;
            }
        }
    }

    // 输出（对齐 CALIPERS_MINAREARECT 尾段）
    let a1 = buffer[1];
    let b1 = buffer[3];
    let a2 = -buffer[3];
    let b2 = buffer[1];

    let left_idx = buffer[0] as i32 as usize;
    let bottom_idx = buffer[5] as i32 as usize;

    let c1 = a1 * points[left_idx].0 + points[left_idx].1 * b1;
    let c2 = a2 * points[bottom_idx].0 + points[bottom_idx].1 * b2;

    let idet = 1.0f32 / (a1 * b2 - a2 * b1);

    let px = (c1 * b2 - c2 * b1) * idet;
    let py = (a1 * c2 - a2 * c1) * idet;

    out[0] = px;
    out[1] = py;
    out[2] = a1 * buffer[2];
    out[3] = b1 * buffer[2];
    out[4] = a2 * buffer[4];
    out[5] = b2 * buffer[4];
}

/// 最小面积外接旋转矩形（对应 `cv::minAreaRect`）。
///
/// 返回 [`RotatedRect`]（angle ∈ [-90, 0)，OpenCV 约定）。
/// 空输入返回 None；单点 size=(0,0)。
pub fn min_area_rect(points: &[Point]) -> Option<RotatedRect> {
    if points.is_empty() {
        return None;
    }

    // convexHull(clockwise=false, returnPoints=true) → f32
    let hull_idx = convex_hull(points, false);
    let hull: Vec<(f32, f32)> = hull_idx
        .iter()
        .map(|&i| (points[i].x as f32, points[i].y as f32))
        .collect();

    let n = hull.len();
    let mut box_ = RotatedRect {
        center: (0.0, 0.0),
        size: (0.0, 0.0),
        angle: 0.0,
    };
    let mut angle = -std::f64::consts::FRAC_PI_2;

    if n > 2 {
        let mut out = [0f32; 6];
        rotating_calipers(&hull, n, 1.0, &mut out);
        box_.center.0 = out[0] + (out[2] + out[4]) * 0.5;
        box_.center.1 = out[1] + (out[3] + out[5]) * 0.5;
        box_.size.0 = (out[4] as f64 * out[4] as f64 + out[5] as f64 * out[5] as f64).sqrt() as f32;
        box_.size.1 = (out[2] as f64 * out[2] as f64 + out[3] as f64 * out[3] as f64).sqrt() as f32;
        if out[2] == 0.0 && out[3] > 0.0 {
            std::mem::swap(&mut box_.size.0, &mut box_.size.1);
        } else {
            angle = -((out[2] as f64).atan2(out[3] as f64));
        }
    } else if n == 2 {
        box_.center.0 = (hull[0].0 + hull[1].0) * 0.5;
        box_.center.1 = (hull[0].1 + hull[1].1) * 0.5;
        let dx = (hull[0].0 - hull[1].0) as f64;
        let dy = (hull[0].1 - hull[1].1) as f64;
        box_.size.0 = 0.0;
        box_.size.1 = (dx * dx + dy * dy).sqrt() as f32;
        if dx == 0.0 {
            std::mem::swap(&mut box_.size.0, &mut box_.size.1);
        } else if dy < 0.0 {
            angle = dy.atan2(dx);
            std::mem::swap(&mut box_.size.0, &mut box_.size.1);
        } else if dy > 0.0 {
            angle = -(dx).atan2(dy);
        }
    } else {
        // n == 1
        box_.center = hull[0];
    }

    box_.angle = (angle * 180.0 / std::f64::consts::PI) as f32;
    Some(box_)
}

/// 旋转矩形四角点（对应 `cv::boxPoints`）。
pub fn box_points(box_: RotatedRect) -> [(f32, f32); 4] {
    let (cx, cy) = box_.center;
    let (w, h) = box_.size;
    let angle = box_.angle as f64 * std::f64::consts::PI / 180.0;
    let b = (angle.cos() as f32) * 0.5f32;
    let a = (angle.sin() as f32) * 0.5f32;

    [
        (cx - a * h - b * w, cy + b * h - a * w),
        (cx + a * h - b * w, cy - b * h - a * w),
        (cx - a * h + b * w, cy - b * h + a * w),
        (cx + a * h + b * w, cy + b * h + a * w),
    ]
}

/// cv::RNG 复刻（MWC：state = lo*4164903690 + hi）。
struct CvRng {
    state: u64,
}

impl CvRng {
    fn new(state: u64) -> Self {
        Self { state }
    }

    fn next(&mut self) -> u32 {
        self.state = (self.state as u32 as u64)
            .wrapping_mul(4164903690)
            .wrapping_add((self.state >> 32) as u32 as u64);
        self.state as u32
    }

    /// uniform(int a, int b)：a==b ? a : next() % (b-a) + a
    fn uniform(&mut self, a: i32, b: i32) -> i32 {
        if a == b {
            a
        } else {
            (self.next() % (b - a) as u32) as i32 + a
        }
    }
}

const MEC_EPS: f32 = 1.0e-4;

fn find_circle_3pts(pts: &[(f32, f32); 3], center: &mut (f32, f32), radius: &mut f32) {
    let v1 = (pts[1].0 - pts[0].0, pts[1].1 - pts[0].1);
    let v2 = (pts[2].0 - pts[0].0, pts[2].1 - pts[0].1);

    let mid1 = ((pts[0].0 + pts[1].0) / 2.0, (pts[0].1 + pts[1].1) / 2.0);
    let c1 = mid1.0 * v1.0 + mid1.1 * v1.1;
    let mid2 = ((pts[0].0 + pts[2].0) / 2.0, (pts[0].1 + pts[2].1) / 2.0);
    let c2 = mid2.0 * v2.0 + mid2.1 * v2.1;
    let det = v1.0 * v2.1 - v1.1 * v2.0;
    if det.abs() <= MEC_EPS {
        return;
    }
    let cx = (c1 * v2.1 - c2 * v1.1) / det;
    let cy = (v1.0 * c2 - v2.0 * c1) / det;
    center.0 = cx;
    center.1 = cy;
    let cx = cx - pts[0].0;
    let cy = cy - pts[0].1;
    *radius = (cx * cx + cy * cy).sqrt() + MEC_EPS;
}

fn find_third_point(pts: &[(f32, f32)], i: usize, j: usize, center: &mut (f32, f32), radius: &mut f32) {
    center.0 = (pts[j].0 + pts[i].0) / 2.0;
    center.1 = (pts[j].1 + pts[i].1) / 2.0;
    let dx = pts[j].0 - pts[i].0;
    let dy = pts[j].1 - pts[i].1;
    *radius = (dx * dx + dy * dy).sqrt() / 2.0 + MEC_EPS;

    for k in 0..j {
        let dx = center.0 - pts[k].0;
        let dy = center.1 - pts[k].1;
        if (dx * dx + dy * dy).sqrt() < *radius {
            continue;
        }
        let ptsf = [pts[i], pts[j], pts[k]];
        find_circle_3pts(&ptsf, center, radius);
    }
}

fn find_second_point(pts: &[(f32, f32)], i: usize, center: &mut (f32, f32), radius: &mut f32) {
    center.0 = (pts[0].0 + pts[i].0) / 2.0;
    center.1 = (pts[0].1 + pts[i].1) / 2.0;
    let dx = pts[0].0 - pts[i].0;
    let dy = pts[0].1 - pts[i].1;
    *radius = (dx * dx + dy * dy).sqrt() / 2.0 + MEC_EPS;

    for j in 1..i {
        let dx = center.0 - pts[j].0;
        let dy = center.1 - pts[j].1;
        if (dx * dx + dy * dy).sqrt() < *radius {
            continue;
        }
        find_third_point(pts, i, j, center, radius);
    }
}

/// 最小外接圆（对应 `cv::minEnclosingCircle`，Welzl + RNG 洗牌复刻）。
///
/// 返回 `(center, radius)`；空输入 `(0,0), 0`；单点 radius=EPS。
pub fn min_enclosing_circle(points: &[Point]) -> ((f32, f32), f32) {
    let count = points.len();
    if count == 0 {
        return ((0.0, 0.0), 0.0);
    }

    let mut pts: Vec<(f32, f32)> = points
        .iter()
        .map(|p| (p.x as f32, p.y as f32))
        .collect();

    if count == 1 {
        return (pts[0], MEC_EPS);
    }

    // 洗牌（对齐 shapedescr.cpp:143-160：seed 由 count/首尾点构造）
    if count > 10 {
        let x0 = pts[0].0.to_bits();
        let y0 = pts[0].1.to_bits();
        let xn = pts[count - 1].0.to_bits();
        let yn = pts[count - 1].1.to_bits();
        let seed = (count as u32) ^ x0 ^ (y0 << 8) ^ (xn << 16) ^ (yn << 24);
        let mut rng = CvRng::new(seed as u64);
        for i in 1..count {
            let j = rng.uniform(0, (i + 1) as i32) as usize;
            pts.swap(i, j);
        }
    }

    let mut center = ((pts[0].0 + pts[1].0) / 2.0, (pts[0].1 + pts[1].1) / 2.0);
    let dx = pts[0].0 - pts[1].0;
    let dy = pts[0].1 - pts[1].1;
    let mut radius = (dx * dx + dy * dy).sqrt() / 2.0 + MEC_EPS;

    for i in 2..count {
        let dx = pts[i].0 - center.0;
        let dy = pts[i].1 - center.1;
        let d = (dx * dx + dy * dy).sqrt();
        if d < radius {
            continue;
        }
        find_second_point(&pts, i, &mut center, &mut radius);
    }

    (center, radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_length_open_and_closed() {
        let line = [Point::new(0, 0), Point::new(3, 0), Point::new(3, 4)];
        assert!((arc_length(&line, false) - 7.0).abs() < 1e-6);
        // 闭合：加回 (0,0)→(3,4) 的 5
        assert!((arc_length(&line, true) - 12.0).abs() < 1e-6);
        assert_eq!(arc_length(&[], false), 0.0);
        assert_eq!(arc_length(&[Point::new(1, 1)], true), 0.0);
    }

    #[test]
    fn convex_hull_square() {
        // 5×5 方块边界点 + 内部点
        let mut pts = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 4),
            Point::new(0, 4),
            Point::new(2, 2), // 内部
            Point::new(2, 0), // 边上
        ];
        let hull = convex_hull(&pts, false);
        let hull_pts: Vec<Point> = hull.iter().map(|&i| pts[i]).collect();
        assert_eq!(hull_pts.len(), 4);
        assert!(hull_pts.contains(&Point::new(0, 0)));
        assert!(hull_pts.contains(&Point::new(4, 0)));
        assert!(hull_pts.contains(&Point::new(4, 4)));
        assert!(hull_pts.contains(&Point::new(0, 4)));
        // 逆时针（OpenCV 默认 clockwise=false）
        let area = contour_area(&hull_pts, true);
        assert!((area - 16.0).abs() < 1e-9, "ccw area>0, got {area}");
        pts.clear();
    }

    #[test]
    fn convex_hull_collinear() {
        let pts = vec![
            Point::new(0, 0),
            Point::new(1, 1),
            Point::new(2, 2),
            Point::new(3, 3),
        ];
        let hull = convex_hull(&pts, false);
        let hull_pts: Vec<Point> = hull.iter().map(|&i| pts[i]).collect();
        // 共线：两端点
        assert!(hull_pts.contains(&Point::new(0, 0)));
        assert!(hull_pts.contains(&Point::new(3, 3)));
    }

    #[test]
    fn is_convex_shapes() {
        let square = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 4),
            Point::new(0, 4),
        ];
        assert!(is_contour_convex(&square));
        let l_shape = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 1),
            Point::new(1, 1),
            Point::new(1, 4),
            Point::new(0, 4),
        ];
        assert!(!is_contour_convex(&l_shape));
        assert!(!is_contour_convex(&[]));
    }

    #[test]
    fn min_area_rect_axis_aligned() {
        let square = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 4),
            Point::new(0, 4),
        ];
        let r = min_area_rect(&square).unwrap();
        assert!((r.center.0 - 2.0).abs() < 1e-4);
        assert!((r.center.1 - 2.0).abs() < 1e-4);
        assert!((r.size.0 - 4.0).abs() < 1e-3 && (r.size.1 - 4.0).abs() < 1e-3);
        assert!(r.angle >= -90.0 && r.angle < 0.0, "angle={}", r.angle);
    }

    #[test]
    fn min_area_rect_rotated() {
        // 45° 细长矩形：长边 10√2
        let pts = vec![
            Point::new(0, 0),
            Point::new(5, 5),
            Point::new(4, 6),
            Point::new(-1, 1),
        ];
        let r = min_area_rect(&pts).unwrap();
        let (w, h) = (r.size.0.max(r.size.1), r.size.0.min(r.size.1));
        assert!((w - (5.0 * 2.0f32.sqrt())).abs() < 1e-2, "w={w}");
        assert!((h - 2.0f32.sqrt()).abs() < 1e-2, "h={h}");
    }

    #[test]
    fn min_area_rect_degenerate() {
        assert!(min_area_rect(&[]).is_none());
        let r = min_area_rect(&[Point::new(3, 7)]).unwrap();
        assert_eq!(r.center, (3.0, 7.0));
        assert_eq!(r.size, (0.0, 0.0));
        let r = min_area_rect(&[Point::new(0, 0), Point::new(3, 4)]).unwrap();
        assert!((r.size.0.max(r.size.1) - 5.0).abs() < 1e-4);
    }

    #[test]
    fn box_points_roundtrip() {
        let square = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 4),
            Point::new(0, 4),
        ];
        let r = min_area_rect(&square).unwrap();
        let pts = box_points(r);
        // 四点都在 [0,4]² 内且包围方块
        for (x, y) in pts {
            assert!(x >= -1e-3 && x <= 4.0 + 1e-3, "x={x}");
            assert!(y >= -1e-3 && y <= 4.0 + 1e-3, "y={y}");
        }
    }

    #[test]
    fn min_enclosing_circle_basic() {
        let square = vec![
            Point::new(0, 0),
            Point::new(4, 0),
            Point::new(4, 4),
            Point::new(0, 4),
        ];
        let (c, r) = min_enclosing_circle(&square);
        assert!((c.0 - 2.0).abs() < 1e-3 && (c.1 - 2.0).abs() < 1e-3);
        assert!((r - 2.0 * 2.0f32.sqrt()).abs() < 1e-2, "r={r}");
        // 所有点在圆内
        for p in &square {
            let d = ((p.x as f32 - c.0).powi(2) + (p.y as f32 - c.1).powi(2)).sqrt();
            assert!(d <= r + 1e-3, "d={d} r={r}");
        }
    }

    #[test]
    fn min_enclosing_circle_degenerate() {
        let (c, r) = min_enclosing_circle(&[]);
        assert_eq!(c, (0.0, 0.0));
        assert_eq!(r, 0.0);
        let (c, r) = min_enclosing_circle(&[Point::new(5, 9)]);
        assert_eq!(c, (5.0, 9.0));
        assert_eq!(r, 1.0e-4);
    }
}
