use super::{
    HttpCancel, MAX_BYTES, MAX_RANGE, MAX_REQUESTS, PREFIX, Photo, Sparse, detect_photo_format,
    finish_header, probe_remote, run_probe,
};
use crate::photo::probe_bytes;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

fn entry(tag: u16, ty: u16, count: u32, value: u32) -> Vec<u8> {
    [
        &tag.to_le_bytes()[..],
        &ty.to_le_bytes(),
        &count.to_le_bytes(),
        &value.to_le_bytes(),
    ]
    .concat()
}

fn small_exif() -> Vec<u8> {
    let mut data = b"II*\0\x08\0\0\0\x01\0".to_vec();
    data.extend(entry(0x0110, 2, 4, u32::from_le_bytes(*b"ABC\0")));
    data.extend([0; 4]);
    data
}

fn p3_profile() -> Vec<u8> {
    let mut b = vec![0; 132 + 12 * 6];
    b[16..20].copy_from_slice(b"RGB ");
    b[36..40].copy_from_slice(b"acsp");
    b[128..132].copy_from_slice(&6u32.to_be_bytes());
    let xyz = [(0.682, 0.3193), (0.2846, 0.6746), (0.1559, 0.0661)];
    for (i, sig) in [b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"]
        .into_iter()
        .enumerate()
    {
        let pos = b.len() as u32;
        let tag = 132 + 12 * i;
        b[tag..tag + 4].copy_from_slice(sig);
        b[tag + 4..tag + 8].copy_from_slice(&pos.to_be_bytes());
        if i < 3 {
            b[tag + 8..tag + 12].copy_from_slice(&20u32.to_be_bytes());
            b.extend(b"XYZ \0\0\0\0");
            let (x, y) = xyz[i];
            for v in [x, y, 1.0 - x - y] {
                b.extend(((v * 65536.0) as i32).to_be_bytes());
            }
        } else {
            b[tag + 8..tag + 12].copy_from_slice(&12u32.to_be_bytes());
            b.extend(b"curv\0\0\0\0\0\0\0\0");
        }
    }
    let len = b.len() as u32;
    b[..4].copy_from_slice(&len.to_be_bytes());
    b
}

fn sparse_photo(data: &[u8]) -> (Photo, Vec<(u64, usize)>) {
    let mut reads = Vec::new();
    let cancel = HttpCancel::default();
    let mut r = Sparse::open(
        |pos, len| {
            let end = (pos as usize + len).min(data.len());
            let body = &data[pos as usize..end];
            reads.push((pos, body.len()));
            Ok((body.to_vec(), Some(data.len() as u64)))
        },
        &cancel,
    )
    .unwrap();
    let format = detect_photo_format(&r.windows[0].1, None).unwrap();
    let photo = finish_header(run_probe(&mut r, format), format);
    (photo, reads)
}

#[test]
fn late_tiff_directories_and_disjoint_icc_use_bounded_ranges() {
    let first = 600_000u32;
    let sub = 1_100_000u32;
    let profile_pos = 2_000_000u32;
    let profile = p3_profile();
    let mut data = b"II*\0".to_vec();
    data.extend(first.to_le_bytes());
    data.resize(first as usize, 0);
    data.extend(5u16.to_le_bytes());
    data.extend(entry(256, 4, 1, 900));
    data.extend(entry(257, 3, 1, 300));
    data.extend(entry(0x0110, 2, 4, u32::from_le_bytes(*b"ABC\0")));
    data.extend(entry(0x8769, 4, 1, sub));
    data.extend(entry(34675, 7, profile.len() as u32, profile_pos));
    data.resize(sub as usize, 0);
    data.extend(1u16.to_le_bytes());
    data.extend(entry(0x8827, 3, 1, 400));
    data.resize(profile_pos as usize, 0);
    data.extend(profile);
    let (photo, reads) = sparse_photo(&data);
    assert_eq!(photo, probe_bytes(&data, None));
    assert_eq!(photo.size, Some((900, 300)));
    assert_eq!(photo.colour.unwrap().primaries, "display-p3");
    let exif = photo.exif.unwrap();
    assert_eq!(exif.camera.as_deref(), Some("ABC"));
    assert_eq!(exif.iso, Some(400));
    assert_eq!(
        reads.iter().map(|r| r.0).collect::<Vec<_>>(),
        [0, u64::from(first), u64::from(sub), u64::from(profile_pos)]
    );
    assert!(reads.iter().map(|r| r.1).sum::<usize>() < 400_000);
}

#[test]
fn bigtiff_seeks_beyond_four_gibibytes() {
    let offset = u64::from(u32::MAX) + 100;
    let mut first = b"MM\0+\0\x08\0\0".to_vec();
    first.extend(offset.to_be_bytes());
    first.resize(PREFIX, 0);
    let mut tail = 2u64.to_be_bytes().to_vec();
    for (tag, value) in [(256u16, 1200u64), (257, 800)] {
        tail.extend(tag.to_be_bytes());
        tail.extend(16u16.to_be_bytes());
        tail.extend(1u64.to_be_bytes());
        tail.extend(value.to_be_bytes());
    }
    let total = offset + tail.len() as u64;
    let cancel = HttpCancel::default();
    let mut r = Sparse::open(
        |pos, _| {
            if pos == 0 {
                Ok((first.clone(), Some(total)))
            } else {
                assert_eq!(pos, offset);
                Ok((tail.clone(), Some(total)))
            }
        },
        &cancel,
    )
    .unwrap();
    let fmt = detect_photo_format(&first, None).unwrap();
    assert_eq!(run_probe(&mut r, fmt).size, Some((1200, 800)));
}

fn atom(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    [&((8 + data.len()) as u32).to_be_bytes()[..], kind, data].concat()
}

fn heif_image(idat: bool) -> Vec<u8> {
    let mut f = atom(b"ftyp", b"mif1\0\0\0\0mif1");
    let mut item = b"\0\0\0\x06Exif\0\0".to_vec();
    item.extend(small_exif());
    let mut storage = vec![0; 800_000];
    let ends = [(400_000usize, &item[..15]), (600_000, &item[15..])];
    let origin = if idat { 0 } else { f.len() + 8 };
    let mut loc = vec![
        1,
        0,
        0,
        0,
        0x44,
        0,
        0,
        1,
        0,
        7,
        0,
        u8::from(idat),
        0,
        0,
        0,
        2,
    ];
    for (off, bytes) in ends {
        storage[off..off + bytes.len()].copy_from_slice(bytes);
        loc.extend(((origin + off) as u32).to_be_bytes());
        loc.extend((bytes.len() as u32).to_be_bytes());
    }
    if !idat {
        f.extend(atom(b"mdat", &storage));
    }
    let mut meta = vec![0; 4];
    meta.extend(atom(b"pitm", &[0, 0, 0, 0, 0, 3]));
    let mut iinf = vec![0, 0, 0, 0, 0, 3];
    iinf.extend(atom(b"infe", &[2, 0]));
    for id in [6, 7] {
        let mut info = vec![2, 0, 0, 0, 0, id, 0, 0];
        info.extend(b"Exif\0");
        iinf.extend(atom(b"infe", &info));
    }
    meta.extend(atom(b"iinf", &iinf));
    let mut iref = vec![0; 4];
    iref.extend(atom(b"cdsc", &[0, 6, 0, 1, 0, 4]));
    iref.extend(atom(b"cdsc", &[0, 7, 0, 1, 0, 3]));
    meta.extend(atom(b"iref", &iref));
    let mut ipco = atom(b"ispe", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2]);
    ipco.extend(atom(b"ispe", &[0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2, 0]));
    ipco.extend(atom(b"irot", &[3]));
    ipco.extend(atom(b"imir", &[0]));
    let mut iprp = atom(b"ipco", &ipco);
    iprp.extend(atom(b"ipma", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 3, 3, 2, 3, 4]));
    meta.extend(atom(b"iprp", &iprp));
    meta.extend(atom(b"iloc", &loc));
    if idat {
        meta.extend(atom(b"idat", &storage));
    }
    f.extend(atom(b"meta", &meta));
    f
}

