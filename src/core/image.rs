use std::any::type_name;
use std::fmt;

use crate::core::error::{Error, Result};
use crate::core::rect::{Rect, Size};

/// 像素标量类型契约（铁律 1）。
///
/// 舍入策略：`from_f32` 采用 round-to-nearest、ties-to-even（对齐 OpenCV `cvRound`）；
/// 整数类型饱和到 `[0, max_value()]`，`NaN` 映射为 `0`。
pub trait Pixel: Copy + Send + Sync + 'static + PartialOrd {
    /// 用于 SIMD 加载/存储的类型，通常与 `Self` 相同。
    type Simd: Copy;

    /// 零值。
    fn zero() -> Self;

    /// 该类型可表示的最大值（u8→255，u16→65535，f32→1.0）。
    fn max_value() -> Self;

    /// 转 `f32`，用于需要浮点中间结果的算法（如高斯）。
    fn to_f32(self) -> f32;

    /// 从 `f32` 转换，round-to-nearest、ties-to-even，越界饱和。
    fn from_f32(v: f32) -> Self;

    /// 从 `f64` 转换，round-to-nearest、ties-to-even，越界饱和。
    ///
    /// 对齐 OpenCV `saturate_cast<T>(double)`（cvRound 语义），
    /// 用于 f64 全程累加的标量路径（box_filter CV_32S 等）。
    fn from_f64_round_even(v: f64) -> Self;
}

impl Pixel for u8 {
    type Simd = u8;

    fn zero() -> Self {
        0
    }

    fn max_value() -> Self {
        255
    }

    fn to_f32(self) -> f32 {
        self as f32
    }

    fn from_f32(v: f32) -> Self {
        v.round_ties_even() as u8
    }

    fn from_f64_round_even(v: f64) -> Self {
        v.round_ties_even().clamp(0.0, 255.0) as u8
    }
}

impl Pixel for u16 {
    type Simd = u16;

    fn zero() -> Self {
        0
    }

    fn max_value() -> Self {
        65535
    }

    fn to_f32(self) -> f32 {
        self as f32
    }

    fn from_f32(v: f32) -> Self {
        v.round_ties_even() as u16
    }

    fn from_f64_round_even(v: f64) -> Self {
        v.round_ties_even().clamp(0.0, 65535.0) as u16
    }
}

impl Pixel for f32 {
    type Simd = f32;

    fn zero() -> Self {
        0.0
    }

    fn max_value() -> Self {
        1.0
    }

    fn to_f32(self) -> f32 {
        self
    }

    fn from_f32(v: f32) -> Self {
        v
    }

    fn from_f64_round_even(v: f64) -> Self {
        v as f32
    }
}

impl Pixel for f64 {
    type Simd = f64;

    fn zero() -> Self {
        0.0
    }

    fn max_value() -> Self {
        1.0
    }

    fn to_f32(self) -> f32 {
        self as f32
    }

    fn from_f32(v: f32) -> Self {
        v as f64
    }

    fn from_f64_round_even(v: f64) -> Self {
        v
    }
}

impl Pixel for i16 {
    type Simd = i16;

    fn zero() -> Self {
        0
    }

    fn max_value() -> Self {
        i16::MAX
    }

    fn to_f32(self) -> f32 {
        self as f32
    }

    fn from_f32(v: f32) -> Self {
        v.round_ties_even() as i16
    }

    fn from_f64_round_even(v: f64) -> Self {
        v.round_ties_even().clamp(-32768.0, 32767.0) as i16
    }
}

impl Pixel for i32 {
    type Simd = i32;

    fn zero() -> Self {
        0
    }

    fn max_value() -> Self {
        i32::MAX
    }

    fn to_f32(self) -> f32 {
        self as f32
    }

    fn from_f32(v: f32) -> Self {
        v.round_ties_even() as i32
    }

