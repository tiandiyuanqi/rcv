//! Canny 分阶段 profiling：定位 ST/MT 各阶段耗时。
//!
//! 用法:
//!   cargo run --release --example canny_profile [-- --threads]

use std::time::Instant;

use rcv::{ApertureSize, canny, imread, to_grayscale};

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
    println!("{name:<32} {us:>10.2} us/iter   {mpix:>10.2} MPix/s");
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

    // 预热：rayon 池 + 堆（bench.rs 里 canny 前有 20 个 bench，这里补齐）
    for _ in 0..50 {
        std::hint::black_box(canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false));
    }

    // 全函数基线（与 bench.rs 完全一致的写法：let _ =）
    bench("canny_full (let _)", pixels, iters, || {
        let _ = canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false);
    });

    // Sobel dx+dy（全图，当前实现）
    bench("sobel_dx+dy (full-image)", pixels, iters, || {
        let border = rcv::BorderPolicy::Clamp;
        std::hint::black_box(rcv::sobel(&gray, 1, 0, 1.0, border));
        std::hint::black_box(rcv::sobel(&gray, 0, 1, 1.0, border));
    });

    // 预计算 dx/dy，供后续阶段复用
    let border = rcv::BorderPolicy::Clamp;
    let dx_img = rcv::sobel(&gray, 1, 0, 1.0, border);
    let dy_img = rcv::sobel(&gray, 0, 1, 1.0, border);
    let dx = dx_img.data();
    let dy = dy_img.data();

    // mag 全图（3 行 ring 逐行）
    bench("mag rows (3-ring, all)", pixels, iters, || {
        let mag_w = w + 2;
        let mut mag: Vec<i32> = vec![0; 3 * mag_w];
        for y in 0..h {
            let slot = (y + 2) % 3;
            let row = &mut mag[slot * mag_w + 1..slot * mag_w + 1 + w];
            rcv::simd_kernels::canny_mag_row(&dx[y * w..y * w + w], &dy[y * w..y * w + w], row, w, false);
        }
        std::hint::black_box(mag);
    });

    // NMS 全图（含 mag 填充 + 偏移转换，即 canny 的 NMS 阶段完整复现）
    let mapstep = (w + 32 + 1 + 31) & !31;
    let (map_after_nms, stack_after_nms) = {
        let mag_w = w + 2;
        let mut map: Vec<u8> = rcv::simd_kernels::alloc_uninit((h + 2) * mapstep);
        map[..mapstep].fill(1);
        map[(h + 1) * mapstep..].fill(1);
        let mut stack: Vec<usize> = Vec::new();
        let mut mag: Vec<i32> = vec![0; 3 * mag_w];
        for y in 0..h {
            let slot = (y + 2) % 3;
            {
                let row = &mut mag[slot * mag_w..slot * mag_w + mag_w];
                row[0] = 0;
                row[w + 1] = 0;
                rcv::simd_kernels::canny_mag_row(&dx[y * w..y * w + w], &dy[y * w..y * w + w], &mut row[1..1 + w], w, false);
            }
            let sp = y % 3;
            let sa = (y + 1) % 3;
            let sn = (y + 2) % 3;
            let p_row = &mag[sp * mag_w..sp * mag_w + mag_w];
            let a_row = &mag[sa * mag_w..sa * mag_w + mag_w];
            let n_row = &mag[sn * mag_w..sn * mag_w + mag_w];
            let pmap = &mut map[(y + 1) * mapstep + 32..(y + 1) * mapstep + 32 + w + 2];
            let len0 = stack.len();
            rcv::simd_kernels::canny_nms_row(p_row, a_row, n_row, &dx[y * w..y * w + w], &dy[y * w..y * w + w], pmap, w, 50, 100, &mut stack);
            let base = (y + 1) * mapstep + 32;
            for off in &mut stack[len0..] {
                *off += base;
            }
        }
        (map, stack)
    };
    bench("nms rows (all, incl mag)", pixels, iters, || {
        let mag_w = w + 2;
        let mut map: Vec<u8> = rcv::simd_kernels::alloc_uninit((h + 2) * mapstep);
        let mut stack: Vec<usize> = Vec::new();
        let mut mag: Vec<i32> = vec![0; 3 * mag_w];
        for y in 0..h {
            let slot = (y + 2) % 3;
            {
                let row = &mut mag[slot * mag_w..slot * mag_w + mag_w];
                row[0] = 0;
                row[w + 1] = 0;
                rcv::simd_kernels::canny_mag_row(&dx[y * w..y * w + w], &dy[y * w..y * w + w], &mut row[1..1 + w], w, false);
            }
            let sp = y % 3;
            let sa = (y + 1) % 3;
            let sn = (y + 2) % 3;
            let p_row = &mag[sp * mag_w..sp * mag_w + mag_w];
            let a_row = &mag[sa * mag_w..sa * mag_w + mag_w];
            let n_row = &mag[sn * mag_w..sn * mag_w + mag_w];
            let pmap = &mut map[(y + 1) * mapstep + 32..(y + 1) * mapstep + 32 + w + 2];
            let len0 = stack.len();
            rcv::simd_kernels::canny_nms_row(p_row, a_row, n_row, &dx[y * w..y * w + w], &dy[y * w..y * w + w], pmap, w, 50, 100, &mut stack);
            let base = (y + 1) * mapstep + 32;
            for off in &mut stack[len0..] {
                *off += base;
            }
        }
        std::hint::black_box((&map, &stack));
    });

    // 洪泛（从 NMS 后的栈开始）
    bench("hysteresis (from stack)", pixels, iters, || {
        let mut m2 = map_after_nms.clone();
        let mut st = stack_after_nms.clone();
        rcv::simd_kernels::canny_hysteresis(&mut m2, mapstep, &mut st);
        std::hint::black_box(&m2);
    });

    // finalPass 全图
    bench("final_pass (all rows)", pixels, iters, || {
        let mut out: Vec<u8> = rcv::simd_kernels::alloc_uninit(w * h);
        for y in 0..h {
            let pmap = &map_after_nms[(y + 1) * mapstep + 32..];
            rcv::simd_kernels::canny_final_pass_row(pmap, &mut out[y * w..y * w + w], w);
        }
        std::hint::black_box(out);
    });
}
