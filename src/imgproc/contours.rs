//! 轮廓查找（Suzuki-Abe 边界跟踪 + TRU 并行算法），对齐 OpenCV 5
//! `modules/imgproc/src/contours_new.cpp` / `contours_truco.cpp`。
//!
//! - `RETR_LIST` 且不需要 hierarchy → TRU 并行算法（rayon 行条带，
//!   对齐 `findTRUContours`）
//! - 其余模式 → Suzuki-Abe 串行扫描器（对齐 `ContourScanner_`）
//!
//! SIMD 内核：行扫描跳转（`find_start_contour_point`/`find_end_contour_point`），
//! AVX2 + AVX512 + 标量三路径（`simd_kernels/contours.rs`）。

use crate::core::image::Image;
use crate::core::point::Point;

mod suzuki;
mod tru;

pub use tru::tru_find_contours;

/// TRU 路径的轮廓近似后处理（对齐 contours_truco.cpp:443 approxContour）。
pub(crate) fn approx_contour(contour: &[Point], method: ContourApproxMethod) -> Vec<Point> {
    let n = contour.len();
    if n <= 1 {
        return contour.to_vec();
    }
    match method {
        ContourApproxMethod::Simple => {
            let mut result = Vec::with_capacity(n);
            for i in 0..n {
                let prev_i = if i == 0 { n - 1 } else { i - 1 };
                let next_i = if i == n - 1 { 0 } else { i + 1 };
                let v1 = contour[i] - contour[prev_i];
                let v2 = contour[next_i] - contour[i];
                if v1 != v2 {
                    result.push(contour[i]);
                }
            }
            if result.is_empty() {
                result.push(contour[0]);
            }
            result
        }
        ContourApproxMethod::Tc89L1 | ContourApproxMethod::Tc89Kcos => {
            // 链码化 + TC89（对齐 truco:464-497）
            let get_code = |d: Point| -> i8 {
                if d.x == 1 {
                    if d.y == 0 { return 0; }
                    if d.y == -1 { return 1; }
                    if d.y == 1 { return 7; }
                } else if d.x == 0 {
                    if d.y == -1 { return 2; }
                    if d.y == 1 { return 6; }
                } else if d.x == -1 {
                    if d.y == -1 { return 3; }
                    if d.y == 0 { return 4; }
                    if d.y == 1 { return 5; }
                }
                0
            };
            let mut codes = Vec::with_capacity(n);
            for i in 0..n {
                let d = contour[(i + 1) % n] - contour[i];
                codes.push(get_code(d));
            }
            crate::simd_kernels::contours::approximate_chain_tc89(&codes, contour[0], method)
        }
        ContourApproxMethod::None => contour.to_vec(),
    }
}

/// 轮廓检索模式，对应 `cv::RetrievalModes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContourRetrievalMode {
    /// 只检索最外层轮廓（`RETR_EXTERNAL`）。
    External,
    /// 检索所有轮廓不建立层级（`RETR_LIST`）。
    List,
    /// 检索所有轮廓，组织为两层（外轮廓/孔）（`RETR_CCOMP`）。
    CComp,
    /// 检索所有轮廓，重建完整嵌套树（`RETR_TREE`）。
    Tree,
}

/// 轮廓近似方法，对应 `cv::ContourApproximationModes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContourApproxMethod {
    /// 存储所有轮廓点（`CHAIN_APPROX_NONE`）。
    None,
    /// 压缩水平/垂直/对角线段，只留端点（`CHAIN_APPROX_SIMPLE`）。
    Simple,
    /// Teh-Chin 链码近似 L1（`CHAIN_APPROX_TC89_L1`）。
    Tc89L1,
    /// Teh-Chin 链码近似 KCOS（`CHAIN_APPROX_TC89_KCOS`）。
    Tc89Kcos,
}

/// 轮廓层级关系，对应 OpenCV hierarchy 的 `Vec4i`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hierarchy {
    /// 同层下一个轮廓（无则 -1）。
    pub next: i32,
    /// 同层上一个轮廓（无则 -1）。
    pub prev: i32,
    /// 第一个子轮廓（无则 -1）。
    pub first_child: i32,
    /// 父轮廓（无则 -1）。
    pub parent: i32,
}

impl Hierarchy {
    pub const fn none() -> Self {
        Self { next: -1, prev: -1, first_child: -1, parent: -1 }
    }
}

