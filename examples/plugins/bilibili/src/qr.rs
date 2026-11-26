//! Compact ISO/IEC 18004 QR Code Byte-Mode (ECC Level L, Versions 1..=6) matrix
//! encoder so Iris renders scannable Bilibili App login QR codes natively.

struct VersionSpec {
    version: usize,
    data_codewords: usize,
    ec_codewords_per_block: usize,
    blocks: usize,
    align: &'static [usize],
}

const VERSIONS: &[VersionSpec] = &[
    VersionSpec {
        version: 1,
        data_codewords: 19,
        ec_codewords_per_block: 7,
        blocks: 1,
        align: &[],
    },
    VersionSpec {
        version: 2,
        data_codewords: 34,
        ec_codewords_per_block: 10,
        blocks: 1,
        align: &[6, 18],
    },
    VersionSpec {
        version: 3,
        data_codewords: 55,
        ec_codewords_per_block: 15,
        blocks: 1,
        align: &[6, 22],
    },
    VersionSpec {
        version: 4,
        data_codewords: 80,
        ec_codewords_per_block: 20,
        blocks: 1,
        align: &[6, 26],
    },
    VersionSpec {
        version: 5,
        data_codewords: 108,
        ec_codewords_per_block: 26,
        blocks: 1,
        align: &[6, 30],
    },
    VersionSpec {
        version: 6,
        data_codewords: 136,
        ec_codewords_per_block: 18,
        blocks: 2,
        align: &[6, 34],
    },
];

/// Encodes `payload` (up to 134 bytes) into rows of `'1'` / `'0'` modules.
pub fn encode_qr_matrix(payload: &str) -> Vec<String> {
    let bytes = payload.as_bytes();
    let Some(spec) = VERSIONS
        .iter()
        .find(|v| bytes.len() + 2 <= v.data_codewords)
    else {
        return Vec::new();
    };
    let data = build_codewords(bytes, spec);
    let size = spec.version * 4 + 17;
    let mut modules = vec![vec![false; size]; size];
    let mut is_func = vec![vec![false; size]; size];

    place_function_patterns(&mut modules, &mut is_func, spec, size);
    place_data_bits(&mut modules, &is_func, &data, size);

    // Choose lowest-penalty mask among all 8 standard masks (Level L = 0b01).
    let mut best_grid = modules.clone();
    let mut best_score = u32::MAX;
    for mask in 0..8u8 {
        let mut candidate = modules.clone();
        apply_mask(&mut candidate, &is_func, mask, size);
        draw_format_bits(&mut candidate, mask, size);
        let score = penalty_score(&candidate, size);
        if score < best_score {
            best_score = score;
            best_grid = candidate;
        }
    }
    best_grid
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|dark| if dark { '1' } else { '0' })
                .collect()
        })
        .collect()
}

fn build_codewords(bytes: &[u8], spec: &VersionSpec) -> Vec<u8> {
    let mut bits = Vec::with_capacity(spec.data_codewords * 8);
    let push_bits = |out: &mut Vec<bool>, val: usize, count: usize| {
        for i in (0..count).rev() {
            out.push(((val >> i) & 1) != 0);
        }
    };
    push_bits(&mut bits, 0b0100, 4); // Byte mode
    push_bits(&mut bits, bytes.len(), 8);
    for &b in bytes {
        push_bits(&mut bits, usize::from(b), 8);
    }
    let cap_bits = spec.data_codewords * 8;
    let term = (cap_bits - bits.len()).min(4);
    push_bits(&mut bits, 0, term);
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    let mut codewords = Vec::with_capacity(spec.data_codewords);
    for chunk in bits.as_chunks::<8>().0 {
        let mut byte = 0u8;
        for &bit in chunk {
            byte = (byte << 1) | u8::from(bit);
        }
        codewords.push(byte);
    }
    for pad in [0xECu8, 0x11u8].into_iter().cycle() {
        if codewords.len() >= spec.data_codewords {
            break;
        }
        codewords.push(pad);
    }
    if spec.blocks == 1 {
        let ec = reed_solomon(&codewords, spec.ec_codewords_per_block);
        codewords.extend_from_slice(&ec);
        codewords
    } else {
        let block_len = spec.data_codewords / spec.blocks;
        let mut data_blocks = Vec::with_capacity(spec.blocks);
        let mut ec_blocks = Vec::with_capacity(spec.blocks);
        for b in 0..spec.blocks {
            let slice = &codewords[b * block_len..(b + 1) * block_len];
            data_blocks.push(slice.to_vec());
            ec_blocks.push(reed_solomon(slice, spec.ec_codewords_per_block));
        }
        let mut interleaved = Vec::new();
        for i in 0..block_len {
            for blk in &data_blocks {
                interleaved.push(blk[i]);
            }
        }
        for i in 0..spec.ec_codewords_per_block {
            for blk in &ec_blocks {
                interleaved.push(blk[i]);
            }
        }
        interleaved
    }
}

fn gf_mul(mut x: u8, mut y: u8) -> u8 {
    let mut z = 0u8;
    for _ in 0..8 {
        if (y & 1) != 0 {
            z ^= x;
        }
        let hi = (x & 0x80) != 0;
        x <<= 1;
        if hi {
            x ^= 0x1D;
        }
        y >>= 1;
    }
    z
}

