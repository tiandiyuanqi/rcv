//! 文字绘制（内置位图字体，支持汉字）。
//!
//! 使用 16x16 点阵位图字体（见 `font_data.rs`），支持 ASCII 与常用汉字。
//! 与 OpenCV `putText` 不同，本实现不依赖 Hershey 矢量字体，而是直接
//! 将位图字形 blit 到图像上，天然支持汉字。

use crate::core::image::Image;
use crate::core::point::Point;
use crate::imgproc::font_data::{FONT_GLYPHS, FONT_SIZE};

/// 查找字符对应的位图字形。返回 `(码点, 位图)`，未找到返回 `None`。
fn lookup_glyph(c: char) -> Option<&'static [u8; 32]> {
    let cp = c as u32;
    // 二分查找（FONT_GLYPHS 按码点升序）
    let idx = FONT_GLYPHS
        .binary_search_by_key(&cp, |&(cp, _)| cp)
        .ok()?;
    Some(&FONT_GLYPHS[idx].1)
}

/// 计算文本的像素尺寸（宽 × 高）。
///
/// `scale` 为字形缩放（1.0 = 16x16）。返回 `(width, height)`。
pub fn get_text_size(text: &str, scale: f32) -> (u32, u32) {
    let cell = (FONT_SIZE as f32 * scale) as u32;
    let n = text.chars().count();
    (cell * n as u32, cell)
}

/// 写文字（单通道）。
///
/// `org` 为文本左上角（OpenCV 语义为基线左下，此处简化为左上角）。
/// `scale` 为字形缩放（1.0 = 16x16）。`thickness` 为笔画粗细（1 = 正常，
/// 更大值会加粗）。
/// 单通道 blit 一个字形到 (cx, cy)，支持缩放与加粗。
#[inline]
fn blit_gray(
    data: &mut [u8],
    w: i32,
    h: i32,
    step: usize,
    glyph: &[u8; 32],
    cx: i32,
    cy: i32,
    cell: i32,
    thick: i32,
    color: u8,
) {
    let r = thick - 1;
    if cell == FONT_SIZE as i32 && thick == 1 {
        // 快路径：1:1 无缩放无加粗，逐像素 blit
        let gx0 = cx.max(0);
        let gy0 = cy.max(0);
        let gx1 = (cx + FONT_SIZE as i32).min(w);
        let gy1 = (cy + FONT_SIZE as i32).min(h);
        if gx0 < gx1 && gy0 < gy1 {
            for py in gy0..gy1 {
                let row = &mut data[py as usize * step..];
                for px in gx0..gx1 {
                    let gx = px - cx;
                    let gy = py - cy;
                    let byte = (gy * FONT_SIZE as i32 + gx) as usize / 8;
                    let bit = 7 - ((gy * FONT_SIZE as i32 + gx) as usize % 8);
                    if (glyph[byte] >> bit) & 1 != 0 {
                        row[px as usize] = color;
                    }
                }
            }
        }
    } else {
        // 通用路径：缩放 + 加粗（逐字形像素扩展）
        for gy in 0..FONT_SIZE as i32 {
            for gx in 0..FONT_SIZE as i32 {
                let byte = (gy * FONT_SIZE as i32 + gx) as usize / 8;
                let bit = 7 - ((gy * FONT_SIZE as i32 + gx) as usize % 8);
                if (glyph[byte] >> bit) & 1 == 0 {
                    continue;
                }
                let x0 = cx + gx * cell / FONT_SIZE as i32;
                let y0 = cy + gy * cell / FONT_SIZE as i32;
                let x1 = cx + (gx + 1) * cell / FONT_SIZE as i32;
                let y1 = cy + (gy + 1) * cell / FONT_SIZE as i32;
                for py in (y0 - r)..y1 {
                    if py < 0 || py >= h {
                        continue;
                    }
                    let row = &mut data[py as usize * step..];
                    for px in (x0 - r)..x1 {
                        if px >= 0 && px < w {
                            row[px as usize] = color;
                        }
                    }
                }
            }
        }
    }
}

