//! 图像编解码（对应 OpenCV `modules/imgcodecs`）。
//!
//! 公开 API 一律 RGB（铁律 6）：彩色图三通道顺序为 R、G、B。
//! 底层使用 `image` crate 作为编解码层（DESIGN.md §5）。

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use image::ImageFormat;

use crate::core::error::{Error, Result};
use crate::core::image::Image;

/// 写入参数（对应 OpenCV `ImwriteFlags` 的常用子集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImwriteParam {
    /// PNG 压缩级别 0-9（对应 `IMWRITE_PNG_COMPRESSION`，OpenCV 默认 1）。
    /// 0 = 不压缩，9 = 最高压缩（最慢）。
    PngCompression(u8),
    /// JPEG 质量 1-100（对应 `IMWRITE_JPEG_QUALITY`，OpenCV 默认 95）。
    /// 越高画质越好、文件越大。
    JpegQuality(u8),
}

/// 读取图像为 RGB u8（对应 OpenCV `imread(path, IMREAD_COLOR)`，但通道为 RGB）。
///
/// 支持 PNG、JPEG 等 `image` crate 可解码的格式。灰度/索引图会被转换为 RGB。
///
/// # Errors
/// 文件不存在或解码失败返回 [`Error::Io`] / [`Error::Codec`]。
pub fn imread<P: AsRef<Path>>(path: P) -> Result<Image<u8, 3>> {
    let dyn_img = image::open(path).map_err(map_image_error)?;
    let rgb = dyn_img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let data = rgb.into_raw();
    Image::from_vec(w as usize, h as usize, data)
}

/// 读取一张图像为灰度 u8（对应 OpenCV `imread(path, IMREAD_GRAYSCALE)`）。
///
/// 灰度转换使用本库 [`crate::to_grayscale`]（与 OpenCV 整数公式逐位一致），
/// 而非 `image` crate 的 `to_luma8`（其舍入与 OpenCV 不同）。
///
/// # Errors
/// 解码失败返回 [`Error::Io`] / [`Error::Codec`]。
pub fn imread_gray<P: AsRef<Path>>(path: P) -> Result<Image<u8, 1>> {
    let rgb = imread(path)?;
    Ok(crate::to_grayscale(&rgb))
}

/// 写入 u8 图像，格式由文件扩展名推断（PNG/JPEG 等）。
///
/// 支持通道数 1（灰度）、3（RGB）、4（RGBA）。JPEG 不支持透明通道，
/// 写入 RGBA 到 JPEG 时透明通道会被丢弃。
///
/// # Errors
/// 扩展名不支持或写入失败返回 [`Error::Codec`] / [`Error::Io`]；
/// 通道数不支持返回 [`Error::UnsupportedChannels`]。
pub fn imwrite<P: AsRef<Path>, const C: usize>(path: P, img: &Image<u8, C>) -> Result<()> {
    imwrite_with_params(path, img, &[])
}