    fn from_f64_round_even(v: f64) -> Self {
        v.round_ties_even().clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

/// 图像维度描述，用于错误信息定位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dims {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
}

/// 强类型图像容器（铁律 1）：`T` 为像素标量类型，`C` 为编译期常量通道数。
///
/// 内存布局：行主序、通道交错、完全紧凑（无行填充），与 OpenCV 连续 `Mat` 等价。
/// 彩色图通道顺序为 R、G、B（铁律 6）。
///
/// 通道数 `C` 必须 ≥ 1；`width`/`height` 允许为 0（空图像）。
#[derive(Clone, PartialEq)]
pub struct Image<T: Pixel, const C: usize> {
    width: usize,
    height: usize,
    data: Vec<T>,
}

impl<T: Pixel, const C: usize> Image<T, C> {
    /// 创建全零图像。复杂度 O(w·h·C)。
    ///
    /// # Panics
    /// 若 `width * height * C` 溢出 `usize`，或 `C == 0`。
    pub fn new(width: usize, height: usize) -> Self {
        assert!(C > 0, "channel count must be >= 1");
        let len = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(C))
            .expect("image size overflow");
        Self {
            width,
            height,
            data: vec![T::zero(); len],
        }
    }

    /// 分配未初始化的输出缓冲（内部使用）。
    ///
    /// # Safety
    /// 调用方必须保证返回的 Image 数据被完全覆写后才被读取
    /// （SIMD 内核整块写出场景）。
    pub(crate) fn new_uninit(width: usize, height: usize) -> Self {
        assert!(C > 0, "channel count must be >= 1");
        let len = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(C))
            .expect("image size overflow");
        Self {
            width,
            height,
            data: crate::simd_kernels::alloc_uninit(len),
        }
    }

    /// 从底层缓冲构建，`data.len()` 必须等于 `width * height * C`。
    ///
    /// # Errors
    /// 长度不符返回 [`Error::DimensionMismatch`]；`C == 0` 返回
    /// [`Error::UnsupportedChannels`]。
    pub fn from_vec(width: usize, height: usize, data: Vec<T>) -> Result<Self> {
        if C == 0 {
            return Err(Error::UnsupportedChannels(0));
        }
        let pixels = width.checked_mul(height);
        let expected = pixels.and_then(|v| v.checked_mul(C));
        if expected == Some(data.len()) {
            return Ok(Self {
                width,
                height,
                data,
            });
        }
        let actual_channels = match pixels {
            Some(p) if p > 0 => data.len() / p,
            _ => data.len(),
        };
        Err(Error::DimensionMismatch {
            expected: Dims {
                width,
                height,
                channels: C,
            },
            actual: Dims {
                width,
                height,
                channels: actual_channels,
            },
        })
    }

    /// 逐像素构造，`f(x, y)` 返回该位置的 C 通道值。复杂度 O(width·height·C)。
    ///
    /// # Panics
    /// 若 `width * height * C` 溢出 `usize`，或 `C == 0`。
    pub fn from_fn<F>(width: usize, height: usize, mut f: F) -> Self
    where
        F: FnMut(usize, usize) -> [T; C],
    {
        assert!(C > 0, "channel count must be >= 1");
        let len = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(C))
            .expect("image size overflow");
        let mut data = Vec::with_capacity(len);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&f(x, y));
            }
        }
        Self {
            width,
            height,
            data,
        }
    }

    /// 用单一像素值填充构造。复杂度 O(width·height·C)。
    ///
    /// # Panics
    /// 若 `width * height * C` 溢出 `usize`，或 `C == 0`。
    pub fn from_pixel(width: usize, height: usize, pixel: [T; C]) -> Self {
        assert!(C > 0, "channel count must be >= 1");
        let len = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(C))
            .expect("image size overflow");
        let mut data = Vec::with_capacity(len);
        for _ in 0..width * height {
            data.extend_from_slice(&pixel);
        }
        Self {
            width,
            height,
            data,
        }
    }

    /// 由已验证的部件构造（内部使用，不校验长度）。
    fn from_parts(width: usize, height: usize, data: Vec<T>) -> Self {
        debug_assert_eq!(data.len(), width * height * C);
        Self {
            width,
            height,
            data,
        }
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn channels(&self) -> usize {
        C
    }

    pub const fn dims(&self) -> Dims {
        Dims {
            width: self.width,
            height: self.height,
            channels: C,
        }
    }

    pub fn size(&self) -> Size {
        Size::new(self.width as i32, self.height as i32)
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 像素总数（width × height）。
    pub const fn pixel_count(&self) -> usize {
        self.width * self.height
    }

    pub fn data(&self) -> &[T] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [T] {
        &mut self.data
    }

    pub fn into_vec(self) -> Vec<T> {
        self.data
    }

    /// 取 `(x, y)` 处的 C 通道切片；越界返回 `None`。O(1)。
    pub fn pixel(&self, x: usize, y: usize) -> Option<&[T]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * C;
        self.data.get(i..i + C)
    }

    /// 取 `(x, y)` 处的 C 通道可变切片；越界返回 `None`。O(1)。
    pub fn pixel_mut(&mut self, x: usize, y: usize) -> Option<&mut [T]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * C;
        self.data.get_mut(i..i + C)
    }

    /// 取第 `y` 行（长度 `width * C`）；越界返回 `None`。O(1)。
    pub fn row(&self, y: usize) -> Option<&[T]> {
        if y >= self.height {
            return None;
        }
        let i = y * self.width * C;
        Some(&self.data[i..i + self.width * C])
    }

    /// 取第 `y` 行可变切片；越界返回 `None`。O(1)。
    pub fn row_mut(&mut self, y: usize) -> Option<&mut [T]> {
        if y >= self.height {
            return None;
        }
        let i = y * self.width * C;
        Some(&mut self.data[i..i + self.width * C])
    }

    /// 按行迭代。O(width·height)。
    pub fn rows(&self) -> impl Iterator<Item = &[T]> {
        let row_len = if self.width == 0 { 1 } else { self.width * C };
        self.data.chunks_exact(row_len)
    }

    /// 按像素（C 通道切片）迭代。O(width·height)。
    pub fn pixels(&self) -> impl Iterator<Item = &[T]> {
        self.data.chunks_exact(C)
    }

    /// 按像素（C 通道可变切片）迭代。O(width·height)。
    pub fn pixels_mut(&mut self) -> impl Iterator<Item = &mut [T]> {
        self.data.chunks_exact_mut(C)
    }

    /// 整幅图像的零拷贝只读视图。
    pub fn as_view(&self) -> ImageView<'_, T, C> {
        ImageView {
            width: self.width,
            height: self.height,
            stride: self.width * C,
            data: &self.data,
        }
    }

    /// 取 ROI（零拷贝只读视图）。矩形必须完全位于图像内。
    ///
    /// # Errors
    /// 矩形越界或为空时返回 [`Error::DimensionMismatch`]。
    pub fn roi(&self, rect: Rect) -> Result<ImageView<'_, T, C>> {
        let (x, y, w, h) = fit_roi(rect, self.width, self.height, C)?;
        let stride = self.width * C;
        let offset = (y * self.width + x) * C;
        Ok(ImageView {
            width: w,
            height: h,
            stride,
            data: &self.data[offset..],
        })
    }

    /// 取 ROI（零拷贝可变视图）。矩形必须完全位于图像内。
    ///
    /// # Errors
    /// 矩形越界或为空时返回 [`Error::DimensionMismatch`]。
    pub fn roi_mut(&mut self, rect: Rect) -> Result<ImageViewMut<'_, T, C>> {
        let (x, y, w, h) = fit_roi(rect, self.width, self.height, C)?;
        let stride = self.width * C;
        let offset = (y * self.width + x) * C;
        Ok(ImageViewMut {
            width: w,
            height: h,
            stride,
            data: &mut self.data[offset..],
        })
    }
}

