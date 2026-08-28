//! Build-time Windows icon embedding.
//!
//! GPUI's Windows backend loads resource id 1 from the application module for
//! the window class icon. Keep the icon in the exe instead of loading an
//! external file at runtime. The SVG in `assets/` is the editable source of
//! truth; this small renderer mirrors its deliberately geometric mark.

use std::fs;
use std::path::{Path, PathBuf};

const BACKGROUND: [u8; 3] = [16, 21, 27];
const BORDER: [u8; 3] = [46, 57, 68];
const CYAN: [u8; 3] = [102, 230, 245];
const WHITE: [u8; 3] = [242, 246, 247];
const AMBER: [u8; 3] = [255, 180, 84];

fn main() {
    println!("cargo:rerun-if-changed=../../assets/phelper-icon.svg");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

fn embed_windows_icon() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let ico_path = out_dir.join("phelper.ico");
    write_ico(&ico_path);

    let rc_path = out_dir.join("phelper-icon.rc");
    let ico_for_rc = ico_path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"");
    fs::write(
        &rc_path,
        format!("#define IDI_PHELPER 1\nIDI_PHELPER ICON \"{ico_for_rc}\"\n"),
    )
    .expect("write generated Windows resource file");

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .unwrap_or_else(|error| panic!("failed to embed phelper icon: {error}"));
}

fn write_ico(path: &Path) {
    let sizes = [16u32, 32u32];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&size| render_dib(size)).collect();
    let directory_size = 6 + sizes.len() * 16;
    let mut ico = Vec::with_capacity(directory_size + images.iter().map(Vec::len).sum::<usize>());

    put_u16(&mut ico, 0); // reserved
    put_u16(&mut ico, 1); // icon type
    put_u16(&mut ico, sizes.len() as u16);

    let mut offset = directory_size as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        ico.push(size as u8); // width; 0 means 256
        ico.push(size as u8); // height; 0 means 256
        ico.push(0); // palette entries
        ico.push(0); // reserved
        put_u16(&mut ico, 1); // color planes
        put_u16(&mut ico, 32); // bits per pixel
        put_u32(&mut ico, image.len() as u32);
        put_u32(&mut ico, offset);
        offset += image.len() as u32;
    }

    for image in images {
        ico.extend(image);
    }
    fs::write(path, ico).expect("write generated Windows icon");
}

