//! 对拍示例：读输入图 -> 跑本库函数 -> dump 原始像素。
//!
//! 用法: cargo run --release --example compare -- <func> <input> <output.bin> [args...]
//!
//! 由 scripts/compare.sh 调用；PSNR 由 scripts/psnr.py 计算。

use std::io::Write;
use std::path::Path;

use rcv::{
    AdaptiveMethod, ApertureSize, BorderPolicy, ContourApproxMethod, ContourRetrievalMode,
    ThresholdType, adaptive_threshold, arc_length, bilateral_filter, bitwise_and, bitwise_not,
    bitwise_or, bitwise_xor, box_filter, canny, cart_to_polar, connected_components, contour_area,
    convex_hull, dilate, erode, find_contours, find_contours_with_hierarchy, gaussian_blur, imread, imread_gray,
    is_contour_convex, laplacian, magnitude, median_blur, min_area_rect, min_enclosing_circle,
    morph_close, morph_gradient, morph_open, phase, scharr, sobel, sobel5, threshold,
    to_grayscale, Connectivity,
};

fn dump(path: &Path, data: &[u8]) {
    let mut f = std::fs::File::create(path).expect("create output");
    f.write_all(data).expect("write output");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: compare <func> <input> <output.bin> [args...]");
        std::process::exit(2);
    }
    let func = &args[1];
    let input = &args[2];
    let output = &args[3];

    match func.as_str() {
        "to_grayscale" => {
            let img = imread(input).expect("imread");
            let gray = to_grayscale(&img);
            dump(Path::new(output), gray.data());
        }
        "threshold" => {
            // args: <thresh> <maxval> <type 0..4>
            let thresh: f64 = args[4].parse().expect("thresh");
            let maxval: f64 = args[5].parse().expect("maxval");
            let type_: ThresholdType = match args[6].as_str() {
                "0" => ThresholdType::Binary,
                "1" => ThresholdType::BinaryInv,
                "2" => ThresholdType::Trunc,
                "3" => ThresholdType::ToZero,
                "4" => ThresholdType::ToZeroInv,
                other => {
                    eprintln!("unknown threshold type: {other}");
                    std::process::exit(3);
                }
            };
            let img = imread_gray(input).expect("imread_gray");
            let (_, out) = threshold(&img, thresh, maxval, type_);
            dump(Path::new(output), out.data());
        }
        "box_filter" => {
            // args: <ksize> <normalize 0|1> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            let ksize: usize = args[4].parse().expect("ksize");
            let normalize: bool = args[5] == "1";
            let border = match args[6].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => BorderPolicy::<u8>::Constant(0),
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 7 && args[7] == "1";
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = box_filter(&img, (ksize, ksize), (-1, -1), normalize, border);
                dump(Path::new(output), out.data());
            } else {
                let img = imread(input).expect("imread");
                let out = box_filter(&img, (ksize, ksize), (-1, -1), normalize, border);
                dump(Path::new(output), out.data());
            }
        }
        "gaussian_blur" => {
            // args: <ksize> <sigma> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            let ksize: usize = args[4].parse().expect("ksize");
            let sigma: f64 = args[5].parse().expect("sigma");
            let border = match args[6].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => BorderPolicy::<u8>::Constant(0),
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 7 && args[7] == "1";
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = gaussian_blur(&img, (ksize, ksize), sigma, border);
                dump(Path::new(output), out.data());
            } else {
                let img = imread(input).expect("imread");
                let out = gaussian_blur(&img, (ksize, ksize), sigma, border);
                dump(Path::new(output), out.data());
            }
        }
        "adaptive_threshold" => {
            // args: <maxValue> <method 0=mean 1=gaussian> <type 0=binary 1=binary_inv> <blockSize> <delta>
            let max_value: f64 = args[4].parse().expect("maxValue");
            let method = if args[5] == "0" {
                AdaptiveMethod::MeanC
            } else {
                AdaptiveMethod::GaussianC
            };
            let type_ = if args[6] == "0" {
                ThresholdType::Binary
            } else {
                ThresholdType::BinaryInv
            };
            let block_size: usize = args[7].parse().expect("blockSize");
            let delta: f64 = args[8].parse().expect("delta");
            let img = imread_gray(input).expect("imread_gray");
            let out = adaptive_threshold(&img, max_value, method, type_, block_size, delta);
            dump(Path::new(output), out.data());
        }
        "median_blur" => {
            // args: <ksize> [gray 0|1]
            let ksize: usize = args[4].parse().expect("ksize");
            let gray = args.len() > 5 && args[5] == "1";
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = median_blur(&img, ksize);
                dump(Path::new(output), out.data());
            } else {
                let img = imread(input).expect("imread");
                let out = median_blur(&img, ksize);
                dump(Path::new(output), out.data());
            }
        }
        "bilateral_filter" => {
            // args: <d> <sigmaColor> <sigmaSpace> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            let d: i32 = args[4].parse().expect("d");
            let sigma_color: f64 = args[5].parse().expect("sigmaColor");
            let sigma_space: f64 = args[6].parse().expect("sigmaSpace");
            let border = match args[7].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => BorderPolicy::<u8>::Constant(0),
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 8 && args[8] == "1";
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = bilateral_filter(&img, d, sigma_color, sigma_space, border);
                dump(Path::new(output), out.data());
            } else {
                let img = imread(input).expect("imread");
                let out = bilateral_filter(&img, d, sigma_color, sigma_space, border);
                dump(Path::new(output), out.data());
            }
        }
        "erode" | "dilate" | "morph_open" | "morph_close" | "morph_gradient" => {
            // args: <ksize> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            let ksize: usize = args[4].parse().expect("ksize");
            // 常量边框值：erode 用 255（恒等），dilate 用 0（恒等），
            // 复合运算（open/close/gradient）两阶段 border 值不同，仅测 Clamp/Reflect。
            let border = match args[5].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => {
                    // 仅 erode/dilate 支持常量边框对拍（复合操作两阶段值不同）
                    assert!(
                        func == "erode" || func == "dilate",
                        "constant border only supported for erode/dilate in compare"
                    );
                    if func == "erode" {
                        BorderPolicy::<u8>::Constant(255)
                    } else {
                        BorderPolicy::<u8>::Constant(0)
                    }
                }
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 6 && args[6] == "1";
            let kernel = vec![1u8; ksize * ksize];
            fn run_morph<const C: usize>(
                func: &str,
                img: &rcv::Image<u8, C>,
                kernel: &[u8],
                ksize: usize,
                border: BorderPolicy<u8>,
            ) -> rcv::Image<u8, C> {
                match func {
                    "erode" => erode(img, kernel, (ksize, ksize), (-1, -1), 1, border),
                    "dilate" => dilate(img, kernel, (ksize, ksize), (-1, -1), 1, border),
                    "morph_open" => morph_open(img, kernel, (ksize, ksize), (-1, -1), 1, border),
                    "morph_close" => morph_close(img, kernel, (ksize, ksize), (-1, -1), 1, border),
                    "morph_gradient" => {
                        morph_gradient(img, kernel, (ksize, ksize), (-1, -1), 1, border)
                    }
                    _ => unreachable!(),
                }
            }
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = run_morph(func.as_str(), &img, &kernel, ksize, border);
                dump(Path::new(output), out.data());
            } else {
                let img = imread(input).expect("imread");
                let out = run_morph(func.as_str(), &img, &kernel, ksize, border);
                dump(Path::new(output), out.data());
            }
        }
        "sobel" | "scharr" | "laplacian" => {
            // args: <dx> <ksize> <scale> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            // sobel/scharr: dx=导数方向（1=x, 0=y）；laplacian: dx 字段忽略（ksize 1|3）
            let dx: u32 = args[4].parse().expect("dx");
            let ksize: usize = args[5].parse().expect("ksize");
            let scale: f64 = args[6].parse().expect("scale");
            let border = match args[7].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => BorderPolicy::<u8>::Constant(0),
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 8 && args[8] == "1";
            fn run_deriv<const C: usize>(
                func: &str,
                img: &rcv::Image<u8, C>,
                dx: u32,
                ksize: usize,
                scale: f64,
                border: BorderPolicy<u8>,
            ) -> rcv::Image<i16, C> {
                match func {
                    "sobel" => {
                        if ksize == 5 {
                            sobel5(img, dx, 1 - dx, scale, border)
                        } else {
                            sobel(img, dx, 1 - dx, scale, border)
                        }
                    }
                    "scharr" => scharr(img, dx, 1 - dx, scale, border),
                    "laplacian" => laplacian(img, ksize, scale, border),
                    _ => unreachable!(),
                }
            }
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = run_deriv(func.as_str(), &img, dx, ksize, scale, border);
                dump(Path::new(output), bytemuck::cast_slice(out.data()));
            } else {
                let img = imread(input).expect("imread");
                let out = run_deriv(func.as_str(), &img, dx, ksize, scale, border);
                dump(Path::new(output), bytemuck::cast_slice(out.data()));
            }
        }
        "magnitude" | "phase" | "cart_to_polar" => {
            // args: <dx> <ksize> <scale> <border 0=clamp 1=reflect 2=constant> [gray 0|1]
            // 输入由 sobel 16S 结果转 f32（对齐 OpenCV 常见用法）
            let dx: u32 = args[4].parse().expect("dx");
            let ksize: usize = args[5].parse().expect("ksize");
            let scale: f64 = args[6].parse().expect("scale");
            let border = match args[7].as_str() {
                "0" => BorderPolicy::<u8>::Clamp,
                "1" => BorderPolicy::<u8>::Reflect,
                "2" => BorderPolicy::<u8>::Constant(0),
                other => {
                    eprintln!("unknown border: {other}");
                    std::process::exit(3);
                }
            };
            let gray = args.len() > 8 && args[8] == "1";
            fn run_float<const C: usize>(
                func: &str,
                img: &rcv::Image<u8, C>,
                dx: u32,
                _ksize: usize,
                scale: f64,
                border: BorderPolicy<u8>,
            ) -> rcv::Image<f32, C> {
                let gx = sobel(img, dx, 1 - dx, scale, border);
                let gy = sobel(img, 1 - dx, dx, scale, border);
                let to_f32 = |g: &rcv::Image<i16, C>| -> rcv::Image<f32, C> {
                    rcv::Image::from_fn(img.width(), img.height(), |x, y| {
                        let mut px = [0f32; C];
                        for (c, v) in px.iter_mut().enumerate() {
                            *v = g.data()[(y * img.width() + x) * C + c] as f32;
                        }
                        px
                    })
                };
                let gxf = to_f32(&gx);
                let gyf = to_f32(&gy);
                match func {
                    "magnitude" => magnitude(&gxf, &gyf),
                    "phase" => phase(&gxf, &gyf, true),
                    "cart_to_polar" => cart_to_polar(&gxf, &gyf, true).1,
                    _ => unreachable!(),
                }
            }
            if gray {
                let img = imread_gray(input).expect("imread_gray");
                let out = run_float(func.as_str(), &img, dx, ksize, scale, border);
                dump(Path::new(output), bytemuck::cast_slice(out.data()));
            } else {
                let img = imread(input).expect("imread");
                let out = run_float(func.as_str(), &img, dx, ksize, scale, border);
                dump(Path::new(output), bytemuck::cast_slice(out.data()));
            }
        }
        "canny" => {
            // args: <low> <high> <aperture 3|5|-1> <l2 0|1>
            let low: f64 = args[4].parse().expect("low");
            let high: f64 = args[5].parse().expect("high");
            let aperture = match args[6].as_str() {
                "3" => ApertureSize::Sobel3,
                "5" => ApertureSize::Sobel5,
                "-1" => ApertureSize::Scharr,
                other => {
                    eprintln!("unknown aperture: {other}");
                    std::process::exit(3);
                }
            };
            let l2: bool = args[7] == "1";
            let img = imread_gray(input).expect("imread_gray");
            let out = canny(&img, low, high, aperture, l2);
            dump(Path::new(output), out.data());
        }
        "find_contours" => {
            // args: <mode 0-3> <method 1-4> <off_x> <off_y> <with_hier 0|1>
            // 输出格式（i32 LE 流）：[n][len_0][x y ...][len_1]...[hier_n][4i*n]
            let mode = match args[4].as_str() {
                "0" => ContourRetrievalMode::External,
                "1" => ContourRetrievalMode::List,
                "2" => ContourRetrievalMode::CComp,
                "3" => ContourRetrievalMode::Tree,
                other => {
                    eprintln!("unknown mode: {other}");
                    std::process::exit(3);
                }
            };
            let method = match args[5].as_str() {
                "1" => ContourApproxMethod::None,
                "2" => ContourApproxMethod::Simple,
                "3" => ContourApproxMethod::Tc89L1,
                "4" => ContourApproxMethod::Tc89Kcos,
                other => {
                    eprintln!("unknown method: {other}");
                    std::process::exit(3);
                }
            };
            let off_x: i32 = args[6].parse().expect("off_x");
            let off_y: i32 = args[7].parse().expect("off_y");
            let with_hier: bool = args[8] == "1";
            let img = imread_gray(input).expect("imread_gray");
            // 二值化（对齐 ref：threshold 127/255）
            let (_, bin) = threshold(&img, 127.0, 255.0, ThresholdType::Binary);
            let (contours, hierarchy) = if with_hier {
                // 带 hierarchy：全部模式走 Suzuki（对齐 OpenCV 带 hierarchy
                // 输出时不走 TRU fast path）
                rcv::find_contours_with_hierarchy(
                    &bin, mode, method, rcv::Point::new(off_x, off_y),
                )
            } else {
                find_contours(&bin, mode, method, rcv::Point::new(off_x, off_y))
            };
            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(&(contours.len() as i32).to_le_bytes());
            for c in &contours {
                buf.extend_from_slice(&(c.len() as i32).to_le_bytes());
                for p in c {
                    buf.extend_from_slice(&p.x.to_le_bytes());
                    buf.extend_from_slice(&p.y.to_le_bytes());
                }
            }
            if with_hier {
                buf.extend_from_slice(&(hierarchy.len() as i32).to_le_bytes());
                for h in &hierarchy {
                    buf.extend_from_slice(&h.next.to_le_bytes());
                    buf.extend_from_slice(&h.prev.to_le_bytes());
                    buf.extend_from_slice(&h.first_child.to_le_bytes());
                    buf.extend_from_slice(&h.parent.to_le_bytes());
                }
            }
            dump(Path::new(output), &buf);
        }
        "geometry" => {
            // args: <op>
            let op = args[4].clone();
            let img = imread_gray(input).expect("imread_gray");
            let (_, bin) = threshold(&img, 127.0, 255.0, ThresholdType::Binary);
            let (contours, _) = find_contours_with_hierarchy(
                &bin,
                ContourRetrievalMode::List,
                ContourApproxMethod::None,
                rcv::Point::new(0, 0),
            );
            let mut buf = Vec::new();
            buf.extend_from_slice(&(contours.len() as i32).to_le_bytes());
            match op.as_str() {
                "arc_length" => {
                    for c in &contours {
                        buf.extend_from_slice(&arc_length(c, true).to_le_bytes());
                    }
                }
                "convex_hull" => {
                    for c in &contours {
                        let hull = convex_hull(c, false);
                        buf.extend_from_slice(&(hull.len() as i32).to_le_bytes());
                        for h in hull {
                            buf.extend_from_slice(&(h as i32).to_le_bytes());
                        }
                    }
                }
                "is_convex" => {
                    for c in &contours {
                        buf.extend_from_slice(&(is_contour_convex(c) as i32).to_le_bytes());
                    }
                }
                "min_area_rect" => {
                    for c in &contours {
                        let r = min_area_rect(c).unwrap();
                        buf.extend_from_slice(&(r.center.0 as f64).to_le_bytes());
                        buf.extend_from_slice(&(r.center.1 as f64).to_le_bytes());
                        buf.extend_from_slice(&(r.size.0 as f64).to_le_bytes());
                        buf.extend_from_slice(&(r.size.1 as f64).to_le_bytes());
                        buf.extend_from_slice(&(r.angle as f64).to_le_bytes());
                    }
                }
                "min_enclosing_circle" => {
                    for c in &contours {
                        let (center, radius) = min_enclosing_circle(c);
                        buf.extend_from_slice(&(center.0 as f64).to_le_bytes());
                        buf.extend_from_slice(&(center.1 as f64).to_le_bytes());
                        buf.extend_from_slice(&(radius as f64).to_le_bytes());
                    }
                }
                "contour_area" => {
                    for c in &contours {
                        buf.extend_from_slice(&contour_area(c, false).to_le_bytes());
                    }
                }
                other => {
                    eprintln!("unknown op: {other}");
                    std::process::exit(3);
                }
            }
            let mut f = std::fs::File::create(output).expect("create output");
            f.write_all(&buf).expect("write output");
        }
        "cc" => {
            // args: <connectivity 4|8>
            let conn = match args[4].as_str() {
                "4" => Connectivity::Connected4,
                _ => Connectivity::Connected8,
            };
            let img = imread_gray(input).expect("imread_gray");
            let (_, bin) = threshold(&img, 127.0, 255.0, ThresholdType::Binary);
            let cc = connected_components(&bin, conn);
            let mut buf = Vec::new();
            let total = cc.labels.data().len();
            buf.extend_from_slice(&(total as i32).to_le_bytes());
            for l in cc.labels.data() {
                buf.extend_from_slice(&l.to_le_bytes());
            }
            buf.extend_from_slice(&(cc.nlabels as i32).to_le_bytes());
            let mut f = std::fs::File::create(output).expect("create output");
            f.write_all(&buf).expect("write output");
        }
        "bitwise" => {
            // args: <op and|or|xor|not>
            let op = args[4].clone();
            let img = imread_gray(input).expect("imread_gray");
            let (w, h) = (img.width(), img.height());
            // 第二操作数：水平翻转（对齐 C++ 侧）
            let other = rcv::Image::from_fn(w, h, |x, y| [img.data()[y * w + (w - 1 - x)]]);
            let out = match op.as_str() {
                "and" => bitwise_and(&img, &other),
                "or" => bitwise_or(&img, &other),
                "xor" => bitwise_xor(&img, &other),
                "not" => bitwise_not(&img),
                other => {
                    eprintln!("unknown op: {other}");
                    std::process::exit(3);
                }
            };
            dump(Path::new(output), out.data());
        }
        "draw" => {
            // args: <shape> <a> <b> <c> <d> <thickness> <line_type> <shift>
            let shape = args[4].clone();
            let a: i32 = args[5].parse().expect("a");
            let b: i32 = args[6].parse().expect("b");
            let c: i32 = args[7].parse().expect("c");
            let d: i32 = args[8].parse().expect("d");
            let thickness: i32 = args[9].parse().expect("thickness");
            let line_type = match args[10].as_str() {
                "4" => rcv::LineType::Connected4,
                "16" => rcv::LineType::AntiAliased,
                _ => rcv::LineType::Connected8,
            };
            let shift: i64 = args[11].parse().expect("shift");
            let img0 = imread_gray(input).expect("imread_gray");
            let (w, h) = (img0.width() as i32, img0.height() as i32);
            let mut img = img0.clone();
            let color = [200u8];
            let p1 = rcv::Point::new(a, b);
            let p2 = rcv::Point::new(c, d);
            match shape.as_str() {
                "line" => rcv::line(&mut img, p1, p2, color, thickness, line_type, shift),
                "rectangle" => rcv::rectangle(&mut img, p1, p2, color, thickness, line_type, shift),
                "circle" => rcv::circle(&mut img, p1, a, color, thickness, line_type, shift),
                "ellipse" => rcv::ellipse(
                    &mut img,
                    rcv::Point::new(w / 2, h / 2),
                    (a, b),
                    c as f64,
                    d as f64,
                    (d + 90) as f64,
                    color,
                    thickness,
                    line_type,
                    shift,
                ),
                "fill_convex_poly" => {
                    let pts = [
                        rcv::Point::new(a, b),
                        rcv::Point::new(c, d),
                        rcv::Point::new(w - a, h - b),
                        rcv::Point::new(w - c, d),
                        rcv::Point::new(a, h - d),
                    ];
                    rcv::fill_convex_poly(&mut img, &pts, color, line_type, shift);
                }
                "fill_poly" => {
                    let c1 = vec![
                        rcv::Point::new(a, b),
                        rcv::Point::new(c, d),
                        rcv::Point::new(w - a, h - b),
                    ];
                    let c2 = vec![
                        rcv::Point::new(w - c, d),
                        rcv::Point::new(w - a, b),
                        rcv::Point::new(c, h - d),
                    ];
                    rcv::fill_poly(&mut img, &[&c1, &c2], color, line_type, shift, rcv::Point::new(0, 0));
                }
                "polylines" => {
                    let c1 = vec![
                        rcv::Point::new(a, b),
                        rcv::Point::new(c, d),
                        rcv::Point::new(w - a, h - b),
                    ];
                    let c2 = vec![
                        rcv::Point::new(w - c, d),
                        rcv::Point::new(w - a, b),
                        rcv::Point::new(c, h - d),
                    ];
                    rcv::polylines(&mut img, &[&c1, &c2], true, color, thickness, line_type, shift);
                }
                "draw_contours" => {
                    let (_, bin) = threshold(&img0, 127.0, 255.0, ThresholdType::Binary);
                    let (contours, hierarchy) = rcv::find_contours_with_hierarchy(
                        &bin,
                        ContourRetrievalMode::CComp,
                        ContourApproxMethod::None,
                        rcv::Point::new(0, 0),
                    );
                    let hier: Option<&[rcv::Hierarchy]> =
                        if b < 0 { None } else { Some(&hierarchy) };
                    rcv::draw_contours(
                        &mut img,
                        &contours,
                        hier,
                        a,
                        color,
                        thickness,
                        line_type,
                        b.max(0),
                        rcv::Point::new(0, 0),
                    );
                }
                other => {
                    eprintln!("unknown shape: {other}");
                    std::process::exit(3);
                }
            }
            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(&(img.data().len() as i32).to_le_bytes());
            buf.extend_from_slice(img.data());
            dump(Path::new(output), &buf);
        }
        _ => {
            eprintln!("unknown func: {func}");
            std::process::exit(3);
        }
    }
}
