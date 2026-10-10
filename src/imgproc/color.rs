//! 颜色空间转换（对应 OpenCV `modules/imgproc` 的 `cvtColor`）。
//!
//! 公开 API 一律 RGB（铁律 6）：彩色图三通道顺序为 R、G、B。

use crate::core::image::{Image, Pixel};

/// RGB 转灰度（对应 OpenCV `cvtColor(..., COLOR_RGB2GRAY)`）。
///
/// 与 OpenCV 逐像素一致：
/// - u8/u16：整数定点公式 `(R·9798 + G·19235 + B·3735 + 16384) >> 15`
///   （round-half-up，对应 OpenCV `CV_DESCALE`）
/// - f32：浮点公式 `R·0.299 + G·0.587 + B·0.114`
///
/// 复杂度 O(width·height·3)。
pub fn to_grayscale<T: GrayConvert + bytemuck::Pod>(src: &Image<T, 3>) -> Image<T, 1> {
    let (w, h) = (src.width(), src.height());
    let mut out = Image::new_uninit(w, h);
    // u8 特派：SIMD 派发（AVX512 → AVX2 → 标量）+ rayon 行并行
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<u8>() && w > 0 && h > 0 {
        let src_bytes: &[u8] = bytemuck::cast_slice(src.data());
        let dst_bytes: &mut [u8] = bytemuck::cast_slice_mut(out.data_mut());
        // 行块并行：带宽型小任务用自旋池（rayon fork-join ~33us/次
        // 超过 1MB 级任务的计算时间）
        // 64 行大块：实测 16 行块在 MT 下损失 ~2x 带宽（与 threshold 结论一致）
        const MIN_ROWS_PER_BLOCK: usize = 64;
        let chunk = MIN_ROWS_PER_BLOCK.min(h.max(1));
        if chunk > 0 {
            // 块数不超过并行度：过订阅（64 块挤 16 核）反而损失带宽
            let max_blocks = rayon::current_num_threads().max(1);
            let nblocks = dst_bytes.len().div_ceil(w * chunk).min(max_blocks);
            if nblocks <= 1 || rayon::current_num_threads() <= 1 {
                // 单块或单线程池：直接串行，避免任务分发开销
                crate::simd_kernels::to_grayscale_u8(src_bytes, dst_bytes, w * h);
            } else if w * h % nblocks == 0 {
                // 像素数可整除：自旋池固定分块（src 块 = 像素数/块 × 3）
                crate::simd_kernels::spin_pool::run_blocks(
                    src_bytes,
                    dst_bytes,
                    nblocks,
                    crate::simd_kernels::to_grayscale_block,
                    w * h / nblocks,
                );
            } else {
                use rayon::prelude::*;
                let row_bytes = w * 3;
                dst_bytes
                    .par_chunks_mut(w * chunk)
                    .enumerate()
                    .for_each(|(i, dst_slice)| {
                        let rows = dst_slice.len() / w;
                        let n = rows * w;
                        let src_slice =
                            &src_bytes[i * row_bytes * chunk..i * row_bytes * chunk + n * 3];
                        crate::simd_kernels::to_grayscale_u8(src_slice, dst_slice, n);
                    });
            }
        }
        return out;
    }
    // 其他类型标量
    for (dst, px) in out.pixels_mut().zip(src.pixels()) {
        dst[0] = T::gray(px[0], px[1], px[2]);
    }
    out
}

/// 灰度转 RGB（对应 OpenCV `cvtColor(..., COLOR_GRAY2RGB)`）。
///
/// 三个通道复制灰度值。复杂度 O(n·3)。
pub fn to_rgb<T: Pixel>(src: &Image<T, 1>) -> Image<T, 3> {
    let mut out = Image::new(src.width(), src.height());
    for (dst_px, src_px) in out.pixels_mut().zip(src.pixels()) {
        let v = src_px[0];
        dst_px[0] = v;
        dst_px[1] = v;
        dst_px[2] = v;
    }
    out
}

/// 灰度转换的内部 trait：按像素类型实现 OpenCV 一致的公式。
/// 公开以支持 `to_grayscale` 的泛型约束。
pub trait GrayConvert: Pixel {
    fn gray(r: Self, g: Self, b: Self) -> Self;
}

