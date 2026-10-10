#![warn(unsafe_code)]
//! rcv：Rust 工业视觉核心库。
//!
//! 公开 API 约定（见 DESIGN.md）：
//! - 彩色图像通道顺序一律 **RGB**（铁律 6），库内不做 BGR 兼容
//! - 强类型图像容器 [`Image<T, C>`]，ROI 为零拷贝视图 [`ImageView`]（铁律 1、2）
//! - 所有算法返回 [`Result`]，错误携带维度/类型/范围信息（铁律 5）
//!
//! 内部模块路径（`core::…`）不对外承诺稳定，请通过 crate 根的重导出使用。

mod core;
mod imgcodecs;
mod imgproc;
pub mod simd_kernels;

pub use crate::core::border::BorderPolicy;
pub use crate::core::error::{Error, Result};
pub use crate::core::image::{Dims, Image, ImageView, ImageViewMut, Pixel};
pub use crate::core::point::Point;
pub use crate::core::rect::{Rect, Size};
pub use crate::imgcodecs::{
    ImwriteParam, imread, imread_gray, imwrite, imwrite_u16, imwrite_with_params,
};
pub use crate::imgproc::bitwise::{bitwise_and, bitwise_not, bitwise_or, bitwise_xor};
pub use crate::imgproc::color::{to_grayscale, to_rgb};
pub use crate::imgproc::connected_components::{
    CCStat, Connectivity, ConnectedComponents, connected_components, connected_components_with_stats,
};
pub use crate::imgproc::contours::{
    ContourApproxMethod, ContourRetrievalMode, Hierarchy, approx_poly_dp, bounding_rect,
    contour_area, find_contours, find_contours_with_hierarchy,
};
pub use crate::imgproc::drawing::{
    LineType, circle, circle_rgb, draw_contours, draw_contours_rgb, ellipse, ellipse_rgb,
    fill_convex_poly, fill_convex_poly_rgb, fill_poly, fill_poly_rgb, line, line_rgb,
    polylines, polylines_rgb, rectangle, rectangle_rgb,
};
pub use crate::imgproc::edges::{ApertureSize, canny};
pub use crate::imgproc::filter::{bilateral_filter, box_filter, gaussian_blur, median_blur};
pub use crate::imgproc::geometry::{
    RotatedRect, arc_length, box_points, convex_hull, is_contour_convex, min_area_rect,
    min_enclosing_circle,
};
pub use crate::imgproc::gradient::{cart_to_polar, laplacian, magnitude, phase, scharr, sobel, sobel5};
pub use crate::imgproc::morphology::{dilate, erode, morph_close, morph_gradient, morph_open};
pub use crate::imgproc::threshold::{AdaptiveMethod, ThresholdType, adaptive_threshold, threshold};

