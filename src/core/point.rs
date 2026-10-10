use std::ops::{Add, AddAssign, Sub, SubAssign};

/// 二维整数点，对应 `cv::Point2i`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl From<(i32, i32)> for Point {
    fn from((x, y): (i32, i32)) -> Self {
        Self { x, y }
    }
}

impl Add for Point {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl Sub for Point {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl AddAssign for Point {
    fn add_assign(&mut self, rhs: Self) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl SubAssign for Point {
    fn sub_assign(&mut self, rhs: Self) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_and_default() {
        let p = Point::new(3, -4);
        assert_eq!(p.x, 3);
        assert_eq!(p.y, -4);
        assert_eq!(Point::default(), Point::new(0, 0));
    }

    #[test]
    fn from_tuple() {
        let p: Point = (5, 6).into();
        assert_eq!(p, Point::new(5, 6));
    }

    #[test]
    fn arithmetic() {
        let a = Point::new(10, 20);
        let b = Point::new(3, 4);
        assert_eq!(a + b, Point::new(13, 24));
        assert_eq!(a - b, Point::new(7, 16));
        let mut c = a;
        c += b;
        assert_eq!(c, Point::new(13, 24));
        c -= b;
        assert_eq!(c, a);
    }
}