impl GrayConvert for u8 {
    fn gray(r: u8, g: u8, b: u8) -> u8 {
        ((r as u32 * 9798 + g as u32 * 19235 + b as u32 * 3735 + 16384) >> 15) as u8
    }
}

impl GrayConvert for u16 {
    fn gray(r: u16, g: u16, b: u16) -> u16 {
        ((r as u32 * 9798 + g as u32 * 19235 + b as u32 * 3735 + 16384) >> 15) as u16
    }
}

impl GrayConvert for f32 {
    fn gray(r: f32, g: f32, b: f32) -> f32 {
        r * 0.299 + g * 0.587 + b * 0.114
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_grayscale_known_values() {
        // 纯黑 → 0，纯白 → 255
        let black = Image::<u8, 3>::from_pixel(1, 1, [0, 0, 0]);
        assert_eq!(to_grayscale(&black).pixel(0, 0).unwrap(), &[0]);

        let white = Image::<u8, 3>::from_pixel(1, 1, [255, 255, 255]);
        assert_eq!(to_grayscale(&white).pixel(0, 0).unwrap(), &[255]);

        // 纯红 R=255 → (255*9798 + 16384)>>15 = (2498490+16384)>>15 = 2514874>>15 = 76
        let red = Image::<u8, 3>::from_pixel(1, 1, [255, 0, 0]);
        assert_eq!(to_grayscale(&red).pixel(0, 0).unwrap(), &[76]);

        // 纯绿 G=255 → (255*19235 + 16384)>>15 = (4904925+16384)>>15 = 4921309>>15 = 150
        let green = Image::<u8, 3>::from_pixel(1, 1, [0, 255, 0]);
        assert_eq!(to_grayscale(&green).pixel(0, 0).unwrap(), &[150]);

        // 纯蓝 B=255 → (255*3735 + 16384)>>15 = (952425+16384)>>15 = 968809>>15 = 29
        let blue = Image::<u8, 3>::from_pixel(1, 1, [0, 0, 255]);
        assert_eq!(to_grayscale(&blue).pixel(0, 0).unwrap(), &[29]);
    }

    #[test]
    fn grayscale_matches_manual() {
        // R=100,G=150,B=200 → (100*9798+150*19235+200*3735+16384)>>15
        // = (979800+2885250+747000+16384)>>15 = 4628434>>15 = 141
        let img = Image::<u8, 3>::from_pixel(1, 1, [100, 150, 200]);
        assert_eq!(to_grayscale(&img).pixel(0, 0).unwrap(), &[141]);
    }

    #[test]
    fn to_rgb_replicates() {
        let gray = Image::<u8, 1>::from_pixel(2, 1, [77]);
        let rgb = to_rgb(&gray);
        assert_eq!(rgb.pixel(0, 0).unwrap(), &[77, 77, 77]);
        assert_eq!(rgb.pixel(1, 0).unwrap(), &[77, 77, 77]);
        assert_eq!(rgb.dims().channels, 3);
    }

    #[test]
    fn grayscale_f32() {
        let img = Image::<f32, 3>::from_pixel(1, 1, [1.0, 0.5, 0.0]);
        let gray = to_grayscale(&img);
        let expected = 1.0 * 0.299 + 0.5 * 0.587 + 0.0 * 0.114;
        assert!((gray.pixel(0, 0).unwrap()[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn grayscale_edge_sizes() {
        let row = Image::<u8, 3>::from_fn(3, 1, |x, _| [x as u8, x as u8, x as u8]);
        let g = to_grayscale(&row);
        assert_eq!(g.width(), 3);
        assert_eq!(g.height(), 1);

        let col = Image::<u8, 3>::from_fn(1, 3, |_, y| [y as u8, y as u8, y as u8]);
        let g = to_grayscale(&col);
        assert_eq!(g.width(), 1);
        assert_eq!(g.height(), 3);

        let empty = Image::<u8, 3>::new(0, 0);
        let g = to_grayscale(&empty);
        assert!(g.is_empty());
    }
}
