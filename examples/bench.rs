//! 轻量吞吐基准（Instant 计时）。
//!
//! 用法:
//!   cargo run --release --example bench            # 单线程
//!   cargo run --release --example bench -- --multi # 多线程（rayon）
//!
//! 输出各函数 MPix/s。与 OpenCV5 同机同图对比（见 compare/bench_opencv5.cpp）。

use std::time::Instant;

use rcv::{
    AdaptiveMethod, ApertureSize, BorderPolicy, Connectivity, ContourApproxMethod,
    ContourRetrievalMode, LineType, ThresholdType, adaptive_threshold, arc_length,
    bilateral_filter, bitwise_and, bitwise_not, bitwise_or, bitwise_xor, box_filter, canny,
    cart_to_polar, circle, connected_components, convex_hull, dilate, draw_contours, ellipse,
    erode, fill_poly, find_contours, find_contours_with_hierarchy, gaussian_blur, imread,
    laplacian, line, magnitude, median_blur, min_area_rect, min_enclosing_circle, morph_close,
    morph_gradient, morph_open, phase, polylines, rectangle, scharr, sobel, threshold,
    to_grayscale,
};

fn bench<F: FnMut()>(name: &str, pixels: usize, iters: usize, mut f: F) {
    f(); // warmup
    // best-of-5：共享机器负载噪声大（内存带宽型用例波动可达 3x），
    // 取多组最小值逼近真实内核能力
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    let mpix = (pixels as f64 * iters as f64) / best / 1e6;
    println!("{name:<28} {mpix:>10.2} MPix/s   (best of 5x{iters})");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let multi = args.iter().any(|a| a == "--threads");
    rayon::ThreadPoolBuilder::new()
        .num_threads(if multi {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            1
        })
        .build_global()
        .unwrap();

    let img = imread("compare/inputs/bench.png").expect("imread");
    let gray = to_grayscale(&img);
    let (w, h) = (img.width(), img.height());
    let pixels = w * h;
    let iters = 200;

    println!(
        "image: {w}x{h} = {pixels} px, threads: {}",
        if multi { num_cpus() } else { 1 }
    );

    // 内存带宽型用例耗时极短（<10ms/200iters），加大 iters 保证计时精度
    let mem_iters = iters * 10;
    bench("to_grayscale", pixels, mem_iters, || {
        std::hint::black_box(to_grayscale(&img));
    });
    bench("threshold_binary", pixels, mem_iters, || {
        let _ = threshold(&gray, 100.0, 255.0, ThresholdType::Binary);
    });
    bench("adaptive_mean_3", pixels, iters, || {
        let _ = adaptive_threshold(
            &gray,
            255.0,
            AdaptiveMethod::MeanC,
            ThresholdType::Binary,
            3,
            2.0,
        );
    });
    bench("box_filter_3x3", pixels, iters, || {
        let _ = box_filter(&img, (3, 3), (-1, -1), true, BorderPolicy::Clamp);
    });
    bench("gaussian_blur_3x3", pixels, iters, || {
        let _ = gaussian_blur(&img, (3, 3), 0.0, BorderPolicy::Clamp);
    });
    bench("median_blur_3x3", pixels, iters, || {
        let _ = median_blur(&img, 3);
    });
    bench("bilateral_d9", pixels, 20, || {
        let _ = bilateral_filter(&img, 9, 75.0, 75.0, BorderPolicy::Reflect);
    });
    let kernel3 = vec![1u8; 9];
    bench("erode_3x3", pixels, iters, || {
        let _ = erode(&gray, &kernel3, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    });
    bench("dilate_3x3", pixels, iters, || {
        let _ = dilate(&gray, &kernel3, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    });
    bench("morph_open_3x3", pixels, iters, || {
        let _ = morph_open(&gray, &kernel3, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    });
    bench("morph_close_3x3", pixels, iters, || {
        let _ = morph_close(&gray, &kernel3, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    });
    bench("morph_gradient_3x3", pixels, iters, || {
        let _ = morph_gradient(&gray, &kernel3, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    });
    bench("sobel_dx_3x3", pixels, iters, || {
        let _ = sobel(&gray, 1, 0, 1.0, BorderPolicy::Clamp);
    });
    bench("scharr_dx_3x3", pixels, iters, || {
        let _ = scharr(&gray, 1, 0, 1.0, BorderPolicy::Clamp);
    });
    bench("laplacian_3x3", pixels, iters, || {
        let _ = laplacian(&gray, 3, 1.0, BorderPolicy::Clamp);
    });
    // 浮点梯度：输入由 sobel 16S 转 f32（对齐 OpenCV 常见用法）
    let gx = sobel(&gray, 1, 0, 1.0, BorderPolicy::Clamp);
    let gy = sobel(&gray, 0, 1, 1.0, BorderPolicy::Clamp);
    let gxf = ImageF32::from_fn(w, h, |x, y| [gx.data()[y * w + x] as f32]);
    let gyf = ImageF32::from_fn(w, h, |x, y| [gy.data()[y * w + x] as f32]);
    bench("magnitude", pixels, iters, || {
        let _ = magnitude(&gxf, &gyf);
    });
    bench("phase", pixels, iters, || {
        let _ = phase(&gxf, &gyf, true);
    });
    bench("cart_to_polar", pixels, iters, || {
        let _ = cart_to_polar(&gxf, &gyf, true);
    });
    bench("canny_50_100", pixels, iters, || {
        let _ = canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false);
    });
    // find_contours：二值化输入（对齐 OpenCV bench 语义）
    let (_, bin) = threshold(&gray, 127.0, 255.0, ThresholdType::Binary);
    bench("find_contours_list", pixels, iters, || {
        let _ = find_contours(
            &bin,
            ContourRetrievalMode::List,
            ContourApproxMethod::None,
            rcv::Point::new(0, 0),
        );
    });
    bench("find_contours_tree", pixels, iters, || {
        let _ = find_contours_with_hierarchy(
            &bin,
            ContourRetrievalMode::Tree,
            ContourApproxMethod::None,
            rcv::Point::new(0, 0),
        );
    });

    // 按位逻辑（第二操作数 = 水平翻转，对齐 OpenCV bench）
    let flipped = rcv::Image::from_fn(w, h, |x, y| [gray.data()[y * w + (w - 1 - x)]]);
    bench("bitwise_and", pixels, mem_iters, || {
        std::hint::black_box(bitwise_and(&gray, &flipped));
    });
    bench("bitwise_or", pixels, mem_iters, || {
        std::hint::black_box(bitwise_or(&gray, &flipped));
    });
    bench("bitwise_xor", pixels, mem_iters, || {
        std::hint::black_box(bitwise_xor(&gray, &flipped));
    });
    bench("bitwise_not", pixels, mem_iters, || {
        std::hint::black_box(bitwise_not(&gray));
    });

    // 绘制家族（对齐 OpenCV bench 语义：灰度 8U 画布，LINE_8）
    let mut canvas = gray.clone();
    let lt8 = LineType::Connected8;
    bench("draw_line", pixels, iters, || {
        line(
            &mut canvas,
            rcv::Point::new(30, 40),
            rcv::Point::new(200, 250),
            [255],
            3,
            lt8,
            0,
        );
    });
    bench("draw_rectangle", pixels, iters, || {
        rectangle(
            &mut canvas,
            rcv::Point::new(30, 40),
            rcv::Point::new(200, 250),
            [255],
            3,
            lt8,
            0,
        );
    });
    bench("draw_circle", pixels, iters, || {
        circle(
            &mut canvas,
            rcv::Point::new(100, 120),
            100,
            [255],
            3,
            lt8,
            0,
        );
    });
    bench("draw_ellipse", pixels, iters, || {
        ellipse(
            &mut canvas,
            rcv::Point::new(100, 120),
            (80, 50),
            30.0,
            10.0,
            100.0,
            [255],
            3,
            lt8,
            0,
        );
    });
    let poly = vec![
        rcv::Point::new(30, 40),
        rcv::Point::new(200, 250),
        rcv::Point::new(60, 45),
        rcv::Point::new(10, 15),
    ];
    bench("draw_polylines", pixels, iters, || {
        polylines(&mut canvas, &[&poly], true, [255], 3, lt8, 0);
    });
    bench("draw_fill_poly", pixels, iters, || {
        fill_poly(&mut canvas, &[&poly], [255], lt8, 0, rcv::Point::new(0, 0));
    });
    let (contours, _) = find_contours_with_hierarchy(
        &bin,
        ContourRetrievalMode::List,
        ContourApproxMethod::None,
        rcv::Point::new(0, 0),
    );
    // 轮廓几何分析（bench.png 的 692 轮廓）
    bench("arc_length", pixels, iters, || {
        for c in &contours {
            std::hint::black_box(arc_length(c, true));
        }
    });
    bench("convex_hull", pixels, iters, || {
        for c in &contours {
            std::hint::black_box(convex_hull(c, false));
        }
    });
    bench("min_area_rect", pixels, iters, || {
        for c in &contours {
            std::hint::black_box(min_area_rect(c));
        }
    });
    bench("min_enclosing_circle", pixels, iters, || {
        for c in &contours {
            std::hint::black_box(min_enclosing_circle(c));
        }
    });
    bench("connected_components", pixels, iters, || {
        std::hint::black_box(connected_components(&bin, Connectivity::Connected8));
    });

    bench("draw_contours", pixels, iters, || {
        draw_contours(
            &mut canvas,
            &contours,
            None,
            -1,
            [255],
            3,
            lt8,
            1,
            rcv::Point::new(0, 0),
        );
    });
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

type ImageF32 = rcv::Image<f32, 1>;
