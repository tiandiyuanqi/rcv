use crate::core::point::Point;

/// 尺寸，对应 `cv::Size2i`。`width`/`height` 非负才有意义，负值视为空。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    /// 面积（`i64` 避免溢出）。
    pub const fn area(&self) -> i64 {
        self.width as i64 * self.height as i64
    }

    /// 是否为空（任一维度 ≤ 0）。
    pub const fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

/// 矩形，对应 `cv::Rect2i`。原点可为任意 `i32`；`width`/`height` ≤ 0 视为空矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 面积（`i64` 乘积，避免溢出）。
    pub const fn area(&self) -> i64 {
        self.width as i64 * self.height as i64
    }

    /// 是否为空（宽或高 ≤ 0）。
    pub const fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    /// 是否包含点 `p`（左闭右开，与 `cv::Rect::contains` 一致）。
    pub const fn contains(&self, p: Point) -> bool {
        (p.x as i64) >= (self.x as i64)
            && (p.y as i64) >= (self.y as i64)
            && (p.x as i64) < (self.x as i64) + (self.width as i64)
            && (p.y as i64) < (self.y as i64) + (self.height as i64)
    }

    /// 转为尺寸。
    pub const fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_basics() {
        let s = Size::new(30, 40);
        assert_eq!(s.area(), 1200);
        assert!(!s.is_empty());
        assert!(Size::new(0, 5).is_empty());
        assert!(Size::new(-1, 5).is_empty());
    }

    #[test]
    fn rect_area_and_empty() {
        let r = Rect::new(10, 20, 30, 40);
        assert_eq!(r.area(), 1200);
        assert!(!r.is_empty());
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert!(Rect::new(0, 0, -3, 5).is_empty());
    }

    #[test]
    fn rect_contains_half_open() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(Point::new(10, 20)));
        assert!(r.contains(Point::new(39, 59)));
        assert!(!r.contains(Point::new(40, 59)));
        assert!(!r.contains(Point::new(9, 20)));
        assert!(!r.contains(Point::new(10, 60)));
    }

    #[test]
    fn rect_size() {
        let r = Rect::new(1, 2, 3, 4);
        assert_eq!(r.size(), Size::new(3, 4));
    }
}
