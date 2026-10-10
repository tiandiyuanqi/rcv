//! median 内核 ISA 基准：AVX2 vs AVX512 vs OpenCV（AVX2）。
//!
//! 用法: cargo run --release --example median_isa_bench
//!
//! OpenCV 5.0 的 medianBlur 派发模式只有 AVX2/BASELINE（无 AVX512），
//! 因此 OpenCV 的成绩即 AVX2 成绩，rcv 的 AVX2 内核必须对标它。

use std::time::Instant;

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
    let mpix = (pixels as f64 * iters as f64) / best / 1e6;
    println!("{name:<36} {mpix:>10.2} MPix/s   (best of 5x{iters})");
}

fn main() {
    // 单线程 rayon 池（与 examples/bench.rs 默认一致），full_api 才可与内核对比
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .unwrap();
    let (w, h) = (1024usize, 1024usize);
    let pixels = w * h;
    let iters = 200;
    let src: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 + 13) as u8).collect();
    let mut dst = vec![0u8; w * h * 3];
    println!("image: {w}x{h} RGB, threads: 1");

    bench("median_avx512_kernel", pixels, iters, || {
        // SAFETY: 本机 isa() 为 Avx512（Xeon Gold 6448Y），已验证
        unsafe {
            rcv::simd_kernels::x86::avx512::median_blur_u8_3x3(&src, &mut dst, w, h, 3);
        }
        std::hint::black_box(&dst);
    });

    bench("median_avx2_kernel", pixels, iters, || {
        // SAFETY: is_x86_feature_detected 运行时验证
        if std::arch::is_x86_feature_detected!("avx2") {
            unsafe {
                rcv::simd_kernels::x86::avx2::median_blur_u8_3x3(&src, &mut dst, w, h, 3);
            }
        }
        std::hint::black_box(&dst);
    });

    // 完整公开 API（当前派发 AVX512）
    use rcv::Image;
    let img: Image<u8, 3> = {
        let data: Vec<u8> = src.clone();
        Image::from_vec(w, h, data).expect("len")
    };
    bench("median_full_api(avx512)", pixels, iters, || {
        std::hint::black_box(rcv::median_blur(&img, 3));
    });
}
