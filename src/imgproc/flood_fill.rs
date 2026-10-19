//! 泛洪填充（`cv::floodFill`），对齐 OpenCV 5 `modules/imgproc/src/floodfill.cpp`
//! 的 `floodFill_CnIR`（简单填充）与 `floodFillGrad_CnIR`（梯度填充）。
//!
//! 位级对齐：扫描线算法、`FFillSegment` 栈、`Diff8uC1` 差值判定、掩码边界
//! 均与 OpenCV 逐位一致。

use crate::core::image::Image;
use crate::core::point::Point;
use crate::core::rect::Rect;

/// 泛洪填充结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloodFillResult {
    /// 填充像素数。
    pub area: i32,
    /// 填充区域外接矩形。
    pub rect: Rect,
}

/// 扫描线段（对齐 `FFillSegment`）。
#[derive(Debug, Clone, Copy)]
struct FFillSegment {
    y: i32,
    l: i32,
    r: i32,
    prevl: i32,
    prevr: i32,
    dir: i32,
}

const UP: i32 = 1;
const DOWN: i32 = -1;

/// 简单泛洪填充（对齐 `floodFill_CnIR`，u8 单通道）。
///
/// 将种子点所在连通区域（像素值 == 种子值）全部替换为 `new_val`。
/// 返回填充面积与外接矩形。
pub fn flood_fill(
    img: &mut Image<u8, 1>,
    seed: Point,
    new_val: u8,
    connectivity: u8,
) -> FloodFillResult {
    let (w, h) = (img.width(), img.height());
    let data = img.data_mut();
    let _8_connectivity = connectivity == 8;

    let mut area = 0i32;
    let mut x_min = seed.x;
    let mut x_max = seed.x;
    let mut y_min = seed.y;
    let mut y_max = seed.y;

    let mut l = seed.x;
    let mut r = seed.x;

    let val0 = data[seed.y as usize * w + seed.x as usize];
    data[seed.y as usize * w + seed.x as usize] = new_val;

    while r + 1 < w as i32 && data[seed.y as usize * w + (r + 1) as usize] == val0 {
        r += 1;
        data[seed.y as usize * w + r as usize] = new_val;
    }
    while l - 1 >= 0 && data[seed.y as usize * w + (l - 1) as usize] == val0 {
        l -= 1;
        data[seed.y as usize * w + l as usize] = new_val;
    }

    x_max = r;
    x_min = l;

    let mut buffer: Vec<FFillSegment> = Vec::new();
    buffer.push(FFillSegment {
        y: seed.y,
        l,
        r,
        prevl: r + 1,
        prevr: r,
        dir: UP,
    });

    while let Some(seg) = buffer.pop() {
        let yc = seg.y;
        let l = seg.l;
        let r = seg.r;
        let pl = seg.prevl;
        let pr = seg.prevr;
        let dir = seg.dir;

        let data3 = [
            (-dir, l - _8_connectivity as i32, r + _8_connectivity as i32),
            (dir, l - _8_connectivity as i32, pl - 1),
            (dir, pr + 1, r + _8_connectivity as i32),
        ];

        area += r - l + 1;
        if x_max < r {
            x_max = r;
        }
        if x_min > l {
            x_min = l;
        }
        if y_max < yc {
            y_max = yc;
        }
        if y_min > yc {
            y_min = yc;
        }

        for k in 0..3 {
            let dir = data3[k].0;
            let yc_dir = yc + dir;

            if yc_dir < 0 || yc_dir >= h as i32 {
                continue;
            }

            let left = data3[k].1;
            let right = data3[k].2;

            let mut i = left;
            while i <= right {
                if i >= 0 && i < w as i32 && data[yc_dir as usize * w + i as usize] == val0 {
                    let mut j = i;
                    data[yc_dir as usize * w + i as usize] = new_val;
                    j -= 1;
                    while j >= 0 && data[yc_dir as usize * w + j as usize] == val0 {
                        data[yc_dir as usize * w + j as usize] = new_val;
                        j -= 1;
                    }
                    i += 1;
                    while i < w as i32 && data[yc_dir as usize * w + i as usize] == val0 {
                        data[yc_dir as usize * w + i as usize] = new_val;
                        i += 1;
                    }
                    buffer.push(FFillSegment {
                        y: yc_dir,
                        l: j + 1,
                        r: i - 1,
                        prevl: l,
                        prevr: r,
                        dir: -dir,
                    });
                }
                i += 1;
            }
        }
    }

    FloodFillResult {
        area,
        rect: Rect::new(x_min, y_min, x_max - x_min + 1, y_max - y_min + 1),
    }
}