#[test]
fn heif_primary_properties_and_split_exif_extents_from_mdat_and_idat() {
    for idat in [false, true] {
        let bytes = heif_image(idat);
        let (photo, reads) = sparse_photo(&bytes);
        assert_eq!(photo, probe_bytes(&bytes, None));
        assert_eq!(photo.size, Some((768, 512)));
        assert_eq!(photo.rotate_cw, 270);
        assert_eq!(photo.mirror, Some(crate::photo::Mirror::Vertical));
        assert_eq!(photo.exif.unwrap().camera.as_deref(), Some("ABC"));
        assert!(reads.iter().map(|r| r.1).sum::<usize>() < bytes.len() / 2);
    }
}

fn webp_image() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend(kind);
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    }
    let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
    chunk(&mut b, b"VP8X", &[0x28, 0, 0, 0, 63, 0, 0, 31, 0, 0]);
    chunk(&mut b, b"ICCP", &p3_profile());
    chunk(&mut b, b"VP8 ", &vec![0; 799_999]);
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend(small_exif());
    chunk(&mut b, b"EXIF", &exif);
    let size = (b.len() - 8) as u32;
    b[4..8].copy_from_slice(&size.to_le_bytes());
    b
}

#[test]
fn remote_webp_reads_tail_exif_and_preserves_size_and_icc_on_partial_responses() {
    enum Server {
        Ranges,
        Prefix,
        WrongRange,
    }
    for mode in [Server::Ranges, Server::Prefix, Server::WrongRange] {
        let image = webp_image();
        let expected = probe_bytes(&image, None);
        let ranges = matches!(mode, Server::Ranges);
        let prefix = matches!(mode, Server::Prefix);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut count = 0;
            loop {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&socket);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let mut requested = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("range")
                    {
                        let (lo, hi) = value
                            .trim()
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        requested =
                            Some((lo.parse::<usize>().unwrap(), hi.parse::<usize>().unwrap()));
                    }
                }
                let (lo, hi) = requested.unwrap();
                count += 1;
                if prefix {
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        image.len()
                    )
                    .unwrap();
                    let _ = socket.write_all(&image);
                } else {
                    let hi = hi.min(image.len() - 1);
                    let echoed_lo = if !ranges && lo != 0 { lo - 1 } else { lo };
                    write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {echoed_lo}-{hi}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", image.len(), hi+1-lo).unwrap();
                    let _ = socket.write_all(&image[lo..=hi]);
                }
            }
            count
        });
        let photo = probe_remote(
            &format!("http://{addr}/item"),
            &BTreeMap::new(),
            None,
            &HttpCancel::default(),
        );
        drop(TcpStream::connect(addr).unwrap());
        assert_eq!(server.join().unwrap(), if prefix { 1 } else { 2 });
        assert_eq!(photo.size, Some((64, 32)));
        assert_eq!(photo.colour.unwrap().primaries, "display-p3");
        if ranges {
            assert_eq!(photo, expected);
            assert_eq!(photo.exif.unwrap().camera.as_deref(), Some("ABC"));
        } else {
            assert!(photo.exif.is_none());
        }
    }
}

