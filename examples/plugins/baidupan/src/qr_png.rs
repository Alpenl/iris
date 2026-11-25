//! Minimal pure-Rust PNG-to-QR-matrix extractor so Baidu Passport's official
//! `passport.baidu.com/v2/api/qrcode` PNG image (`245x245`, 1-bit palette) is
//! rendered faithfully inside Iris's native `Widget::QrCode` grid.

use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;

const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_PNG_DIM: usize = 1024;
const MAX_DECODED_BYTES: usize = 4 * 1024 * 1024;

/// Decodes a Baidu Passport QR PNG into a square `'1'` / `'0'` module matrix
/// (`21..=96` modules wide, matching Iris UI's `Widget::QrCode` constraints).
pub fn decode_qr_png_to_matrix(png: &[u8]) -> Result<Vec<String>, String> {
    let (width, height, dark) = decode_png_dark_pixels(png)?;

    let mut min_x = width;
    let mut max_x = 0usize;
    let mut min_y = height;
    let mut max_y = 0usize;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if dark[y * width + x] {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                found = true;
            }
        }
    }
    if !found {
        return Err("二维码图片未包含有效像素".into());
    }
    let bw = max_x - min_x + 1;
    let bh = max_y - min_y + 1;
    if bw < 21 || bh < 21 {
        return Err("二维码尺寸过小".into());
    }

    // Measure the top-left 7-module finder pattern's top dark bar.
    let mut finder_run = 0usize;
    while min_x + finder_run <= max_x && dark[min_y * width + min_x + finder_run] {
        finder_run += 1;
    }
    if finder_run < 7 {
        return Err("未能识别二维码定位符".into());
    }
    let mod_px = finder_run as f64 / 7.0;
    let raw_modules = ((bw.max(bh) as f64) / mod_px).round() as usize;
    // Standard ISO/IEC 18004 QR versions have dimensions `17 + 4*V` (21..=177).
    // Snap to the nearest valid QR dimension if within 1 module.
    let modules = if raw_modules >= 21 && (raw_modules - 17).is_multiple_of(4) {
        raw_modules
    } else {
        let v = (((raw_modules.saturating_sub(17)) as f64) / 4.0).round() as usize;
        (17 + 4 * v.clamp(1, 19)).min(96)
    };
    if !(21..=96).contains(&modules) {
        return Err(format!("二维码阶数超出渲染上限 ({modules})"));
    }

    let step_x = bw as f64 / modules as f64;
    let step_y = bh as f64 / modules as f64;
    let mut matrix = Vec::with_capacity(modules);
    for my in 0..modules {
        let py = (min_y as f64 + (my as f64 + 0.5) * step_y)
            .floor()
            .clamp(0.0, (height - 1) as f64) as usize;
        let mut row = String::with_capacity(modules);
        for mx in 0..modules {
            let px = (min_x as f64 + (mx as f64 + 0.5) * step_x)
                .floor()
                .clamp(0.0, (width - 1) as f64) as usize;
            row.push(if dark[py * width + px] { '1' } else { '0' });
        }
        matrix.push(row);
    }
    Ok(matrix)
}