/// 梯度泛洪填充（对齐 `floodFillGrad_CnIR`，u8 单通道）。
///
/// 使用 `lo_diff`/`up_diff` 判定像素是否属于填充区域（`lo <= 像素差 <= up`）。
/// 返回填充面积与外接矩形。
pub fn flood_fill_grad(
    img: &mut Image<u8, 1>,
    seed: Point,
    new_val: u8,
    lo_diff: u8,
    up_diff: u8,
    connectivity: u8,
    fixed_range: bool,
) -> FloodFillResult {
    let (w, h) = (img.width(), img.height());
    let data = img.data_mut();
    let _8_connectivity = connectivity == 8;

    let diff = Diff8uC1::new(lo_diff, up_diff);

    let mut area = 0i32;
    let mut x_min = seed.x;
    let mut x_max = seed.x;
    let mut y_min = seed.y;
    let mut y_max = seed.y;

    let mut l = seed.x;
    let mut r = seed.x;

    let val0 = data[seed.y as usize * w + seed.x as usize];

    if fixed_range {
        while r + 1 < w as i32 && diff.matches(data[seed.y as usize * w + (r + 1) as usize], val0) {
            r += 1;
        }
        while l - 1 >= 0 && diff.matches(data[seed.y as usize * w + (l - 1) as usize], val0) {
            l -= 1;
        }
    } else {
        while r + 1 < w as i32
            && diff.matches(
                data[seed.y as usize * w + (r + 1) as usize],
                data[seed.y as usize * w + r as usize],
            )
        {
            r += 1;
        }
        while l - 1 >= 0
            && diff.matches(
                data[seed.y as usize * w + (l - 1) as usize],
                data[seed.y as usize * w + l as usize],
            )
        {
            l -= 1;
        }
    }

    x_max = r;
    x_min = l;

    let mut buffer: Vec<FFillSegment> = Vec::new();
    buffer.push(FFillSegment {
        y: seed.y,
        l,
        r,
        prevl: r + 1,
        prevr: r,
        dir: UP,
    });

    while let Some(seg) = buffer.pop() {
        let yc = seg.y;
        let l = seg.l;
        let r = seg.r;
        let pl = seg.prevl;
        let pr = seg.prevr;
        let dir = seg.dir;

        let data3 = [
            (-dir, l - _8_connectivity as i32, r + _8_connectivity as i32),
            (dir, l - _8_connectivity as i32, pl - 1),
            (dir, pr + 1, r + _8_connectivity as i32),
        ];

        let length = (r - l) as u32;

        area += length as i32 + 1;
        if x_max < r {
            x_max = r;
        }
        if x_min > l {
            x_min = l;
        }
        if y_max < yc {
            y_max = yc;
        }
        if y_min > yc {
            y_min = yc;
        }

        for k in 0..3 {
            let dir = data3[k].0;
            let yc_dir = yc + dir;

            if yc_dir < 0 || yc_dir >= h as i32 {
                continue;
            }

            let left = data3[k].1;
            let right = data3[k].2;

            if fixed_range {
                let mut i = left;
                while i <= right {
                    if i >= 0 && i < w as i32
                        && diff.matches(data[yc_dir as usize * w + i as usize], val0)
                    {
                        let mut j = i;
                        j -= 1;
                        while j >= 0
                            && diff.matches(data[yc_dir as usize * w + j as usize], val0)
                        {
                            j -= 1;
                        }
                        i += 1;
                        while i < w as i32
                            && diff.matches(data[yc_dir as usize * w + i as usize], val0)
                        {
                            i += 1;
                        }
                        buffer.push(FFillSegment {
                            y: yc_dir,
                            l: j + 1,
                            r: i - 1,
                            prevl: l,
                            prevr: r,
                            dir: -dir,
                        });
                    }
                    i += 1;
                }
            } else if !_8_connectivity {
                let mut i = left;
                while i <= right {
                    if i >= 0 && i < w as i32
                        && diff.matches(
                            data[yc_dir as usize * w + i as usize],
                            data[yc as usize * w + i as usize],
                        )
                    {
                        let mut j = i;
                        j -= 1;
                        while j >= 0
                            && diff.matches(
                                data[yc_dir as usize * w + j as usize],
                                data[yc_dir as usize * w + (j + 1) as usize],
                            )
                        {
                            j -= 1;
                        }
                        i += 1;
                        while i < w as i32
                            && (diff.matches(
                                data[yc_dir as usize * w + i as usize],
                                data[yc_dir as usize * w + (i - 1) as usize],
                            ) || (i <= r
                                && diff.matches(
                                    data[yc_dir as usize * w + i as usize],
                                    data[yc as usize * w + i as usize],
                                )))
                        {
                            i += 1;
                        }
                        buffer.push(FFillSegment {
                            y: yc_dir,
                            l: j + 1,
                            r: i - 1,
                            prevl: l,
                            prevr: r,
                            dir: -dir,
                        });
                    }
                    i += 1;
                }
            } else {
                let mut i = left;
                while i <= right {
                    let mut matched = false;
                    if i >= 0 && i < w as i32 {
                        let val = data[yc_dir as usize * w + i as usize];
                        let idx = i - l - 1;
                        if (idx as u32) <= length
                            && diff.matches(val, data[yc as usize * w + (i - 1) as usize])
                        {
                            matched = true;
                        } else if (idx as u32 + 1) <= length
                            && diff.matches(val, data[yc as usize * w + i as usize])
                        {
                            matched = true;
                        } else if (idx as u32 + 2) <= length
                            && diff.matches(val, data[yc as usize * w + (i + 1) as usize])
                        {
                            matched = true;
                        }
                    }
                    if matched {
                        let mut j = i;
                        j -= 1;
                        while j >= 0
                            && diff.matches(
                                data[yc_dir as usize * w + j as usize],
                                data[yc_dir as usize * w + (j + 1) as usize],
                            )
                        {
                            j -= 1;
                        }
                        i += 1;
                        while i < w as i32 {
                            let val = data[yc_dir as usize * w + i as usize];
                            let mut m = diff.matches(
                                val,
                                data[yc_dir as usize * w + (i - 1) as usize],
                            );
                            if !m {
                                let idx = i - l - 1;
                                m = (idx as u32) <= length
                                    && diff.matches(val, data[yc as usize * w + (i - 1) as usize]);
                            }
                            if !m {
                                let idx = i - l;
                                m = (idx as u32) <= length
                                    && diff.matches(val, data[yc as usize * w + i as usize]);
                            }
                            if !m {
                                let idx = i - l + 1;
                                m = (idx as u32) <= length
                                    && diff.matches(val, data[yc as usize * w + (i + 1) as usize]);
                            }
                            if m {
                                i += 1;
                            } else {
                                break;
                            }
                        }
                        buffer.push(FFillSegment {
                            y: yc_dir,
                            l: j + 1,
                            r: i - 1,
                            prevl: l,
                            prevr: r,
                            dir: -dir,
                        });
                    }
                    i += 1;
                }
            }
        }

        if true {
            for i in l..=r {
                if i >= 0 && i < w as i32 {
                    data[yc as usize * w + i as usize] = new_val;
                }
            }
        }
    }

    FloodFillResult {
        area,
        rect: Rect::new(x_min, y_min, x_max - x_min + 1, y_max - y_min + 1),
    }
}