/// 校验 ROI 矩形完全位于 `img_w × img_h` 内，返回 `(x, y, w, h)`。
fn fit_roi(
    rect: Rect,
    img_w: usize,
    img_h: usize,
    channels: usize,
) -> Result<(usize, usize, usize, usize)> {
    let ok = rect.x >= 0
        && rect.y >= 0
        && rect.width > 0
        && rect.height > 0
        && (rect.x as i64) + (rect.width as i64) <= img_w as i64
        && (rect.y as i64) + (rect.height as i64) <= img_h as i64;
    if ok {
        Ok((
            rect.x as usize,
            rect.y as usize,
            rect.width as usize,
            rect.height as usize,
        ))
    } else {
        Err(Error::DimensionMismatch {
            expected: Dims {
                width: img_w,
                height: img_h,
                channels,
            },
            actual: Dims {
                width: rect.width.max(0) as usize,
                height: rect.height.max(0) as usize,
                channels,
            },
        })
    }
}

/// 图像的零拷贝只读视图（铁律 2）：借用底层数据，带行跨度，支持 ROI。
#[derive(Clone, Copy)]
pub struct ImageView<'a, T: Pixel, const C: usize> {
    width: usize,
    height: usize,
    stride: usize,
    data: &'a [T],
}

impl<'a, T: Pixel, const C: usize> ImageView<'a, T, C> {
    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn channels(&self) -> usize {
        C
    }

