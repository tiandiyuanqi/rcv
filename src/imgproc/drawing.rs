//! 基础绘制（对齐 OpenCV 5 `modules/imgproc/src/drawing.cpp`）。
//!
//! 定点算法：坐标左移 `XY_SHIFT`(16) 位，Bresenham 增量在定点域进行，
//! 保证与 OpenCV 逐位一致。LINE_AA 仅支持 8U 图像（对齐 OpenCV）。
//!
//! 支持的图像类型：`Image<u8, 1>` / `Image<u8, 3>` / `Image<u8, 4>`。

use crate::core::image::Image;
use crate::core::point::Point;

pub(crate) const XY_SHIFT: i64 = 16;
pub(crate) const XY_ONE: i64 = 1 << XY_SHIFT;
const MAX_THICKNESS: i32 = 32767;

/// 线型（对应 `cv::LineTypes`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineType {
    /// 4 连通 Bresenham。
    Connected4,
    /// 8 连通 Bresenham（默认）。
    #[default]
    Connected8,
    /// 抗锯齿（仅 8U 图像，否则退化为 8 连通）。
    AntiAliased,
}

impl LineType {
    /// OpenCV 整数值（LINE_4=4/LINE_8=8/LINE_AA=16）。
    pub const fn as_int(self) -> i32 {
        match self {
            LineType::Connected4 => 4,
            LineType::Connected8 => 8,
            LineType::AntiAliased => 16,
        }
    }

    /// 是否 < LINE_AA（drawing.cpp 的 `line_type < cv::LINE_AA` 判定）。
    pub(crate) const fn is_raster(self) -> bool {
        !matches!(self, LineType::AntiAliased)
    }
}

/// 定点二维点（Point2l）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Point2l {
    pub x: i64,
    pub y: i64,
}

impl Point2l {
    pub(crate) const fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }
}

impl From<Point> for Point2l {
    fn from(p: Point) -> Self {
        Self::new(p.x as i64, p.y as i64)
    }
}

/// 画布抽象：u8 图像（1/3/4 通道）+ 颜色（按通道数展开）。
pub(crate) struct Canvas<'a> {
    pub data: &'a mut [u8],
    pub width: i32,
    pub height: i32,
    pub pix_size: usize,
    pub step: usize,
    /// 颜色（已按通道展开，长度 = pix_size）。
    pub color: [u8; 4],
}

impl<'a> Canvas<'a> {
    pub(crate) fn new<T: crate::core::image::Pixel + bytemuck::Pod>(
        img: &'a mut Image<T, 1>,
        color: [T; 1],
    ) -> Self {
        let w = img.width();
        let h = img.height();
        let step = w;
        let data: &mut [u8] = bytemuck::cast_slice_mut(img.data_mut());
        Self {
            data,
            width: w as i32,
            height: h as i32,
            pix_size: 1,
            step,
            color: [bytemuck::cast(color[0]), 0, 0, 0],
        }
    }

    pub(crate) fn new3<T: crate::core::image::Pixel + bytemuck::Pod>(
        img: &'a mut Image<T, 3>,
        color: [T; 3],
    ) -> Self {
        let w = img.width();
        let h = img.height();
        let step = w * 3;
        let data: &mut [u8] = bytemuck::cast_slice_mut(img.data_mut());
        Self {
            data,
            width: w as i32,
            height: h as i32,
            pix_size: 3,
            step,
            color: [
                bytemuck::cast(color[0]),
                bytemuck::cast(color[1]),
                bytemuck::cast(color[2]),
                0,
            ],
        }
    }

    /// ICV_PUT_POINT：写单像素（无边界检查，调用方保证）。
    #[inline(always)]
    pub(crate) fn put_point(&mut self, x: i32, y: i32) {
        let off = y as usize * self.step + x as usize * self.pix_size;
        let p = &mut self.data[off..off + self.pix_size];
        p.copy_from_slice(&self.color[..self.pix_size]);
    }

    /// ICV_HLINE：水平填充 [xl, xr]（无边界检查，调用方保证）。
    #[inline(always)]
    pub(crate) fn hline(&mut self, y: i32, xl: i32, xr: i32) {
        let row = y as usize * self.step;
        let start = row + xl as usize * self.pix_size;
        let end = row + (xr as usize + 1) * self.pix_size;
        if self.pix_size == 1 {
            self.data[start..end].fill(self.color[0]);
        } else {
            let mut i = start;
            while i < end {
                self.data[i..i + self.pix_size].copy_from_slice(&self.color[..self.pix_size]);
                i += self.pix_size;
            }
        }
    }
}

/// clipLine（对齐 drawing.cpp:93，i64 定点域）。
/// 注意 OpenCV 用引用实时读写 x1/y1/x2/y2（第一段 clip 修改后
/// 第二段用新值），这里用宏模拟引用语义。
pub(crate) fn clip_line(size_w: i64, size_h: i64, pt1: &mut Point2l, pt2: &mut Point2l) -> bool {
    if size_w <= 0 || size_h <= 0 {
        return false;
    }
    let right = size_w - 1;
    let bottom = size_h - 1;
    let mut c1 = (pt1.x < 0) as i32
        + ((pt1.x > right) as i32) * 2
        + ((pt1.y < 0) as i32) * 4
        + ((pt1.y > bottom) as i32) * 8;
    let mut c2 = (pt2.x < 0) as i32
        + ((pt2.x > right) as i32) * 2
        + ((pt2.y < 0) as i32) * 4
        + ((pt2.y > bottom) as i32) * 8;

    // 引用语义宏：x1/y1/x2/y2 直接读写 pt1/pt2
    macro_rules! x1 { () => { pt1.x } }
    macro_rules! y1 { () => { pt1.y } }
    macro_rules! x2 { () => { pt2.x } }
    macro_rules! y2 { () => { pt2.y } }

    if (c1 & c2) == 0 && (c1 | c2) != 0 {
        let mut a: i64;
        if c1 & 12 != 0 {
            a = if c1 < 8 { 0 } else { bottom };
            x1!() += ((a - y1!()) as f64 * (x2!() - x1!()) as f64 / (y2!() - y1!()) as f64) as i64;
            y1!() = a;
            c1 = (x1!() < 0) as i32 + ((x1!() > right) as i32) * 2;
        }
        if c2 & 12 != 0 {
            a = if c2 < 8 { 0 } else { bottom };
            x2!() += ((a - y2!()) as f64 * (x2!() - x1!()) as f64 / (y2!() - y1!()) as f64) as i64;
            y2!() = a;
            c2 = (x2!() < 0) as i32 + ((x2!() > right) as i32) * 2;
        }
        if (c1 & c2) == 0 && (c1 | c2) != 0 {
            if c1 != 0 {
                a = if c1 == 1 { 0 } else { right };
                y1!() += ((a - x1!()) as f64 * (y2!() - y1!()) as f64
                    / (x2!() - x1!()) as f64) as i64;
                x1!() = a;
                c1 = 0;
            }
            if c2 != 0 {
                a = if c2 == 1 { 0 } else { right };
                y2!() += ((a - x2!()) as f64 * (y2!() - y1!()) as f64
                    / (x2!() - x1!()) as f64) as i64;
                x2!() = a;
                c2 = 0;
            }
        }
    }
    (c1 | c2) == 0
}

