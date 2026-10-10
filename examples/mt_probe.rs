//! MT 探针 7：洪泛在 MT canny 内的真实耗时（块内局部洪泛 + 全局洪泛分开测）
use std::time::Instant;
use rcv::{ApertureSize, BorderPolicy, canny, imread, sobel, to_grayscale};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let nt: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(64);
    rayon::ThreadPoolBuilder::new().num_threads(nt).build_global().unwrap();
    let img = imread("compare/inputs/bench.png").expect("imread");
    let gray = to_grayscale(&img);
    let (w, h) = (gray.width(), gray.height());
    let iters = 100;
    println!("threads: {nt}");

    let b = BorderPolicy::Clamp;
    let dx_img = sobel(&gray, 1, 0, 1.0, b);
    let dy_img = sobel(&gray, 0, 1, 1.0, b);
    let dx = dx_img.data();
    let dy = dy_img.data();
    let mapstep = (w + 32 + 1 + 31) & !31;
    use rayon::prelude::*;

    // 变体 A：NMS 块并行，无任何洪泛（收集 peaks 后丢弃）
    let mut tA = f64::MAX;
    // 变体 B：NMS 块并行 + 块内局部洪泛（真实结构）
    let mut tB = f64::MAX;
    // 变体 C：B + 全局洪泛
    let mut tC = f64::MAX;

    let run = |flood: bool, global: bool| -> f64 {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let s = Instant::now();
            for _ in 0..iters {
                let mut map: Vec<u8> = rcv::simd_kernels::alloc_uninit((h + 2) * mapstep);
                map[..mapstep].fill(1);
                map[(h + 1) * mapstep..].fill(1);
                let threads = rayon::current_num_threads().max(1);
                let chunk = (h / threads).max(4);
                let nblocks = h.div_ceil(chunk);
                let full = &mut map[mapstep..];
                let mut blocks: Vec<&mut [u8]> = Vec::with_capacity(nblocks);
                let mut r = full;
                for bi in 0..nblocks {
                    let y0 = bi * chunk;
                    let y1 = (y0 + chunk).min(h);
                    let (blk, rr) = r.split_at_mut((y1 - y0) * mapstep);
                    blocks.push(blk);
                    r = rr;
                }
                let peaks: Vec<Vec<usize>> = blocks.into_par_iter().enumerate().map(|(bi, mblk)| {
                    let y0 = bi * chunk;
                    let y1 = (y0 + chunk).min(h);
                    let mag_w = w + 2;
                    let mut mag: Vec<i32> = vec![0; 3 * mag_w];
                    let mut stack: Vec<usize> = Vec::new();
                    let fill = |rr: i64, mag: &mut [i32]| {
                        let slot = rr.rem_euclid(3) as usize;
                        let row = &mut mag[slot*mag_w..slot*mag_w+mag_w];
                        if rr >= 0 && (rr as usize) < h {
                            row[0] = 0; row[w+1] = 0;
                            rcv::simd_kernels::canny_mag_row(&dx[rr as usize * w..rr as usize * w + w], &dy[rr as usize * w..rr as usize * w + w], &mut row[1..1+w], w, false);
                        } else { row.fill(0); }
                    };
                    fill(y0 as i64 - 1, &mut mag);
                    fill(y0 as i64, &mut mag);
                    if y0 + 1 < y1 { fill(y0 as i64 + 1, &mut mag); }
                    for y in y0..y1 {
                        fill(y as i64 + 1, &mut mag);
                        let sp = (y as i64 - 1).rem_euclid(3) as usize;
                        let sa = (y as i64).rem_euclid(3) as usize;
                        let sn = ((y+1) as i64).rem_euclid(3) as usize;
                        let p = &mag[sp*mag_w..sp*mag_w+mag_w];
                        let a = &mag[sa*mag_w..sa*mag_w+mag_w];
                        let n = &mag[sn*mag_w..sn*mag_w+mag_w];
                        let pmap = &mut mblk[(y-y0)*mapstep+32..(y-y0)*mapstep+32+w+2];
                        let l0 = stack.len();
                        rcv::simd_kernels::canny_nms_row(p, a, n, &dx[y*w..y*w+w], &dy[y*w..y*w+w], pmap, w, 50, 100, &mut stack);
                        let base = (y-y0)*mapstep+32;
                        for off in &mut stack[l0..] { *off += base; }
                    }
                    let mut peaks = Vec::new();
                    let lo = 0usize;
                    let hi = (y1 - y0) * mapstep;
                    if flood {
                        // 块内洪泛（复现 flood_within 语义）
                        while let Some(off) = stack.pop() {
                            let on_border = off < lo + mapstep || off >= hi - mapstep;
                            if on_border { peaks.push(off); continue; }
                            // flood_within
                            mblk[off] = 2;
                            let nbs = [
                                off.wrapping_sub(mapstep + 1), off.wrapping_sub(mapstep),
                                off.wrapping_sub(mapstep - 1), off - 1, off + 1,
                                off + mapstep - 1, off + mapstep, off + mapstep + 1,
                            ];
                            for nb in nbs {
                                if nb < lo || nb >= hi { continue; }
                                if mblk[nb] == 0 {
                                    let onb = nb < lo + mapstep || nb >= hi - mapstep;
                                    mblk[nb] = 2;
                                    if onb { peaks.push(nb); } else { stack.push(nb); }
                                }
                            }
                        }
                    } else {
                        while let Some(off) = stack.pop() {
                            if off < lo + mapstep || off >= hi - mapstep {
                                peaks.push(off);
                            }
                        }
                    }
                    for off in &mut peaks { *off += (y0 + 1) * mapstep; }
                    peaks
                }).collect();
                if global {
                    let mut all: Vec<usize> = peaks.into_iter().flatten().collect();
                    if !all.is_empty() {
                        rcv::simd_kernels::canny_hysteresis(&mut map, mapstep, &mut all);
                    }
                    std::hint::black_box(&map);
                } else {
                    let total: usize = peaks.iter().map(|p| p.len()).sum();
                    std::hint::black_box((map, total));
                }
            }
            best = best.min(s.elapsed().as_secs_f64() / iters as f64);
        }
        best
    };

    tA = run(false, false);
    tB = run(true, false);
    tC = run(true, true);
    println!("A: NMS only (no flood):   {:>8.1} us", tA * 1e6);
    println!("B: + local flood:         {:>8.1} us", tB * 1e6);
    println!("C: + global flood:        {:>8.1} us", tC * 1e6);
}
