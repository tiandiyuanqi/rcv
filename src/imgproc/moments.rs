//! 轮廓矩（`cv::moments` 轮廓路径 + `cv::HuMoments`），对齐 OpenCV 5
//! `modules/geometry/src/moments.cpp` 的 `contourMoments` 与 `HuMoments`。
//!
//! 位级对齐：累加顺序、`DBL_EPSILON`/`FLT_EPSILON` 阈值、归一化公式均与
//! OpenCV 逐位一致。

use crate::core::point::Point;

/// 图像矩（对齐 `cv::Moments`）。
///
/// - `m*`：空间矩（spatial moments）
/// - `mu*`：中心矩（central moments）
/// - `nu*`：归一化中心矩（normalized central moments）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moments {
    // 空间矩
    pub m00: f64,
    pub m10: f64,
    pub m01: f64,
    pub m20: f64,
    pub m11: f64,
    pub m02: f64,
    pub m30: f64,
    pub m21: f64,
    pub m12: f64,
    pub m03: f64,
    // 中心矩
    pub mu20: f64,
    pub mu11: f64,
    pub mu02: f64,
    pub mu30: f64,
    pub mu21: f64,
    pub mu12: f64,
    pub mu03: f64,
    // 归一化中心矩
    pub nu20: f64,
    pub nu11: f64,
    pub nu02: f64,
    pub nu30: f64,
    pub nu21: f64,
    pub nu12: f64,
    pub nu03: f64,
}

impl Default for Moments {
    fn default() -> Self {
        Self {
            m00: 0.0, m10: 0.0, m01: 0.0, m20: 0.0, m11: 0.0, m02: 0.0,
            m30: 0.0, m21: 0.0, m12: 0.0, m03: 0.0,
            mu20: 0.0, mu11: 0.0, mu02: 0.0, mu30: 0.0, mu21: 0.0, mu12: 0.0, mu03: 0.0,
            nu20: 0.0, nu11: 0.0, nu02: 0.0, nu30: 0.0, nu21: 0.0, nu12: 0.0, nu03: 0.0,
        }
    }
}

impl Moments {
    /// 由 10 个空间矩构造，并计算中心矩与归一化中心矩
    ///（对齐 `cv::Moments(double, ...)` 全参构造）。
    pub fn from_spatial(
        m00: f64, m10: f64, m01: f64, m20: f64, m11: f64,
        m02: f64, m30: f64, m21: f64, m12: f64, m03: f64,
    ) -> Self {
        let mut m = Self {
            m00, m10, m01, m20, m11, m02, m30, m21, m12, m03,
            ..Self::default()
        };
        m.complete_state();
        m
    }

    /// 由空间矩计算中心矩与归一化中心矩（对齐 `completeMomentState`）。
    fn complete_state(&mut self) {
        let mut cx = 0.0;
        let mut cy = 0.0;
        let mut inv_m00 = 0.0;

        if self.m00.abs() > f64::EPSILON {
            inv_m00 = 1.0 / self.m00;
            cx = self.m10 * inv_m00;
            cy = self.m01 * inv_m00;
        }

        let mu20 = self.m20 - self.m10 * cx;
        let mut mu11 = self.m11 - self.m10 * cy;
        let mu02 = self.m02 - self.m01 * cy;

        self.mu20 = mu20;
        self.mu11 = mu11;
        self.mu02 = mu02;

        self.mu30 = self.m30 - cx * (3.0 * mu20 + cx * self.m10);
        mu11 += mu11;
        self.mu21 = self.m21 - cx * (mu11 + cx * self.m01) - cy * mu20;
        self.mu12 = self.m12 - cy * (mu11 + cy * self.m10) - cx * mu02;
        self.mu03 = self.m03 - cy * (3.0 * mu02 + cy * self.m01);

        let inv_sqrt_m00 = inv_m00.abs().sqrt();
        let s2 = inv_m00 * inv_m00;
        let s3 = s2 * inv_sqrt_m00;

        self.nu20 = self.mu20 * s2;
        self.nu11 = self.mu11 * s2;
        self.nu02 = self.mu02 * s2;
        self.nu30 = self.mu30 * s3;
        self.nu21 = self.mu21 * s3;
        self.nu12 = self.mu12 * s3;
        self.nu03 = self.mu03 * s3;
    }
}