/// 3 通道 blit 一个字形到 (cx, cy)，支持缩放与加粗。
#[inline]
fn blit_rgb(
    data: &mut [u8],
    w: i32,
    h: i32,
    step: usize,
    glyph: &[u8; 32],
    cx: i32,
    cy: i32,
    cell: i32,
    thick: i32,
    color: [u8; 3],
) {
    let r = thick - 1;
    if cell == FONT_SIZE as i32 && thick == 1 {
        // 快路径：1:1 无缩放无加粗
        let gx0 = cx.max(0);
        let gy0 = cy.max(0);
        let gx1 = (cx + FONT_SIZE as i32).min(w);
        let gy1 = (cy + FONT_SIZE as i32).min(h);
        if gx0 < gx1 && gy0 < gy1 {
            for py in gy0..gy1 {
                let row = &mut data[py as usize * step..];
                for px in gx0..gx1 {
                    let gx = px - cx;
                    let gy = py - cy;
                    let byte = (gy * FONT_SIZE as i32 + gx) as usize / 8;
                    let bit = 7 - ((gy * FONT_SIZE as i32 + gx) as usize % 8);
                    if (glyph[byte] >> bit) & 1 != 0 {
                        let idx = px as usize * 3;
                        row[idx] = color[0];
                        row[idx + 1] = color[1];
                        row[idx + 2] = color[2];
                    }
                }
            }
        }
    } else {
        for gy in 0..FONT_SIZE as i32 {
            for gx in 0..FONT_SIZE as i32 {
                let byte = (gy * FONT_SIZE as i32 + gx) as usize / 8;
                let bit = 7 - ((gy * FONT_SIZE as i32 + gx) as usize % 8);
                if (glyph[byte] >> bit) & 1 == 0 {
                    continue;
                }
                let x0 = cx + gx * cell / FONT_SIZE as i32;
                let y0 = cy + gy * cell / FONT_SIZE as i32;
                let x1 = cx + (gx + 1) * cell / FONT_SIZE as i32;
                let y1 = cy + (gy + 1) * cell / FONT_SIZE as i32;
                for py in (y0 - r)..y1 {
                    if py < 0 || py >= h {
                        continue;
                    }
                    let row = &mut data[py as usize * step..];
                    for px in (x0 - r)..x1 {
                        if px >= 0 && px < w {
                            let idx = px as usize * 3;
                            row[idx] = color[0];
                            row[idx + 1] = color[1];
                            row[idx + 2] = color[2];
                        }
                    }
                }
            }
        }
    }
}

pub fn put_text<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 1>,
    text: &str,
    org: Point,
    color: [T; 1],
    scale: f32,
    thickness: i32,
) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let step = img.width();
    let data: &mut [u8] = bytemuck::cast_slice_mut(img.data_mut());
    let color_byte = bytemuck::cast(color[0]);
    let cell = (FONT_SIZE as f32 * scale).max(1.0) as i32;
    let thick = thickness.max(1) as i32;

    let mut cx = org.x;
    let mut cy = org.y;
    for c in text.chars() {
        if c == '\n' {
            cx = org.x;
            cy += cell;
            continue;
        }
        if let Some(glyph) = lookup_glyph(c) {
            blit_gray(data, w, h, step, glyph, cx, cy, cell, thick, color_byte);
        }
        cx += cell;
    }
}