    pub const fn dims(&self) -> Dims {
        Dims {
            width: self.width,
            height: self.height,
            channels: C,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 行跨度（元素数，含通道）。
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// 取第 `y` 行（长度 `width * C`）；越界返回 `None`。O(1)。
    pub fn row(&self, y: usize) -> Option<&'a [T]> {
        if y >= self.height {
            return None;
        }
        let data: &'a [T] = self.data;
        let i = y * self.stride;
        Some(&data[i..i + self.width * C])
    }

    /// 取 `(x, y)` 处的 C 通道切片；越界返回 `None`。O(1)。
    pub fn pixel(&self, x: usize, y: usize) -> Option<&'a [T]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let data: &'a [T] = self.data;
        let i = y * self.stride + x * C;
        data.get(i..i + C)
    }

    /// 按行迭代。O(width·height)。
    pub fn rows(&self) -> impl Iterator<Item = &'a [T]> + '_ {
        let stride = self.stride;
        let row_len = self.width * C;
        let data: &'a [T] = self.data;
        let height = self.height;
        (0..height).map(move |y| &data[y * stride..y * stride + row_len])
    }

    /// 按像素（C 通道切片）迭代。O(width·height)。
    pub fn pixels(&self) -> impl Iterator<Item = &'a [T]> + '_ {
        self.rows().flat_map(|row| row.chunks_exact(C))
    }

    /// 取子 ROI（零拷贝只读视图）。矩形必须完全位于视图内。
    ///
    /// # Errors
    /// 矩形越界或为空时返回 [`Error::DimensionMismatch`]。
    pub fn roi(&self, rect: Rect) -> Result<ImageView<'a, T, C>> {
        let (x, y, w, h) = fit_roi(rect, self.width, self.height, C)?;
        let offset = y * self.stride + x * C;
        let data: &'a [T] = self.data;
        Ok(ImageView {
            width: w,
            height: h,
            stride: self.stride,
            data: &data[offset..],
        })
    }

    /// 拷贝为紧凑连续的 [`Image`]（消除行跨度）。O(width·height·C)。
    pub fn to_image(&self) -> Image<T, C> {
        let mut data = Vec::with_capacity(self.width * self.height * C);
        for row in self.rows() {
            data.extend_from_slice(row);
        }
        Image::from_parts(self.width, self.height, data)
    }
}

/// 图像的零拷贝可变视图（铁律 2、3）：借用底层数据，带行跨度，支持 ROI。
pub struct ImageViewMut<'a, T: Pixel, const C: usize> {
    width: usize,
    height: usize,
    stride: usize,
    data: &'a mut [T],
}

impl<'a, T: Pixel, const C: usize> ImageViewMut<'a, T, C> {
    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn channels(&self) -> usize {
        C
    }

    pub const fn dims(&self) -> Dims {
        Dims {
            width: self.width,
            height: self.height,
            channels: C,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 行跨度（元素数，含行）。
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// 取第 `y` 行可变切片；越界返回 `None`。O(1)。
    pub fn row_mut(&mut self, y: usize) -> Option<&mut [T]> {
        if y >= self.height {
            return None;
        }
        let i = y * self.stride;
        Some(&mut self.data[i..i + self.width * C])
    }

    /// 取 `(x, y)` 处的 C 通道可变切片；越界返回 `None`。O(1)。
    pub fn pixel_mut(&mut self, x: usize, y: usize) -> Option<&mut [T]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = y * self.stride + x * C;
        self.data.get_mut(i..i + C)
    }

    /// 按行（可变切片）迭代。O(width·height)。
    pub fn rows_mut(&mut self) -> RowsMut<'_, T> {
        RowsMut {
            data: self.data,
            stride: self.stride,
            row_len: self.width * C,
            height: self.height,
            y: 0,
        }
    }