#[test]
fn sparse_reader_enforces_both_budgets_cancellation_and_consistent_lengths() {
    let end = 1u64 << 32;
    for large_reads in [false, true] {
        let cancel = HttpCancel::default();
        let (mut bytes, mut calls) = (0, 0);
        let mut reader = Sparse::open(
            |_, n| {
                bytes += n;
                calls += 1;
                Ok((vec![0; n], Some(end)))
            },
            &cancel,
        )
        .unwrap();
        let mut buf = vec![0; if large_reads { MAX_RANGE } else { 1 }];
        for i in 1..32 {
            reader.seek(SeekFrom::Start(i * 2_000_000)).unwrap();
            if reader.read(&mut buf).is_err() {
                break;
            }
        }
        assert!(bytes <= MAX_BYTES && calls <= MAX_REQUESTS);
        if large_reads {
            assert_eq!(bytes, MAX_BYTES);
        } else {
            assert_eq!(calls, MAX_REQUESTS);
        }
    }
    let cancel = HttpCancel::default();
    let mut calls = 0;
    let mut reader = Sparse::open(
        |pos, n| {
            calls += 1;
            Ok((vec![0; n], Some(end + u64::from(pos != 0))))
        },
        &cancel,
    )
    .unwrap();
    reader.seek(SeekFrom::Start(500_000)).unwrap();
    assert!(reader.read_exact(&mut [0]).is_err());
    assert!(reader.read_exact(&mut [0]).is_err());
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_exact(&mut [0]).unwrap();
    cancel.cancel();
    assert!(reader.read_exact(&mut [0]).is_err());
    assert_eq!(calls, 2);
    assert!(Sparse::open(|_, _| panic!("cancelled before first request"), &cancel).is_none());
}

