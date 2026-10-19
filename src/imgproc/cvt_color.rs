//! 颜色空间转换（对齐 OpenCV 5 `cvtColor`）。
//!
//! 公开 API 一律 RGB（铁律 6）：彩色图三通道顺序为 R、G、B。
//! 支持 u8 图像的 RGB↔BGR、RGB↔RGBA、RGB↔GRAY、RGB↔HSV、RGB↔YCrCb。

use crate::core::image::Image;

/// 颜色转换类型（对应 OpenCV `ColorConversionCodes` 的常用子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorConversion {
    /// RGB ↔ BGR（通道交换）。
    RgbToBgr,
    /// BGR → RGB（通道交换）。
    BgrToRgb,
    /// RGB → RGBA（alpha=255）。
    RgbToRgba,
    /// RGBA → RGB（丢弃 alpha）。
    RgbaToRgb,
    /// RGB → 灰度。
    RgbToGray,
    /// 灰度 → RGB。
    GrayToRgb,
    /// RGB → HSV（H ∈ [0,180)）。
    RgbToHsv,
    /// HSV → RGB。
    HsvToRgb,
    /// RGB → YCrCb。
    RgbToYCrCb,
    /// YCrCb → RGB。
    YCrCbToRgb,
}

/// 颜色空间转换（对齐 `cv::cvtColor`）。
///
/// 输入输出通道数由转换类型决定。仅支持 u8。
pub fn cvt_color(src: &Image<u8, 3>, code: ColorConversion) -> Image<u8, 3> {
    let (w, h) = (src.width(), src.height());
    let sdata = src.data();
    let mut out = Image::<u8, 3>::new(w, h);
    let ddata = out.data_mut();
    match code {
        ColorConversion::RgbToBgr | ColorConversion::BgrToRgb => {
            for (i, px) in sdata.chunks(3).enumerate() {
                ddata[i * 3] = px[2];
                ddata[i * 3 + 1] = px[1];
                ddata[i * 3 + 2] = px[0];
            }
        }
        ColorConversion::RgbToGray => {
            for (i, px) in sdata.chunks(3).enumerate() {
                let g = ((px[0] as u32 * 9798 + px[1] as u32 * 19235 + px[2] as u32 * 3735 + 16384) >> 15) as u8;
                ddata[i * 3] = g;
                ddata[i * 3 + 1] = g;
                ddata[i * 3 + 2] = g;
            }
        }
        ColorConversion::RgbToHsv => {
            rgb_to_hsv(sdata, ddata, w * h, 180);
        }
        ColorConversion::HsvToRgb => {
            hsv_to_rgb(sdata, ddata, w, h, 180);
        }
        ColorConversion::RgbToYCrCb => {
            rgb_to_ycrcb(sdata, ddata, w * h);
        }
        ColorConversion::YCrCbToRgb => {
            ycrcb_to_rgb(sdata, ddata, w * h);
        }
        _ => unreachable!("unsupported conversion for 3-channel input"),
    }
    out
}

/// RGBA 输入的颜色转换（RGB↔RGBA）。
pub fn cvt_color_rgba(src: &Image<u8, 4>, code: ColorConversion) -> Image<u8, 4> {
    let (w, h) = (src.width(), src.height());
    let sdata = src.data();
    let mut out = Image::<u8, 4>::new(w, h);
    let ddata = out.data_mut();
    match code {
        ColorConversion::RgbaToRgb => {
            // 4→3：丢弃 alpha
            let mut rgb = Image::<u8, 3>::new(w, h);
            let rd = rgb.data_mut();
            for (i, px) in sdata.chunks(4).enumerate() {
                rd[i * 3] = px[0];
                rd[i * 3 + 1] = px[1];
                rd[i * 3 + 2] = px[2];
            }
            return cvt_color(&rgb, ColorConversion::RgbToBgr).into_rgba();
        }
        _ => unreachable!("unsupported conversion for 4-channel input"),
    }
}

impl Image<u8, 3> {
    fn into_rgba(self) -> Image<u8, 4> {
        let (w, h) = (self.width(), self.height());
        let mut out = Image::<u8, 4>::new(w, h);
        let d = out.data_mut();
        for (i, px) in self.data().chunks(3).enumerate() {
            d[i * 4] = px[0];
            d[i * 4 + 1] = px[1];
            d[i * 4 + 2] = px[2];
            d[i * 4 + 3] = 255;
        }
        out
    }
}

// ---- RGB2HSV（对齐 color_hsv.simd.hpp RGB2HSV_b 标量路径）----

