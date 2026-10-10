use rcv::{ApertureSize, BorderPolicy, canny, imread, sobel, to_grayscale};

fn main() {
    let img = imread("compare/inputs/bench.png").expect("imread");
    let gray = to_grayscale(&img);
    let (w, h) = (gray.width(), gray.height());
    
    // Full-image Sobel
    let border = BorderPolicy::Clamp;
    let dx_full = sobel(&gray, 1, 0, 1.0, border);
    let dy_full = sobel(&gray, 0, 1, 1.0, border);
    
    // Block-internal Sobel for a single block (whole image)
    // Replicate the compute_row logic
    let dx_row_k1 = 1i32;
    let dy_row_k0 = 2i32;
    let dy_row_k1 = 1i32;
    let dx_col_f0 = 2i32;
    let dx_col_f1 = 1i32;
    let dy_col_f1 = 1i32;
    
    let mut dx_ring: Vec<i16> = vec![0; 3 * w];
    let mut dy_ring: Vec<i16> = vec![0; 3 * w];
    let mut mid_dx: Vec<i32> = vec![0; 3 * w];
    let mut mid_dy: Vec<i32> = vec![0; 3 * w];
    
    let row_stage = |r: i64, mid_dx: &mut [i32], mid_dy: &mut [i32]| {
        let sy = r.clamp(0, h as i64 - 1) as usize;
        let src_row = &gray.data()[sy * w..(sy + 1) * w];
        let mslot = (r.rem_euclid(3)) as usize;
        let mrow = &mut mid_dx[mslot * w..mslot * w + w];
        rcv::simd_kernels::deriv_row_asymm_k3_cn1(src_row, mrow, w, dx_row_k1, border);
        let mrow_dy = &mut mid_dy[mslot * w..mslot * w + w];
        rcv::simd_kernels::deriv_row_symm_k3_cn1(src_row, mrow_dy, w, dy_row_k0, dy_row_k1, border);
    };
    let col_stage = |r: i64, dx_ring: &mut [i16], dy_ring: &mut [i16], mid_dx: &[i32], mid_dy: &[i32]| {
        let dslot = (r.rem_euclid(3)) as usize;
        let m0 = &mid_dx[((r - 1).rem_euclid(3)) as usize * w..((r - 1).rem_euclid(3)) as usize * w + w];
        let m1 = &mid_dx[((r).rem_euclid(3)) as usize * w..((r).rem_euclid(3)) as usize * w + w];
        let m2 = &mid_dx[((r + 1).rem_euclid(3)) as usize * w..((r + 1).rem_euclid(3)) as usize * w + w];
        let drow = &mut dx_ring[dslot * w..dslot * w + w];
        rcv::simd_kernels::deriv_col_symm_k3([m0, m1, m2], drow, w, dx_col_f0, dx_col_f1, 0);
        let n0 = &mid_dy[((r - 1).rem_euclid(3)) as usize * w..((r - 1).rem_euclid(3)) as usize * w + w];
        let n1 = &mid_dy[((r).rem_euclid(3)) as usize * w..((r).rem_euclid(3)) as usize * w + w];
        let n2 = &mid_dy[((r + 1).rem_euclid(3)) as usize * w..((r + 1).rem_euclid(3)) as usize * w + w];
        let drow_dy = &mut dy_ring[dslot * w..dslot * w + w];
        rcv::simd_kernels::deriv_col_asymm_k3([n0, n1, n2], drow_dy, w, dy_col_f1, 0);
    };
    
    // Pre-fill mid rows -1, 0
    row_stage(-1, &mut mid_dx, &mut mid_dy);
    row_stage(0, &mut mid_dx, &mut mid_dy);
    
    let mut mismatches = 0;
    for y in 0..h {
        row_stage(y as i64 + 1, &mut mid_dx, &mut mid_dy);
        col_stage(y as i64, &mut dx_ring, &mut dy_ring, &mid_dx, &mid_dy);
        let dslot = (y as i64).rem_euclid(3) as usize;
        let dx_row = &dx_ring[dslot * w..dslot * w + w];
        let dy_row = &dy_ring[dslot * w..dslot * w + w];
        let dx_ref = &dx_full.data()[y * w..y * w + w];
        let dy_ref = &dy_full.data()[y * w..y * w + w];
        for x in 0..w {
            if dx_row[x] != dx_ref[x] || dy_row[x] != dy_ref[x] {
                if mismatches < 10 {
                    println!("mismatch at ({x},{y}): dx={} vs {}, dy={} vs {}", dx_row[x], dx_ref[x], dy_row[x], dy_ref[x]);
                }
                mismatches += 1;
            }
        }
    }
    println!("total mismatches: {mismatches}");
}