/// 写文字（3 通道）。
pub fn put_text_rgb<T: crate::core::image::Pixel + bytemuck::Pod>(
    img: &mut Image<T, 3>,
    text: &str,
    org: Point,
    color: [T; 3],
    scale: f32,
    thickness: i32,
) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let step = img.width() * 3;
    let data: &mut [u8] = bytemuck::cast_slice_mut(img.data_mut());
    let color_bytes = [
        bytemuck::cast(color[0]),
        bytemuck::cast(color[1]),
        bytemuck::cast(color[2]),
    ];
    let cell = (FONT_SIZE as f32 * scale).max(1.0) as i32;
    let thick = thickness.max(1) as i32;

    let mut cx = org.x;
    let mut cy = org.y;
    for c in text.chars() {
        if c == '\n' {
            cx = org.x;
            cy += cell;
            continue;
        }
        if let Some(glyph) = lookup_glyph(c) {
            blit_rgb(data, w, h, step, glyph, cx, cy, cell, thick, color_bytes);
        }
        cx += cell;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_text_size_basic() {
        assert_eq!(get_text_size("中", 1.0), (16, 16));
        assert_eq!(get_text_size("中", 2.0), (32, 32));
        assert_eq!(get_text_size("AB", 1.0), (32, 16));
        assert_eq!(get_text_size("", 1.0), (0, 16));
    }

    #[test]
    fn put_text_gray_renders() {
        let mut img = Image::<u8, 1>::new(16, 16);
        put_text(&mut img, "中", Point::new(0, 0), [255u8], 1.0, 1);
        let nz = img.data().iter().filter(|&&v| v != 0).count();
        assert!(nz > 20, "中 should render many pixels, got {nz}");
    }

    #[test]
    fn put_text_ascii_renders() {
        let mut img = Image::<u8, 1>::new(16, 16);
        put_text(&mut img, "A", Point::new(0, 0), [255u8], 1.0, 1);
        let nz = img.data().iter().filter(|&&v| v != 0).count();
        assert!(nz > 5, "A should render, got {nz}");
    }

    #[test]
    fn put_text_scale() {
        let mut img1 = Image::<u8, 1>::new(16, 16);
        put_text(&mut img1, "中", Point::new(0, 0), [255u8], 1.0, 1);
        let nz1 = img1.data().iter().filter(|&&v| v != 0).count();

        let mut img2 = Image::<u8, 1>::new(32, 32);
        put_text(&mut img2, "中", Point::new(0, 0), [255u8], 2.0, 1);
        let nz2 = img2.data().iter().filter(|&&v| v != 0).count();
        assert!(nz2 > nz1 * 3, "2x scale should have ~4x pixels, {nz1} -> {nz2}");
    }

    #[test]
    fn put_text_thickness() {
        let mut img1 = Image::<u8, 1>::new(16, 16);
        put_text(&mut img1, "中", Point::new(0, 0), [255u8], 1.0, 1);
        let nz1 = img1.data().iter().filter(|&&v| v != 0).count();

        let mut img3 = Image::<u8, 1>::new(16, 16);
        put_text(&mut img3, "中", Point::new(0, 0), [255u8], 1.0, 3);
        let nz3 = img3.data().iter().filter(|&&v| v != 0).count();
        assert!(nz3 > nz1, "thicker should have more pixels, {nz1} -> {nz3}");
    }

    #[test]
    fn put_text_rgb_renders() {
        let mut img = Image::<u8, 3>::new(16, 16);
        put_text_rgb(&mut img, "中", Point::new(0, 0), [255u8, 0, 0], 1.0, 1);
        let nz = img.data().chunks(3).filter(|c| c[0] != 0).count();
        assert!(nz > 20, "中 RGB should render, got {nz}");
        // 验证颜色正确
        let red = img.data().chunks(3).find(|c| c[0] != 0).unwrap();
        assert_eq!(red, &[255, 0, 0]);
    }

    #[test]
    fn put_text_clips_at_bounds() {
        // 文本超出图像边界不应 panic
        let mut img = Image::<u8, 1>::new(8, 8);
        put_text(&mut img, "中文字符串很长", Point::new(0, 0), [255u8], 1.0, 1);
        // 负坐标
        put_text(&mut img, "中", Point::new(-5, -5), [255u8], 1.0, 1);
    }

    #[test]
    fn put_text_unknown_char_skips() {
        // 未收录字符（如生僻字）应跳过不 panic
        let mut img = Image::<u8, 1>::new(32, 16);
        put_text(&mut img, "A\u{20000}B", Point::new(0, 0), [255u8], 1.0, 1);
        // A 和 B 应渲染，生僻字跳过
        let nz = img.data().iter().filter(|&&v| v != 0).count();
        assert!(nz > 5);
    }
}

    #[test]
    fn put_text_newline_advances_y() {
        let mut img = Image::<u8, 1>::new(32, 40);
        put_text(&mut img, "A\nB", Point::new(0, 0), [255u8], 1.0, 1);
        // 第一行 'A' 在 y=0..16，第二行 'B' 在 y=16..32（图像宽 32）
        let row0 = img.data()[0..16 * 32].iter().filter(|&&v| v != 0).count();
        let row16 = img.data()[16 * 32..32 * 32].iter().filter(|&&v| v != 0).count();
        assert!(row0 > 0, "first line should render");
        assert!(row16 > 0, "second line should render at y+16");
    }
