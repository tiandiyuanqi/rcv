//! 自旋线程池：低延迟 fork-join（带宽型小任务专用）。
//!
//! rayon 的 work-stealing fork-join 在 16 线程下 ~33us/次，对 1MB 级
//! 带宽型任务（threshold ~25us 计算）开销超过计算本身，MT 反而慢于 ST。
//! OpenCV 的 pthreads 池（active-wait + 原子标志）fork-join 亚微秒。
//!
//! 本池对齐 OpenCV 策略：常驻 worker 等待 epoch 变化，固定分块
//! （worker i 处理块 i，无任务竞争），主线程处理最后一块并等待完成。
//!
//! 停车策略：worker 先自旋等待（低延迟），超过自旋预算后 `park` 睡眠，
//! 由下一次 `run_blocks` 逐个 `unpark` 唤醒。避免空闲 worker 烧满 CPU
//! 饿死同进程内其他线程（rayon 池等）。
//!
//! # Safety
//! 本模块是 `simd_kernels` 的一部分（crate 内唯一允许 unsafe 的区域）。
//! 对外暴露的安全接口 [`run_blocks`] 内部用裸指针切片，契约由参数保证。

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Mutex;

/// 自旋预算：超过此次数后停车（每次约数十 ns，总计 ~1ms 量级）。
/// bench 循环内两次调用间隔可能达数百 us，预算太小会导致 worker 频繁
/// park/unpark（唤醒风暴 ~100-400us），反而劣化吞吐。
const SPIN_BUDGET: usize = 2_000_000;

static EPOCH: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static SRC: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static DST: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static BLOCK: AtomicUsize = AtomicUsize::new(0);
/// 第二输入缓冲（`run_blocks2`：src + src2 → dst，如 adaptive 尾循环的
/// src + mean → dst）。`run_blocks` 不设置（保持 null），`BlockFn2` 不读。
static SRC2: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static SRC_BLOCK: AtomicUsize = AtomicUsize::new(0);
/// 已启动的 worker 数（`init` 用 Mutex 保护，可随需求增长）。
static NW: AtomicUsize = AtomicUsize::new(0);
/// 当前调用激活的 worker 数（`nblocks - 1`）。池常驻 worker 数可能多于本次
/// 需要的块数（首次调用启动 63 个，后续调用可能只需 15 个），`wid >= NWORKERS`
/// 的 worker 必须跳过本次处理，否则会越界写不存在的块。
static NWORKERS: AtomicUsize = AtomicUsize::new(0);
/// worker 的 Thread 句柄，`run_blocks` 唤醒停车中的 worker 用。
/// 索引即 wid；`init` 启动 worker 时写入（Mutex 保护，之后只读）。
static HANDLES: Mutex<Vec<Option<std::thread::Thread>>> = Mutex::new(Vec::new());

/// 每块的工作函数指针（threshold/to_grayscale 等带宽型内核）。
/// 参数：(src 块, dst 块, dst 块长, 上下文)。
type BlockFn = unsafe fn(*const u8, *mut u8, usize, usize);
/// 双输入版（adaptive 尾循环等）：参数 (src 块, src2 块, dst 块, 长度, 上下文)。
type BlockFn2 = unsafe fn(*const u8, *const u8, *mut u8, usize, usize);
/// 每次 `run_blocks` 调用前更新，worker 在 epoch 变化后读取。
/// 用 `AtomicPtr` 而非 `OnceLock`：`run_blocks` 会被不同内核（threshold/
/// to_grayscale 等）复用，`OnceLock` 只能 set 一次，第二次调用会静默失败，
/// 导致 worker 用错内核（如 to_grayscale 读 3 字节/像素越界）→ 内存损坏。
static FN: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static CTX: AtomicUsize = AtomicUsize::new(0);
/// FN 的实际签名：0 = BlockFn（run_blocks），1 = BlockFn2（run_blocks2）。
/// worker 按 FN2 选择 transmute 目标，避免两种签名共用一个指针类型。
static FN2: AtomicUsize = AtomicUsize::new(0);