    /// 按像素（C 通道可变切片）迭代。O(width·height)。
    pub fn pixels_mut(&mut self) -> PixelsMut<'_, T, C> {
        let take = self.stride.min(self.data.len());
        let (row, rest) = std::mem::take(&mut self.data).split_at_mut(take);
        PixelsMut {
            row,
            rest,
            stride: self.stride,
            width: self.width,
            height: self.height,
            y: 0,
            x: 0,
        }
    }

    /// 取子视图（零拷贝可变视图）。矩形必须完全位于视图内。
    ///
    /// # Errors
    /// 矩形越界或为空时返回 [`Error::DimensionMismatch`]。
    pub fn roi_mut(&mut self, rect: Rect) -> Result<ImageViewMut<'_, T, C>> {
        let (x, y, w, h) = fit_roi(rect, self.width, self.height, C)?;
        let offset = y * self.stride + x * C;
        Ok(ImageViewMut {
            width: w,
            height: h,
            stride: self.stride,
            data: &mut self.data[offset..],
        })
    }

    /// 拷贝为紧凑连续的 [`Image`]（消除行跨度）。O(width·height·C)。
    pub fn to_image(&self) -> Image<T, C> {
        let data: &[T] = self.data;
        let mut out = Vec::with_capacity(self.width * self.height * C);
        for y in 0..self.height {
            out.extend_from_slice(&data[y * self.stride..y * self.stride + self.width * C]);
        }
        Image::from_parts(self.width, self.height, out)
    }
}

impl<T: Pixel, const C: usize> fmt::Debug for Image<T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("pixel_type", &type_name::<T>())
            .field("channels", &C)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl<T: Pixel, const C: usize> fmt::Debug for ImageView<'_, T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageView")
            .field("pixel_type", &type_name::<T>())
            .field("channels", &C)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .finish()
    }
}

impl<T: Pixel, const C: usize> fmt::Debug for ImageViewMut<'_, T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageViewMut")
            .field("pixel_type", &type_name::<T>())
            .field("channels", &C)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .finish()
    }
}

/// 按行（可变切片）迭代器，支持行跨度。
pub struct RowsMut<'a, T> {
    data: &'a mut [T],
    stride: usize,
    row_len: usize,
    height: usize,
    y: usize,
}

impl<'a, T> Iterator for RowsMut<'a, T> {
    type Item = &'a mut [T];

    fn next(&mut self) -> Option<&'a mut [T]> {
        if self.y >= self.height {
            return None;
        }
        self.y += 1;
        let take = self.stride.min(self.data.len());
        let (row, rest) = std::mem::take(&mut self.data).split_at_mut(take);
        self.data = rest;
        let n = self.row_len.min(row.len());
        Some(&mut row[..n])
    }
}

/// 按像素（C 通道可变切片）迭代器，用于带跨度视图。
pub struct PixelsMut<'a, T, const C: usize> {
    row: &'a mut [T],
    rest: &'a mut [T],
    stride: usize,
    width: usize,
    height: usize,
    y: usize,
    x: usize,
}

impl<'a, T, const C: usize> Iterator for PixelsMut<'a, T, C> {
    type Item = &'a mut [T];

    fn next(&mut self) -> Option<&'a mut [T]> {
        if self.y >= self.height {
            return None;
        }
        if self.x >= self.width {
            self.x = 0;
            self.y += 1;
            if self.y >= self.height {
                return None;
            }
            let take = self.stride.min(self.rest.len());
            let (row, rest) = std::mem::take(&mut self.rest).split_at_mut(take);
            self.row = row;
            self.rest = rest;
        }
        let (px, rest) = std::mem::take(&mut self.row).split_at_mut(C);
        self.row = rest;
        self.x += 1;
        Some(px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_u8_rounding() {
        assert_eq!(u8::from_f32(0.4), 0);
        assert_eq!(u8::from_f32(0.5), 0); // ties-to-even
        assert_eq!(u8::from_f32(1.5), 2);
        assert_eq!(u8::from_f32(2.5), 2);
        assert_eq!(u8::from_f32(3.5), 4);
        assert_eq!(u8::from_f32(-0.3), 0);
        assert_eq!(u8::from_f32(-3.0), 0);
        assert_eq!(u8::from_f32(300.0), 255);
        assert_eq!(u8::from_f32(f32::NAN), 0);
        assert_eq!(u8::from_f32(255.0), 255);
    }

    #[test]
    fn pixel_16_rounding() {
        assert_eq!(u16::from_f32(2.5), 2);
        assert_eq!(u16::from_f32(3.5), 4);
        assert_eq!(u16::from_f32(-1.0), 0);
        assert_eq!(u16::from_f32(65535.7), 65535);
        assert_eq!(u16::from_f32(70000.0), 65535);
        assert_eq!(u16::from_f32(f32::NAN), 0);
    }

    #[test]
    fn pixel_f32_identity() {
        assert_eq!(f32::from_f32(0.5), 0.5);
        assert_eq!(f32::to_f32(0.5), 0.5);
        assert!(f32::from_f32(f32::NAN).is_nan());
    }

    #[test]
    fn pixel_constants() {
        assert_eq!(u8::zero(), 0);
        assert_eq!(<u8 as Pixel>::max_value(), 255);
        assert_eq!(u16::zero(), 0);
        assert_eq!(<u16 as Pixel>::max_value(), 65535);
        assert_eq!(f32::zero(), 0.0);
        assert_eq!(<f32 as Pixel>::max_value(), 1.0);
        assert_eq!(u8::to_f32(255), 255.0);
        assert_eq!(u16::to_f32(65535), 65535.0);
    }

    #[test]
    fn new_zeroed() {
        let img = Image::<u8, 3>::new(2, 2);
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 2);
        assert_eq!(img.channels(), 3);
        assert_eq!(
            img.dims(),
            Dims {
                width: 2,
                height: 2,
                channels: 3
            }
        );
        assert_eq!(img.pixel_count(), 4);
        assert!(!img.is_empty());
        assert!(img.data().iter().all(|&v| v == 0));
    }

