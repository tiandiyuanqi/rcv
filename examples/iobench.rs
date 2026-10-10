//! imwrite 编码速度基准：rcv vs OpenCV5。
fn main() {
    let img = rcv::imread("compare/inputs/bench.png").expect("imread");
    let gray = rcv::to_grayscale(&img);
    let (w, h) = (img.width(), img.height());
    let pixels = w * h;
    let iters = 20;

    let bench = |name: &str, f: &mut dyn FnMut()| {
        f(); // warmup
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            for _ in 0..iters { f(); }
            best = best.min(t.elapsed().as_secs_f64());
        }
        let mpix = pixels as f64 * iters as f64 / best / 1e6;
        println!("{name:<32} {mpix:>10.2} MPix/s");
    };

    bench("imwrite_png_rgb_default", &mut || {
        let _ = rcv::imwrite("/tmp/opencode/io_rgb.png", &img);
    });
    bench("imwrite_png_rgb_c6", &mut || {
        let _ = rcv::imwrite_with_params("/tmp/opencode/io_rgb.png", &img, &[rcv::ImwriteParam::PngCompression(6)]);
    });
    bench("imwrite_png_gray_default", &mut || {
        let _ = rcv::imwrite("/tmp/opencode/io_gray.png", &gray);
    });
    bench("imwrite_jpeg_rgb_q95", &mut || {
        let _ = rcv::imwrite_with_params("/tmp/opencode/io_rgb.jpg", &img, &[rcv::ImwriteParam::JpegQuality(95)]);
    });
    bench("imread_png_rgb", &mut || {
        let _ = rcv::imread("/tmp/opencode/io_rgb.png");
    });
}