const HSV_SHIFT: i32 = 12;

pub(crate) fn hsv_sdiv_table() -> [i32; 256] {
    let mut t = [0i32; 256];
    for i in 1..256 {
        t[i] = ((255 << HSV_SHIFT) as f64 / i as f64).round() as i32;
    }
    t
}

pub(crate) fn hsv_hdiv_table(hrange: i32) -> [i32; 256] {
    let mut t = [0i32; 256];
    for i in 1..256 {
        t[i] = ((hrange << HSV_SHIFT) as f64 / (6.0 * i as f64)).round() as i32;
    }
    t
}

fn rgb_to_hsv(src: &[u8], dst: &mut [u8], n: usize, hrange: i32) {
    crate::simd_kernels::cvt_color::rgb_to_hsv(src, dst, n, hrange);
}

// ---- HSV2RGB（对齐 color_hsv.simd.hpp HSV2RGB_b）----
//
// OpenCV 的 HSV2RGB_b 按行处理，每行 n = width。dispatch 用 AVX2
// （vsize=8，4*vsize=32）：
//   - width >= 32 时，每行前 floor(w/32)*32 个像素走 SIMD 路径：
//     sector 用 v_trunc，输出用 v_trunc（截断），且 tab2/tab3 的
//     `1 - s*h` / `1 - s*(1-h)` 被编译器 FMA 融合（vfnmadd132ps）。
//     尾部 w % 32 个像素走标量路径（cvRound，RNE，无 FMA）。
//   - width < 32 时全部走标量路径。

fn hsv_to_rgb(src: &[u8], dst: &mut [u8], w: usize, h: usize, hrange: i32) {
    let hscale = 6.0f32 / hrange as f32;
    let sector_data: [[usize; 3]; 6] = [
        [1, 3, 0],
        [1, 0, 2],
        [3, 0, 1],
        [0, 2, 1],
        [0, 1, 3],
        [2, 1, 0],
    ];
    if w >= 32 {
        // 每行前 floor(w/32)*32 个像素走 SIMD（截断 + FMA），
        // 尾部 w % 32 个像素走标量（RNE）。
        let simd_per_row = (w / 32) * 32;
        for y in 0..h {
            let row = y * w;
            // SIMD 部分（截断 + FMA）
            crate::simd_kernels::cvt_color::hsv_to_rgb(
                &src[row * 3..(row + simd_per_row) * 3],
                &mut dst[row * 3..(row + simd_per_row) * 3],
                simd_per_row,
                hrange,
            );
            for i in simd_per_row..w {
                let idx = (row + i) * 3;
                let h = src[idx] as f32;
                let s = src[idx + 1] as f32 * (1.0f32 / 255.0f32);
                let v = src[idx + 2] as f32 * (1.0f32 / 255.0f32);
                let (b, g, r) = if s == 0.0 {
                    (v, v, v)
                } else {
                    let mut h = h * hscale;
                    let mut sector = h.floor() as i32;
                    h -= sector as f32;
                    sector %= 6;
                    if sector < 0 {
                        sector += 6;
                    }
                    let tab = [
                        v,
                        v * (1.0 - s),
                        v * (1.0 - s * h),
                        v * (1.0 - s * (1.0 - h)),
                    ];
                    let sd = sector_data[sector as usize];
                    (tab[sd[0]], tab[sd[1]], tab[sd[2]])
                };
                // 对齐 OpenCV 标量路径：saturate_cast<uchar> = cvRound（RNE）。
                dst[idx] = sat_u8_f(r * 255.0);
                dst[idx + 1] = sat_u8_f(g * 255.0);
                dst[idx + 2] = sat_u8_f(b * 255.0);
            }
        }
    } else {
        // 全部走标量路径：RNE，无 FMA
        let n = w * h;
        for i in 0..n {
            let h = src[i * 3] as f32;
            let s = src[i * 3 + 1] as f32 * (1.0f32 / 255.0f32);
            let v = src[i * 3 + 2] as f32 * (1.0f32 / 255.0f32);
            let (b, g, r) = if s == 0.0 {
                (v, v, v)
            } else {
                let mut h = h * hscale;
                let mut sector = h.floor() as i32;
                h -= sector as f32;
                sector %= 6;
                if sector < 0 {
                    sector += 6;
                }
                let tab = [
                    v,
                    v * (1.0 - s),
                    v * (1.0 - s * h),
                    v * (1.0 - s * (1.0 - h)),
                ];
                let sd = sector_data[sector as usize];
                (tab[sd[0]], tab[sd[1]], tab[sd[2]])
            };
            // HSV2RGB_native 返回 BGR 顺序；RGB 输出需交换 b/r。
            // 对齐 OpenCV 标量路径：saturate_cast<uchar> = cvRound（RNE）。
            dst[i * 3] = sat_u8_f(r * 255.0);
            dst[i * 3 + 1] = sat_u8_f(g * 255.0);
            dst[i * 3 + 2] = sat_u8_f(b * 255.0);
        }
    }
}