fn render_dib(size: u32) -> Vec<u8> {
    let width = size as usize;
    let row_bytes = width * 4;
    let mask_row_bytes = width.div_ceil(32) * 4;
    let image_bytes = row_bytes * width + mask_row_bytes * width;
    let mut dib = Vec::with_capacity(40 + image_bytes);

    put_u32(&mut dib, 40); // BITMAPINFOHEADER size
    put_i32(&mut dib, size as i32);
    put_i32(&mut dib, (size * 2) as i32); // color bitmap + AND mask
    put_u16(&mut dib, 1); // planes
    put_u16(&mut dib, 32); // BGRA
    put_u32(&mut dib, 0); // BI_RGB
    put_u32(&mut dib, image_bytes as u32);
    put_i32(&mut dib, 0); // horizontal pixels per meter
    put_i32(&mut dib, 0); // vertical pixels per meter
    put_u32(&mut dib, 0); // colors used
    put_u32(&mut dib, 0); // important colors

    // DIB rows are bottom-up. Windows uses the alpha channel for modern
    // icons and the AND mask as the compatibility fallback.
    for y in (0..size).rev() {
        for x in 0..size {
            let [r, g, b, a] = pixel(x, y, size);
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_row_bytes];
        for x in 0..size {
            if pixel(x, y, size)[3] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        dib.extend(row);
    }
    dib
}

fn pixel(x: u32, y: u32, size: u32) -> [u8; 4] {
    const SAMPLES: u32 = 4;
    let mut sum = [0u32; 4];
    let scale = 256.0 / size as f32;

    for sy in 0..SAMPLES {
        for sx in 0..SAMPLES {
            let px = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
            let py = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
            let color = sample(px, py);
            for (channel, value) in color.into_iter().enumerate() {
                sum[channel] += value as u32;
            }
        }
    }

    let samples = SAMPLES * SAMPLES;
    [
        (sum[0] / samples) as u8,
        (sum[1] / samples) as u8,
        (sum[2] / samples) as u8,
        (sum[3] / samples) as u8,
    ]
}

fn sample(x: f32, y: f32) -> [u8; 4] {
    if rounded_rect_sdf(x, y, 128.0, 128.0, 232.0, 58.0) > 0.0 {
        return [0, 0, 0, 0];
    }

    let border = rounded_rect_sdf(x, y, 128.0, 128.0, 229.0, 56.5) >= 0.0;
    let stem = distance_to_segment(x, y, (78.0, 202.0), (78.0, 72.0)) <= 12.0;
    let bowl = distance_to_path(x, y) <= 12.0;
    let control_point = distance_to_point(x, y, 188.0, 75.0) <= 12.0;

    let rgb = if control_point {
        AMBER
    } else if bowl {
        WHITE
    } else if stem {
        CYAN
    } else if border {
        BORDER
    } else {
        BACKGROUND
    };
    [rgb[0], rgb[1], rgb[2], 255]
}

fn rounded_rect_sdf(x: f32, y: f32, cx: f32, cy: f32, side: f32, radius: f32) -> f32 {
    let half = side / 2.0;
    let qx = (x - cx).abs() - (half - radius);
    let qy = (y - cy).abs() - (half - radius);
    let outside_x = qx.max(0.0);
    let outside_y = qy.max(0.0);
    (outside_x * outside_x + outside_y * outside_y).sqrt() + qx.max(qy).min(0.0) - radius
}

fn distance_to_path(x: f32, y: f32) -> f32 {
    let mut distance = distance_to_segment(x, y, (78.0, 75.0), (139.0, 75.0));
    for i in 0..16 {
        let t0 = i as f32 / 16.0;
        let t1 = (i + 1) as f32 / 16.0;
        distance = distance.min(distance_to_segment(
            x,
            y,
            cubic(
                (139.0, 75.0),
                (169.0, 75.0),
                (188.0, 93.0),
                (188.0, 119.0),
                t0,
            ),
            cubic(
                (139.0, 75.0),
                (169.0, 75.0),
                (188.0, 93.0),
                (188.0, 119.0),
                t1,
            ),
        ));
        distance = distance.min(distance_to_segment(
            x,
            y,
            cubic(
                (188.0, 119.0),
                (188.0, 146.0),
                (169.0, 163.0),
                (139.0, 163.0),
                t0,
            ),
            cubic(
                (188.0, 119.0),
                (188.0, 146.0),
                (169.0, 163.0),
                (139.0, 163.0),
                t1,
            ),
        ));
    }
    distance.min(distance_to_segment(x, y, (139.0, 163.0), (78.0, 163.0)))
}

fn cubic(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    (
        u.powi(3) * p0.0
            + 3.0 * u.powi(2) * t * p1.0
            + 3.0 * u * t.powi(2) * p2.0
            + t.powi(3) * p3.0,
        u.powi(3) * p0.1
            + 3.0 * u.powi(2) * t * p1.1
            + 3.0 * u * t.powi(2) * p2.1
            + t.powi(3) * p3.1,
    )
}

fn distance_to_segment(x: f32, y: f32, start: (f32, f32), end: (f32, f32)) -> f32 {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared == 0.0 {
        0.0
    } else {
        ((x - start.0) * dx + (y - start.1) * dy) / length_squared
    }
    .clamp(0.0, 1.0);
    distance_to_point(x, y, start.0 + t * dx, start.1 + t * dy)
}

fn distance_to_point(x: f32, y: f32, px: f32, py: f32) -> f32 {
    ((x - px).powi(2) + (y - py).powi(2)).sqrt()
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