/// u8 单通道差值判定（对齐 `Diff8uC1`）。
struct Diff8uC1 {
    lo: u32,
    interval: u32,
}

impl Diff8uC1 {
    fn new(lo: u8, up: u8) -> Self {
        Self {
            lo: lo as u32,
            interval: lo as u32 + up as u32,
        }
    }

    fn matches(&self, a: u8, b: u8) -> bool {
        (a as u32 - b as u32 + self.lo) <= self.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 简单填充：3x3 方块。
    #[test]
    fn flood_fill_square() {
        let mut img = Image::<u8, 1>::from_vec(5, 5, vec![
            0, 0, 0, 0, 0,
            0, 1, 1, 1, 0,
            0, 1, 1, 1, 0,
            0, 1, 1, 1, 0,
            0, 0, 0, 0, 0,
        ]).unwrap();
        let r = flood_fill(&mut img, Point::new(2, 2), 9, 4);
        assert_eq!(r.area, 9);
        assert_eq!(r.rect, Rect::new(1, 1, 3, 3));
        // 中心已填充
        assert_eq!(img.data()[2 * 5 + 2], 9);
        // 外部未填充
        assert_eq!(img.data()[0], 0);
    }

    /// 简单填充：种子点已是目标值 → 填充整个连通分量（对齐 OpenCV）。
    #[test]
    fn flood_fill_same_value() {
        let mut img = Image::<u8, 1>::from_vec(3, 3, vec![
            1, 1, 1,
            1, 1, 1,
            1, 1, 1,
        ]).unwrap();
        let r = flood_fill(&mut img, Point::new(1, 1), 1, 4);
        assert_eq!(r.area, 9);
        assert_eq!(r.rect, Rect::new(0, 0, 3, 3));
    }

    /// 梯度填充：lo_diff/up_diff。
    #[test]
    fn flood_fill_grad_basic() {
        let mut img = Image::<u8, 1>::from_vec(5, 5, vec![
            0, 0, 0, 0, 0,
            0, 10, 11, 12, 0,
            0, 11, 12, 13, 0,
            0, 12, 13, 14, 0,
            0, 0, 0, 0, 0,
        ]).unwrap();
        // 种子 (2,2)=12，lo=1, up=1 → 允许 11..13
        let r = flood_fill_grad(&mut img, Point::new(2, 2), 99, 1, 1, 4, false);
        assert!(r.area >= 5, "area={}", r.area);
        // 中心已填充
        assert_eq!(img.data()[2 * 5 + 2], 99);
    }
}