fn worker(wid: usize) {
    let mut local = 0usize;
    loop {
        // 先自旋（低延迟），超预算后停车（不烧 CPU）。
        // park/unpark 与 EPOCH 检查的顺序保证无丢失唤醒：
        // - unpark 在 epoch 已递增之后发生；
        // - worker 在 park 前最后一次检查 EPOCH，若已递增则不 park。
        let mut spun = 0usize;
        loop {
            let e = EPOCH.load(Ordering::Acquire);
            if e != local {
                local = e;
                break;
            }
            if spun >= SPIN_BUDGET {
                // 停车前再查一次 EPOCH（Acquire），与 run_blocks 的
                // EPOCH.fetch_add(Release) 配对：若 epoch 已变则不睡。
                if EPOCH.load(Ordering::Acquire) != local {
                    local = EPOCH.load(Ordering::Acquire);
                    break;
                }
                std::thread::park();
                continue;
            }
            std::hint::spin_loop();
            spun += 1;
        }
        if local == usize::MAX {
            return;
        }
        // 本次调用只激活前 nworkers 个 worker；池常驻 worker 数可能更多，
        // 多余的 worker 跳过（不处理、不递增 DONE），等待下一 epoch。
        if wid >= NWORKERS.load(Ordering::Acquire) {
            continue;
        }
        // SAFETY: 契约由 run_blocks/run_blocks2 保证：SRC/DST/SRC2 指向的缓冲
        // 在本次 epoch 期间有效，块 wid 的偏移在界内；FN 在 EPOCH 递增前已由
        // 调用方更新为非空指针
        unsafe {
            let src = SRC.load(Ordering::Acquire);
            let dst = DST.load(Ordering::Acquire);
            let block = BLOCK.load(Ordering::Acquire);
            let src_block = SRC_BLOCK.load(Ordering::Acquire);
            let ctx = CTX.load(Ordering::Acquire);
            if FN2.load(Ordering::Acquire) == 1 {
                let f: BlockFn2 = std::mem::transmute(FN.load(Ordering::Acquire));
                let src2 = SRC2.load(Ordering::Acquire);
                f(
                    src.add(wid * src_block),
                    src2.add(wid * src_block),
                    dst.add(wid * block),
                    block,
                    ctx,
                );
            } else {
                let f: BlockFn = std::mem::transmute(FN.load(Ordering::Acquire));
                f(src.add(wid * src_block), dst.add(wid * block), block, ctx);
            }
        }
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

/// 初始化池：确保至少 `nworkers` 个常驻 worker 已启动。
/// 用 Mutex 保护启动计数，可随需求增长（首次调用启动一批，后续不足时补足）。
fn init(nworkers: usize) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let mut spawned = NW.load(Ordering::Acquire);
    while spawned < nworkers {
        let wid = spawned;
        let handle = std::thread::Builder::new()
            .name(format!("rcv-spin-{wid}"))
            .spawn(move || worker(wid))
            .expect("spawn spin worker");
        // 记录 Thread 句柄供 unpark；HANDLES 在本锁内写入，worker 侧不读。
        HANDLES
            .lock()
            .unwrap()
            .resize(wid + 1, None);
        HANDLES.lock().unwrap()[wid] = Some(handle.thread().clone());
        spawned += 1;
        NW.store(spawned, Ordering::Release);
    }
}

/// 用自旋池并行执行 `f`：dst 按 `nblocks` 等分（每块 `dst.len()/nblocks` 字节），
/// src 按比例取 `src_block = src.len()/nblocks` 字节，worker i 处理块 i，
/// 主线程处理最后一块。适合 src/dst 字节比恒定的带宽型内核（如 RGB→灰度 3:1）。
///
/// `ctx` 为传给 `f` 的上下文（如 threshold 的 t/mv/type 打包）。
/// `f` 必须是纯内存操作（无锁、无分配、无 panic）。
///
/// 全局状态（EPOCH/SRC/DST/...）非线程安全，用 `CALL_LOCK` 串行化并发调用。
///
/// # Panics
/// `nblocks < 2` 或缓冲长度不是 `nblocks` 的整数倍时 panic。
/// 串行化所有 run_blocks* 调用（全局状态 EPOCH/FN/SRC/... 非线程安全）。
/// 模块级共享：run_blocks 与 run_blocks2 必须互斥，否则并发调用竞态。
static CALL_LOCK: Mutex<()> = Mutex::new(());

pub fn run_blocks(src: &[u8], dst: &mut [u8], nblocks: usize, f: BlockFn, ctx: usize) {
    let _call_guard = CALL_LOCK.lock().unwrap();
    assert!(nblocks >= 2, "nblocks must be >= 2");
    let block = dst.len() / nblocks;
    assert_eq!(dst.len(), block * nblocks, "dst len must be divisible");
    let src_block = src.len() / nblocks;
    assert_eq!(src.len(), src_block * nblocks, "src len must be divisible");
    let nworkers = nblocks - 1;
    init(nworkers);
    // 先更新 FN 再递增 EPOCH：worker 在 EPOCH 变化后读取 FN，保证看到本次内核
    FN.store(f as *mut (), Ordering::Release);
    // SAFETY: FN2 是普通 static bool，仅在本锁（CALL_LOCK）内写入、
    // worker 在 EPOCH 递增后读取；Release/Acquire 序保证可见性
    FN2.store(0, Ordering::Release);
    CTX.store(ctx, Ordering::Release);
    SRC.store(src.as_ptr() as *mut u8, Ordering::Release);    DST.store(dst.as_mut_ptr(), Ordering::Release);
    BLOCK.store(block, Ordering::Release);
    SRC_BLOCK.store(src_block, Ordering::Release);
    NWORKERS.store(nworkers, Ordering::Release);
    DONE.store(0, Ordering::Release);
    EPOCH.fetch_add(1, Ordering::Release);
    // 唤醒所有可能停车中的 worker（unpark 对运行中的线程是空操作）
    {
        let handles = HANDLES.lock().unwrap();
        for t in handles.iter().take(nworkers).flatten() {
            t.unpark();
        }
    }
    // 主线程处理最后一块
    // SAFETY: 块 nworkers 的偏移在界内（block*nworkers < dst.len()）
    unsafe {
        f(
            src.as_ptr().add(nworkers * src_block),
            dst.as_mut_ptr().add(nworkers * block),
            block,
            ctx,
        );
    }
    while DONE.load(Ordering::Acquire) < nworkers {
        std::hint::spin_loop();
    }
}

/// [`run_blocks`] 的双输入版：src 与 src2 等长（字节比 1:1:1），
/// worker i 处理 (src 块 i, src2 块 i) → dst 块 i。
/// 适合 adaptive 尾循环这类「两输入一输出」的带宽型内核。
///
/// 其余契约同 [`run_blocks`]。
///
/// # Panics
/// `nblocks < 2`、`src.len() != src2.len()` 或缓冲长度不是 `nblocks` 的整数倍时 panic。
pub fn run_blocks2(
    src: &[u8],
    src2: &[u8],
    dst: &mut [u8],
    nblocks: usize,
    f: BlockFn2,
    ctx: usize,
) {
    let _call_guard = CALL_LOCK.lock().unwrap();
    assert!(nblocks >= 2, "nblocks must be >= 2");
    assert_eq!(src.len(), src2.len(), "src/src2 must have equal length");
    let block = dst.len() / nblocks;
    assert_eq!(dst.len(), block * nblocks, "dst len must be divisible");
    let src_block = src.len() / nblocks;
    assert_eq!(src.len(), src_block * nblocks, "src len must be divisible");
    let nworkers = nblocks - 1;
    init(nworkers);
    FN.store(f as *mut (), Ordering::Release);
    // SAFETY: 同 run_blocks 的 FN2 写入（CALL_LOCK + Release/Acquire 序）
    FN2.store(1, Ordering::Release);
    CTX.store(ctx, Ordering::Release);
    SRC.store(src.as_ptr() as *mut u8, Ordering::Release);
    SRC2.store(src2.as_ptr() as *mut u8, Ordering::Release);
    DST.store(dst.as_mut_ptr(), Ordering::Release);
    BLOCK.store(block, Ordering::Release);
    SRC_BLOCK.store(src_block, Ordering::Release);
    NWORKERS.store(nworkers, Ordering::Release);
    DONE.store(0, Ordering::Release);
    EPOCH.fetch_add(1, Ordering::Release);
    {
        let handles = HANDLES.lock().unwrap();
        for t in handles.iter().take(nworkers).flatten() {
            t.unpark();
        }
    }
    // 主线程处理最后一块
    // SAFETY: 块 nworkers 的偏移在界内（block*nworkers < dst.len()）
    unsafe {
        f(
            src.as_ptr().add(nworkers * src_block),
            src2.as_ptr().add(nworkers * src_block),
            dst.as_mut_ptr().add(nworkers * block),
            block,
            ctx,
        );
    }
    while DONE.load(Ordering::Acquire) < nworkers {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本正确性：两轮 run_blocks 结果正确（第二轮复用已启动的 worker）。
    #[test]
    fn run_blocks_two_rounds() {
        fn fill(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
            unsafe {
                let s = std::slice::from_raw_parts(src, len);
                let d = std::slice::from_raw_parts_mut(dst, len);
                for i in 0..len {
                    d[i] = s[i].wrapping_add(ctx as u8);
                }
            }
        }
        let src = vec![1u8; 256];
        let mut dst = vec![0u8; 256];
        run_blocks(&src, &mut dst, 4, fill, 7);
        assert!(dst.iter().all(|&v| v == 8));
        // 第二轮：不同 ctx、不同内核，验证 FN/CTX 正确传播
        fn fill2(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
            unsafe {
                let s = std::slice::from_raw_parts(src, len);
                let d = std::slice::from_raw_parts_mut(dst, len);
                for i in 0..len {
                    d[i] = s[i] ^ (ctx as u8);
                }
            }
        }
        run_blocks(&src, &mut dst, 4, fill2, 0x0f);
        assert!(dst.iter().all(|&v| v == 1 ^ 0x0f));
    }

    /// run_blocks2 基本正确性 + 与 run_blocks 交替（FN2 切换正确传播）。
    #[test]
    fn run_blocks2_two_rounds_and_switch() {
        fn add2(src: *const u8, src2: *const u8, dst: *mut u8, len: usize, ctx: usize) {
            unsafe {
                let s = std::slice::from_raw_parts(src, len);
                let m = std::slice::from_raw_parts(src2, len);
                let d = std::slice::from_raw_parts_mut(dst, len);
                for i in 0..len {
                    d[i] = s[i].wrapping_add(m[i]).wrapping_add(ctx as u8);
                }
            }
        }
        fn fill(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
            unsafe {
                let s = std::slice::from_raw_parts(src, len);
                let d = std::slice::from_raw_parts_mut(dst, len);
                for i in 0..len {
                    d[i] = s[i].wrapping_add(ctx as u8);
                }
            }
        }
        let src = vec![10u8; 256];
        let src2 = vec![20u8; 256];
        let mut dst = vec![0u8; 256];
        run_blocks2(&src, &src2, &mut dst, 4, add2, 5);
        assert!(dst.iter().all(|&v| v == 35));
        // 交替调用 run_blocks（FN2 切回 0），验证签名选择正确
        run_blocks(&src, &mut dst, 4, fill, 1);
        assert!(dst.iter().all(|&v| v == 11));
        // 再切回 run_blocks2
        run_blocks2(&src, &src2, &mut dst, 4, add2, 0);
        assert!(dst.iter().all(|&v| v == 30));
    }

    /// 并发调用 run_blocks：CALL_LOCK 串行化，结果均正确。
    #[test]
    fn run_blocks_concurrent() {
        fn fill(src: *const u8, dst: *mut u8, len: usize, ctx: usize) {
            unsafe {
                let s = std::slice::from_raw_parts(src, len);
                let d = std::slice::from_raw_parts_mut(dst, len);
                for i in 0..len {
                    d[i] = s[i].wrapping_add(ctx as u8);
                }
            }
        }
        let src = vec![3u8; 512];
        let mut joins = Vec::new();
        for t in 0..4u8 {
            let src = src.clone();
            joins.push(std::thread::spawn(move || {
                let mut dst = vec![0u8; 512];
                run_blocks(&src, &mut dst, 8, fill, t as usize);
                dst
            }));
        }
        for (t, j) in joins.into_iter().enumerate() {
            let dst = j.join().unwrap();
            assert!(dst.iter().all(|&v| v == 3u8.wrapping_add(t as u8)));
        }
    }
}
