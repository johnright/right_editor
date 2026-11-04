//! Renders assets/icon.svg into assets/icon-256.png (preview), assets/icon.ico
//! and assets/icon.res. The .res goes straight to lld-link via build.rs, which
//! saves us from needing rc.exe/llvm-rc when cross-compiling.

use std::fs;
use std::path::Path;

use resvg::{tiny_skia, usvg};

const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

fn main() {
    // `icon render <in.svg> <out.png>` for one-off previews
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [cmd, input, output] = args.as_slice()
        && cmd == "render"
    {
        let svg = fs::read(input).expect("cannot read the SVG");
        let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("invalid SVG");
        let size = tree.size();
        let mut pixmap = tiny_skia::Pixmap::new(size.width() as u32, size.height() as u32).unwrap();
        resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
        fs::write(output, pixmap.encode_png().unwrap()).unwrap();
        return;
    }

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let svg = fs::read(assets.join("icon.svg")).expect("assets/icon.svg missing");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("invalid SVG");
    let pngs: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, render(&tree, s))).collect();

    fs::write(assets.join("icon-256.png"), &pngs[pngs.len() - 1].1).unwrap();
    fs::write(assets.join("icon.ico"), ico(&pngs)).unwrap();
    fs::write(assets.join("icon.res"), res(&pngs)).unwrap();
    println!("generated {} sizes: {SIZES:?}", pngs.len());
}

fn render(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).unwrap();
    let scale = size as f32 / tree.size().width();
    resvg::render(tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().unwrap()
}

// shared by ICONDIRENTRY and GRPICONDIRENTRY; 256 is stored as 0
fn dir_entry(out: &mut Vec<u8>, size: u32, bytes: usize) {
    let dim = if size >= 256 { 0 } else { size as u8 };
    out.extend([dim, dim, 0, 0]); // width, height, colors, reserved
    out.extend(1u16.to_le_bytes()); // planes
    out.extend(32u16.to_le_bytes()); // bits per pixel
    out.extend((bytes as u32).to_le_bytes());
}

fn ico(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend([0u16, 1, pngs.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * pngs.len();
    for (size, png) in pngs {
        dir_entry(&mut out, *size, png.len());
        out.extend((offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in pngs {
        out.extend(png);
    }
    out
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

// one RESOURCEHEADER + data, numeric type and id
fn res_entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes()); // DataSize
    out.extend(32u32.to_le_bytes()); // HeaderSize
    out.extend([0xFFFFu16, kind, 0xFFFF, id].iter().flat_map(|v| v.to_le_bytes()));
    out.extend(0u32.to_le_bytes()); // DataVersion
    out.extend(flags.to_le_bytes()); // MemoryFlags
    out.extend(0u16.to_le_bytes()); // LanguageId: neutral
    out.extend(0u32.to_le_bytes()); // Version
    out.extend(0u32.to_le_bytes()); // Characteristics
    out.extend(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn res(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    // empty first entry marks a 32-bit .res
    res_entry(&mut out, 0, 0, 0, &[]);
    // PNG-compressed images, fine since Vista
    for (i, (_, png)) in pngs.iter().enumerate() {
        res_entry(&mut out, RT_ICON, i as u16 + 1, 0x1010, png);
    }
    // group id 1 = the exe's icon
    let mut group = Vec::new();
    group.extend([0u16, 1, pngs.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    for (i, (size, png)) in pngs.iter().enumerate() {
        dir_entry(&mut group, *size, png.len());
        group.extend((i as u16 + 1).to_le_bytes());
    }
    res_entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    out
}
