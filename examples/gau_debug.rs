use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::time::Instant;

static EPOCH: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static SRC: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static DST: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static BLOCK: AtomicUsize = AtomicUsize::new(0);
static NW: AtomicUsize = AtomicUsize::new(0);

fn worker(wid: usize) {
    let mut local = 0usize;
    loop {
        while EPOCH.load(Ordering::Acquire) == local {
            std::hint::spin_loop();
        }
        local = EPOCH.load(Ordering::Acquire);
        if local == usize::MAX { return; }
        let src = SRC.load(Ordering::Acquire) as *const u8;
        let dst = DST.load(Ordering::Acquire);
        let block = BLOCK.load(Ordering::Acquire);
        // 固定块：worker wid 处理块 wid（无任务竞争）
        unsafe {
            let s = std::slice::from_raw_parts(src.add(wid * block), block);
            let d = std::slice::from_raw_parts_mut(dst.add(wid * block), block);
            rcv::simd_kernels::threshold_u8(s, d, block, 100, 255, 0);
        }
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

fn main() {
    let nw = 15usize;
    let handles: Vec<_> = (0..nw).map(|i| std::thread::spawn(move || worker(i))).collect();
    NW.store(nw, Ordering::Release);
    let (w, h) = (1024usize, 1024usize);
    let src: Vec<u8> = (0..w * h).map(|i| (i & 0xFF) as u8).collect();
    let mut dst = vec![0u8; w * h];
    let block = src.len() / 16;
    SRC.store(src.as_ptr() as *mut u8, Ordering::Release);
    DST.store(dst.as_mut_ptr(), Ordering::Release);
    BLOCK.store(block, Ordering::Release);
    // 预热
    DONE.store(0, Ordering::Release);
    EPOCH.fetch_add(1, Ordering::Release);
    unsafe {
        let s = std::slice::from_raw_parts(src.as_ptr().add(15 * block), block);
        let d = std::slice::from_raw_parts_mut(dst.as_mut_ptr().add(15 * block), block);
        rcv::simd_kernels::threshold_u8(s, d, block, 100, 255, 0);
    }
    while DONE.load(Ordering::Acquire) < nw { std::hint::spin_loop(); }
    let iters = 2000;
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..iters {
            DONE.store(0, Ordering::Release);
            EPOCH.fetch_add(1, Ordering::Release);
            unsafe {
                let s = std::slice::from_raw_parts(src.as_ptr().add(15 * block), block);
                let d = std::slice::from_raw_parts_mut(dst.as_mut_ptr().add(15 * block), block);
                rcv::simd_kernels::threshold_u8(s, d, block, 100, 255, 0);
            }
            while DONE.load(Ordering::Acquire) < nw { std::hint::spin_loop(); }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!("spin pool v2: {:.0} GB/s", (w * h) as f64 * iters as f64 * 2.0 / best / 1e9);
    EPOCH.store(usize::MAX, Ordering::Release);
    for h in handles { let _ = h.join(); }
}
