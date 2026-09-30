//! 生成「横向成品」背景条（status-bg-strip）——用与运行时**完全相同**的
//! cover 缩放/裁剪（image crate 的 Lanczos3），保证快速路径与旧路径逐像素一致。
//!
//!   cargo run --release --example gen_strip -- \
//!       src/services/templates/status-bg.jpg out.png 779 2240
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: gen_strip <src.jpg> <out.png> <iw> <ih>");
        std::process::exit(2);
    }
    let iw: u32 = a[3].parse().unwrap();
    let ih: u32 = a[4].parse().unwrap();
    let img = image::load_from_memory(&std::fs::read(&a[1]).unwrap()).unwrap();
    let (bw, bh) = (img.width(), img.height());
    let scale = (iw as f32 / bw as f32).max(ih as f32 / bh as f32);
    let sw = (bw as f32 * scale).ceil() as u32;
    let sh = (bh as f32 * scale).ceil() as u32;
    let rgb = img.to_rgb8();
    let scaled = image::imageops::resize(&rgb, sw, sh, image::imageops::FilterType::Lanczos3);
    let ox = (sw - iw) / 2;
    let oy = (sh - ih) / 2;
    let crop = image::imageops::crop_imm(&scaled, ox, oy, iw, ih).to_image();
    crop.save(&a[2]).unwrap();
    println!("wrote {} {}x{} (src {}x{} scale {:.5})", a[2], iw, ih, bw, bh, scale);
}
