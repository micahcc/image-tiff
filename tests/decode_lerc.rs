//! Decode tests for the LERC codec (compression tag 34887), behind the `lerc`
//! feature. Fixtures were produced with ImageMagick, e.g.:
//!   magick -size 64x64 gradient: -depth 16 -define quantum:format=unsigned \
//!       lerc-gradient-1c-16b-lossless.tif
//!   magick lerc-gradient-1c-16b-lossless.tif -depth 16 \
//!       -define quantum:format=unsigned -compress LERC lerc-gradient-1c-16b.tif
//! The `-lossless` variant is the same pixels stored uncompressed; since these
//! blobs use maxZError 0 the LERC decode must reproduce them exactly.
#![cfg(feature = "lerc")]

extern crate tiff;

use tiff::decoder::{Decoder, DecodingSampleBuffer};
use tiff::ColorType;

use std::fs::File;
use std::path::PathBuf;

const TEST_IMAGE_DIR: &str = "./tests/images/";

fn decode(file: &str) -> (ColorType, DecodingSampleBuffer) {
    let path = PathBuf::from(TEST_IMAGE_DIR).join(file);
    let img_file = File::open(path).expect("Cannot find test image!");
    let mut decoder = Decoder::open(img_file).expect("Cannot create decoder");
    decoder.next_image().expect("Cannot read image IFD");
    let ct = decoder.colortype().unwrap();
    let img = decoder.read_image().unwrap();
    (ct, img)
}

fn u16_sum(file: &str, expected_type: ColorType, expected_sum: u64) {
    let (ct, img) = decode(file);
    assert_eq!(ct, expected_type);
    match img {
        DecodingSampleBuffer::U16(res) => {
            let sum: u64 = res.into_iter().map(u64::from).sum();
            assert_eq!(sum, expected_sum);
        }
        _ => panic!("expected U16 samples"),
    }
}

/// Assert a LERC file decodes to exactly the same samples as its uncompressed
/// twin (both u16). Compares the raw sample vectors, not a lossy statistic.
fn assert_lerc_matches_lossless_u16(lerc: &str, lossless: &str) {
    let (cta, a) = decode(lerc);
    let (ctb, b) = decode(lossless);
    assert_eq!(cta, ctb);
    match (a, b) {
        (DecodingSampleBuffer::U16(x), DecodingSampleBuffer::U16(y)) => assert_eq!(x, y),
        _ => panic!("expected U16 samples"),
    }
}

#[test]
fn lerc_gradient_u16() {
    u16_sum("lerc-gradient-1c-16b.tif", ColorType::Gray(16), 134215680);
    assert_lerc_matches_lossless_u16(
        "lerc-gradient-1c-16b.tif",
        "lerc-gradient-1c-16b-lossless.tif",
    );
}

#[test]
fn lerc_plasma_u16() {
    u16_sum("lerc-plasma-1c-16b.tif", ColorType::Gray(16), 76364475);
    assert_lerc_matches_lossless_u16("lerc-plasma-1c-16b.tif", "lerc-plasma-1c-16b-lossless.tif");
}

#[test]
fn lerc_checker_u16() {
    u16_sum("lerc-checker-1c-16b.tif", ColorType::Gray(16), 161058816);
    assert_lerc_matches_lossless_u16(
        "lerc-checker-1c-16b.tif",
        "lerc-checker-1c-16b-lossless.tif",
    );
}

#[test]
fn lerc_gradient_f32() {
    // Float path (the DEM use case). Compare against the uncompressed twin
    // rather than a float sum so accumulation order is irrelevant.
    let (cta, a) = decode("lerc-gradient-1c-32b-float.tif");
    let (ctb, b) = decode("lerc-gradient-1c-32b-float-lossless.tif");
    assert_eq!(cta, ColorType::Gray(32));
    assert_eq!(cta, ctb);
    match (a, b) {
        (DecodingSampleBuffer::F32(x), DecodingSampleBuffer::F32(y)) => assert_eq!(x, y),
        _ => panic!("expected F32 samples"),
    }
}