/// 计算轮廓矩（对齐 `cv::moments` 的轮廓路径 `contourMoments`）。
///
/// 输入为 `i32` 坐标点集（`Point`），输出 `Moments`。
/// 空轮廓返回全零 `Moments`。
pub fn moments(contour: &[Point]) -> Moments {
    let lpt = contour.len();
    let mut m = Moments::default();
    if lpt == 0 {
        return m;
    }

    let mut a00 = 0.0f64;
    let mut a10 = 0.0f64;
    let mut a01 = 0.0f64;
    let mut a20 = 0.0f64;
    let mut a11 = 0.0f64;
    let mut a02 = 0.0f64;
    let mut a30 = 0.0f64;
    let mut a21 = 0.0f64;
    let mut a12 = 0.0f64;
    let mut a03 = 0.0f64;

    let mut xi_1 = contour[lpt - 1].x as f64;
    let mut yi_1 = contour[lpt - 1].y as f64;
    let mut xi_12 = xi_1 * xi_1;
    let mut yi_12 = yi_1 * yi_1;

    for i in 0..lpt {
        let xi = contour[i].x as f64;
        let yi = contour[i].y as f64;

        let xi2 = xi * xi;
        let yi2 = yi * yi;
        let dxy = xi_1 * yi - xi * yi_1;
        let xii_1 = xi_1 + xi;
        let yii_1 = yi_1 + yi;

        a00 += dxy;
        a10 += dxy * xii_1;
        a01 += dxy * yii_1;
        a20 += dxy * (xi_1 * xii_1 + xi2);
        a11 += dxy * (xi_1 * (yii_1 + yi_1) + xi * (yii_1 + yi));
        a02 += dxy * (yi_1 * yii_1 + yi2);
        a30 += dxy * xii_1 * (xi_12 + xi2);
        a03 += dxy * yii_1 * (yi_12 + yi2);
        a21 += dxy * (xi_12 * (3.0 * yi_1 + yi) + 2.0 * xi * xi_1 * yii_1
            + xi2 * (yi_1 + 3.0 * yi));
        a12 += dxy * (yi_12 * (3.0 * xi_1 + xi) + 2.0 * yi * yi_1 * xii_1
            + yi2 * (xi_1 + 3.0 * xi));

        xi_1 = xi;
        yi_1 = yi;
        xi_12 = xi2;
        yi_12 = yi2;
    }

    if a00.abs() > f32::EPSILON as f64 {
        let (db1_2, db1_6, db1_12, db1_24, db1_20, db1_60) = if a00 > 0.0 {
            (
                0.5,
                0.16666666666666666666666666666667,
                0.083333333333333333333333333333333,
                0.041666666666666666666666666666667,
                0.05,
                0.016666666666666666666666666666667,
            )
        } else {
            (
                -0.5,
                -0.16666666666666666666666666666667,
                -0.083333333333333333333333333333333,
                -0.041666666666666666666666666666667,
                -0.05,
                -0.016666666666666666666666666666667,
            )
        };

        m.m00 = a00 * db1_2;
        m.m10 = a10 * db1_6;
        m.m01 = a01 * db1_6;
        m.m20 = a20 * db1_12;
        m.m11 = a11 * db1_24;
        m.m02 = a02 * db1_12;
        m.m30 = a30 * db1_20;
        m.m21 = a21 * db1_60;
        m.m12 = a12 * db1_60;
        m.m03 = a03 * db1_20;

        m.complete_state();
    }
    m
}

/// 轮廓匹配方法，对应 `cv::ContourMatchModes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContourMatchMethod {
    /// `CONTOURS_MATCH_I1`：`Σ |1/m_i^A - 1/m_i^B|`
    I1,
    /// `CONTOURS_MATCH_I2`：`Σ |m_i^A - m_i^B|`
    I2,
    /// `CONTOURS_MATCH_I3`：`max |m_i^A - m_i^B| / |m_i^A|`
    I3,
}

