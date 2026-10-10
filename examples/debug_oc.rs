use rcv::{BorderPolicy, morph_open, erode, dilate, Image};

fn main() {
    let src = Image::<u8, 1>::from_vec(3, 3, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
    let kernel = [1u8; 9];
    // 参考路径
    let e = erode(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    let d_of_e = dilate(&e, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    println!("erode:        {:?}", e.data());
    println!("dilate(erode):{:?}", d_of_e.data());
    // 融合路径
    let opened = morph_open(&src, &kernel, (3, 3), (-1, -1), 1, BorderPolicy::Clamp);
    println!("fused open:   {:?}", opened.data());
}
