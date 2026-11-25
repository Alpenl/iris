//! Pure-Rust RFC 1321 MD5, FIPS 180-4 SHA-1, Baidu PCS Locate v4 signing
//! (`devuid` + `rand`, aligned with `BaiduPCS-Rust` / `BaiduPCS-Go`), query encoding,
//! text sanitization, and HTTP body decompression.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

const LOCATE_MAGIC: &[u8] = b"ebrcUYiuxaZv2XGu7KIYKxUrqfnOfpDF";

/// RFC 3986 unreserved characters (`A-Z a-z 0-9 - _ . ~`), uppercase hex escapes.
const QUERY_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Strips control and Unicode bidi characters so host validation never rejects
/// remote Baidu Netdisk file names, folder paths, or error strings.
pub fn sanitize_text(raw: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut last_space = true;
    for ch in raw.chars() {
        if matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        let mapped = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if out.len() + mapped.len_utf8() > max_bytes {
            break;
        }
        out.push(mapped);
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

pub fn encode_query_component(raw: &str) -> String {
    utf8_percent_encode(raw, QUERY_ENCODE_SET).to_string()
}

pub fn build_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                encode_query_component(k),
                encode_query_component(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub fn md5_digest(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (idx, word) in [a0, b0, c0, d0].into_iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

pub fn sha1_digest(input: &[u8]) -> [u8; 20] {
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xefcdab89;
    let mut h2: u32 = 0x98badcfe;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xc3d2e1f0;

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdcu32),
                _ => (b ^ c ^ d, 0xca62c1d6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (idx, word) in [h0, h1, h2, h3, h4].into_iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// Baidu PCS device identifier (`UPPER(MD5(bduss)) + "|0"`).
pub fn generate_devuid(bduss: &str) -> String {
    format!("{}|0", hex_upper(&md5_digest(bduss.as_bytes())))
}

/// Baidu PCS `method=locatedownload` v4 request signature (`BaiduPCS-Go` / `BaiduPCS-Rust`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocateSign {
    pub time: u64,
    pub rand: String,
    pub devuid: String,
}

impl LocateSign {
    pub fn new(uid: u64, bduss: &str, now_secs: u64) -> Self {
        let devuid = generate_devuid(bduss);
        Self::with_time_and_devuid(now_secs, devuid, uid, bduss)
    }

    pub fn with_time_and_devuid(time: u64, devuid: String, uid: u64, bduss: &str) -> Self {
        let bduss_sha1 = hex_lower(&sha1_digest(bduss.as_bytes()));
        let mut payload = Vec::with_capacity(bduss_sha1.len() + 96);
        payload.extend_from_slice(bduss_sha1.as_bytes());
        payload.extend_from_slice(uid.to_string().as_bytes());
        payload.extend_from_slice(LOCATE_MAGIC);
        payload.extend_from_slice(time.to_string().as_bytes());
        payload.extend_from_slice(devuid.as_bytes());
        let rand = hex_lower(&sha1_digest(&payload));
        Self { time, rand, devuid }
    }

    pub fn url_params(&self) -> String {
        let encoded_devuid = encode_query_component(&self.devuid);
        format!(
            "time={}&rand={}&devuid={}&cuid={}",
            self.time, self.rand, encoded_devuid, encoded_devuid
        )
    }
}

/// Generates a Baidu passport `gid` GUID (`XXXXXXX-XXXX-4XXX-YXXX-XXXXXXXXXXXX`).
pub fn generate_gid(entropy: &[u8; 16]) -> String {
    let hex: Vec<u8> = entropy
        .iter()
        .flat_map(|&b| [b >> 4, b & 0x0F])
        .map(|n| b"0123456789ABCDEF"[usize::from(n)])
        .collect();
    let s = |a: usize, b: usize| std::str::from_utf8(&hex[a..b]).unwrap_or("0");
    let variant = b"89AB"[usize::from(entropy[8] & 3)] as char;
    format!(
        "{}-{}-4{}-{}{}-{}",
        s(0, 7),
        s(7, 11),
        s(11, 14),
        variant,
        s(14, 17),
        s(17, 29)
    )
}

/// Formats byte counts concisely for UI stats and file subtitles.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= TIB {
        format!("{:.2} TB", b / TIB)
    } else if b >= GIB {
        format!("{:.2} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Decodes `gzip` / `deflate` HTTP responses with strict output-size bounds.
pub fn decode_http_body(
    headers: &[(String, String)],
    body: &[u8],
    max_output: usize,
) -> Result<Vec<u8>, &'static str> {
    let encoding = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if encoding.is_empty() || encoding == "identity" {
        return Ok(body.to_vec());
    }
    if encoding == "gzip" || encoding == "x-gzip" {
        return decompress_gzip(body, max_output);
    }
    if encoding == "deflate" {
        return miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(body, max_output)
            .or_else(|_| miniz_oxide::inflate::decompress_to_vec_with_limit(body, max_output))
            .map_err(|_| "deflate decompression failed");
    }
    Err("unsupported content encoding")
}

fn decompress_gzip(input: &[u8], max_output: usize) -> Result<Vec<u8>, &'static str> {
    if input.len() < 18 || input[0] != 0x1F || input[1] != 0x8B || input[2] != 8 {
        return Err("invalid gzip header");
    }
    let flags = input[3];
    let mut pos = 10usize;
    if (flags & 0x04) != 0 {
        if pos + 2 > input.len() {
            return Err("truncated gzip extra field");
        }
        let xlen = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
        pos = pos.checked_add(2 + xlen).ok_or("gzip offset overflow")?;
    }
    for mask in [0x08u8, 0x10u8] {
        if (flags & mask) != 0 {
            let rel = input
                .get(pos..)
                .ok_or("truncated gzip string")?
                .iter()
                .position(|&b| b == 0)
                .ok_or("unterminated gzip string")?;
            pos = pos.checked_add(rel + 1).ok_or("gzip offset overflow")?;
        }
    }
    if (flags & 0x02) != 0 {
        pos = pos.checked_add(2).ok_or("gzip offset overflow")?;
    }
    if pos + 8 > input.len() {
        return Err("truncated gzip body");
    }
    let deflate_slice = &input[pos..input.len() - 8];
    miniz_oxide::inflate::decompress_to_vec_with_limit(deflate_slice, max_output)
        .map_err(|_| "gzip inflate failed")
}