#[test]
fn jpeg_requires_all_icc_segments_and_accepts_out_of_order_segments() {
    fn segment(out: &mut Vec<u8>, seq: u8, data: &[u8]) {
        out.extend([0xff, 0xe2]);
        out.extend(((data.len() + 16) as u16).to_be_bytes());
        out.extend(b"ICC_PROFILE\0");
        out.extend([seq, 2]);
        out.extend(data);
    }
    let profile = p3_profile();
    let mut incomplete = vec![0xff, 0xd8];
    // A usable ICC header in a segment advertising a missing continuation is still incomplete.
    segment(&mut incomplete, 1, &profile);
    assert!(probe_bytes(&incomplete, None).colour.is_none());
    let mut complete = vec![0xff, 0xd8];
    segment(&mut complete, 2, &profile[100..]);
    segment(&mut complete, 1, &profile[..100]);
    assert_eq!(
        probe_bytes(&complete, None).colour.unwrap().primaries,
        "display-p3"
    );
}

#[test]
fn heif_prefix_keeps_complete_primary_properties() {
    let data = heif_image(true);
    let photo = probe_bytes(&data[..PREFIX], None);
    assert_eq!(photo.size, Some((768, 512)));
    assert_eq!(photo.rotate_cw, 270);
    assert!(photo.exif.is_none());
}

#[test]
fn sparse_reads_short_contiguous_replies() {
    let bytes = b"abcdefghi";
    let cancel = HttpCancel::default();
    let mut reader = Sparse::open(
        |start, size| {
            let start = start as usize;
            let end = (start + size.min(3)).min(bytes.len());
            Ok((bytes[start..end].to_vec(), Some(bytes.len() as u64)))
        },
        &cancel,
    )
    .unwrap();
    let mut found = Vec::new();
    reader.read_to_end(&mut found).unwrap();
    assert_eq!(found, bytes);
}

#[test]
fn jpeg_fill_bytes_do_not_count_as_markers() {
    let mut data = vec![0xff, 0xd8];
    data.extend(std::iter::repeat_n(0xff, 5000));
    data.extend([0xc0, 0, 7, 8, 0, 9, 0, 16, 0xff, 0xd9]);
    assert_eq!(probe_bytes(&data, None).size, Some((16, 9)));
}

#[test]
fn reads_long_exif_iso_value() {
    let mut data = b"II*\0\x08\0\0\0\x01\0".to_vec();
    data.extend(entry(0x8769, 4, 1, 26));
    data.extend([0; 4]);
    data.extend(1u16.to_le_bytes());
    data.extend(entry(0x8827, 4, 1, 102_400));
    data.extend([0; 4]);
    assert_eq!(
        probe_bytes(&data, None).exif.and_then(|exif| exif.iso),
        Some(102_400)
    );
}
