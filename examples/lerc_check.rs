//! Decode a LERC-compressed TIFF and compare against a raw reference produced
//! by GDAL (`gdal_translate -of ENVI`), for whichever sample type the file uses.
//!
//! Usage: cargo run --features lerc --example lerc_check -- <tile.tif> <gt.raw>

use std::env;
use std::fs::File;
use std::io::Read;

use tiff::decoder::{Decoder, DecodingResult};

fn as_f64(img: DecodingResult) -> Vec<f64> {
    match img {
        DecodingResult::U8(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::I8(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::I16(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::U32(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::F32(v) => v.into_iter().map(|x| x as f64).collect(),
        DecodingResult::F64(v) => v,
        other => panic!("unhandled decoding result {:?}", std::mem::discriminant(&other)),
    }
}

fn raw_as_f64(bytes: &[u8], type_size: usize, kind: &str) -> Vec<f64> {
    bytes
        .chunks_exact(type_size)
        .map(|c| match (kind, type_size) {
            ("u", 1) => c[0] as f64,
            ("i", 1) => c[0] as i8 as f64,
            ("u", 2) => u16::from_le_bytes([c[0], c[1]]) as f64,
            ("i", 2) => i16::from_le_bytes([c[0], c[1]]) as f64,
            ("u", 4) => u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64,
            ("i", 4) => i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64,
            ("f", 4) => f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64,
            ("f", 8) => f64::from_le_bytes([
                c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
            ]),
            _ => panic!("bad kind/size {kind}/{type_size}"),
        })
        .collect()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let tif = &args[1];
    let raw = &args[2];
    // kind/size describe how to read the raw reference; default f32.
    let kind = args.get(3).map(|s| s.as_str()).unwrap_or("f");
    let type_size: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4);

    let mut dec = Decoder::new(File::open(tif).unwrap()).unwrap();
    let got = as_f64(dec.read_image().unwrap());

    let mut gt_bytes = Vec::new();
    File::open(raw).unwrap().read_to_end(&mut gt_bytes).unwrap();
    let gt = raw_as_f64(&gt_bytes, type_size, kind);

    assert_eq!(got.len(), gt.len(), "length mismatch {} vs {}", got.len(), gt.len());

    let mut max_abs = 0.0f64;
    let mut worst = (0usize, 0.0f64, 0.0f64);
    for (i, (&a, &b)) in got.iter().zip(gt.iter()).enumerate() {
        let d = (a - b).abs();
        if d > max_abs {
            max_abs = d;
            worst = (i, a, b);
        }
    }

    println!("pixels: {}  max abs diff vs GDAL: {max_abs}", got.len());
    println!("worst: idx {} ours {} gdal {}", worst.0, worst.1, worst.2);
    if max_abs == 0.0 {
        println!("EXACT");
    } else {
        std::process::exit(1);
    }
}
