//! 按位逻辑运算（对应 OpenCV `cv::bitwise_and/or/xor/not`，arithm.cpp:378-402）。
//!
//! OpenCV 语义：按**字节**做位运算（与像素类型/通道数无关，u8/u16/f32
//! 均按底层位模式操作）。本模块对任意 `T: Pod` 的连续数据按位运算，
//! 与 OpenCV 逐位一致。
//!
//! 公开 API 一律 RGB（铁律 6）；逐通道独立处理，通道数任意。

use crate::core::image::{Image, Pixel};

/// 按位与（对应 `cv::bitwise_and`）。
///
/// 两图尺寸/类型/通道数必须一致，否则 panic。
pub fn bitwise_and<T: Pixel + bytemuck::Pod, const C: usize>(
    a: &Image<T, C>,
    b: &Image<T, C>,
) -> Image<T, C> {
    binary(a, b, BinOp::And)
}

/// 按位或（对应 `cv::bitwise_or`）。
pub fn bitwise_or<T: Pixel + bytemuck::Pod, const C: usize>(
    a: &Image<T, C>,
    b: &Image<T, C>,
) -> Image<T, C> {
    binary(a, b, BinOp::Or)
}

/// 按位异或（对应 `cv::bitwise_xor`）。
pub fn bitwise_xor<T: Pixel + bytemuck::Pod, const C: usize>(
    a: &Image<T, C>,
    b: &Image<T, C>,
) -> Image<T, C> {
    binary(a, b, BinOp::Xor)
}

/// 按位取反（对应 `cv::bitwise_not`）。
pub fn bitwise_not<T: Pixel + bytemuck::Pod, const C: usize>(a: &Image<T, C>) -> Image<T, C> {
    let mut out = Image::new_uninit(a.width(), a.height());
    let src: &[u8] = bytemuck::cast_slice(a.data());
    let dst: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
    crate::simd_kernels::bitwise_not_bytes(src, dst, src.len());
    out
}

enum BinOp {
    And,
    Or,
    Xor,
}

fn binary<T: Pixel + bytemuck::Pod, const C: usize>(
    a: &Image<T, C>,
    b: &Image<T, C>,
    op: BinOp,
) -> Image<T, C> {
    assert_eq!(
        a.size(),
        b.size(),
        "bitwise: dimension mismatch {:?} vs {:?}",
        a.size(),
        b.size()
    );
    let mut out = Image::new_uninit(a.width(), a.height());
    let sa: &[u8] = bytemuck::cast_slice(a.data());
    let sb: &[u8] = bytemuck::cast_slice(b.data());
    let dst: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
    match op {
        BinOp::And => crate::simd_kernels::bitwise_and_bytes(sa, sb, dst, sa.len()),
        BinOp::Or => crate::simd_kernels::bitwise_or_bytes(sa, sb, dst, sa.len()),
        BinOp::Xor => crate::simd_kernels::bitwise_xor_bytes(sa, sb, dst, sa.len()),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_or_xor_not_u8() {
        let a = Image::<u8, 1>::from_fn(4, 2, |x, y| [(x * 3 + y * 5) as u8]);
        let b = Image::<u8, 1>::from_fn(4, 2, |x, y| [(x * 7 + y * 11 + 3) as u8]);
        let and = bitwise_and(&a, &b);
        let or = bitwise_or(&a, &b);
        let xor = bitwise_xor(&a, &b);
        let not = bitwise_not(&a);
        for i in 0..8 {
            let (x, y) = (a.data()[i], b.data()[i]);
            assert_eq!(and.data()[i], x & y);
            assert_eq!(or.data()[i], x | y);
            assert_eq!(xor.data()[i], x ^ y);
            assert_eq!(not.data()[i], !x);
        }
    }

    #[test]
    fn rgb_channels_independent() {
        let a = Image::<u8, 3>::from_fn(3, 3, |x, y| [x as u8, y as u8, (x + y) as u8]);
        let b = Image::<u8, 3>::from_fn(3, 3, |x, y| [y as u8, x as u8, 255]);
        let and = bitwise_and(&a, &b);
        for i in 0..27 {
            assert_eq!(and.data()[i], a.data()[i] & b.data()[i]);
        }
    }

    #[test]
    fn u16_bit_pattern() {
        // u16 按底层位模式运算（对齐 OpenCV：与类型无关）
        let a = Image::<u16, 1>::from_fn(2, 2, |x, y| [(x * 1000 + y * 7) as u16]);
        let b = Image::<u16, 1>::from_fn(2, 2, |_, _| [0x0F0F]);
        let and = bitwise_and(&a, &b);
        for i in 0..4 {
            assert_eq!(and.data()[i], a.data()[i] & 0x0F0F);
        }
    }

    #[test]
    fn f32_bit_pattern() {
        // f32 同样按位运算（NaN 位模式等），对齐 OpenCV bitwise 语义
        let a = Image::<f32, 1>::from_fn(2, 2, |x, y| [x as f32 + y as f32 * 0.5]);
        let b = Image::<f32, 1>::from_fn(2, 2, |_, _| [-0.0]);
        let xor = bitwise_xor(&a, &b);
        for i in 0..4 {
            let av = a.data()[i].to_bits();
            let bv = b.data()[i].to_bits();
            assert_eq!(xor.data()[i].to_bits(), av ^ bv);
        }
    }

    #[test]
    fn dimension_mismatch_panics() {
        let a = Image::<u8, 1>::new(4, 4);
        let b = Image::<u8, 1>::new(3, 5);
        let result = std::panic::catch_unwind(|| bitwise_and(&a, &b));
        assert!(result.is_err());
    }

    #[test]
    fn not_involutive() {
        let a = Image::<u8, 1>::from_fn(5, 5, |x, y| [(x * 51 + y * 17) as u8]);
        let not_not = bitwise_not(&bitwise_not(&a));
        assert_eq!(not_not, a);
    }
}
