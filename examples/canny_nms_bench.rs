//! Canny NMS 微基准：定位 NMS 阶段内部瓶颈（候选密度、栈开销、批预筛）。
//!
//! 用法:
//!   cargo run --release --example canny_nms_bench

use std::time::Instant;

use rcv::{ApertureSize, BorderPolicy, canny, imread, sobel, to_grayscale};

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
    println!("{name:<44} {us:>10.2} us/iter   {mpix:>10.2} MPix/s");
}

fn main() {
    let img = imread("compare/inputs/bench.png").expect("imread");
    let gray = to_grayscale(&img);
    let (w, h) = (gray.width(), gray.height());
    let pixels = w * h;
    let iters = 100;
    println!("image: {w}x{h}");

    let border = BorderPolicy::Clamp;
    let dx_img = sobel(&gray, 1, 0, 1.0, border);
    let dy_img = sobel(&gray, 0, 1, 1.0, border);
    let dx = dx_img.data();
    let dy = dy_img.data();

    // 候选密度统计（low=50）
    let mag_w = w + 2;
    let mut cand = 0usize;
    let mut edge = 0usize;
    {
        let mut mag: Vec<i32> = vec![0; 3 * mag_w];
        for y in 0..h {
            let slot = (y + 2) % 3;
            let row = &mut mag[slot * mag_w + 1..slot * mag_w + 1 + w];
            rcv::simd_kernels::canny_mag_row(&dx[y * w..y * w + w], &dy[y * w..y * w + w], row, w, false);
            cand += row.iter().filter(|&&m| m > 50).count();
            edge += row.iter().filter(|&&m| m > 100).count();
        }
    }
    println!("candidates (m>low): {cand} ({:.1}%), m>high: {edge} ({:.1}%)",
             cand as f64 / pixels as f64 * 100.0, edge as f64 / pixels as f64 * 100.0);

    let mapstep = (w + 32 + 1 + 31) & !31;

    // 1. 完整 NMS 行循环（当前实现，含 mag + 偏移转换）
    bench("nms rows (full, current)", pixels, iters, || {
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

    // 2. NMS 但栈收集换成计数（测栈 push 开销）
    //    —— 通过 high 设为 i32::MAX 使所有候选都入栈（最坏情况）对比
    bench("nms rows (high=MAX, all push)", pixels, iters, || {
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
            rcv::simd_kernels::canny_nms_row(p_row, a_row, n_row, &dx[y * w..y * w + w], &dy[y * w..y * w + w], pmap, w, 50, i32::MAX, &mut stack);
            let base = (y + 1) * mapstep + 32;
            for off in &mut stack[len0..] {
                *off += base;
            }
        }
        std::hint::black_box((&map, &stack));
    });

    // 3. 只有 mag（无 NMS）—— 已知 ~143us
    bench("mag only (3-ring)", pixels, iters, || {
        let mag_w = w + 2;
        let mut mag: Vec<i32> = vec![0; 3 * mag_w];
        for y in 0..h {
            let slot = (y + 2) % 3;
            let row = &mut mag[slot * mag_w + 1..slot * mag_w + 1 + w];
            rcv::simd_kernels::canny_mag_row(&dx[y * w..y * w + w], &dy[y * w..y * w + w], row, w, false);
        }
        std::hint::black_box(mag);
    });

    // 4. NMS 但 dx/dy 置 0（全部走水平分支，无对角分支预测）
    bench("nms rows (dx=dy=0, horiz only)", pixels, iters, || {
        let mag_w = w + 2;
        let zeros = vec![0i16; w];
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
            rcv::simd_kernels::canny_nms_row(p_row, a_row, n_row, &zeros, &zeros, pmap, w, 50, 100, &mut stack);
            let base = (y + 1) * mapstep + 32;
            for off in &mut stack[len0..] {
                *off += base;
            }
        }
        std::hint::black_box((&map, &stack));
    });

    // 5. 全函数（对照）
    bench("canny_full", pixels, iters, || {
        let _ = canny(&gray, 50.0, 100.0, ApertureSize::Sobel3, false);
    });
}