fn decode_png_dark_pixels(png: &[u8]) -> Result<(usize, usize, Vec<bool>), String> {
    if png.len() < 33 || &png[..8] != PNG_SIG {
        return Err("不是有效的 PNG 图片".into());
    }
    let mut pos = 8usize;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut idat = Vec::new();
    let mut saw_ihdr = false;

    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let kind = &png[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start
            .checked_add(len)
            .ok_or_else(|| "PNG 块长度溢出".to_owned())?;
        if data_end + 4 > png.len() {
            return Err("PNG 数据截断".into());
        }
        let data = &png[data_start..data_end];
        pos = data_end + 4;

        match kind {
            b"IHDR" => {
                if len < 13 {
                    return Err("PNG IHDR 无效".into());
                }
                width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
                bit_depth = data[8];
                color_type = data[9];
                let compression = data[10];
                let filter = data[11];
                let interlace = data[12];
                if width == 0
                    || height == 0
                    || width > MAX_PNG_DIM
                    || height > MAX_PNG_DIM
                    || compression != 0
                    || filter != 0
                    || interlace != 0
                {
                    return Err("不支持的 PNG 编码格式".into());
                }
                saw_ihdr = true;
            }
            b"PLTE" => {
                if !len.is_multiple_of(3) {
                    return Err("PNG 调色板格式错误".into());
                }
                palette = data.as_chunks::<3>().0.to_vec();
            }
            b"IDAT" => {
                if idat.len() + len > 2 * 1024 * 1024 {
                    return Err("PNG 图片过大".into());
                }
                idat.extend_from_slice(data);
            }
            b"IEND" => break,
            _ => {}
        }
    }

    if !saw_ihdr || idat.is_empty() {
        return Err("PNG 缺少必要块".into());
    }

    let (channels, bpp_bytes, stride) = match (color_type, bit_depth) {
        (0 | 3, 1 | 2 | 4 | 8) => (1usize, 1usize, (width * usize::from(bit_depth)).div_ceil(8)),
        (2, 8) => (3, 3, width * 3),
        (4, 8) => (2, 2, width * 2),
        (6, 8) => (4, 4, width * 4),
        _ => {
            return Err(format!(
                "不支持的 PNG 色彩类型 color_type={color_type}, bit_depth={bit_depth}"
            ));
        }
    };
    let expected = height
        .checked_mul(stride + 1)
        .ok_or_else(|| "PNG 缓冲区溢出".to_owned())?;
    if expected > MAX_DECODED_BYTES {
        return Err("PNG 解码体积超限".into());
    }
    let raw = decompress_to_vec_zlib_with_limit(&idat, expected)
        .map_err(|_| "PNG zlib 解压失败".to_owned())?;
    if raw.len() < expected {
        return Err("PNG 像素流不完整".into());
    }

    let mut prev_row = vec![0u8; stride];
    let mut cur_row = vec![0u8; stride];
    let mut dark = vec![false; width * height];

    for y in 0..height {
        let base = y * (stride + 1);
        let filter_type = raw[base];
        let src = &raw[base + 1..base + 1 + stride];
        unfilter_scanline(filter_type, src, &prev_row, &mut cur_row, bpp_bytes)?;

        for x in 0..width {
            let is_dark = match color_type {
                3 => {
                    let idx = sample_packed(&cur_row, x, bit_depth) as usize;
                    let rgb = palette.get(idx).copied().unwrap_or([255, 255, 255]);
                    u16::from(rgb[0]) + u16::from(rgb[1]) + u16::from(rgb[2]) < 384
                }
                0 => {
                    let max_v = (1u16 << bit_depth) - 1;
                    let v = u16::from(sample_packed(&cur_row, x, bit_depth));
                    v * 2 < max_v
                }
                2 => {
                    let o = x * 3;
                    u16::from(cur_row[o]) + u16::from(cur_row[o + 1]) + u16::from(cur_row[o + 2])
                        < 384
                }
                4 => {
                    let o = x * 2;
                    cur_row[o + 1] > 127 && cur_row[o] < 128
                }
                6 => {
                    let o = x * 4;
                    let lum = u16::from(cur_row[o])
                        + u16::from(cur_row[o + 1])
                        + u16::from(cur_row[o + 2]);
                    cur_row[o + 3] > 127 && lum < 384
                }
                _ => {
                    let _ = channels;
                    false
                }
            };
            dark[y * width + x] = is_dark;
        }
        prev_row.copy_from_slice(&cur_row);
    }
    Ok((width, height, dark))
}

fn sample_packed(row: &[u8], x: usize, bit_depth: u8) -> u8 {
    match bit_depth {
        1 => (row[x / 8] >> (7 - (x % 8))) & 0x01,
        2 => (row[x / 4] >> (6 - (x % 4) * 2)) & 0x03,
        4 => (row[x / 2] >> (4 - (x % 2) * 4)) & 0x0F,
        _ => row[x],
    }
}

fn unfilter_scanline(
    filter: u8,
    src: &[u8],
    prev: &[u8],
    dst: &mut [u8],
    bpp: usize,
) -> Result<(), String> {
    match filter {
        0 => dst.copy_from_slice(src),
        1 => {
            for i in 0..src.len() {
                let left = if i >= bpp { dst[i - bpp] } else { 0 };
                dst[i] = src[i].wrapping_add(left);
            }
        }
        2 => {
            for i in 0..src.len() {
                dst[i] = src[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..src.len() {
                let left = if i >= bpp { u16::from(dst[i - bpp]) } else { 0 };
                let up = u16::from(prev[i]);
                dst[i] = src[i].wrapping_add(((left + up) / 2) as u8);
            }
        }
        4 => {
            for i in 0..src.len() {
                let a = if i >= bpp { dst[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                dst[i] = src[i].wrapping_add(paeth(a, b, c));
            }
        }
        other => return Err(format!("不支持的 PNG 滤镜类型 {other}")),
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}