/// 形状匹配（对齐 `cv::matchShapes`）。
///
/// 基于两个轮廓的 Hu 不变矩计算相似度距离。返回值越小越相似。
/// 若仅一个轮廓有非零矩，返回 `f64::MAX`（对齐 OpenCV `DBL_MAX`）。
pub fn match_shapes(
    contour1: &[Point],
    contour2: &[Point],
    method: ContourMatchMethod,
) -> f64 {
    let ma = hu_moments(&moments(contour1));
    let mb = hu_moments(&moments(contour2));

    let eps = 1e-5;
    let mut result = 0.0f64;
    let mut any_a = false;
    let mut any_b = false;

    for i in 0..7 {
        let mut ama = ma[i].abs();
        let mut amb = mb[i].abs();

        if ama > 0.0 {
            any_a = true;
        }
        if amb > 0.0 {
            any_b = true;
        }

        let sma = if ma[i] > 0.0 { 1 } else if ma[i] < 0.0 { -1 } else { 0 };
        let smb = if mb[i] > 0.0 { 1 } else if mb[i] < 0.0 { -1 } else { 0 };

        if ama > eps && amb > eps {
            match method {
                ContourMatchMethod::I1 => {
                    ama = 1.0 / (sma as f64 * ama.log10());
                    amb = 1.0 / (smb as f64 * amb.log10());
                    result += (-ama + amb).abs();
                }
                ContourMatchMethod::I2 => {
                    ama = sma as f64 * ama.log10();
                    amb = smb as f64 * amb.log10();
                    result += (-ama + amb).abs();
                }
                ContourMatchMethod::I3 => {
                    ama = sma as f64 * ama.log10();
                    amb = smb as f64 * amb.log10();
                    let mmm = ((ama - amb) / ama).abs();
                    if result < mmm {
                        result = mmm;
                    }
                }
            }
        }
    }

    if any_a != any_b {
        result = f64::MAX;
    }

    result
}