/// 写入 u8 图像并指定编码参数（对应 OpenCV `imwrite(path, img, params)`）。
///
/// 参数按格式生效：[`ImwriteParam::PngCompression`] 仅对 PNG，
/// [`ImwriteParam::JpegQuality`] 仅对 JPEG；不匹配的参数被忽略。
/// 无参数时行为与 [`imwrite`] 一致。
///
/// # Errors
/// 同 [`imwrite`]。
pub fn imwrite_with_params<P: AsRef<Path>, const C: usize>(
    path: P,
    img: &Image<u8, C>,
    params: &[ImwriteParam],
) -> Result<()> {
    let format = format_from_path(path.as_ref())?;
    let color = color_type_u8(C)?;
    let width = img.width() as u32;
    let height = img.height() as u32;
    let file = File::create(path.as_ref()).map_err(Error::Io)?;
    let mut writer = BufWriter::new(file);
    match format {
        ImageFormat::Png => {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            use image::ImageEncoder;
            // OpenCV 默认 IMWRITE_PNG_COMPRESSION=1；image crate 默认 Fast
            let mut level: Option<u8> = None;
            for p in params {
                if let ImwriteParam::PngCompression(l) = p {
                    level = Some(*l);
                }
            }
            let compression = match level {
                None => CompressionType::Default,
                Some(0) => CompressionType::Uncompressed,
                Some(l) => CompressionType::Level(l.min(9)),
            };
            let enc = PngEncoder::new_with_quality(&mut writer, compression, FilterType::Adaptive);
            enc.write_image(img.data(), width, height, color.into())
                .map_err(map_image_error)
        }
        ImageFormat::Jpeg => {
            use image::codecs::jpeg::JpegEncoder;
            use image::ImageEncoder;
            // OpenCV 默认 IMWRITE_JPEG_QUALITY=95；image crate 默认 75
            let mut quality: u8 = 95;
            for p in params {
                if let ImwriteParam::JpegQuality(q) = p {
                    quality = *q;
                }
            }
            let enc = JpegEncoder::new_with_quality(&mut writer, quality.clamp(1, 100));
            enc.write_image(img.data(), width, height, color.into())
                .map_err(map_image_error)
        }
        _ => image::save_buffer_with_format(path, img.data(), width, height, color, format)
            .map_err(map_image_error),
    }
}

/// 写入 16 位图像（PNG 支持 16 位）。格式由扩展名推断。
///
/// 支持通道数 1（灰度）、3（RGB）、4（RGBA）。
///
/// # Errors
/// 扩展名不支持或写入失败返回 [`Error::Codec`] / [`Error::Io`]；
/// 通道数不支持返回 [`Error::UnsupportedChannels`]。
pub fn imwrite_u16<P: AsRef<Path>, const C: usize>(path: P, img: &Image<u16, C>) -> Result<()> {
    let format = format_from_path(path.as_ref())?;
    if format != ImageFormat::Png {
        return Err(Error::Codec(format!(
            "16-bit images only supported for PNG, got {:?}",
            format
        )));
    }
    let color = color_type_u16(C)?;
    let data: Vec<u8> = img.data().iter().flat_map(|&v| v.to_le_bytes()).collect();
    image::save_buffer(path, &data, img.width() as u32, img.height() as u32, color)
        .map_err(map_image_error)
}

fn color_type_u8(channels: usize) -> Result<image::ColorType> {
    match channels {
        1 => Ok(image::ColorType::L8),
        3 => Ok(image::ColorType::Rgb8),
        4 => Ok(image::ColorType::Rgba8),
        n => Err(Error::UnsupportedChannels(n)),
    }
}

fn color_type_u16(channels: usize) -> Result<image::ColorType> {
    match channels {
        1 => Ok(image::ColorType::L16),
        3 => Ok(image::ColorType::Rgb16),
        4 => Ok(image::ColorType::Rgba16),
        n => Err(Error::UnsupportedChannels(n)),
    }
}

fn format_from_path(path: &Path) -> Result<ImageFormat> {
    ImageFormat::from_path(path).map_err(|_| {
        Error::Codec(format!(
            "unsupported or missing image extension: {}",
            path.display()
        ))
    })
}

