//! Canny MT 分阶段 profiling：块内计时（Sobel / NMS / 局部洪泛）+ 全局段。
//!
//! 用法:
//!   cargo run --release --example canny_mt_profile [-- --threads]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rcv::{ApertureSize, BorderPolicy, canny, imread, to_grayscale};

fn bench<F: FnMut()>(name: &str, pixels: usize, iters: usize, mut f: F) {
    f(); // warmup
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    let us = best / iters as f64 * 1e6;
    let mpix = pixels as f64 / best / iters as f64 / 1e6;
    println!("{name:<36} {us:>10.2} us/iter   {mpix:>10.2} MPix/s");
}

#[derive(Default)]
struct PhaseTimes {
    sobel: AtomicU64,
    nms: AtomicU64,
    local_hyst: AtomicU64,
    global_hyst: AtomicU64,
    final_pass: AtomicU64,
    total: AtomicU64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let multi = args.iter().any(|a| a == "--threads");
    let nthreads = if multi {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        1
    };
    rayon::ThreadPoolBuilder::new().num_threads(nthreads).build_global().unwrap();

    let img = imread("compare/inputs/bench.png").expect("imread");
    let gray = to_grayscale(&img);
    let (w, h) = (gray.width(), gray.height());
    let pixels = w * h;
    let iters = 100;
    println!("image: {w}x{h}, threads: {nthreads}");

    for _ in 0..50 {
        std::hint::black_box(canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false));
    }

    bench("canny_full (let _)", pixels, iters, || {
        let _ = canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false);
    });

    // 全图 Sobel（当前实现：canny 内部先全图 Sobel）
    bench("sobel_dx+dy (full-image)", pixels, iters, || {
        let border = BorderPolicy::Clamp;
        std::hint::black_box(rcv::sobel(&gray, 1, 0, 1.0, border));
        std::hint::black_box(rcv::sobel(&gray, 0, 1, 1.0, border));
    });

    // 块内 Sobel（OpenCV 结构：每块只算自己行 + 2 行 halo）
    // 复刻 canny 的块划分
    let threads = rayon::current_num_threads().max(1);
    let min_grain = 4usize;
    let chunk = (h / threads).max(min_grain);
    let nblocks = h.div_ceil(chunk);
    bench("sobel_dx+dy (per-block)", pixels, iters, || {
        use rayon::prelude::*;
        (0..nblocks).into_par_iter().for_each(|bi| {
            let y0 = bi * chunk;
            let y1 = (y0 + chunk).min(h);
            let rs = y0.saturating_sub(1);
            let re = (y1 + 1).min(h);
            let sub = gray
                .roi(rcv::Rect::new(0, rs as i32, w as i32, (re - rs) as i32))
                .expect("roi")
                .to_image();
            let border = BorderPolicy::Clamp;
            std::hint::black_box(rcv::sobel(&sub, 1, 0, 1.0, border));
            std::hint::black_box(rcv::sobel(&sub, 0, 1, 1.0, border));
        });
    });
}