/// 计算 7 个 Hu 不变矩（对齐 `cv::HuMoments`）。
///
/// 返回 `[f64; 7]`，顺序与 OpenCV `hu[0..7]` 一致。
pub fn hu_moments(m: &Moments) -> [f64; 7] {
    let mut t0 = m.nu30 + m.nu12;
    let mut t1 = m.nu21 + m.nu03;

    let q0 = t0 * t0;
    let q1 = t1 * t1;

    let n4 = 4.0 * m.nu11;
    let s = m.nu20 + m.nu02;
    let d = m.nu20 - m.nu02;

    let mut hu = [0.0f64; 7];
    hu[0] = s;
    hu[1] = d * d + n4 * m.nu11;
    hu[3] = q0 + q1;
    hu[5] = d * (q0 - q1) + n4 * t0 * t1;

    t0 *= q0 - 3.0 * q1;
    t1 *= 3.0 * q0 - q1;

    let q0 = m.nu30 - 3.0 * m.nu12;
    let q1 = 3.0 * m.nu21 - m.nu03;

    hu[2] = q0 * q0 + q1 * q1;
    hu[4] = q0 * t0 + q1 * t1;
    hu[6] = q1 * t0 - q0 * t1;

    hu
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空轮廓 → 全零。
    #[test]
    fn empty_contour() {
        let m = moments(&[]);
        assert_eq!(m, Moments::default());
    }

    /// 单位正方形（逆时针）的矩。
    #[test]
    fn unit_square() {
        // 逆时针正方形 (0,0),(1,0),(1,1),(0,1)
        let c = [
            Point::new(0, 0),
            Point::new(1, 0),
            Point::new(1, 1),
            Point::new(0, 1),
        ];
        let m = moments(&c);
        // 面积 = 1
        assert!((m.m00 - 1.0).abs() < 1e-12, "m00={}", m.m00);
        // 质心 (0.5, 0.5)
        assert!((m.m10 / m.m00 - 0.5).abs() < 1e-12, "cx={}", m.m10 / m.m00);
        assert!((m.m01 / m.m00 - 0.5).abs() < 1e-12, "cy={}", m.m01 / m.m00);
    }

    /// 3x3 方块（OpenCV 实测面积 4，质心 (3,3)）。
    #[test]
    fn square_3x3() {
        // 3x3 方块边界环（8 点空心环），从 (2,2) 出发顺时针
        let c = [
            Point::new(2, 2), Point::new(3, 2), Point::new(4, 2),
            Point::new(4, 3), Point::new(4, 4), Point::new(3, 4),
            Point::new(2, 4), Point::new(2, 3),
        ];
        let m = moments(&c);
        assert!((m.m00 - 4.0).abs() < 1e-9, "m00={}", m.m00);
        assert!((m.m10 / m.m00 - 3.0).abs() < 1e-9, "cx={}", m.m10 / m.m00);
        assert!((m.m01 / m.m00 - 3.0).abs() < 1e-9, "cy={}", m.m01 / m.m00);
    }

    /// Hu 矩对平移/旋转/缩放不变（近似）。
    #[test]
    fn hu_invariance() {
        let c = [
            Point::new(0, 0), Point::new(4, 0), Point::new(4, 4), Point::new(0, 4),
        ];
        let m1 = moments(&c);
        let hu1 = hu_moments(&m1);

        // 平移
        let c2: Vec<Point> = c.iter().map(|p| Point::new(p.x + 10, p.y + 20)).collect();
        let hu2 = hu_moments(&moments(&c2));
        for (a, b) in hu1.iter().zip(&hu2) {
            assert!((a - b).abs() < 1e-9, "hu {a} vs {b}");
        }

        // 缩放 2x
        let c3: Vec<Point> = c.iter().map(|p| Point::new(p.x * 2, p.y * 2)).collect();
        let hu3 = hu_moments(&moments(&c3));
        for (a, b) in hu1.iter().zip(&hu3) {
            assert!((a - b).abs() < 1e-9, "hu {a} vs {b}");
        }
    }

    /// match_shapes：相同形状距离为 0，不同形状距离 > 0。
    #[test]
    fn match_shapes_identical() {
        let c = [
            Point::new(0, 0), Point::new(4, 0), Point::new(4, 4), Point::new(0, 4),
        ];
        for m in [ContourMatchMethod::I1, ContourMatchMethod::I2, ContourMatchMethod::I3] {
            let d = match_shapes(&c, &c, m);
            assert!(d.abs() < 1e-9, "{m:?} identical distance {d}");
        }
    }

    /// match_shapes：平移/缩放不变（近似）。
    #[test]
    fn match_shapes_invariance() {
        let c = [
            Point::new(0, 0), Point::new(4, 0), Point::new(4, 4), Point::new(0, 4),
        ];
        let c2: Vec<Point> = c.iter().map(|p| Point::new(p.x + 10, p.y + 20)).collect();
        let c3: Vec<Point> = c.iter().map(|p| Point::new(p.x * 2, p.y * 2)).collect();
        for m in [ContourMatchMethod::I1, ContourMatchMethod::I2, ContourMatchMethod::I3] {
            let d1 = match_shapes(&c, &c2, m);
            let d2 = match_shapes(&c, &c3, m);
            assert!(d1.abs() < 1e-6, "{m:?} translate {d1}");
            assert!(d2.abs() < 1e-6, "{m:?} scale {d2}");
        }
    }

    /// match_shapes：不同形状距离 > 0。
    #[test]
    fn match_shapes_different() {
        let square = [
            Point::new(0, 0), Point::new(4, 0), Point::new(4, 4), Point::new(0, 4),
        ];
        let triangle = [
            Point::new(0, 0), Point::new(4, 0), Point::new(2, 4),
        ];
        for m in [ContourMatchMethod::I1, ContourMatchMethod::I2, ContourMatchMethod::I3] {
            let d = match_shapes(&square, &triangle, m);
            assert!(d > 1e-6, "{m:?} different distance {d}");
        }
    }
}