fn map_image_error(e: image::ImageError) -> Error {
    match e {
        image::ImageError::IoError(io) => Error::Io(io),
        other => Error::Codec(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imread_imwrite_rgb_roundtrip() {
        let path = std::env::temp_dir().join("rcv_test_rgb.png");
        let img = Image::<u8, 3>::from_fn(4, 3, |x, y| [x as u8, y as u8, (x + y) as u8]);
        imwrite(&path, &img).unwrap();
        let back = imread(&path).unwrap();
        assert_eq!(back, img);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imwrite_gray_roundtrip() {
        let path = std::env::temp_dir().join("rcv_test_gray.png");
        let img = Image::<u8, 1>::from_fn(3, 2, |x, y| [(x * 2 + y) as u8]);
        imwrite(&path, &img).unwrap();
        let loaded = imread_gray(&path).unwrap();
        assert_eq!(loaded, img);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imread_missing_file() {
        let err = imread("/nonexistent/rcv_missing.png").unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn imwrite_unsupported_extension() {
        let img = Image::<u8, 3>::new(2, 2);
        let err = imwrite("/tmp/rcv_test.xyz", &img).unwrap_err();
        assert!(matches!(err, Error::Codec(_)));
    }

    #[test]
    fn imwrite_unsupported_channels() {
        let img = Image::<u8, 2>::new(2, 2);
        let err = imwrite("/tmp/rcv_test.png", &img).unwrap_err();
        assert!(matches!(err, Error::UnsupportedChannels(2)));
    }

    #[test]
    fn imwrite_u16_png_roundtrip() {
        let path = std::env::temp_dir().join("rcv_test_u16.png");
        let img = Image::<u16, 1>::from_fn(2, 2, |x, y| [(x * 1000 + y * 100) as u16]);
        imwrite_u16(&path, &img).unwrap();
        let loaded = imread_gray(&path).unwrap();
        assert_eq!(loaded.width(), 2);
        assert_eq!(loaded.height(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imwrite_u16_non_png_rejected() {
        let img = Image::<u16, 1>::new(2, 2);
        let err = imwrite_u16("/tmp/rcv_test.jpg", &img).unwrap_err();
        assert!(matches!(err, Error::Codec(_)));
    }

    #[test]
    fn imwrite_png_compression_levels() {
        let img = Image::<u8, 1>::from_fn(64, 64, |x, y| [((x * 7 + y * 13) % 256) as u8]);
        let dir = std::env::temp_dir();
        let mut sizes = Vec::new();
        for level in [0u8, 6, 9] {
            let path = dir.join(format!("rcv_test_png_c{level}.png"));
            imwrite_with_params(&path, &img, &[ImwriteParam::PngCompression(level)]).unwrap();
            sizes.push(std::fs::metadata(&path).unwrap().len());
            let _ = std::fs::remove_file(&path);
        }
        // 高压缩级别文件应更小（梯度图案可压缩）
        assert!(sizes[2] < sizes[0], "level9 {sizes:?} should < level0");
        // 往返无损
        let path = dir.join("rcv_test_png_c6.png");
        imwrite_with_params(&path, &img, &[ImwriteParam::PngCompression(6)]).unwrap();
        let back = imread_gray(&path).unwrap();
        assert_eq!(back, img);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imwrite_jpeg_quality_roundtrip() {
        let img = Image::<u8, 3>::from_fn(32, 32, |x, y| [x as u8 * 8, y as u8 * 8, 128]);
        let dir = std::env::temp_dir();
        let mut sizes = Vec::new();
        for q in [10u8, 95] {
            let path = dir.join(format!("rcv_test_jpg_q{q}.jpg"));
            imwrite_with_params(&path, &img, &[ImwriteParam::JpegQuality(q)]).unwrap();
            sizes.push(std::fs::metadata(&path).unwrap().len());
            let _ = std::fs::remove_file(&path);
        }
        // 高质量文件应更大
        assert!(sizes[1] > sizes[0], "q95 {sizes:?} should > q10");
        // 默认（无参数）= q95
        let p1 = dir.join("rcv_test_jpg_default.jpg");
        imwrite(&p1, &img).unwrap();
        let p2 = dir.join("rcv_test_jpg_q95b.jpg");
        imwrite_with_params(&p2, &img, &[ImwriteParam::JpegQuality(95)]).unwrap();
        assert_eq!(
            std::fs::metadata(&p1).unwrap().len(),
            std::fs::metadata(&p2).unwrap().len()
        );
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn imwrite_params_ignored_for_mismatched_format() {
        // JPEG 参数传给 PNG：被忽略，正常写出
        let img = Image::<u8, 1>::from_fn(4, 4, |x, y| [(x * y) as u8]);
        let path = std::env::temp_dir().join("rcv_test_mixed.png");
        imwrite_with_params(&path, &img, &[ImwriteParam::JpegQuality(50)]).unwrap();
        let back = imread_gray(&path).unwrap();
        assert_eq!(back, img);
        let _ = std::fs::remove_file(&path);
    }
}