    #[test]
    fn new_empty_and_single_row() {
        let empty = Image::<u8, 1>::new(0, 0);
        assert!(empty.is_empty());
        assert_eq!(empty.pixel_count(), 0);
        assert!(empty.data().is_empty());

        let single_row = Image::<u8, 1>::new(3, 1);
        assert_eq!(single_row.height(), 1);
        assert_eq!(single_row.row(0).unwrap().len(), 3);
        assert!(single_row.row(1).is_none());

        let single_col = Image::<u8, 1>::new(1, 3);
        assert_eq!(single_col.width(), 1);
        assert_eq!(single_col.row(2).unwrap().len(), 1);
    }

    #[test]
    fn from_vec_ok() {
        let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let img = Image::<u8, 3>::from_vec(2, 2, data).unwrap();
        assert_eq!(img.pixel(0, 0).unwrap(), &[1, 2, 3]);
        assert_eq!(img.pixel(1, 1).unwrap(), &[10, 11, 12]);
    }

    #[test]
    fn from_vec_wrong_len() {
        let err = Image::<u8, 3>::from_vec(2, 2, vec![1, 2, 3]).unwrap_err();
        match err {
            Error::DimensionMismatch { expected, actual } => {
                assert_eq!(
                    expected,
                    Dims {
                        width: 2,
                        height: 2,
                        channels: 3
                    }
                );
                assert_eq!(actual.channels, 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn from_vec_zero_channels() {
        let err = Image::<u8, 0>::from_vec(2, 2, vec![]).unwrap_err();
        assert!(matches!(err, Error::UnsupportedChannels(0)));
    }

    #[test]
    fn from_vec_empty_ok() {
        let img = Image::<u8, 1>::from_vec(0, 0, vec![]).unwrap();
        assert!(img.is_empty());
    }

    #[test]
    fn from_fn() {
        let img = Image::<u8, 2>::from_fn(2, 2, |x, y| [x as u8, y as u8]);
        assert_eq!(img.pixel(0, 0).unwrap(), &[0, 0]);
        assert_eq!(img.pixel(1, 0).unwrap(), &[1, 0]);
        assert_eq!(img.pixel(0, 1).unwrap(), &[0, 1]);
        assert_eq!(img.pixel(1, 1).unwrap(), &[1, 1]);
    }

    #[test]
    fn pixel_access_bounds() {
        let img = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 3, 4]).unwrap();
        assert_eq!(img.pixel(0, 0).unwrap(), &[1]);
        assert_eq!(img.pixel(1, 1).unwrap(), &[4]);
        assert!(img.pixel(2, 0).is_none());
        assert!(img.pixel(0, 2).is_none());
    }

    #[test]
    fn pixel_mut_modifies() {
        let mut img = Image::<u8, 3>::new(1, 1);
        img.pixel_mut(0, 0).unwrap().copy_from_slice(&[10, 20, 30]);
        assert_eq!(img.pixel(0, 0).unwrap(), &[10, 20, 30]);
    }

    #[test]
    fn row_and_rows() {
        let img = Image::<u8, 2>::from_vec(2, 2, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(img.row(0).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(img.row(1).unwrap(), &[5, 6, 7, 8]);
        assert!(img.row(2).is_none());
        let rows: Vec<_> = img.rows().collect();
        assert_eq!(rows, vec![&[1, 2, 3, 4][..], &[5, 6, 7, 8][..]]);
    }

    #[test]
    fn pixels_iter() {
        let img = Image::<u8, 2>::from_vec(2, 1, vec![1, 2, 3, 4]).unwrap();
        let px: Vec<_> = img.pixels().collect();
        assert_eq!(px, vec![&[1, 2][..], &[3, 4][..]]);
    }

    #[test]
    fn roi_view() {
        let img = Image::<u8, 1>::from_fn(4, 4, |x, y| [(y * 4 + x) as u8]);
        let view = img.roi(Rect::new(1, 1, 2, 2)).unwrap();
        assert_eq!(view.width(), 2);
        assert_eq!(view.height(), 2);
        assert_eq!(view.stride(), 4);
        assert_eq!(view.row(0).unwrap(), &[5, 6]);
        assert_eq!(view.row(1).unwrap(), &[9, 10]);
        assert_eq!(view.pixel(0, 0).unwrap(), &[5]);
        assert_eq!(view.pixel(1, 1).unwrap(), &[10]);
    }

    #[test]
    fn roi_out_of_bounds() {
        let img = Image::<u8, 1>::new(4, 4);
        assert!(img.roi(Rect::new(3, 0, 2, 2)).is_err());
        assert!(img.roi(Rect::new(-1, 0, 2, 2)).is_err());
        assert!(img.roi(Rect::new(0, 0, 0, 2)).is_err());
        assert!(img.roi(Rect::new(0, 0, 5, 5)).is_err());
    }

    #[test]
    fn roi_nested() {
        let img = Image::<u8, 1>::from_fn(6, 6, |x, y| [(y * 6 + x) as u8]);
        let outer = img.roi(Rect::new(1, 1, 4, 4)).unwrap();
        let inner = outer.roi(Rect::new(1, 1, 2, 2)).unwrap();
        assert_eq!(inner.row(0).unwrap(), &[14, 15]);
        assert_eq!(inner.row(1).unwrap(), &[20, 21]);
    }

    #[test]
    fn roi_mut_modifies_image() {
        let mut img = Image::<u8, 1>::new(4, 4);
        {
            let mut view = img.roi_mut(Rect::new(0, 0, 2, 2)).unwrap();
            for row in view.rows_mut() {
                for v in row.iter_mut() {
                    *v = 7;
                }
            }
        }
        assert_eq!(img.pixel(0, 0).unwrap(), &[7]);
        assert_eq!(img.pixel(1, 1).unwrap(), &[7]);
        assert_eq!(img.pixel(2, 0).unwrap(), &[0]);
        assert_eq!(img.pixel(3, 3).unwrap(), &[0]);
    }

    #[test]
    fn view_to_image_roundtrip() {
        let img = Image::<u8, 1>::from_vec(4, 4, (0..16).collect()).unwrap();
        let view = img.roi(Rect::new(1, 1, 2, 2)).unwrap();
        let out = view.to_image();
        assert_eq!(
            out,
            Image::<u8, 1>::from_vec(2, 2, vec![5, 6, 9, 10]).unwrap()
        );
    }

    #[test]
    fn as_view_full() {
        let img = Image::<u8, 1>::from_vec(2, 2, vec![1, 2, 3, 4]).unwrap();
        let view = img.as_view();
        assert_eq!(
            view.dims(),
            Dims {
                width: 2,
                height: 2,
                channels: 1
            }
        );
        assert_eq!(view.to_image(), img);
    }

    #[test]
    fn into_vec_roundtrip() {
        let data = vec![1u8, 2, 3, 4];
        let img = Image::<u8, 1>::from_vec(2, 2, data.clone()).unwrap();
        assert_eq!(img.into_vec(), data);
    }

    #[test]
    fn debug_does_not_dump_data() {
        let img = Image::<u8, 1>::new(100, 100);
        let s = format!("{img:?}");
        assert!(s.contains("100"));
        assert!(s.contains("u8"));
        assert!(s.len() < 200);
    }
}