/// 查找二值图像中的轮廓。
///
/// 非零像素视为 1（对齐 OpenCV：内部 threshold(image, 0, 1)）。
/// 返回 `(contours, hierarchy)`；`List` 模式走 TRU 并行快路径
///（对齐 OpenCV 5 无 hierarchy 时的 fast path），hierarchy 为全 -1
/// 占位；其余模式走 Suzuki-Abe，hierarchy 有效。
///
/// 轮廓顺序、点序、层级关系与 OpenCV 5 逐位一致。
pub fn find_contours(
    src: &Image<u8, 1>,
    mode: ContourRetrievalMode,
    method: ContourApproxMethod,
    offset: Point,
) -> (Vec<Vec<Point>>, Vec<Hierarchy>) {
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "find_contours: empty image");

    // RETR_LIST 无 hierarchy 需求 → TRU 并行快路径（对齐 OpenCV 5
    // findContours 的 fast path 分支）
    if mode == ContourRetrievalMode::List {
        let contours = tru_find_contours(src, method, offset);
        let hierarchy = vec![Hierarchy::none(); contours.len()];
        return (contours, hierarchy);
    }

    suzuki::find_contours_impl(src, mode, method, offset)
}

/// 查找轮廓（带完整 hierarchy，全部模式走 Suzuki-Abe 串行路径）。
///
/// 对齐 OpenCV 5 `findContours` 带 hierarchy 输出的行为：`List` 模式
/// 也输出 next/prev 兄弟链（此时不走 TRU 并行快路径）。
pub fn find_contours_with_hierarchy(
    src: &Image<u8, 1>,
    mode: ContourRetrievalMode,
    method: ContourApproxMethod,
    offset: Point,
) -> (Vec<Vec<Point>>, Vec<Hierarchy>) {
    let (w, h) = (src.width(), src.height());
    assert!(w > 0 && h > 0, "find_contours: empty image");
    suzuki::find_contours_impl(src, mode, method, offset)
}

/// `contour_area`：鞋带公式（对齐 `cv::contourArea`，非 oriented 取绝对值）。
pub fn contour_area(contour: &[Point], oriented: bool) -> f64 {
    if contour.is_empty() {
        return 0.0;
    }
    let mut a00 = 0.0f64;
    let mut prev = contour[contour.len() - 1];
    for &p in contour {
        // f64 累加（OpenCV 用 double 累加 float 坐标；i32 坐标精确）
        a00 += prev.x as f64 * p.y as f64 - prev.y as f64 * p.x as f64;
        prev = p;
    }
    a00 *= 0.5;
    if !oriented {
        a00 = a00.abs();
    }
    a00
}

/// `bounding_rect`：点集外接矩形（对齐 `cv::boundingRect` 点集路径）。
pub fn bounding_rect(contour: &[Point]) -> crate::core::rect::Rect {
    if contour.is_empty() {
        return crate::core::rect::Rect::default();
    }
    let mut xmin = i32::MAX;
    let mut ymin = i32::MAX;
    let mut xmax = i32::MIN;
    let mut ymax = i32::MIN;
    for &p in contour {
        xmin = xmin.min(p.x);
        ymin = ymin.min(p.y);
        xmax = xmax.max(p.x);
        ymax = ymax.max(p.y);
    }
    crate::core::rect::Rect::new(xmin, ymin, xmax - xmin + 1, ymax - ymin + 1)
}

