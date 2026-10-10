use rcv::{BorderPolicy, erode, to_grayscale, imread};

fn main() {
    rayon::ThreadPoolBuilder::new().num_threads(1).build_global().unwrap();
    let img = imread("compare/inputs/bench.png").unwrap();
    let gray = to_grayscale(&img);
    let kernel = vec![1u8; 9];
    let iters = 200;
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let t = std::time::Instant::now();
        for _ in 0..iters {
            let _ = erode(&gray, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
        }
        best = best.min(t.elapsed().as_secs_f64() / iters as f64);
    }
    let pixels = (gray.width() * gray.height()) as f64 / 1e6;
    println!("erode full: {:.1} us ({:.0} MPix/s)", best * 1e6, pixels / best);
}