// ---- RGB2YCrCb（对齐 color_yuv.simd.hpp RGB2YCrCb_i<uchar> 标量路径）----

const YUV_SHIFT: i32 = 14;
const R2Y: i32 = 4899;
const G2Y: i32 = 9617;
const B2Y: i32 = 1868;
const YCRI: i32 = 11682;
const YCBI: i32 = 9241;

fn rgb_to_ycrcb(src: &[u8], dst: &mut [u8], n: usize) {
    crate::simd_kernels::cvt_color::rgb_to_ycrcb(src, dst, n);
}

/// saturate_cast<uchar>(int)：clamp 到 [0,255]。
#[inline]
fn sat_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// saturate_cast<uchar>(float)：cvRound（RNE）+ clamp。
#[inline]
fn sat_u8_f(v: f32) -> u8 {
    v.round_ties_even().clamp(0.0, 255.0) as u8
}

/// 截断 + clamp（对齐 OpenCV SIMD 路径的 v_trunc + v_pack_u）。
#[inline]
fn sat_u8_trunc(v: f32) -> u8 {
    v.trunc().clamp(0.0, 255.0) as u8
}

// ---- YCrCb2RGB（对齐 color_yuv.simd.hpp YCrCb2RGB_i<uchar> 标量路径，整数定点）----

const CR2RI: i32 = 22987;
const CR2GI: i32 = -11698;
const CB2GI: i32 = -5636;
const CB2BI: i32 = 29049;

fn ycrcb_to_rgb(src: &[u8], dst: &mut [u8], n: usize) {
    crate::simd_kernels::cvt_color::ycrcb_to_rgb(src, dst, n);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_bgr() {
        let img = Image::<u8, 3>::from_pixel(1, 1, [10, 20, 30]);
        let out = cvt_color(&img, ColorConversion::RgbToBgr);
        assert_eq!(out.pixel(0, 0).unwrap(), &[30, 20, 10]);
    }

    #[test]
    fn rgb_to_gray() {
        let img = Image::<u8, 3>::from_pixel(1, 1, [255, 0, 0]);
        let out = cvt_color(&img, ColorConversion::RgbToGray);
        // (255*9798 + 16384)>>15 = 76
        assert_eq!(out.pixel(0, 0).unwrap(), &[76, 76, 76]);
    }

    #[test]
    fn rgb_to_hsv_known() {
        // 纯红 R=255 → H=0, S=255, V=255
        let img = Image::<u8, 3>::from_pixel(1, 1, [255, 0, 0]);
        let out = cvt_color(&img, ColorConversion::RgbToHsv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0, 255, 255]);

        // 纯绿 G=255 → H=60, S=255, V=255
        let img = Image::<u8, 3>::from_pixel(1, 1, [0, 255, 0]);
        let out = cvt_color(&img, ColorConversion::RgbToHsv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[60, 255, 255]);

        // 纯蓝 B=255 → H=120, S=255, V=255
        let img = Image::<u8, 3>::from_pixel(1, 1, [0, 0, 255]);
        let out = cvt_color(&img, ColorConversion::RgbToHsv);
        assert_eq!(out.pixel(0, 0).unwrap(), &[120, 255, 255]);
    }

    #[test]
    fn rgb_to_ycrcb_known() {
        // 纯白 → Y=255, Cr=128, Cb=128
        let img = Image::<u8, 3>::from_pixel(1, 1, [255, 255, 255]);
        let out = cvt_color(&img, ColorConversion::RgbToYCrCb);
        assert_eq!(out.pixel(0, 0).unwrap(), &[255, 128, 128]);

        // 纯黑 → Y=0, Cr=128, Cb=128
        let img = Image::<u8, 3>::from_pixel(1, 1, [0, 0, 0]);
        let out = cvt_color(&img, ColorConversion::RgbToYCrCb);
        assert_eq!(out.pixel(0, 0).unwrap(), &[0, 128, 128]);
    }
}