/// `approx_poly_dp`：Douglas-Peucker（对齐 `cv::approxPolyDP`，含最终
/// 直线段清理 pass）。
pub fn approx_poly_dp(curve: &[Point], epsilon: f64, closed: bool) -> Vec<Point> {
    crate::simd_kernels::contours::approx_poly_dp_impl(curve, epsilon, closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Image<u8, 1> {
        let mut data = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                data[y * w + x] = f(x, y);
            }
        }
        Image::from_vec(w, h, data).unwrap()
    }

    /// 单个 3x3 方块：RETR_EXTERNAL 应恰好 1 条轮廓，8 点（角点环）。
    #[test]
    #[allow(clippy::manual_range_contains)]
    fn single_square_external() {
        let src = img(8, 8, |x, y| (x >= 2 && x <= 4 && y >= 2 && y <= 4) as u8 * 255);
        let (contours, hier) = find_contours(
            &src,
            ContourRetrievalMode::External,
            ContourApproxMethod::None,
            Point::new(0, 0),
        );
        assert_eq!(contours.len(), 1);
        assert_eq!(hier.len(), 1);
        // 外轮廓从最上最左点 (2,2) 出发
        assert_eq!(contours[0][0], Point::new(2, 2));
        // 面积 = 3x3 方块边界环（8 点空心环）的鞋带面积（OpenCV 实测 4）
        let a = contour_area(&contours[0], false);
        assert!((a - 4.0).abs() < 1e-9, "area={a}");
        let r = bounding_rect(&contours[0]);
        assert_eq!(r, crate::core::rect::Rect::new(2, 2, 3, 3));
    }

    /// 空图：无轮廓。
    #[test]
    fn empty_image() {
        let src = img(4, 4, |_, _| 0);
        for mode in [
            ContourRetrievalMode::External,
            ContourRetrievalMode::List,
            ContourRetrievalMode::CComp,
            ContourRetrievalMode::Tree,
        ] {
            let (contours, _) = find_contours(
                &src,
                mode,
                ContourApproxMethod::None,
                Point::new(0, 0),
            );
            assert!(contours.is_empty(), "{mode:?}");
        }
    }

    /// 嵌套方块（外框 + 内孔）：Tree 模式层级正确。
    #[test]
    fn nested_tree_hierarchy() {
        // 7x7 实心框（1 环）内 3x3 孔
        #[allow(clippy::manual_range_contains)]
        let src = img(9, 9, |x, y| {
            let on_ring = x >= 1 && x <= 7 && y >= 1 && y <= 7 && !(x >= 3 && x <= 5 && y >= 3 && y <= 5);
            on_ring as u8 * 255
        });
        let (contours, hier) = find_contours(
            &src,
            ContourRetrievalMode::Tree,
            ContourApproxMethod::None,
            Point::new(0, 0),
        );
        assert_eq!(contours.len(), 2, "外轮廓 + 孔轮廓");
        // 找外轮廓（isHole=false 的那个：parent == -1）
        let outer_idx = hier.iter().position(|h| h.parent == -1).unwrap();
        let hole_idx = 1 - outer_idx;
        assert_eq!(hier[outer_idx].first_child, hole_idx as i32);
        assert_eq!(hier[hole_idx].parent, outer_idx as i32);
    }

    /// RETR_LIST 与 RETR_TREE 轮廓集合一致（顺序可能不同，点集相同）。
    #[test]
    #[allow(clippy::manual_range_contains)]
    fn list_vs_tree_consistency() {
        #[allow(clippy::manual_range_contains)]
        let src = img(12, 12, |x, y| {
            let a = x >= 1 && x <= 5 && y >= 1 && y <= 5 && !(x == 3 && y == 3);
            let b = x >= 8 && x <= 10 && y >= 8 && y <= 10;
            (a || b) as u8 * 255
        });
        let (list_c, _) = find_contours(
            &src,
            ContourRetrievalMode::List,
            ContourApproxMethod::None,
            Point::new(0, 0),
        );
        let (tree_c, _) = find_contours(
            &src,
            ContourRetrievalMode::Tree,
            ContourApproxMethod::None,
            Point::new(0, 0),
        );
        assert_eq!(list_c.len(), tree_c.len());
        // 每条 Tree 轮廓（旋转任意起点）都在 List 中
        for tc in &tree_c {
            let found = list_c.iter().any(|lc| {
                lc.len() == tc.len()
                    && (0..lc.len()).any(|s| {
                        lc.iter().cycle().skip(s).take(lc.len()).copied().eq(tc.iter().copied())
                    })
            });
            assert!(found, "tree contour {tc:?} not in list");
        }
    }

    /// offset 平移。
    #[test]
    fn offset_shift() {
        let src = img(6, 6, |x, y| (x == 1 && y == 1) as u8 * 255);
        let (contours, _) = find_contours(
            &src,
            ContourRetrievalMode::List,
            ContourApproxMethod::None,
            Point::new(10, 20),
        );
        assert_eq!(contours.len(), 1);
        assert_eq!(contours[0], vec![Point::new(11, 21)]);
    }

    /// CHAIN_APPROX_SIMPLE：直线段只留端点。
    #[test]
    fn approx_simple_collapses_runs() {
        // 5x1 横线
        #[allow(clippy::manual_range_contains)]
        let src = img(7, 3, |x, y| (y == 1 && x >= 1 && x <= 5) as u8 * 255);
        let (contours, _) = find_contours(
            &src,
            ContourRetrievalMode::List,
            ContourApproxMethod::Simple,
            Point::new(0, 0),
        );
        assert_eq!(contours.len(), 1);
        // 1 像素高横线的轮廓是上下两条边，SIMPLE 后各 2 端点
        assert!(contours[0].len() <= 6, "{:?}", contours[0]);
        let r = bounding_rect(&contours[0]);
        assert_eq!(r, crate::core::rect::Rect::new(1, 1, 5, 1));
    }

    /// approx_poly_dp：矩形轮廓 → 4 角点。
    #[test]
    fn approx_poly_dp_rect() {
        let rect: Vec<Point> = vec![
            Point::new(0, 0), Point::new(4, 0), Point::new(8, 0),
            Point::new(8, 4), Point::new(8, 8), Point::new(4, 8),
            Point::new(0, 8), Point::new(0, 4),
        ];
        let out = approx_poly_dp(&rect, 0.5, true);
        assert_eq!(out.len(), 4, "{out:?}");
        assert_eq!(bounding_rect(&out), bounding_rect(&rect));
    }

    /// contour_area 有向/无向。
    #[test]
    fn contour_area_oriented() {
        let cw = [Point::new(0, 0), Point::new(4, 0), Point::new(4, 4), Point::new(0, 4)];
        let a = contour_area(&cw, true);
        assert!((a - 16.0).abs() < 1e-9, "{a}");
        assert!((contour_area(&cw, false) - 16.0).abs() < 1e-9);
        let ccw: Vec<Point> = cw.iter().rev().copied().collect();
        assert!((contour_area(&ccw, true) + 16.0).abs() < 1e-9);
    }
}