fn reed_solomon(data: &[u8], degree: usize) -> Vec<u8> {
    let mut generator = vec![0u8; degree];
    generator[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            generator[j] = gf_mul(generator[j], root);
            if j + 1 < degree {
                generator[j] ^= generator[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    let mut rem = vec![0u8; degree];
    for &byte in data {
        let factor = byte ^ rem[0];
        rem.rotate_left(1);
        rem[degree - 1] = 0;
        for (r, &g) in rem.iter_mut().zip(&generator) {
            *r ^= gf_mul(g, factor);
        }
    }
    rem
}

fn place_function_patterns(
    modules: &mut [Vec<bool>],
    is_func: &mut [Vec<bool>],
    spec: &VersionSpec,
    size: usize,
) {
    let set_fn = |m: &mut [Vec<bool>], f: &mut [Vec<bool>], x: usize, y: usize, dark: bool| {
        m[y][x] = dark;
        f[y][x] = true;
    };
    for i in 0..size {
        set_fn(modules, is_func, 6, i, i % 2 == 0);
        set_fn(modules, is_func, i, 6, i % 2 == 0);
    }
    for (ox, oy) in [
        (3isize, 3isize),
        (size as isize - 4, 3),
        (3, size as isize - 4),
    ] {
        for dy in -4..=4isize {
            for dx in -4..=4isize {
                let (x, y) = (ox + dx, oy + dy);
                if x >= 0 && y >= 0 && (x as usize) < size && (y as usize) < size {
                    let dist = dx.abs().max(dy.abs());
                    set_fn(
                        modules,
                        is_func,
                        x as usize,
                        y as usize,
                        dist != 2 && dist != 4,
                    );
                }
            }
        }
    }
    let n = spec.align.len();
    for (i, &cy) in spec.align.iter().enumerate() {
        for (j, &cx) in spec.align.iter().enumerate() {
            if (i == 0 && j == 0) || (i == 0 && j == n - 1) || (i == n - 1 && j == 0) {
                continue;
            }
            for dy in -2..=2isize {
                for dx in -2..=2isize {
                    let dist = dx.abs().max(dy.abs());
                    set_fn(
                        modules,
                        is_func,
                        (cx as isize + dx) as usize,
                        (cy as isize + dy) as usize,
                        dist != 1,
                    );
                }
            }
        }
    }
    // Reserve format information areas + dark module.
    for i in 0..=8 {
        if i != 6 {
            is_func[8][i] = true;
            is_func[i][8] = true;
        }
    }
    for i in 0..8 {
        is_func[8][size - 1 - i] = true;
        is_func[size - 1 - i][8] = true;
    }
    set_fn(modules, is_func, 8, size - 8, true);
}

fn place_data_bits(modules: &mut [Vec<bool>], is_func: &[Vec<bool>], data: &[u8], size: usize) {
    let mut bit_idx = 0usize;
    let total_bits = data.len() * 8;
    let mut right = size as isize - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        let upward = ((right + 1) & 2) == 0;
        for v in 0..size {
            let y = if upward { size - 1 - v } else { v };
            for j in 0..2 {
                let x = (right - j) as usize;
                if !is_func[y][x] {
                    let dark = if bit_idx < total_bits {
                        ((data[bit_idx / 8] >> (7 - (bit_idx % 8))) & 1) != 0
                    } else {
                        false
                    };
                    modules[y][x] = dark;
                    bit_idx += 1;
                }
            }
        }
        right -= 2;
    }
}

fn apply_mask(modules: &mut [Vec<bool>], is_func: &[Vec<bool>], mask: u8, size: usize) {
    for y in 0..size {
        for x in 0..size {
            if is_func[y][x] {
                continue;
            }
            let invert = match mask {
                0 => (x + y) % 2 == 0,
                1 => y % 2 == 0,
                2 => x % 3 == 0,
                3 => (x + y) % 3 == 0,
                4 => (y / 2 + x / 3) % 2 == 0,
                5 => (x * y) % 2 + (x * y) % 3 == 0,
                6 => ((x * y) % 2 + (x * y) % 3) % 2 == 0,
                _ => ((x + y) % 2 + (x * y) % 3) % 2 == 0,
            };
            if invert {
                modules[y][x] = !modules[y][x];
            }
        }
    }
}

fn draw_format_bits(modules: &mut [Vec<bool>], mask: u8, size: usize) {
    // ECC Level L = 0b01
    let data = (0b01u16 << 3) | u16::from(mask);
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    let bits = ((data << 10) | rem) ^ 0x5412;
    let bit = |i: usize| ((bits >> i) & 1) != 0;

    for (i, row) in modules.iter_mut().enumerate().take(6) {
        row[8] = bit(i);
    }
    modules[7][8] = bit(6);
    modules[8][8] = bit(7);
    modules[8][7] = bit(8);
    for i in 9..15 {
        modules[8][14 - i] = bit(i);
    }
    for i in 0..8 {
        modules[8][size - 1 - i] = bit(i);
    }
    for i in 8..15 {
        modules[size - 15 + i][8] = bit(i);
    }
    modules[size - 8][8] = true;
}

fn penalty_score(modules: &[Vec<bool>], size: usize) -> u32 {
    let mut score = 0u32;
    for row in modules.iter().take(size) {
        let mut run = 1usize;
        for x in 1..size {
            if row[x] == row[x - 1] {
                run += 1;
                if run == 5 {
                    score += 3;
                } else if run > 5 {
                    score += 1;
                }
            } else {
                run = 1;
            }
        }
    }
    for x in 0..size {
        let mut run = 1usize;
        for y in 1..size {
            if modules[y][x] == modules[y - 1][x] {
                run += 1;
                if run == 5 {
                    score += 3;
                } else if run > 5 {
                    score += 1;
                }
            } else {
                run = 1;
            }
        }
    }
    score
}
