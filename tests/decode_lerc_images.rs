extern crate tiff;

use tiff::decoder::{Decoder, DecodingSampleBuffer};
use tiff::ColorType;

use std::fs::File;
use std::path::PathBuf;

const TEST_IMAGE_DIR: &str = "./tests/images/";
const WIDTH: usize = 73;
const HEIGHT: usize = 47;
const HW: usize = WIDTH / 2; // 36
const HH: usize = HEIGHT / 2; // 23

/// 73x47 image split into quadrants:
/// - Top-left (36x23): horizontal ramp, pixel[y][x] = (x * 255) / 35
/// - Top-right (37x23): vertical ramp, pixel[y][x] = (y * 255) / 22
/// - Bottom-left (36x24): 4x4 checkerboard (255 or 0)
/// - Bottom-right (37x24): constant 200
///
/// Test images generated with:
///   # write 73x47 raw u8 image with the pattern from expected_u8_pixel() below
///   # then create an ENVI .hdr alongside it (data type = 1, bsq)
///   gdal_translate -of GTiff -co COMPRESS=LERC -co MAX_Z_ERROR=0 pattern.dat lerc-u8-73x47.tiff
///   gdal_translate -of GTiff -co COMPRESS=LERC_DEFLATE -co MAX_Z_ERROR=0 pattern.dat lerc-deflate-u8-73x47.tiff
///   # same but with data type = 4 (float32) in the .hdr
///   gdal_translate -of GTiff -co COMPRESS=LERC -co MAX_Z_ERROR=0 pattern_f32.dat lerc-f32-73x47.tiff
///   gdal_translate -of GTiff -co COMPRESS=LERC -co MAX_Z_ERROR=0.1 pattern_f32.dat lerc-f32-lossy-73x47.tiff
fn expected_u8_pixel(x: usize, y: usize) -> u8 {
    if y < HH && x < HW {
        ((x * 255) / (HW - 1)) as u8
    } else if y < HH && x >= HW {
        ((y * 255) / (HH - 1)) as u8
    } else if y >= HH && x < HW {
        if ((x / 4) + (y / 4)) % 2 == 0 {
            255
        } else {
            0
        }
    } else {
        200
    }
}

fn verify_u8_pattern(data: &[u8]) {
    assert_eq!(data.len(), WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let got = data[y * WIDTH + x];
            let expected = expected_u8_pixel(x, y);
            assert_eq!(
                got, expected,
                "mismatch at ({x}, {y}): got {got}, expected {expected}"
            );
        }
    }
}

#[test]
fn test_lerc_u8() {
    let path = PathBuf::from(TEST_IMAGE_DIR).join("lerc-u8-73x47.tiff");
    let file = File::open(&path).unwrap();
    let mut decoder = Decoder::new(file).unwrap();

    assert_eq!(decoder.dimensions().unwrap(), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(decoder.colortype().unwrap(), ColorType::Gray(8));

    let data = match decoder.read_image().unwrap() {
        DecodingSampleBuffer::U8(d) => d,
        _ => panic!("expected U8"),
    };
    verify_u8_pattern(&data);
}

#[test]
fn test_lerc_deflate_u8() {
    let path = PathBuf::from(TEST_IMAGE_DIR).join("lerc-deflate-u8-73x47.tiff");
    let file = File::open(&path).unwrap();
    let mut decoder = Decoder::new(file).unwrap();

    assert_eq!(decoder.dimensions().unwrap(), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(decoder.colortype().unwrap(), ColorType::Gray(8));

    let data = match decoder.read_image().unwrap() {
        DecodingSampleBuffer::U8(d) => d,
        _ => panic!("expected U8"),
    };
    verify_u8_pattern(&data);
}

#[test]
fn test_lerc_f32() {
    let path = PathBuf::from(TEST_IMAGE_DIR).join("lerc-f32-73x47.tiff");
    let file = File::open(&path).unwrap();
    let mut decoder = Decoder::new(file).unwrap();

    assert_eq!(decoder.dimensions().unwrap(), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(decoder.colortype().unwrap(), ColorType::Gray(32));

    let data = match decoder.read_image().unwrap() {
        DecodingSampleBuffer::F32(d) => d,
        _ => panic!("expected F32"),
    };
    assert_eq!(data.len(), WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let got = data[y * WIDTH + x];
            let expected = expected_u8_pixel(x, y) as f32;
            assert_eq!(
                got, expected,
                "mismatch at ({x}, {y}): got {got}, expected {expected}"
            );
        }
    }
}

#[test]
fn test_lerc_f32_lossy() {
    let path = PathBuf::from(TEST_IMAGE_DIR).join("lerc-f32-lossy-73x47.tiff");
    let file = File::open(&path).unwrap();
    let mut decoder = Decoder::new(file).unwrap();

    assert_eq!(decoder.dimensions().unwrap(), (WIDTH as u32, HEIGHT as u32));
    assert_eq!(decoder.colortype().unwrap(), ColorType::Gray(32));

    let data = match decoder.read_image().unwrap() {
        DecodingSampleBuffer::F32(d) => d,
        _ => panic!("expected F32"),
    };
    assert_eq!(data.len(), WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let got = data[y * WIDTH + x];
            let expected = expected_u8_pixel(x, y) as f32;
            assert!(
                (got - expected).abs() <= 0.1,
                "at ({x}, {y}): got {got}, expected {expected}, diff {}",
                (got - expected).abs()
            );
        }
    }
}