/// Line（对齐 drawing.cpp:263，整数坐标 Bresenham，connectivity 4/8）。
/// 步进语义对齐 LineIterator::operator++（ptmode 分支）。
pub(crate) fn line_raster(
    cv: &mut Canvas,
    pt1: Point,
    pt2: Point,
    line_type: LineType,
) {
    let connectivity = match line_type {
        LineType::Connected4 => 4,
        _ => 8,
    };
    let mut p1 = Point2l::from(pt1);
    let mut p2 = Point2l::from(pt2);
    if !clip_line(cv.width as i64, cv.height as i64, &mut p1, &mut p2) {
        return;
    }
    let mut pt1 = Point::new(p1.x as i32, p1.y as i32);
    let mut pt2 = Point::new(p2.x as i32, p2.y as i32);

    let mut delta_x = 1i32;
    let mut delta_y = 1i32;
    let mut dx = pt2.x - pt1.x;
    let mut dy = pt2.y - pt1.y;

    if dx < 0 {
        // leftToRight = true（对齐 Line() 的 LineIterator 构造）：
        // 交换端点，从左往右画
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

    let mut x = pt1.x;
    let mut y = pt1.y;
    for _ in 0..count {
        cv.put_point(x, y);
        // LineIterator::operator++（ptmode 分支）
        let mask = if err < 0 { -1i32 } else { 0 };
        err += minus_delta + (plus_delta & mask);
        x += minus_shift + (plus_shift & mask);
        y += minus_step + (plus_step & mask);
    }
}

/// 斜率校正表（对齐 drawing.cpp:296）。
const SLOPE_CORR_TABLE: [u8; 32] = [
    181, 181, 181, 182, 182, 183, 184, 185, 187, 188, 190, 192, 194, 196, 198, 201,
    203, 206, 209, 211, 214, 218, 221, 224, 227, 231, 235, 238, 242, 246, 250, 254,
];

/// 抗锯齿滤波表（对齐 drawing.cpp:301）。
const FILTER_TABLE: [i32; 64] = [
    168, 177, 185, 194, 202, 210, 218, 224, 231, 236, 241, 246, 249, 252, 254, 254,
    254, 254, 252, 249, 246, 241, 236, 231, 224, 218, 210, 202, 194, 185, 177, 168,
    158, 149, 140, 131, 122, 114, 105, 97, 89, 82, 75, 68, 62, 56, 50, 45,
    40, 36, 32, 28, 25, 22, 19, 16, 14, 12, 11, 9, 8, 7, 5, 5,
];

/// Line2（对齐 drawing.cpp:635，定点坐标 Bresenham，含边界检查）。
pub(crate) fn line_fixed(cv: &mut Canvas, mut pt1: Point2l, mut pt2: Point2l) {
    let size_w = (cv.width as i64) << XY_SHIFT;
    let size_h = (cv.height as i64) << XY_SHIFT;
    if !clip_line(size_w, size_h, &mut pt1, &mut pt2) {
        return;
    }

    let mut dx = pt2.x - pt1.x;
    let mut dy = pt2.y - pt1.y;

    let j = if dx < 0 { -1i64 } else { 0 };
    let ax = (dx ^ j) - j;
    let i = if dy < 0 { -1i64 } else { 0 };
    let ay = (dy ^ i) - i;

    let x_step;
    let y_step;
    let ecount;

    if ax > ay {
        dy = (dy ^ j) - j;
        // swap(pt1.x, pt2.x) / swap(pt1.y, pt2.y)（j = -1 时）
        if j != 0 {
            std::mem::swap(&mut pt1.x, &mut pt2.x);
            std::mem::swap(&mut pt1.y, &mut pt2.y);
        }
        x_step = XY_ONE;
        y_step = ((dy as u64) << XY_SHIFT) as i64 / (ax | 1);
        ecount = ((pt2.x - pt1.x) >> XY_SHIFT) as i32;
    } else {
        dx = (dx ^ i) - i;
        if i != 0 {
            std::mem::swap(&mut pt1.x, &mut pt2.x);
            std::mem::swap(&mut pt1.y, &mut pt2.y);
        }
        x_step = ((dx as u64) << XY_SHIFT) as i64 / (ay | 1);
        y_step = XY_ONE;
        ecount = ((pt2.y - pt1.y) >> XY_SHIFT) as i32;
    }

    pt1.x += XY_ONE >> 1;
    pt1.y += XY_ONE >> 1;

    // 端点（pt2 + 0.5）
    let ex = ((pt2.x + (XY_ONE >> 1)) >> XY_SHIFT) as i32;
    let ey = ((pt2.y + (XY_ONE >> 1)) >> XY_SHIFT) as i32;
    put_point_checked(cv, ex, ey);

    if ax > ay {
        let mut x = (pt1.x >> XY_SHIFT) as i32;
        let mut e = ecount;
        while e >= 0 {
            put_point_checked(cv, x, (pt1.y >> XY_SHIFT) as i32);
            x += 1;
            pt1.y += y_step;
            e -= 1;
        }
    } else {
        let mut y = (pt1.y >> XY_SHIFT) as i32;
        let mut e = ecount;
        while e >= 0 {
            put_point_checked(cv, (pt1.x >> XY_SHIFT) as i32, y);
            pt1.x += x_step;
            y += 1;
            e -= 1;
        }
    }
}

#[inline(always)]
fn put_point_checked(cv: &mut Canvas, x: i32, y: i32) {
    if 0 <= x && x < cv.width && 0 <= y && y < cv.height {
        cv.put_point(x, y);
    }
}

/// Size2l 辅助（clip_line 的定点尺寸参数）。
pub(crate) struct Size2l {
    pub w: i64,
    pub h: i64,
}

/// LineAA（对齐 drawing.cpp:315，抗锯齿线，仅 8U）。
pub(crate) fn line_aa(cv: &mut Canvas, mut pt1: Point2l, mut pt2: Point2l) {
    let size0_w = cv.width;
    let size0_h = cv.height;
    let nch = cv.pix_size as i64;
    let step = cv.step as i64;

    let size_w = (cv.width as i64) << XY_SHIFT;
    let size_h = (cv.height as i64) << XY_SHIFT;
    if !clip_line(size_w, size_h, &mut pt1, &mut pt2) {
        return;
    }

    let (cb, cg, cr, ca) = (
        cv.color[0] as i32,
        cv.color[1] as i32,
        cv.color[2] as i32,
        cv.color[3] as i32,
    );

    let mut dx = pt2.x - pt1.x;
    let mut dy = pt2.y - pt1.y;

    let j = if dx < 0 { -1i64 } else { 0 };
    let ax = (dx ^ j) - j;
    let i = if dy < 0 { -1i64 } else { 0 };
    let ay = (dy ^ i) - i;

    let x_step;
    let y_step;
    let ecount;
    let i_frac;
    let j_frac;
    let slope_raw;

    if ax > ay {
        dy = (dy ^ j) - j;
        if j != 0 {
            std::mem::swap(&mut pt1.x, &mut pt2.x);
            std::mem::swap(&mut pt1.y, &mut pt2.y);
        }
        x_step = XY_ONE;
        y_step = ((dy as u64) << XY_SHIFT) as i64 / (ax | 1);
        pt2.x += XY_ONE;
        ecount = ((pt2.x >> XY_SHIFT) - (pt1.x >> XY_SHIFT)) as i32;
        let jj = -(pt1.x & (XY_ONE - 1));
        pt1.y += ((y_step * jj) >> XY_SHIFT) + (XY_ONE >> 1);
        let mut s = ((y_step >> (XY_SHIFT - 5)) & 0x3f) as i32;
        s ^= if y_step < 0 { 0x3f } else { 0 };
        slope_raw = s;
        i_frac = (pt1.x >> (XY_SHIFT - 7)) & 0x78;
        j_frac = (pt2.x >> (XY_SHIFT - 7)) & 0x78;
    } else {
        dx = (dx ^ i) - i;
        if i != 0 {
            std::mem::swap(&mut pt1.x, &mut pt2.x);
            std::mem::swap(&mut pt1.y, &mut pt2.y);
        }
        x_step = ((dx as u64) << XY_SHIFT) as i64 / (ay | 1);
        y_step = XY_ONE;
        pt2.y += XY_ONE;
        ecount = ((pt2.y >> XY_SHIFT) - (pt1.y >> XY_SHIFT)) as i32;
        let jj = -(pt1.y & (XY_ONE - 1));
        pt1.x += ((x_step * jj) >> XY_SHIFT) + (XY_ONE >> 1);
        let mut s = ((x_step >> (XY_SHIFT - 5)) & 0x3f) as i32;
        s ^= if x_step < 0 { 0x3f } else { 0 };
        slope_raw = s;
        i_frac = (pt1.y >> (XY_SHIFT - 7)) & 0x78;
        j_frac = (pt2.y >> (XY_SHIFT - 7)) & 0x78;
    }

    let slope = if slope_raw & 0x20 != 0 {
        0x100
    } else {
        SLOPE_CORR_TABLE[(slope_raw & 0x1f) as usize] as i32
    };

    // 端点校正表
    let mut ep_table = [0i32; 9];
    {
        let t0 = (slope as i64) << 7;
        let t1 = ((0x78 - i_frac) | 4) * slope as i64;
        let t2 = (j_frac | 4) * slope as i64;

        ep_table[0] = 0;
        ep_table[8] = slope;
        ep_table[1] = (((((j_frac - i_frac) & 0x78) | 4) * slope as i64 >> 8) & 0x1ff) as i32;
        ep_table[3] = ep_table[1];
        ep_table[2] = ((t1 >> 8) & 0x1ff) as i32;
        ep_table[4] = (((((j_frac - i_frac) + 0x80) | 4) * slope as i64 >> 8) & 0x1ff) as i32;
        ep_table[5] = (((t1 + t0) >> 8) & 0x1ff) as i32;
        ep_table[6] = ((t2 >> 8) & 0x1ff) as i32;
        ep_table[7] = (((t2 + t0) >> 8) & 0x1ff) as i32;
    }

    // 混合写点（alpha = a/256，两次 8bit 逼近，对齐 ICV_PUT_POINT）
    macro_rules! put_aa {
        ($x:expr, $y:expr, $a:expr) => {{
            let (x, y, a) = ($x as i64, $y as i64, $a);
            if (0..size0_w as i64).contains(&x) && (0..size0_h as i64).contains(&y) {
                let off = (y * step + x * nch) as usize;
                for c in 0..nch as usize {
                    let target = [cb, cg, cr, ca][c];
                    let mut v = cv.data[off + c] as i32;
                    v += ((target - v) * a + 127) >> 8;
                    v += ((target - v) * a + 127) >> 8;
                    cv.data[off + c] = v as u8;
                }
            }
        }};
    }

    let mut scount = 0i32;
    if ax > ay {
        let mut x = (pt1.x >> XY_SHIFT) as i32;
        let mut e = ecount;
        while e >= 0 {
            if (0..size0_w).contains(&x) {
                let y = ((pt1.y >> XY_SHIFT) - 1) as i32;
                let ep_corr = ep_table
                    [(((scount >= 2) as i32 + 1) & (scount | 2)) as usize * 3
                        + (((e >= 2) as i32 + 1) & (e | 2)) as usize];
                let dist = ((pt1.y >> (XY_SHIFT - 5)) & 31) as usize;

                let a = (ep_corr * FILTER_TABLE[dist + 32] >> 8) & 0xff;
                if (0..size0_h).contains(&y) {
                    put_aa!(x, y, a);
                }
                let a = (ep_corr * FILTER_TABLE[dist] >> 8) & 0xff;
                if (0..size0_h).contains(&(y + 1)) {
                    put_aa!(x, y + 1, a);
                }
                let a = (ep_corr * FILTER_TABLE[63 - dist] >> 8) & 0xff;
                if (0..size0_h).contains(&(y + 2)) {
                    put_aa!(x, y + 2, a);
                }
            }
            x += 1;
            pt1.y += y_step;
            scount += 1;
            e -= 1;
        }
    } else {
        let mut y = (pt1.y >> XY_SHIFT) as i32;
        let mut e = ecount;
        while e >= 0 {
            if (0..size0_h).contains(&y) {
                let x = ((pt1.x >> XY_SHIFT) - 1) as i32;
                let ep_corr = ep_table
                    [(((scount >= 2) as i32 + 1) & (scount | 2)) as usize * 3
                        + (((e >= 2) as i32 + 1) & (e | 2)) as usize];
                let dist = ((pt1.x >> (XY_SHIFT - 5)) & 31) as usize;

                let a = (ep_corr * FILTER_TABLE[dist + 32] >> 8) & 0xff;
                if (0..size0_w).contains(&x) {
                    put_aa!(x, y, a);
                }
                let a = (ep_corr * FILTER_TABLE[dist] >> 8) & 0xff;
                if (0..size0_w).contains(&(x + 1)) {
                    put_aa!(x + 1, y, a);
                }
                let a = (ep_corr * FILTER_TABLE[63 - dist] >> 8) & 0xff;
                if (0..size0_w).contains(&(x + 2)) {
                    put_aa!(x + 2, y, a);
                }
            }
            y += 1;
            pt1.x += x_step;
            scount += 1;
            e -= 1;
        }
    }
}


/// Circle（对齐 drawing.cpp:1499，整数中点圆，fill=false 画环/true 填充）。
pub(crate) fn circle_raster(cv: &mut Canvas, center: Point, radius: i32, fill: bool) {
    let size_w = cv.width;
    let size_h = cv.height;
    let step = cv.step as i64;
    let pix_size = cv.pix_size as i64;

    let mut err: i64 = 0;
    let mut dx: i64 = radius as i64;
    let mut dy: i64 = 0;
    let mut plus: i64 = 1;
    let mut minus: i64 = ((radius as i64) << 1) - 1;
    let inside = center.x >= radius
        && center.x < size_w - radius
        && center.y >= radius
        && center.y < size_h - radius;

    macro_rules! put_pt {
        ($tptr:expr, $x:expr) => {{
            let off = $x as usize * pix_size as usize;
            $tptr[off..off + pix_size as usize].copy_from_slice(&cv.color[..pix_size as usize]);
        }};
    }

    while dx >= dy {
        let y11 = center.y as i64 - dy;
        let y12 = center.y as i64 + dy;
        let y21 = center.y as i64 - dx;
        let y22 = center.y as i64 + dx;
        let mut x11 = center.x as i64 - dx;
        let mut x12 = center.x as i64 + dx;
        let mut x21 = center.x as i64 - dy;
        let mut x22 = center.x as i64 + dy;

        if inside {
            let off1 = (y11 * step) as usize;
            let off2 = (y12 * step) as usize;
            if !fill {
                let (o1, o2) = (off1, off2);
                put_pt!(cv.data[o1..].as_mut(), x11);
                put_pt!(cv.data[o2..].as_mut(), x11);
                put_pt!(cv.data[o1..].as_mut(), x12);
                put_pt!(cv.data[o2..].as_mut(), x12);
            } else {
                cv.hline(y11 as i32, x11 as i32, x12 as i32);
                cv.hline(y12 as i32, x11 as i32, x12 as i32);
            }
            let off3 = (y21 * step) as usize;
            let off4 = (y22 * step) as usize;
            if !fill {
                let (o3, o4) = (off3, off4);
                put_pt!(cv.data[o3..].as_mut(), x21);
                put_pt!(cv.data[o4..].as_mut(), x21);
                put_pt!(cv.data[o3..].as_mut(), x22);
                put_pt!(cv.data[o4..].as_mut(), x22);
            } else {
                cv.hline(y21 as i32, x21 as i32, x22 as i32);
                cv.hline(y22 as i32, x21 as i32, x22 as i32);
            }
        } else if x11 < size_w as i64
            && x12 >= 0
            && y21 < size_h as i64
            && y22 >= 0
        {
            if fill {
                x11 = x11.max(0);
                x12 = x12.min(size_w as i64 - 1);
            }

            if y11 >= 0 && y11 < size_h as i64 {
                let off = (y11 * step) as usize;
                if !fill {
                    if x11 >= 0 {
                        put_pt!(cv.data[off..].as_mut(), x11);
                    }
                    if x12 < size_w as i64 {
                        put_pt!(cv.data[off..].as_mut(), x12);
                    }
                } else {
                    cv.hline(y11 as i32, x11 as i32, x12 as i32);
                }
            }
            if y12 >= 0 && y12 < size_h as i64 {
                let off = (y12 * step) as usize;
                if !fill {
                    if x11 >= 0 {
                        put_pt!(cv.data[off..].as_mut(), x11);
                    }
                    if x12 < size_w as i64 {
                        put_pt!(cv.data[off..].as_mut(), x12);
                    }
                } else {
                    cv.hline(y12 as i32, x11 as i32, x12 as i32);
                }
            }
            if x21 < size_w as i64 && x22 >= 0 {
                if fill {
                    x21 = x21.max(0);
                    x22 = x22.min(size_w as i64 - 1);
                }
                if y21 >= 0 && y21 < size_h as i64 {
                    let off = (y21 * step) as usize;
                    if !fill {
                        if x21 >= 0 {
                            put_pt!(cv.data[off..].as_mut(), x21);
                        }
                        if x22 < size_w as i64 {
                            put_pt!(cv.data[off..].as_mut(), x22);
                        }
                    } else {
                        cv.hline(y21 as i32, x21 as i32, x22 as i32);
                    }
                }
                if y22 >= 0 && y22 < size_h as i64 {
                    let off = (y22 * step) as usize;
                    if !fill {
                        if x21 >= 0 {
                            put_pt!(cv.data[off..].as_mut(), x21);
                        }
                        if x22 < size_w as i64 {
                            put_pt!(cv.data[off..].as_mut(), x22);
                        }
                    } else {
                        cv.hline(y22 as i32, x21 as i32, x22 as i32);
                    }
                }
            }
        }
        dy += 1;
        err += plus;
        plus += 2;

        // 对齐 (err <= 0) - 1：err<=0 → 0，err>0 → -1
        let mask = if err > 0 { -1i64 } else { 0 };
        err -= minus & mask;
        dx += mask;
        minus -= mask & 2;
    }
}

/// ThickLine（对齐 drawing.cpp:1646）。
/// `flags`：bit0 = 画 p0 端帽，bit1 = 画 p1 端帽。
pub(crate) fn thick_line(
    cv: &mut Canvas,
    mut p0: Point2l,
    mut p1: Point2l,
    thickness: i32,
    line_type: LineType,
    flags: i32,
    shift: i64,
) {
    // 越界粗线：margin 平移后 clip
    if thickness > 1 && shift == 0 {
        let contains = |p: &Point2l| {
            p.x >= 0 && p.y >= 0 && p.x < cv.width as i64 && p.y < cv.height as i64
        };
        if !contains(&p0) || !contains(&p1) {
            let margin = thickness as i64;
            p0.x += margin;
            p0.y += margin;
            p1.x += margin;
            p1.y += margin;
            clip_line(
                (cv.width as i64) + 2 * margin,
                (cv.height as i64) + 2 * margin,
                &mut p0,
                &mut p1,
            );
            p0.x -= margin;
            p0.y -= margin;
            p1.x -= margin;
            p1.y -= margin;
        }
    }

    p0.x <<= XY_SHIFT - shift;
    p0.y <<= XY_SHIFT - shift;
    p1.x <<= XY_SHIFT - shift;
    p1.y <<= XY_SHIFT - shift;

    if thickness <= 1 {
        if line_type.is_raster() {
            // 对齐 OpenCV：line_type == 1 || line_type == 8 || shift == 0
            // （LINE_4=4 ≠ 1，故 Connected4 且 shift>0 走 Line2 定点路径）
            if line_type.as_int() == 1
                || line_type.as_int() == 8
                || shift == 0
            {
                let q0 = Point::new(
                    ((p0.x + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                    ((p0.y + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                );
                let q1 = Point::new(
                    ((p1.x + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                    ((p1.y + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                );
                line_raster(cv, q0, q1, line_type);
            } else {
                line_fixed(cv, p0, p1);
            }
        } else {
            line_aa(cv, p0, p1);
        }
    } else {
        let inv_xy_one = 1.0 / XY_ONE as f64;
        let dx = (p0.x - p1.x) as f64 * inv_xy_one;
        let dy = (p1.y - p0.y) as f64 * inv_xy_one;
        let mut r = dx * dx + dy * dy;
        let odd_thickness = thickness & 1;
        let thickness = (thickness as i64) << (XY_SHIFT - 1);

        if r.abs() > f64::EPSILON {
            r = (thickness as f64 + odd_thickness as f64 * XY_ONE as f64 * 0.5)
                / r.sqrt();
            let dpx = (dy * r).round() as i64;
            let dpy = (dx * r).round() as i64;

            let pt = [
                Point2l::new(p0.x + dpx, p0.y + dpy),
                Point2l::new(p0.x - dpx, p0.y - dpy),
                Point2l::new(p1.x - dpx, p1.y - dpy),
                Point2l::new(p1.x + dpx, p1.y + dpy),
            ];
            fill_convex_poly_fixed(cv, &pt, line_type, XY_SHIFT);
        }

        let mut p0 = p0;
        for i in 0..2 {
            if flags & (i + 1) != 0 {
                if line_type.is_raster() {
                    let center = Point::new(
                        ((p0.x + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                        ((p0.y + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                    );
                    circle_raster(
                        cv,
                        center,
                        ((thickness + (XY_ONE >> 1)) >> XY_SHIFT) as i32,
                        true,
                    );
                } else {
                    let axes = Size2l {
                        w: thickness,
                        h: thickness,
                    };
                    ellipse_ex(cv, p0, axes, 0, 0, 360, -1, line_type);
                }
            }
            p0 = p1;
        }
    }
}

/// PolyLine（对齐 drawing.cpp:1735）。
pub(crate) fn poly_line(
    cv: &mut Canvas,
    v: &[Point2l],
    is_closed: bool,
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    if v.is_empty() {
        return;
    }
    let count = v.len();
    let i0 = if is_closed { count - 1 } else { 0 };
    let mut flags = 2 + (!is_closed) as i32;
    let mut p0 = v[i0];
    // 对齐 PolyLine：循环从 !is_closed（closed=1，open=0）开始
    let mut i = (!is_closed) as usize;
    while i < count {
        let p = v[i];
        thick_line(cv, p0, p, thickness, line_type, flags, shift);
        p0 = p;
        flags = 2;
        i += 1;
    }
}

/// FillConvexPoly（对齐 drawing.cpp:1095，定点顶点）。
pub(crate) fn fill_convex_poly_fixed(
    cv: &mut Canvas,
    v: &[Point2l],
    line_type: LineType,
    shift: i64,
) {
    #[derive(Clone, Copy)]
    struct Edge {
        idx: i32,
        di: i32,
        x: i64,
        dx: i64,
        ye: i32,
    }
    let npts = v.len();
    let mut edge = [Edge { idx: 0, di: 0, x: 0, dx: 0, ye: 0 }; 2];

    let delta = (1i64 << shift) >> 1;
    let mut imin = 0usize;
    let mut edges = npts as i32;
    let size_w = cv.width;
    let size_h = cv.height;

    let (delta1, delta2) = if !matches!(line_type, LineType::AntiAliased) {
        (XY_ONE >> 1, XY_ONE >> 1)
    } else {
        (XY_ONE - 1, 0)
    };

    let mut p0 = v[npts - 1];
    p0.x <<= XY_SHIFT - shift;
    p0.y <<= XY_SHIFT - shift;

    let mut xmin = v[0].x;
    let mut xmax = v[0].x;
    let mut ymin = v[0].y;
    let mut ymax = v[0].y;

    for i in 0..npts {
        let mut p = v[i];
        if p.y < ymin {
            ymin = p.y;
            imin = i;
        }
        ymax = ymax.max(p.y);
        xmax = xmax.max(p.x);
        xmin = xmin.min(p.x);

        p.x <<= XY_SHIFT - shift;
        p.y <<= XY_SHIFT - shift;

        if line_type.is_raster() {
            if shift == 0 {
                let q0 = Point::new((p0.x >> XY_SHIFT) as i32, (p0.y >> XY_SHIFT) as i32);
                let q1 = Point::new((p.x >> XY_SHIFT) as i32, (p.y >> XY_SHIFT) as i32);
                line_raster(cv, q0, q1, line_type);
            } else {
                line_fixed(cv, p0, p);
            }
        } else {
            line_aa(cv, p0, p);
        }
        p0 = p;
    }

    let xmin = (xmin + delta) >> shift;
    let xmax = (xmax + delta) >> shift;
    let ymin = (ymin + delta) >> shift;
    let ymax = (ymax + delta) >> shift;

    if npts < 3
        || xmax < 0
        || ymax < 0
        || xmin >= size_w as i64
        || ymin >= size_h as i64
    {
        return;
    }

    let ymax = ymax.min(size_h as i64 - 1) as i32;
    let ymin = ymin as i32;
    edge[0].idx = imin as i32;
    edge[1].idx = imin as i32;
    edge[0].ye = ymin;
    edge[1].ye = ymin;
    let mut y = ymin;
    edge[0].di = 1;
    edge[1].di = npts as i32 - 1;
    edge[0].x = -XY_ONE;
    edge[1].x = -XY_ONE;
    edge[0].dx = 0;
    edge[1].dx = 0;

    loop {
        if !matches!(line_type, LineType::AntiAliased)
            || y < ymax
            || y == ymin
        {
            for i in 0..2usize {
                if y >= edge[i].ye {
                    let mut idx0 = edge[i].idx;
                    let di = edge[i].di;
                    let mut idx = idx0 + di;
                    if idx >= npts as i32 {
                        idx -= npts as i32;
                    }

                    // 对齐 for(; edges-- > 0; )：判断后减，edges 可到 -1
                    while {
                        let cond = edges > 0;
                        edges -= 1;
                        cond
                    } {
                        let ty = ((v[idx as usize].y + delta) >> shift) as i32;
                        if ty > y {
                            let mut xs = v[idx0 as usize].x;
                            let mut xe = v[idx as usize].x;
                            if shift != XY_SHIFT {
                                xs <<= XY_SHIFT - shift;
                                xe <<= XY_SHIFT - shift;
                            }
                            edge[i].ye = ty;
                            edge[i].dx = ((xe - xs) * 2 + (ty as i64 - y as i64))
                                / (2 * (ty as i64 - y as i64));
                            edge[i].x = xs;
                            edge[i].idx = idx;
                            break;
                        }
                        idx0 = idx;
                        idx += di;
                        if idx >= npts as i32 {
                            idx -= npts as i32;
                        }
                    }
                }
            }
        }

        if edges < 0 {
            break;
        }

        if y >= 0 {
            let (mut left, mut right) = (0usize, 1usize);
            if edge[0].x > edge[1].x {
                left = 1;
                right = 0;
            }
            let xx1 = ((edge[left].x + delta1) >> XY_SHIFT) as i32;
            let xx2 = ((edge[right].x + delta2) >> XY_SHIFT) as i32;
            if xx2 >= 0 && xx1 < size_w {
                let xx1 = xx1.max(0);
                let xx2 = xx2.min(size_w - 1);
                cv.hline(y, xx1, xx2);
            }
        }

        edge[0].x += edge[0].dx;
        edge[1].x += edge[1].dx;
        y += 1;
        if y > ymax {
            break;
        }
    }
}

/// 多边形边（对齐 PolyEdge）。
#[derive(Clone, Copy, Default)]
pub(crate) struct PolyEdge {
    y0: i32,
    y1: i32,
    x: i64,
    dx: i64,
    next: i32, // edges 数组下标，-1 = 空
}

/// CollectPolyEdges（对齐 drawing.cpp:1262）。
pub(crate) fn collect_poly_edges(
    cv: &mut Canvas,
    v: &[Point2l],
    edges: &mut Vec<PolyEdge>,
    line_type: LineType,
    shift: i64,
    offset: Point,
) {
    let count = v.len();
    let delta = offset.y as i64 + ((1i64 << shift) >> 1);
    let mut pt0 = v[count - 1];
    pt0.x = (pt0.x + offset.x as i64) << (XY_SHIFT - shift);
    pt0.y = (pt0.y + delta) >> shift;
    let mut pt1;

    for i in 0..count {
        let mut t0;
        let mut t1;
        pt1 = v[i];
        pt1.x = (pt1.x + offset.x as i64) << (XY_SHIFT - shift);
        pt1.y = (pt1.y + delta) >> shift;

        let mut pt0c = pt0;
        let mut pt1c = pt1;

        if line_type.is_raster() {
            t0 = Point2l::new((pt0.x + (XY_ONE >> 1)) >> XY_SHIFT, pt0.y);
            t1 = Point2l::new((pt1.x + (XY_ONE >> 1)) >> XY_SHIFT, pt1.y);
            line_raster(
                cv,
                Point::new(t0.x as i32, t0.y as i32),
                Point::new(t1.x as i32, t1.y as i32),
                line_type,
            );

            // 用 clip 后端点构造更精确的 PolyEdge
            if t0.x as u32 >= cv.width as u32
                || t1.x as u32 >= cv.width as u32
                || t0.y as u32 >= cv.height as u32
                || t1.y as u32 >= cv.height as u32
            {
                // clipLine(img.size(), t0, t1)（整数域）
                let mut a = Point2l::from(Point::new(t0.x as i32, t0.y as i32));
                let mut b = Point2l::from(Point::new(t1.x as i32, t1.y as i32));
                clip_line(cv.width as i64, cv.height as i64, &mut a, &mut b);
                t0 = a;
                t1 = b;
                if t0.y != t1.y {
                    pt0c.y = t0.y;
                    pt1c.y = t1.y;
                }
            }
            pt0c.x = t0.x << XY_SHIFT;
            pt1c.x = t1.x << XY_SHIFT;
        } else {
            t0 = Point2l::new(pt0.x, pt0.y << XY_SHIFT);
            t1 = Point2l::new(pt1.x, pt1.y << XY_SHIFT);
            line_aa(cv, t0, t1);
        }

        if pt0.y == pt1.y {
            pt0 = pt1;
            continue;
        }

        let mut edge = PolyEdge::default();
        edge.dx = (pt1c.x - pt0c.x) / (pt1c.y - pt0c.y);
        if pt0.y < pt1.y {
            edge.y0 = pt0.y as i32;
            edge.y1 = pt1.y as i32;
            edge.x = pt0c.x + (pt0.y - pt0c.y) * edge.dx;
        } else {
            edge.y0 = pt1.y as i32;
            edge.y1 = pt0.y as i32;
            edge.x = pt1c.x + (pt1.y - pt1c.y) * edge.dx;
        }
        edges.push(edge);
        pt0 = pt1;
    }
}

/// FillEdgeCollection（对齐 drawing.cpp:1347，AET 扫描线填充）。
pub(crate) fn fill_edge_collection(cv: &mut Canvas, edges: &mut Vec<PolyEdge>) {
    let total = edges.len();
    let size_w = cv.width;
    let size_h = cv.height;
    let delta = XY_ONE - 1;

    if total < 2 {
        return;
    }

    let mut y_max = i32::MIN;
    let mut y_min = i32::MAX;
    let mut x_max = i64::MIN;
    let mut x_min = i64::MAX;

    for e1 in edges.iter().take(total) {
        debug_assert!(e1.y0 < e1.y1);
        let x1 = e1.x + (e1.y1 as i64 - e1.y0 as i64) * e1.dx;
        y_min = y_min.min(e1.y0);
        y_max = y_max.max(e1.y1);
        x_min = x_min.min(e1.x);
        x_max = x_max.max(e1.x);
        x_min = x_min.min(x1);
        x_max = x_max.max(x1);
    }

    if y_max < 0
        || y_min >= size_h
        || x_max < 0
        || x_min >= (size_w as i64) << XY_SHIFT
    {
        return;
    }

    // 按 y0 排序（稳定，对齐 std::sort + CmpEdges）
    edges.sort_by(|e1, e2| {
        if e1.y0 != e2.y0 {
            e1.y0.cmp(&e2.y0)
        } else {
            e1.x.cmp(&e2.x)
        }
    });

    // 哨兵（对齐 tmp.next = 0/null：next = -1 表示链表空）
    edges.push(PolyEdge { y0: i32::MAX, next: -1, ..Default::default() });
    // OpenCV 用指针链表；这里用下标链表：tmp = 哨兵节点下标（total），
    // next 字段存 edges 下标，-1 = 空
    let tmp = total as i32; // 哨兵节点下标
    let mut i = 0usize;
    let mut e_cur = 0usize;
    y_max = y_max.min(size_h);

    let mut y = edges[0].y0;
    while y < y_max {
        let mut draw = 0;
        let clipline = y < 0;
        let mut prelast = tmp;
        let mut last = edges[tmp as usize].next;
        let mut keep_prelast;

        loop {
            let last_valid = last >= 0;
            let e_y0 = edges[e_cur].y0;
            if !(last_valid || e_y0 == y) {
                break;
            }
            if last_valid && edges[last as usize].y1 == y {
                // 排除到达下端点的边
                let ln = edges[last as usize].next;
                edges[prelast as usize].next = ln;
                last = ln;
                continue;
            }
            keep_prelast = prelast;
            if last_valid && (edges[e_cur].y0 > y || edges[last as usize].x < edges[e_cur].x) {
                // 活动链表前进
                prelast = last;
                last = edges[last as usize].next;
            } else if i < total {
                // 插入新边到活动链表
                edges[prelast as usize].next = e_cur as i32;
                edges[e_cur].next = last;
                prelast = e_cur as i32;
                e_cur += 1;
                i += 1;
            } else {
                break;
            }

            if draw != 0 {
                if !clipline {
                    let (x1, x2);
                    if edges[keep_prelast as usize].x > edges[prelast as usize].x {
                        x1 = ((edges[prelast as usize].x + delta) >> XY_SHIFT) as i32;
                        x2 = (edges[keep_prelast as usize].x >> XY_SHIFT) as i32;
                    } else {
                        x1 = ((edges[keep_prelast as usize].x + delta) >> XY_SHIFT) as i32;
                        x2 = (edges[prelast as usize].x >> XY_SHIFT) as i32;
                    }
                    if x1 < size_w && x2 >= 0 {
                        let x1 = x1.max(0);
                        let x2 = x2.min(size_w - 1);
                        cv.hline(y, x1, x2);
                    }
                }
                edges[keep_prelast as usize].x += edges[keep_prelast as usize].dx;
                edges[prelast as usize].x += edges[prelast as usize].dx;
            }
            draw ^= 1;
        }

        // 冒泡排序活动链表（按 x）
        let mut keep_prelast_loop: i32 = -1;
        loop {
            let mut prelast = tmp;
            let mut last = edges[tmp as usize].next;
            let mut last_exchange: i32 = -1;

            while last != keep_prelast_loop && edges[last as usize].next != -1 {
                let te = edges[last as usize].next;
                if edges[last as usize].x > edges[te as usize].x {
                    // swap
                    edges[prelast as usize].next = te;
                    edges[last as usize].next = edges[te as usize].next;
                    edges[te as usize].next = last;
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
            if keep_prelast_loop == edges[tmp as usize].next || keep_prelast_loop == tmp {
                break;
            }
        }

        y += 1;
    }
    edges.pop();
}

/// SinTable（对齐 drawing.cpp:831，450 项，度数索引）。
const SIN_TABLE: [f32; 451] = [
    0.0000000, 0.0174524, 0.0348995, 0.0523360, 0.0697565, 0.0871557,
    0.1045285, 0.1218693, 0.1391731, 0.1564345, 0.1736482, 0.1908090,
    0.2079117, 0.2249511, 0.2419219, 0.2588190, 0.2756374, 0.2923717,
    0.3090170, 0.3255682, 0.3420201, 0.3583679, 0.3746066, 0.3907311,
    0.4067366, 0.4226183, 0.4383711, 0.4539905, 0.4694716, 0.4848096,
    0.5000000, 0.5150381, 0.5299193, 0.5446390, 0.5591929, 0.5735764,
    0.5877853, 0.6018150, 0.6156615, 0.6293204, 0.6427876, 0.6560590,
    0.6691306, 0.6819984, 0.6946584, 0.7071068, 0.7193398, 0.7313537,
    0.7431448, 0.7547096, 0.7660444, 0.7771460, 0.7880108, 0.7986355,
    0.8090170, 0.8191520, 0.8290376, 0.8386706, 0.8480481, 0.8571673,
    0.8660254, 0.8746197, 0.8829476, 0.8910065, 0.8987940, 0.9063078,
    0.9135455, 0.9205049, 0.9271839, 0.9335804, 0.9396926, 0.9455186,
    0.9510565, 0.9563048, 0.9612617, 0.9659258, 0.9702957, 0.9743701,
    0.9781476, 0.9816272, 0.9848078, 0.9876883, 0.9902681, 0.9925462,
    0.9945219, 0.9961947, 0.9975641, 0.9986295, 0.9993908, 0.9998477,
    1.0000000, 0.9998477, 0.9993908, 0.9986295, 0.9975641, 0.9961947,
    0.9945219, 0.9925462, 0.9902681, 0.9876883, 0.9848078, 0.9816272,
    0.9781476, 0.9743701, 0.9702957, 0.9659258, 0.9612617, 0.9563048,
    0.9510565, 0.9455186, 0.9396926, 0.9335804, 0.9271839, 0.9205049,
    0.9135455, 0.9063078, 0.8987940, 0.8910065, 0.8829476, 0.8746197,
    0.8660254, 0.8571673, 0.8480481, 0.8386706, 0.8290376, 0.8191520,
    0.8090170, 0.7986355, 0.7880108, 0.7771460, 0.7660444, 0.7547096,
    0.7431448, 0.7313537, 0.7193398, 0.7071068, 0.6946584, 0.6819984,
    0.6691306, 0.6560590, 0.6427876, 0.6293204, 0.6156615, 0.6018150,
    0.5877853, 0.5735764, 0.5591929, 0.5446390, 0.5299193, 0.5150381,
    0.5000000, 0.4848096, 0.4694716, 0.4539905, 0.4383711, 0.4226183,
    0.4067366, 0.3907311, 0.3746066, 0.3583679, 0.3420201, 0.3255682,
    0.3090170, 0.2923717, 0.2756374, 0.2588190, 0.2419219, 0.2249511,
    0.2079117, 0.1908090, 0.1736482, 0.1564345, 0.1391731, 0.1218693,
    0.1045285, 0.0871557, 0.0697565, 0.0523360, 0.0348995, 0.0174524,
    0.0000000, -0.0174524, -0.0348995, -0.0523360, -0.0697565, -0.0871557,
    -0.1045285, -0.1218693, -0.1391731, -0.1564345, -0.1736482, -0.1908090,
    -0.2079117, -0.2249511, -0.2419219, -0.2588190, -0.2756374, -0.2923717,
    -0.3090170, -0.3255682, -0.3420201, -0.3583679, -0.3746066, -0.3907311,
    -0.4067366, -0.4226183, -0.4383711, -0.4539905, -0.4694716, -0.4848096,
    -0.5000000, -0.5150381, -0.5299193, -0.5446390, -0.5591929, -0.5735764,
    -0.5877853, -0.6018150, -0.6156615, -0.6293204, -0.6427876, -0.6560590,
    -0.6691306, -0.6819984, -0.6946584, -0.7071068, -0.7193398, -0.7313537,
    -0.7431448, -0.7547096, -0.7660444, -0.7771460, -0.7880108, -0.7986355,
    -0.8090170, -0.8191520, -0.8290376, -0.8386706, -0.8480481, -0.8571673,
    -0.8660254, -0.8746197, -0.8829476, -0.8910065, -0.8987940, -0.9063078,
    -0.9135455, -0.9205049, -0.9271839, -0.9335804, -0.9396926, -0.9455186,
    -0.9510565, -0.9563048, -0.9612617, -0.9659258, -0.9702957, -0.9743701,
    -0.9781476, -0.9816272, -0.9848078, -0.9876883, -0.9902681, -0.9925462,
    -0.9945219, -0.9961947, -0.9975641, -0.9986295, -0.9993908, -0.9998477,
    -1.0000000, -0.9998477, -0.9993908, -0.9986295, -0.9975641, -0.9961947,
    -0.9945219, -0.9925462, -0.9902681, -0.9876883, -0.9848078, -0.9816272,
    -0.9781476, -0.9743701, -0.9702957, -0.9659258, -0.9612617, -0.9563048,
    -0.9510565, -0.9455186, -0.9396926, -0.9335804, -0.9271839, -0.9205049,
    -0.9135455, -0.9063078, -0.8987940, -0.8910065, -0.8829476, -0.8746197,
    -0.8660254, -0.8571673, -0.8480481, -0.8386706, -0.8290376, -0.8191520,
    -0.8090170, -0.7986355, -0.7880108, -0.7771460, -0.7660444, -0.7547096,
    -0.7431448, -0.7313537, -0.7193398, -0.7071068, -0.6946584, -0.6819984,
    -0.6691306, -0.6560590, -0.6427876, -0.6293204, -0.6156615, -0.6018150,
    -0.5877853, -0.5735764, -0.5591929, -0.5446390, -0.5299193, -0.5150381,
    -0.5000000, -0.4848096, -0.4694716, -0.4539905, -0.4383711, -0.4226183,
    -0.4067366, -0.3907311, -0.3746066, -0.3583679, -0.3420201, -0.3255682,
    -0.3090170, -0.2923717, -0.2756374, -0.2588190, -0.2419219, -0.2249511,
    -0.2079117, -0.1908090, -0.1736482, -0.1564345, -0.1391731, -0.1218693,
    -0.1045285, -0.0871557, -0.0697565, -0.0523360, -0.0348995, -0.0174524,
    -0.0000000, 0.0174524, 0.0348995, 0.0523360, 0.0697565, 0.0871557,
    0.1045285, 0.1218693, 0.1391731, 0.1564345, 0.1736482, 0.1908090,
    0.2079117, 0.2249511, 0.2419219, 0.2588190, 0.2756374, 0.2923717,
    0.3090170, 0.3255682, 0.3420201, 0.3583679, 0.3746066, 0.3907311,
    0.4067366, 0.4226183, 0.4383711, 0.4539905, 0.4694716, 0.4848096,
    0.5000000, 0.5150381, 0.5299193, 0.5446390, 0.5591929, 0.5735764,
    0.5877853, 0.6018150, 0.6156615, 0.6293204, 0.6427876, 0.6560590,
    0.6691306, 0.6819984, 0.6946584, 0.7071068, 0.7193398, 0.7313537,
    0.7431448, 0.7547096, 0.7660444, 0.7771460, 0.7880108, 0.7986355,
    0.8090170, 0.8191520, 0.8290376, 0.8386706, 0.8480481, 0.8571673,
    0.8660254, 0.8746197, 0.8829476, 0.8910065, 0.8987940, 0.9063078,
    0.9135455, 0.9205049, 0.9271839, 0.9335804, 0.9396926, 0.9455186,
    0.9510565, 0.9563048, 0.9612617, 0.9659258, 0.9702957, 0.9743701,
    0.9781476, 0.9816272, 0.9848078, 0.9876883, 0.9902681, 0.9925462,
    0.9945219, 0.9961947, 0.9975641, 0.9986295, 0.9993908, 0.9998477,
    1.0000000,
];

/// sincos（对齐 drawing.cpp:911）。注意 OpenCV 参数序：
/// `sincos(angle, alpha, beta)` 中 **alpha = cos，beta = sin**。
fn sincos(angle: i32) -> (f32, f32) {
    let angle = angle + if angle < 0 { 360 } else { 0 };
    (SIN_TABLE[(450 - angle) as usize], SIN_TABLE[angle as usize])
}

/// ellipse2Poly（Point2d 版，对齐 drawing.cpp:949）。
pub(crate) fn ellipse2_poly(
    center: (f64, f64),
    axes: (f64, f64),
    mut angle: i32,
    mut arc_start: i32,
    mut arc_end: i32,
    delta: i32,
) -> Vec<(f64, f64)> {
    debug_assert!(0 < delta && delta <= 180);

    while angle < 0 {
        angle += 360;
    }
    while angle > 360 {
        angle -= 360;
    }

    if arc_start > arc_end {
        std::mem::swap(&mut arc_start, &mut arc_end);
    }
    while arc_start < 0 {
        arc_start += 360;
        arc_end += 360;
    }
    while arc_end > 360 {
        arc_end -= 360;
        arc_start -= 360;
    }
    if arc_end - arc_start > 360 {
        arc_start = 0;
        arc_end = 360;
    }
    let (alpha, beta) = sincos(angle);
    let mut pts = Vec::new();

    let mut i = arc_start;
    while i < arc_end + delta {
        let mut a = i;
        if a > arc_end {
            a = arc_end;
        }
        if a < 0 {
            a += 360;
        }
        let x = axes.0 * SIN_TABLE[(450 - a) as usize] as f64;
        let y = axes.1 * SIN_TABLE[a as usize] as f64;
        pts.push((
            center.0 + x * alpha as f64 - y * beta as f64,
            center.1 + x * beta as f64 + y * alpha as f64,
        ));
        i += delta;
    }
    pts
}

/// EllipseEx（对齐 drawing.cpp:1013）。
pub(crate) fn ellipse_ex(
    cv: &mut Canvas,
    center: Point2l,
    axes: Size2l,
    angle: i32,
    arc_start: i32,
    arc_end: i32,
    thickness: i32,
    line_type: LineType,
) {
    let axes_w = axes.w.abs();
    let axes_h = axes.h.abs();
    let mut delta = (axes_w.max(axes_h) + (XY_ONE >> 1)) >> XY_SHIFT;
    delta = if delta < 3 {
        90
    } else if delta < 10 {
        30
    } else if delta < 15 {
        18
    } else {
        5
    };

    let v_f = ellipse2_poly(
        (center.x as f64, center.y as f64),
        (axes_w as f64, axes_h as f64),
        angle,
        arc_start,
        arc_end,
        delta as i32,
    );

    let mut v: Vec<Point2l> = Vec::with_capacity(v_f.len());
    let mut prev = Point2l::new(i64::MIN, i64::MIN);
    for (x, y) in v_f {
        let mut pt = Point2l::new(
            ((x / XY_ONE as f64).round() as i64) << XY_SHIFT,
            ((y / XY_ONE as f64).round() as i64) << XY_SHIFT,
        );
        pt.x += (x - pt.x as f64).round() as i64;
        pt.y += (y - pt.y as f64).round() as i64;
        if pt != prev {
            v.push(pt);
            prev = pt;
        }
    }

    if v.len() <= 1 {
        v.push(center);
        v.push(center);
    }

    if thickness >= 0 {
        poly_line(cv, &v, false, thickness, line_type, XY_SHIFT);
    } else if arc_end - arc_start >= 360 {
        fill_convex_poly_fixed(cv, &v, line_type, XY_SHIFT);
    } else {
        v.push(center);
        let mut edges = Vec::new();
        collect_poly_edges(cv, &v, &mut edges, line_type, XY_SHIFT, Point::new(0, 0));
        fill_edge_collection(cv, &mut edges);
    }
}

// ---------------------------------------------------------------------------
// 公开 API（对齐 cv::line/rectangle/circle/ellipse/polylines/fillConvexPoly/
// fillPoly/drawContours）
// ---------------------------------------------------------------------------

/// 画直线（对齐 cv::line）。`thickness < 0` 无意义（line 要求 > 0）。
pub fn line<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    pt1: Point,
    pt2: Point,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!((1..=MAX_THICKNESS).contains(&thickness), "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    thick_line(&mut cv, pt1.into(), pt2.into(), thickness, line_type, 3, shift);
}

/// 画直线（3 通道）。
pub fn line_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    pt1: Point,
    pt2: Point,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!((1..=MAX_THICKNESS).contains(&thickness), "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    thick_line(&mut cv, pt1.into(), pt2.into(), thickness, line_type, 3, shift);
}

/// 画矩形（对齐 cv::rectangle）。`thickness < 0` 填充。
pub fn rectangle<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    pt1: Point,
    pt2: Point,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(thickness <= MAX_THICKNESS, "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    let pt = [
        Point2l::from(pt1),
        Point2l::new(pt2.x as i64, pt1.y as i64),
        Point2l::from(pt2),
        Point2l::new(pt1.x as i64, pt2.y as i64),
    ];
    if thickness >= 0 {
        poly_line(&mut cv, &pt, true, thickness, line_type, shift);
    } else {
        fill_convex_poly_fixed(&mut cv, &pt, line_type, shift);
    }
}

/// 画矩形（3 通道）。
pub fn rectangle_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    pt1: Point,
    pt2: Point,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(thickness <= MAX_THICKNESS, "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    let pt = [
        Point2l::from(pt1),
        Point2l::new(pt2.x as i64, pt1.y as i64),
        Point2l::from(pt2),
        Point2l::new(pt1.x as i64, pt2.y as i64),
    ];
    if thickness >= 0 {
        poly_line(&mut cv, &pt, true, thickness, line_type, shift);
    } else {
        fill_convex_poly_fixed(&mut cv, &pt, line_type, shift);
    }
}

/// 画圆（对齐 cv::circle）。`thickness < 0` 填充。
pub fn circle<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    center: Point,
    radius: i32,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(radius >= 0 && thickness <= MAX_THICKNESS, "radius/thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    if thickness > 1 || !matches!(line_type, LineType::Connected8) || shift > 0 {
        let mut c = Point2l::from(center);
        let mut r = radius as i64;
        c.x <<= XY_SHIFT - shift;
        c.y <<= XY_SHIFT - shift;
        r <<= XY_SHIFT - shift;
        ellipse_ex(&mut cv, c, Size2l { w: r, h: r }, 0, 0, 360, thickness, line_type);
    } else {
        circle_raster(&mut cv, center, radius, thickness < 0);
    }
}

/// 画圆（3 通道）。
pub fn circle_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    center: Point,
    radius: i32,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(radius >= 0 && thickness <= MAX_THICKNESS, "radius/thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    if thickness > 1 || !matches!(line_type, LineType::Connected8) || shift > 0 {
        let mut c = Point2l::from(center);
        let mut r = radius as i64;
        c.x <<= XY_SHIFT - shift;
        c.y <<= XY_SHIFT - shift;
        r <<= XY_SHIFT - shift;
        ellipse_ex(&mut cv, c, Size2l { w: r, h: r }, 0, 0, 360, thickness, line_type);
    } else {
        circle_raster(&mut cv, center, radius, thickness < 0);
    }
}

/// 画椭圆（对齐 cv::ellipse，角度制）。
pub fn ellipse<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    center: Point,
    axes: (i32, i32),
    angle: f64,
    start_angle: f64,
    end_angle: f64,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(axes.0 >= 0 && axes.1 >= 0 && thickness <= MAX_THICKNESS, "axes/thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    let angle_i = angle.round() as i32;
    let start_i = start_angle.round() as i32;
    let end_i = end_angle.round() as i32;
    let mut c = Point2l::from(center);
    let mut a = Size2l { w: axes.0 as i64, h: axes.1 as i64 };
    c.x <<= XY_SHIFT - shift;
    c.y <<= XY_SHIFT - shift;
    a.w <<= XY_SHIFT - shift;
    a.h <<= XY_SHIFT - shift;
    ellipse_ex(&mut cv, c, a, angle_i, start_i, end_i, thickness, line_type);
}

/// 画椭圆（3 通道）。
pub fn ellipse_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    center: Point,
    axes: (i32, i32),
    angle: f64,
    start_angle: f64,
    end_angle: f64,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!(axes.0 >= 0 && axes.1 >= 0 && thickness <= MAX_THICKNESS, "axes/thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    let angle_i = angle.round() as i32;
    let start_i = start_angle.round() as i32;
    let end_i = end_angle.round() as i32;
    let mut c = Point2l::from(center);
    let mut a = Size2l { w: axes.0 as i64, h: axes.1 as i64 };
    c.x <<= XY_SHIFT - shift;
    c.y <<= XY_SHIFT - shift;
    a.w <<= XY_SHIFT - shift;
    a.h <<= XY_SHIFT - shift;
    ellipse_ex(&mut cv, c, a, angle_i, start_i, end_i, thickness, line_type);
}

/// 画多边形折线（对齐 cv::polylines）。
pub fn polylines<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    pts: &[&[Point]],
    is_closed: bool,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!((0..=MAX_THICKNESS).contains(&thickness), "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    for contour in pts {
        let v: Vec<Point2l> = contour.iter().map(|&p| p.into()).collect();
        poly_line(&mut cv, &v, is_closed, thickness, line_type, shift);
    }
}

/// 画多边形折线（3 通道）。
pub fn polylines_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    pts: &[&[Point]],
    is_closed: bool,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    shift: i64,
) {
    assert!((0..=MAX_THICKNESS).contains(&thickness), "thickness out of range");
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    for contour in pts {
        let v: Vec<Point2l> = contour.iter().map(|&p| p.into()).collect();
        poly_line(&mut cv, &v, is_closed, thickness, line_type, shift);
    }
}

/// 填充凸多边形（对齐 cv::fillConvexPoly）。
pub fn fill_convex_poly<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    pts: &[Point],
    color: [T; 1],
    line_type: LineType,
    shift: i64,
) {
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    if pts.is_empty() {
        return;
    }
    let mut cv = Canvas::new(img, color);
    let v: Vec<Point2l> = pts.iter().map(|&p| p.into()).collect();
    fill_convex_poly_fixed(&mut cv, &v, line_type, shift);
}

/// 填充凸多边形（3 通道）。
pub fn fill_convex_poly_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    pts: &[Point],
    color: [T; 3],
    line_type: LineType,
    shift: i64,
) {
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    if pts.is_empty() {
        return;
    }
    let mut cv = Canvas::new3(img, color);
    let v: Vec<Point2l> = pts.iter().map(|&p| p.into()).collect();
    fill_convex_poly_fixed(&mut cv, &v, line_type, shift);
}

/// 填充任意多边形（对齐 cv::fillPoly，偶奇规则）。
pub fn fill_poly<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    pts: &[&[Point]],
    color: [T; 1],
    line_type: LineType,
    shift: i64,
    offset: Point,
) {
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new(img, color);
    let mut edges = Vec::new();
    let mut total = 0usize;
    for c in pts {
        total += c.len();
    }
    edges.reserve(total + 1);
    for c in pts {
        if c.is_empty() {
            continue;
        }
        let v: Vec<Point2l> = c.iter().map(|&p| p.into()).collect();
        collect_poly_edges(&mut cv, &v, &mut edges, line_type, shift, offset);
    }
    fill_edge_collection(&mut cv, &mut edges);
}

/// 填充任意多边形（3 通道）。
pub fn fill_poly_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    pts: &[&[Point]],
    color: [T; 3],
    line_type: LineType,
    shift: i64,
    offset: Point,
) {
    assert!((0..=XY_SHIFT).contains(&shift), "shift out of range");
    let mut cv = Canvas::new3(img, color);
    let mut edges = Vec::new();
    let mut total = 0usize;
    for c in pts {
        total += c.len();
    }
    edges.reserve(total + 1);
    for c in pts {
        if c.is_empty() {
            continue;
        }
        let v: Vec<Point2l> = c.iter().map(|&p| p.into()).collect();
        collect_poly_edges(&mut cv, &v, &mut edges, line_type, shift, offset);
    }
    fill_edge_collection(&mut cv, &mut edges);
}

/// 绘制轮廓（对齐 cv::drawContours）。
///
/// - `contour_idx >= 0`：只画该轮廓；否则画全部
/// - `hierarchy`：Some 时按 maxLevel 过滤（父链深度）
/// - `thickness < 0`：填充（fillPoly 语义）
pub fn draw_contours<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    contours: &[Vec<Point>],
    hierarchy: Option<&[crate::imgproc::contours::Hierarchy]>,
    contour_idx: i32,
    color: [T; 1],
    thickness: i32,
    line_type: LineType,
    max_level: i32,
    offset: Point,
) {
    assert!(thickness <= MAX_THICKNESS, "thickness out of range");
    let ncontours = contours.len();
    if ncontours == 0 {
        return;
    }
    let mut cv = Canvas::new(img, color);

    let (i_start, end) = if contour_idx >= 0 {
        (contour_idx as usize, contour_idx as usize + 1)
    } else {
        (0, ncontours)
    };

    // 收集要画的轮廓索引（对齐 indexesToFill 逻辑）
    let mut indexes_to_fill: Vec<usize> = Vec::new();
    let hier = hierarchy.filter(|h| h.len() == ncontours);
    match hier {
        None => {
            indexes_to_fill.extend(i_start..end);
        }
        Some(h) => {
            if max_level == 0 {
                indexes_to_fill.extend(i_start..end);
            } else {
                // 栈遍历（对齐 std::stack 逻辑）
                let mut stack: Vec<i32> = Vec::new();
                for i in i_start..end {
                    if h[i].parent < 0 || contour_idx >= 0 {
                        stack.push(i as i32);
                    }
                }
                while let Some(cur) = stack.pop() {
                    // 深度
                    let mut cur_level = -1i32;
                    let mut par = cur;
                    while par >= 0 {
                        par = h[par as usize].parent;
                        cur_level += 1;
                    }
                    if cur_level <= max_level {
                        indexes_to_fill.push(cur as usize);
                    }
                    let mut next = h[cur as usize].first_child;
                    while next > 0 {
                        stack.push(next);
                        next = h[next as usize].next;
                    }
                }
            }
        }
    }

    if thickness < 0 {
        // fillPoly
        let mut edges = Vec::new();
        let mut total = 0usize;
        for &idx in &indexes_to_fill {
            total += contours[idx].len();
        }
        edges.reserve(total + 1);
        for &idx in &indexes_to_fill {
            let c = &contours[idx];
            if c.is_empty() {
                continue;
            }
            let v: Vec<Point2l> = c.iter().map(|&p| p.into()).collect();
            collect_poly_edges(&mut cv, &v, &mut edges, line_type, 0, offset);
        }
        fill_edge_collection(&mut cv, &mut edges);
    } else {
        for &idx in &indexes_to_fill {
            let cnt = &contours[idx];
            if cnt.is_empty() {
                continue;
            }
            let npoints = cnt.len();
            for j in 0..npoints {
                let is_last = j == npoints - 1;
                let pt1 = cnt[j] + offset;
                let pt2 = cnt[if is_last { 0 } else { j + 1 }] + offset;
                thick_line(
                    &mut cv,
                    pt1.into(),
                    pt2.into(),
                    thickness,
                    line_type,
                    2,
                    0,
                );
            }
        }
    }
}

/// 绘制轮廓（3 通道）。
pub fn draw_contours_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    contours: &[Vec<Point>],
    hierarchy: Option<&[crate::imgproc::contours::Hierarchy]>,
    contour_idx: i32,
    color: [T; 3],
    thickness: i32,
    line_type: LineType,
    max_level: i32,
    offset: Point,
) {
    assert!(thickness <= MAX_THICKNESS, "thickness out of range");
    let ncontours = contours.len();
    if ncontours == 0 {
        return;
    }
    let mut cv = Canvas::new3(img, color);

    let (i_start, end) = if contour_idx >= 0 {
        (contour_idx as usize, contour_idx as usize + 1)
    } else {
        (0, ncontours)
    };

    let mut indexes_to_fill: Vec<usize> = Vec::new();
    let hier = hierarchy.filter(|h| h.len() == ncontours);
    match hier {
        None => {
            indexes_to_fill.extend(i_start..end);
        }
        Some(h) => {
            if max_level == 0 {
                indexes_to_fill.extend(i_start..end);
            } else {
                let mut stack: Vec<i32> = Vec::new();
                for i in i_start..end {
                    if h[i].parent < 0 || contour_idx >= 0 {
                        stack.push(i as i32);
                    }
                }
                while let Some(cur) = stack.pop() {
                    let mut cur_level = -1i32;
                    let mut par = cur;
                    while par >= 0 {
                        par = h[par as usize].parent;
                        cur_level += 1;
                    }
                    if cur_level <= max_level {
                        indexes_to_fill.push(cur as usize);
                    }
                    let mut next = h[cur as usize].first_child;
                    while next > 0 {
                        stack.push(next);
                        next = h[next as usize].next;
                    }
                }
            }
        }
    }

    if thickness < 0 {
        let mut edges = Vec::new();
        let mut total = 0usize;
        for &idx in &indexes_to_fill {
            total += contours[idx].len();
        }
        edges.reserve(total + 1);
        for &idx in &indexes_to_fill {
            let c = &contours[idx];
            if c.is_empty() {
                continue;
            }
            let v: Vec<Point2l> = c.iter().map(|&p| p.into()).collect();
            collect_poly_edges(&mut cv, &v, &mut edges, line_type, 0, offset);
        }
        fill_edge_collection(&mut cv, &mut edges);
    } else {
        for &idx in &indexes_to_fill {
            let cnt = &contours[idx];
            if cnt.is_empty() {
                continue;
            }
            let npoints = cnt.len();
            for j in 0..npoints {
                let is_last = j == npoints - 1;
                let pt1 = cnt[j] + offset;
                let pt2 = cnt[if is_last { 0 } else { j + 1 }] + offset;
                thick_line(
                    &mut cv,
                    pt1.into(),
                    pt2.into(),
                    thickness,
                    line_type,
                    2,
                    0,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_raster_8_connected() {
        let mut img = Image::<u8, 1>::from_pixel(8, 8, [0]);
        line(&mut img, Point::new(1, 1), Point::new(6, 4), [255], 1, LineType::Connected8, 0);
        let d = img.data();
        // 端点必画；8 连通 dx=5 > dy=3 → 6 个点（dx+1）
        assert_eq!(d[1 * 8 + 1], 255, "起点");
        assert_eq!(d[4 * 8 + 6], 255, "终点");
        let count: usize = d.iter().filter(|&&v| v == 255).count();
        assert_eq!(count, 6);
    }

    #[test]
    fn line_raster_4_connected() {
        let mut img = Image::<u8, 1>::from_pixel(8, 8, [0]);
        line(&mut img, Point::new(1, 1), Point::new(4, 4), [255], 1, LineType::Connected4, 0);
        let d = img.data();
        // 4 连通对角线：每步 x 或 y 之一前进
        let count: usize = d.iter().filter(|&&v| v == 255).count();
        assert_eq!(count, 7); // dx+dy+1 = 3+3+1
    }

    #[test]
    fn circle_fill_matches_opencv_shape() {
        // 3×3 方块中心 (2,2) 半径 1 填充圆 = 十字 5 点（对齐 OpenCV Circle）
        let mut img = Image::<u8, 1>::from_pixel(5, 5, [0]);
        circle(&mut img, Point::new(2, 2), 1, [200], -1, LineType::Connected8, 0);
        let d = img.data();
        let mut pts: Vec<(usize, usize)> =
            (0..25).filter(|&i| d[i] == 200).map(|i| (i % 5, i / 5)).collect();
        pts.sort();
        assert_eq!(pts, vec![(1, 2), (2, 1), (2, 2), (2, 3), (3, 2)]);
    }

    #[test]
    fn fill_convex_poly_square() {
        let mut img = Image::<u8, 1>::from_pixel(8, 8, [0]);
        let pts = [Point::new(2, 2), Point::new(5, 2), Point::new(5, 5), Point::new(2, 5)];
        fill_convex_poly(&mut img, &pts, [255], LineType::Connected8, 0);
        let d = img.data();
        for y in 2..=5 {
            for x in 2..=5 {
                assert_eq!(d[y * 8 + x], 255, "({x},{y})");
            }
        }
    }

    #[test]
    fn fill_poly_even_odd() {
        // 两个嵌套三角形：偶奇规则 → 环形填充
        let mut img = Image::<u8, 1>::from_pixel(20, 20, [0]);
        let outer = vec![
            Point::new(2, 2),
            Point::new(17, 2),
            Point::new(2, 17),
        ];
        let inner = vec![
            Point::new(6, 6),
            Point::new(6, 12),
            Point::new(12, 6),
        ];
        fill_poly(&mut img, &[&outer, &inner], [255], LineType::Connected8, 0, Point::new(0, 0));
        let d = img.data();
        assert_eq!(d[4 * 20 + 4], 255, "外环内");
        assert_eq!(d[8 * 20 + 8], 0, "内环洞");
    }

    #[test]
    fn rectangle_outline_and_fill() {
        let mut img = Image::<u8, 1>::from_pixel(10, 10, [0]);
        rectangle(&mut img, Point::new(2, 2), Point::new(7, 6), [255], 1, LineType::Connected8, 0);
        let d = img.data();
        // 边框
        assert_eq!(d[2 * 10 + 2], 255);
        assert_eq!(d[6 * 10 + 7], 255);
        // 内部不画
        assert_eq!(d[4 * 10 + 4], 0);

        let mut img2 = Image::<u8, 1>::from_pixel(10, 10, [0]);
        rectangle(&mut img2, Point::new(2, 2), Point::new(7, 6), [255], -1, LineType::Connected8, 0);
        assert_eq!(img2.data()[4 * 10 + 4], 255, "填充内部");
    }

    #[test]
    fn clip_line_basic() {
        let mut a = Point2l::new(-5, 5);
        let mut b = Point2l::new(15, 5);
        assert!(clip_line(10, 10, &mut a, &mut b));
        assert_eq!(a.x, 0);
        assert_eq!(b.x, 9);

        let mut c = Point2l::new(-5, -5);
        let mut d = Point2l::new(-1, -1);
        assert!(!clip_line(10, 10, &mut c, &mut d));
    }

    #[test]
    fn ellipse2_poly_angles() {
        // 0° 点 = center + (axes.w, 0)
        let pts = ellipse2_poly((0.0, 0.0), (10.0, 5.0), 0, 0, 90, 90);
        assert!((pts[0].0 - 10.0).abs() < 1e-4 && pts[0].1.abs() < 1e-4);
        // 90° 点 = center + (0, axes.h)
        assert!(pts[1].0.abs() < 1e-4 && (pts[1].1 - 5.0).abs() < 1e-4);
    }
}
