use crate::core::image::Pixel;

/// 边界处理策略（铁律 4），与 `Image<T, C>` 的 `T` 绑定。
///
/// 对应 OpenCV `borderInterpolate` 的边界类型：
/// - [`Clamp`](Self::Clamp)：越界坐标钳位到边缘像素，对应 `BORDER_REPLICATE`
/// - [`Reflect`](Self::Reflect)：镜像反射（不重复边缘像素），对应 `BORDER_REFLECT_101`（即 OpenCV `BORDER_DEFAULT`）
/// - [`Constant`](Self::Constant)：越界处填充常量值，对应 `BORDER_CONSTANT`
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BorderPolicy<T: Pixel> {
    Clamp,
    Reflect,
    Constant(T),
}

impl<T: Pixel> BorderPolicy<T> {
    /// 把可能越界的坐标 `p` 映射回 `[0, len)`（对应 `cv::borderInterpolate`）。
    ///
    /// - `p` 已在 `[0, len)` 内时原样返回（任何策略）
    /// - [`Clamp`](Self::Clamp)/[`Reflect`](Self::Reflect) 在 `len > 0` 时总返回 `Some`
    /// - [`Constant`](Self::Constant) 越界时返回 `None`，调用方应使用常量值
    /// - `len == 0` 时恒返回 `None`
    ///
    /// 复杂度：O(1)（`Reflect` 最坏 O(|p|/len)）。
    pub fn interpolate(&self, p: isize, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let len_i = len as i64;
        let mut p = p as i64;
        if p >= 0 && p < len_i {
            return Some(p as usize);
        }
        match self {
            BorderPolicy::Clamp => Some(if p < 0 { 0 } else { len - 1 }),
            BorderPolicy::Reflect => {
                if len == 1 {
                    return Some(0);
                }
                // BORDER_REFLECT_101：delta = 1
                loop {
                    if p < 0 {
                        p = -p - 1 + 1;
                    } else {
                        p = len_i - 1 - (p - len_i) - 1;
                    }
                    if p >= 0 && p < len_i {
                        return Some(p as usize);
                    }
                }
            }
            BorderPolicy::Constant(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_interpolate() {
        let p = BorderPolicy::<u8>::Clamp;
        assert_eq!(p.interpolate(-1, 5), Some(0));
        assert_eq!(p.interpolate(-100, 5), Some(0));
        assert_eq!(p.interpolate(0, 5), Some(0));
        assert_eq!(p.interpolate(3, 5), Some(3));
        assert_eq!(p.interpolate(5, 5), Some(4));
        assert_eq!(p.interpolate(100, 5), Some(4));
        assert_eq!(p.interpolate(0, 1), Some(0));
    }

    #[test]
    fn reflect_interpolate() {
        let p = BorderPolicy::<u8>::Reflect;
        // len = 5，序列 0 1 2 3 4 | 3 2 1 0 1 2 3 4 | 3 2 ...
        assert_eq!(p.interpolate(-1, 5), Some(1));
        assert_eq!(p.interpolate(-2, 5), Some(2));
        assert_eq!(p.interpolate(-4, 5), Some(4));
        assert_eq!(p.interpolate(-5, 5), Some(3));
        assert_eq!(p.interpolate(-6, 5), Some(2));
        assert_eq!(p.interpolate(5, 5), Some(3));
        assert_eq!(p.interpolate(6, 5), Some(2));
        assert_eq!(p.interpolate(8, 5), Some(0));
        assert_eq!(p.interpolate(9, 5), Some(1));
        assert_eq!(p.interpolate(10, 5), Some(2));
        // len = 1：任意坐标映射到 0
        assert_eq!(p.interpolate(-100, 1), Some(0));
        assert_eq!(p.interpolate(7, 1), Some(0));
        // len = 2：序列 0 1 | 0 1 | 0 1 ...
        assert_eq!(p.interpolate(4, 2), Some(0));
        assert_eq!(p.interpolate(5, 2), Some(1));
        assert_eq!(p.interpolate(-1, 2), Some(1));
    }

    #[test]
    fn constant_interpolate() {
        let p = BorderPolicy::Constant(7u8);
        assert_eq!(p.interpolate(0, 5), Some(0));
        assert_eq!(p.interpolate(4, 5), Some(4));
        assert_eq!(p.interpolate(5, 5), None);
        assert_eq!(p.interpolate(-1, 5), None);
    }

    #[test]
    fn zero_len_is_none() {
        assert_eq!(BorderPolicy::<u8>::Clamp.interpolate(0, 0), None);
        assert_eq!(BorderPolicy::<u8>::Reflect.interpolate(0, 0), None);
        assert_eq!(BorderPolicy::Constant(1u8).interpolate(0, 0), None);
    }
}
