//! Pure-Rust decoder for the LERC (Limited Error Raster Compression) codec,
//! TIFF compression tag 34887.
//!
//! This is a port of the decode path of Esri's reference implementation
//! (<https://github.com/Esri/lerc>, Apache-2.0), covering the LERC2 bitstream
//! (magic `"Lerc2 "`, versions 1-6). The much older LERC1 format (`"CntZImage"`)
//! and the Huffman-coded paths (byte data at maxZError 0.5, lossless float) are
//! not implemented and return an error.
//!
//! A LERC TIFF tile is an optionally "additionally compressed" wrapper around a
//! LERC2 blob: GDAL's `LERC_ZSTD`/`LERC_DEFLATE` apply zstd/zlib to the whole
//! LERC2 blob. We sniff the wrapper from the first bytes and undo it before
//! decoding the LERC2 stream itself.
//!
//! The decoder emits raw pixel bytes in **native** byte order (LERC stores and
//! reconstructs values as machine-native words), so callers should treat the
//! decompressed stream as host-endian and skip the TIFF endianness fixup.

use std::io::{self, Cursor, Read};

use crate::error::{TiffError, TiffResult, TiffUnsupportedError};
use crate::tags::CompressionMethod;

/// LERC2 sample data types, matching Esri's `Lerc2::DataType` enum order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DataType {
    Char = 0,
    Byte = 1,
    Short = 2,
    UShort = 3,
    Int = 4,
    UInt = 5,
    Float = 6,
    Double = 7,
}

impl DataType {
    fn from_i32(v: i32) -> Option<DataType> {
        Some(match v {
            0 => DataType::Char,
            1 => DataType::Byte,
            2 => DataType::Short,
            3 => DataType::UShort,
            4 => DataType::Int,
            5 => DataType::UInt,
            6 => DataType::Float,
            7 => DataType::Double,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            DataType::Char | DataType::Byte => 1,
            DataType::Short | DataType::UShort => 2,
            DataType::Int | DataType::UInt | DataType::Float => 4,
            DataType::Double => 8,
        }
    }
}

/// `Lerc2::GetDataTypeUsed` -- the reduced storage type used for a tile's offset
/// value, selected by the 2-bit `bits67` code in the tile's compression flag.
fn get_data_type_used(dt: DataType, tc: i32) -> Option<DataType> {
    let validate = |v: i32| DataType::from_i32(v);
    match dt {
        DataType::Short | DataType::Int => validate(dt as i32 - tc),
        DataType::UShort | DataType::UInt => validate(dt as i32 - 2 * tc),
        DataType::Float => Some(if tc == 0 {
            dt
        } else if tc == 1 {
            DataType::Short
        } else {
            DataType::Byte
        }),
        DataType::Double => {
            if tc == 0 {
                Some(dt)
            } else {
                validate(dt as i32 - 2 * tc + 1)
            }
        }
        _ => Some(dt),
    }
}

/// A little-endian byte cursor mirroring the reference's `(ppByte, nBytesRemaining)`
/// pair. LERC blobs are always stored little-endian.
struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        ByteReader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn need(&self, n: usize) -> TiffResult<()> {
        if self.remaining() < n {
            return Err(lerc_err("unexpected end of LERC blob"));
        }
        Ok(())
    }

    fn read_u8(&mut self) -> TiffResult<u8> {
        self.need(1)?;
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_bytes(&mut self, n: usize) -> TiffResult<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn read_i32(&mut self) -> TiffResult<i32> {
        let s = self.read_bytes(4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn read_u32(&mut self) -> TiffResult<u32> {
        let s = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn read_f64(&mut self) -> TiffResult<f64> {
        let s = self.read_bytes(8)?;
        Ok(f64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    /// `Lerc2::ReadVariableDataType`: read one value of the given storage type,
    /// returned as f64 (the reference widens everything to double here).
    fn read_variable(&mut self, dt: DataType) -> TiffResult<f64> {
        Ok(match dt {
            DataType::Char => self.read_u8()? as i8 as f64,
            DataType::Byte => self.read_u8()? as f64,
            DataType::Short => {
                let s = self.read_bytes(2)?;
                i16::from_le_bytes([s[0], s[1]]) as f64
            }
            DataType::UShort => {
                let s = self.read_bytes(2)?;
                u16::from_le_bytes([s[0], s[1]]) as f64
            }
            DataType::Int => self.read_i32()? as f64,
            DataType::UInt => self.read_u32()? as f64,
            DataType::Float => f32::from_bits(self.read_u32()?) as f64,
            DataType::Double => self.read_f64()?,
        })
    }
}

fn lerc_err(msg: &str) -> TiffError {
    TiffError::IoError(io::Error::new(io::ErrorKind::InvalidData, msg.to_string()))
}

/// Parsed LERC2 header (`Lerc2::HeaderInfo`), only the fields the decoder needs.
struct HeaderInfo {
    version: i32,
    n_rows: i32,
    n_cols: i32,
    n_depth: i32,
    num_valid_pixel: i32,
    micro_block_size: i32,
    blob_size: i32,
    dt: DataType,
    max_z_error: f64,
    z_min: f64,
    z_max: f64,
}

/// A validity mask over the tile pixels; bit k (MSB-first within each byte)
/// is 1 when pixel k is valid. Matches Esri's `BitMask`.
struct BitMask {
    bits: Vec<u8>,
    all_valid: bool,
}

impl BitMask {
    fn all_valid() -> BitMask {
        BitMask {
            bits: Vec::new(),
            all_valid: true,
        }
    }

    fn is_valid(&self, k: i64) -> bool {
        if self.all_valid {
            return true;
        }
        let byte = self.bits[(k >> 3) as usize];
        let bit = (1u8 << 7) >> (k & 7);
        (byte & bit) != 0
    }
}

/// LERC2 decoder state, carrying the header and per-depth min/max ranges read
/// for version >= 4.
struct Lerc2Decoder {
    hd: HeaderInfo,
    mask: BitMask,
    z_min_vec: Vec<f64>,
    z_max_vec: Vec<f64>,
}

const LERC2_MAGIC: &[u8] = b"Lerc2 ";
const LERC1_MAGIC: &[u8] = b"CntZImage";
const LERC2_CURRENT_VERSION: i32 = 6;

impl Lerc2Decoder {
    fn read_header(r: &mut ByteReader) -> TiffResult<HeaderInfo> {
        let key = r.read_bytes(LERC2_MAGIC.len())?;
        if key != LERC2_MAGIC {
            return Err(lerc_err("not a LERC2 blob"));
        }

        let version = r.read_i32()?;
        if !(0..=LERC2_CURRENT_VERSION).contains(&version) {
            return Err(lerc_err("unsupported LERC2 version"));
        }

        if version >= 3 {
            // checksum -- read past it; we don't verify (the payload we get
            // from the TIFF layer has already been length-bounded).
            let _checksum = r.read_u32()?;
        }

        let n_rows = r.read_i32()?;
        let n_cols = r.read_i32()?;
        let n_depth = if version >= 4 { r.read_i32()? } else { 1 };
        let num_valid_pixel = r.read_i32()?;
        let micro_block_size = r.read_i32()?;
        let blob_size = r.read_i32()?;
        let dt_raw = r.read_i32()?;

        if version >= 6 {
            let _n_blobs_more = r.read_i32()?;
            // bPassNoDataValues, bIsInt, bReserved3, bReserved4
            let _ = r.read_bytes(4)?;
        }

        let max_z_error = r.read_f64()?;
        let z_min = r.read_f64()?;
        let z_max = r.read_f64()?;

        if version >= 6 {
            let _no_data_val = r.read_f64()?;
            let _no_data_val_orig = r.read_f64()?;
        }

        let dt = DataType::from_i32(dt_raw).ok_or_else(|| lerc_err("bad LERC2 data type"))?;

        if n_rows <= 0
            || n_cols <= 0
            || n_depth <= 0
            || num_valid_pixel < 0
            || micro_block_size <= 0
            || micro_block_size > 32
            || blob_size <= 0
        {
            return Err(lerc_err("invalid LERC2 header dimensions"));
        }

        let num_pixel = n_rows as i64 * n_cols as i64;
        if num_pixel > i32::MAX as i64 || num_valid_pixel as i64 > num_pixel {
            return Err(lerc_err("invalid LERC2 pixel count"));
        }

        Ok(HeaderInfo {
            version,
            n_rows,
            n_cols,
            n_depth,
            num_valid_pixel,
            micro_block_size,
            blob_size,
            dt,
            max_z_error,
            z_min,
            z_max,
        })
    }

    /// `Lerc2::ReadMask`: an int byte-count, then (if > 0) an RLE-compressed
    /// MSB-first bit mask.
    fn read_mask(&mut self, r: &mut ByteReader) -> TiffResult<()> {
        let num_valid = self.hd.num_valid_pixel;
        let w = self.hd.n_cols;
        let h = self.hd.n_rows;
        let num_total = w * h;

        let num_bytes_mask = r.read_i32()?;
        if num_bytes_mask < 0 {
            return Err(lerc_err("negative mask byte count"));
        }

        if (num_valid == 0 || num_valid == num_total) && num_bytes_mask != 0 {
            return Err(lerc_err("mask stored for all/none-valid tile"));
        }

        let mask_size = ((num_total as i64 + 7) >> 3) as usize;

        if num_valid == 0 {
            self.mask = BitMask {
                bits: vec![0u8; mask_size],
                all_valid: false,
            };
        } else if num_valid == num_total {
            self.mask = BitMask::all_valid();
        } else if num_bytes_mask > 0 {
            let rle = r.read_bytes(num_bytes_mask as usize)?;
            let mut bits = vec![0u8; mask_size];
            rle_decompress(rle, &mut bits)?;
            self.mask = BitMask {
                bits,
                all_valid: false,
            };
        } else {
            // num_bytes_mask == 0 with a partially-valid tile: reuse previous
            // mask. We have none across tiles (single blob), so treat as error.
            return Err(lerc_err("missing LERC2 mask"));
        }

        Ok(())
    }

    /// `Lerc2::ReadMinMaxRanges` (version >= 4): nDepth zMin values then nDepth
    /// zMax values, each in the header data type.
    fn read_min_max_ranges(&mut self, r: &mut ByteReader) -> TiffResult<()> {
        let n = self.hd.n_depth as usize;
        let dt = self.hd.dt;
        self.z_min_vec = (0..n).map(|_| r.read_variable(dt)).collect::<TiffResult<_>>()?;
        self.z_max_vec = (0..n).map(|_| r.read_variable(dt)).collect::<TiffResult<_>>()?;
        Ok(())
    }

    fn check_min_max_equal(&self) -> bool {
        self.z_min_vec == self.z_max_vec
    }

    /// Fill every valid pixel with the constant `zMin` (`Lerc2::FillConstImage`).
    fn fill_const_image(&self, out: &mut [u8]) {
        let hd = &self.hd;
        let n_depth = hd.n_depth as usize;
        let per_depth_min = self.hd.z_min != self.hd.z_max && self.z_min_vec.len() == n_depth;
        let mut k: i64 = 0;
        for _i in 0..hd.n_rows {
            for _j in 0..hd.n_cols {
                if self.mask.is_valid(k) {
                    let m = (k as usize) * n_depth;
                    for d in 0..n_depth {
                        let z = if per_depth_min {
                            self.z_min_vec[d]
                        } else {
                            hd.z_min
                        };
                        write_value(out, m + d, hd.dt, z);
                    }
                }
                k += 1;
            }
        }
    }

    /// `Lerc2::ReadDataOneSweep`: valid pixels stored verbatim, row-major.
    fn read_data_one_sweep(&self, r: &mut ByteReader, out: &mut [u8]) -> TiffResult<()> {
        let hd = &self.hd;
        let n_depth = hd.n_depth as usize;
        let elem = hd.dt.size();
        let len = elem * n_depth;

        let mut k: i64 = 0;
        for _i in 0..hd.n_rows {
            for _j in 0..hd.n_cols {
                if self.mask.is_valid(k) {
                    let src = r.read_bytes(len)?;
                    let m = (k as usize) * n_depth;
                    // Values are stored little-endian on disk; re-emit native.
                    for d in 0..n_depth {
                        let v = read_native_from_le(&src[d * elem..(d + 1) * elem], hd.dt);
                        write_value(out, m + d, hd.dt, v);
                    }
                }
                k += 1;
            }
        }
        Ok(())
    }

    /// `Lerc2::ReadTiles` -> `ReadTile`: walk micro-blocks in (row, col, depth)
    /// order, decoding each.
    fn read_tiles(&self, r: &mut ByteReader, out: &mut [u8]) -> TiffResult<()> {
        let hd = &self.hd;
        let mb = hd.micro_block_size;
        let n_depth = hd.n_depth;

        let num_tiles_vert = (hd.n_rows + mb - 1) / mb;
        let num_tiles_hori = (hd.n_cols + mb - 1) / mb;

        for i_tile in 0..num_tiles_vert {
            let mut tile_h = mb;
            let i0 = i_tile * tile_h;
            if i_tile == num_tiles_vert - 1 {
                tile_h = hd.n_rows - i0;
            }
            for j_tile in 0..num_tiles_hori {
                let mut tile_w = mb;
                let j0 = j_tile * tile_w;
                if j_tile == num_tiles_hori - 1 {
                    tile_w = hd.n_cols - j0;
                }
                for i_depth in 0..n_depth {
                    self.read_tile(r, out, i0, i0 + tile_h, j0, j0 + tile_w, i_depth)?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn read_tile(
        &self,
        r: &mut ByteReader,
        out: &mut [u8],
        i0: i32,
        i1: i32,
        j0: i32,
        j1: i32,
        i_depth: i32,
    ) -> TiffResult<()> {
        let hd = &self.hd;
        let n_cols = hd.n_cols;
        let n_depth = hd.n_depth;

        let mut compr_flag = r.read_u8()?;

        let b_diff_enc = if hd.version >= 5 {
            (compr_flag & 4) != 0
        } else {
            false
        };
        let pattern: u8 = if hd.version >= 5 { 14 } else { 15 };

        // Integrity check: bits 2..5 of the flag must echo (j0 >> 3) & pattern.
        if ((compr_flag >> 2) & pattern) != (((j0 >> 3) as u8) & pattern) {
            return Err(lerc_err("LERC2 tile integrity check failed"));
        }
        if b_diff_enc && i_depth == 0 {
            return Err(lerc_err("invalid LERC2 diff-encoded tile"));
        }

        let bits67 = (compr_flag >> 6) as i32;
        compr_flag &= 3;

        if compr_flag == 2 {
            // Entire tile constant 0 (valid pixels).
            let mut k = i0 as i64 * n_cols as i64 + j0 as i64;
            for _i in i0..i1 {
                let mut kk = k;
                for _j in j0..j1 {
                    if self.mask.is_valid(kk) {
                        let m = (kk as usize) * n_depth as usize + i_depth as usize;
                        if b_diff_enc {
                            copy_prev_depth(out, m, hd.dt);
                        } else {
                            write_value(out, m, hd.dt, 0.0);
                        }
                    }
                    kk += 1;
                }
                k += n_cols as i64;
            }
            return Ok(());
        }

        if compr_flag == 0 {
            // Raw binary: sizeof(T) per valid pixel, on-disk little-endian.
            if b_diff_enc {
                return Err(lerc_err("unexpected diff-encoded raw LERC2 tile"));
            }
            let elem = hd.dt.size();
            let mut k = i0 as i64 * n_cols as i64 + j0 as i64;
            for _i in i0..i1 {
                let mut kk = k;
                for _j in j0..j1 {
                    if self.mask.is_valid(kk) {
                        let src = r.read_bytes(elem)?;
                        let v = read_native_from_le(src, hd.dt);
                        let m = (kk as usize) * n_depth as usize + i_depth as usize;
                        write_value(out, m, hd.dt, v);
                    }
                    kk += 1;
                }
                k += n_cols as i64;
            }
            return Ok(());
        }

        // compr_flag == 1 (bit-stuffed) or 3 (constant zMin).
        let base_dt = if b_diff_enc && (hd.dt as i32) < (DataType::Float as i32) {
            DataType::Int
        } else {
            hd.dt
        };
        let dt_used =
            get_data_type_used(base_dt, bits67).ok_or_else(|| lerc_err("bad LERC2 reduced type"))?;

        let offset = r.read_variable(dt_used)?;

        let z_max = if hd.version >= 4 && n_depth > 1 {
            self.z_max_vec[i_depth as usize]
        } else {
            hd.z_max
        };

        if compr_flag == 3 {
            // Constant offset over the tile.
            let mut k = i0 as i64 * n_cols as i64 + j0 as i64;
            for _i in i0..i1 {
                let mut kk = k;
                for _j in j0..j1 {
                    if self.mask.is_valid(kk) {
                        let m = (kk as usize) * n_depth as usize + i_depth as usize;
                        if b_diff_enc {
                            let z = offset + read_value(out, m - 1, hd.dt);
                            write_value(out, m, hd.dt, z.min(z_max));
                        } else {
                            write_value(out, m, hd.dt, offset);
                        }
                    }
                    kk += 1;
                }
                k += n_cols as i64;
            }
            return Ok(());
        }

        // Bit-stuffed quantized values.
        let max_element_count = (i1 - i0) as usize * (j1 - j0) as usize;
        let quant = bit_stuffer2_decode(r, max_element_count, hd.version)?;
        let inv_scale = 2.0 * hd.max_z_error;

        // When every pixel in the tile is valid the encoder omits the mask
        // check; otherwise only valid pixels consumed a quantized value.
        let all_valid = quant.len() == max_element_count;
        let mut idx = 0usize;
        let mut k = i0 as i64 * n_cols as i64 + j0 as i64;
        for _i in i0..i1 {
            let mut kk = k;
            for _j in j0..j1 {
                if all_valid || self.mask.is_valid(kk) {
                    if idx >= quant.len() {
                        return Err(lerc_err("LERC2 tile ran out of quantized values"));
                    }
                    let m = (kk as usize) * n_depth as usize + i_depth as usize;
                    let q = quant[idx];
                    idx += 1;
                    let mut z = offset + q as f64 * inv_scale;
                    if b_diff_enc {
                        z += read_value(out, m - 1, hd.dt);
                    }
                    write_value(out, m, hd.dt, z.min(z_max));
                }
                kk += 1;
            }
            k += n_cols as i64;
        }

        Ok(())
    }
}

/// `BitStuffer2::Decode` (version >= 3 path): read the packed-int header byte,
/// element count, and unstuff into a `Vec<u32>`. Handles both simple and LUT
/// modes.
fn bit_stuffer2_decode(
    r: &mut ByteReader,
    max_element_count: usize,
    _lerc2_version: i32,
) -> TiffResult<Vec<u32>> {
    let num_bits_byte = r.read_u8()?;

    let bits67 = num_bits_byte >> 6;
    let nb = if bits67 == 0 { 4 } else { 3 - bits67 } as i32;

    let do_lut = (num_bits_byte & (1 << 5)) != 0;
    let num_bits = (num_bits_byte & 31) as i32;

    let num_elements = decode_uint(r, nb)? as usize;
    if num_elements > max_element_count {
        return Err(lerc_err("LERC2 element count exceeds tile"));
    }

    if !do_lut {
        let mut data = vec![0u32; num_elements];
        if num_bits > 0 {
            bit_unstuff(r, &mut data, num_elements, num_bits)?;
        }
        return Ok(data);
    }

    // LUT mode.
    if num_bits == 0 {
        return Err(lerc_err("LERC2 LUT with zero bits"));
    }
    let n_lut_byte = r.read_u8()?;
    let n_lut = n_lut_byte as i32 - 1;
    if n_lut < 1 {
        return Err(lerc_err("LERC2 LUT too small"));
    }

    let mut lut = vec![0u32; n_lut as usize];
    bit_unstuff(r, &mut lut, n_lut as usize, num_bits)?;

    let mut n_bits_lut = 0i32;
    while (n_lut >> n_bits_lut) != 0 {
        n_bits_lut += 1;
    }
    if n_bits_lut == 0 {
        return Err(lerc_err("LERC2 LUT bit width zero"));
    }

    let mut indexes = vec![0u32; num_elements];
    bit_unstuff(r, &mut indexes, num_elements, n_bits_lut)?;

    // Put the implicit 0 back at the front, then map indexes to values.
    let mut lut_full = Vec::with_capacity(lut.len() + 1);
    lut_full.push(0u32);
    lut_full.extend_from_slice(&lut);

    let mut data = vec![0u32; num_elements];
    for i in 0..num_elements {
        let ix = indexes[i] as usize;
        if ix >= lut_full.len() {
            return Err(lerc_err("LERC2 LUT index out of range"));
        }
        data[i] = lut_full[ix];
    }
    Ok(data)
}

/// `BitStuffer2::DecodeUInt`: little-endian 1/2/4-byte count.
fn decode_uint(r: &mut ByteReader, num_bytes: i32) -> TiffResult<u32> {
    Ok(match num_bytes {
        1 => r.read_u8()? as u32,
        2 => {
            let s = r.read_bytes(2)?;
            u16::from_le_bytes([s[0], s[1]]) as u32
        }
        4 => r.read_u32()?,
        _ => return Err(lerc_err("bad LERC2 count byte width")),
    })
}

/// Bytes of the packed tail that carry no bits, per `NumTailBytesNotNeeded`.
fn num_tail_bytes_not_needed(num_elements: usize, num_bits: i32) -> usize {
    let num_bits_tail = ((num_elements as u64 * num_bits as u64) & 31) as i32;
    let num_bytes_tail = (num_bits_tail + 7) >> 3;
    if num_bytes_tail > 0 {
        (4 - num_bytes_tail) as usize
    } else {
        0
    }
}

/// `BitStuffer2::BitUnStuff` (version >= 3). Values are packed LSB-first into
/// little-endian 32-bit words.
fn bit_unstuff(
    r: &mut ByteReader,
    data: &mut [u32],
    num_elements: usize,
    num_bits: i32,
) -> TiffResult<()> {
    if num_elements == 0 || num_bits >= 32 || num_bits <= 0 {
        return Err(lerc_err("bad LERC2 bit-unstuff params"));
    }

    let num_uints = (num_elements * num_bits as usize).div_ceil(32);
    let num_bytes = num_uints * 4;
    let num_bytes_used = num_bytes - num_tail_bytes_not_needed(num_elements, num_bits);

    let src_bytes = r.read_bytes(num_bytes_used)?;

    // Assemble into 32-bit little-endian words; the final word is zero-padded.
    let mut tmp = vec![0u32; num_uints];
    for (i, &b) in src_bytes.iter().enumerate() {
        tmp[i / 4] |= (b as u32) << ((i % 4) * 8);
    }

    let mut src_idx = 0usize;
    let mut bit_pos = 0i32;
    let nb = 32 - num_bits;

    for d in data.iter_mut().take(num_elements) {
        if nb - bit_pos >= 0 {
            *d = (tmp[src_idx] << (nb - bit_pos)) >> nb;
            bit_pos += num_bits;
            if bit_pos == 32 {
                src_idx += 1;
                bit_pos = 0;
            }
        } else {
            let low = tmp[src_idx] >> bit_pos;
            src_idx += 1;
            let high = (tmp[src_idx] << (64 - num_bits - bit_pos)) >> nb;
            *d = low | high;
            bit_pos -= nb;
        }
    }

    Ok(())
}

/// `RLE::decompress`: LERC's simple RLE over the mask bytes. Counts are
/// little-endian i16; positive N = N literal bytes, non-positive -N = one byte
/// repeated N times, -32768 terminates.
fn rle_decompress(src: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut sp = 0usize;
    let mut op = 0usize;

    let read_count = |sp: &mut usize| -> TiffResult<i16> {
        if *sp + 2 > src.len() {
            return Err(lerc_err("truncated LERC RLE"));
        }
        let c = i16::from_le_bytes([src[*sp], src[*sp + 1]]);
        *sp += 2;
        Ok(c)
    };

    let mut cnt = read_count(&mut sp)?;
    while cnt != -32768 {
        let i = if cnt <= 0 { -(cnt as i32) } else { cnt as i32 } as usize;
        if cnt > 0 {
            if sp + i > src.len() || op + i > out.len() {
                return Err(lerc_err("LERC RLE literal overrun"));
            }
            out[op..op + i].copy_from_slice(&src[sp..sp + i]);
            sp += i;
            op += i;
        } else {
            if sp + 1 > src.len() || op + i > out.len() {
                return Err(lerc_err("LERC RLE run overrun"));
            }
            let b = src[sp];
            sp += 1;
            for _ in 0..i {
                out[op] = b;
                op += 1;
            }
        }
        cnt = read_count(&mut sp)?;
    }
    Ok(())
}

/// Reinterpret `sizeof(dt)` little-endian bytes as that data type, returned as
/// f64 (used by the raw-binary and one-sweep paths).
fn read_native_from_le(src: &[u8], dt: DataType) -> f64 {
    match dt {
        DataType::Char => src[0] as i8 as f64,
        DataType::Byte => src[0] as f64,
        DataType::Short => i16::from_le_bytes([src[0], src[1]]) as f64,
        DataType::UShort => u16::from_le_bytes([src[0], src[1]]) as f64,
        DataType::Int => i32::from_le_bytes([src[0], src[1], src[2], src[3]]) as f64,
        DataType::UInt => u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as f64,
        DataType::Float => f32::from_le_bytes([src[0], src[1], src[2], src[3]]) as f64,
        DataType::Double => f64::from_le_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ]),
    }
}

/// Write a value (given as f64) to output element `idx`, in native byte order,
/// mirroring the reference's `(T)std::min(z, zMax)` cast semantics.
fn write_value(out: &mut [u8], idx: usize, dt: DataType, v: f64) {
    let sz = dt.size();
    let off = idx * sz;
    match dt {
        DataType::Char => out[off] = (v as i8) as u8,
        DataType::Byte => out[off] = v as u8,
        DataType::Short => out[off..off + 2].copy_from_slice(&(v as i16).to_ne_bytes()),
        DataType::UShort => out[off..off + 2].copy_from_slice(&(v as u16).to_ne_bytes()),
        DataType::Int => out[off..off + 4].copy_from_slice(&(v as i32).to_ne_bytes()),
        DataType::UInt => out[off..off + 4].copy_from_slice(&(v as u32).to_ne_bytes()),
        DataType::Float => out[off..off + 4].copy_from_slice(&(v as f32).to_ne_bytes()),
        DataType::Double => out[off..off + 8].copy_from_slice(&v.to_ne_bytes()),
    }
}

/// Read back a previously written output element as f64 (diff-encoding path).
fn read_value(out: &[u8], idx: usize, dt: DataType) -> f64 {
    let sz = dt.size();
    let off = idx * sz;
    match dt {
        DataType::Char => out[off] as i8 as f64,
        DataType::Byte => out[off] as f64,
        DataType::Short => i16::from_ne_bytes([out[off], out[off + 1]]) as f64,
        DataType::UShort => u16::from_ne_bytes([out[off], out[off + 1]]) as f64,
        DataType::Int => {
            i32::from_ne_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]) as f64
        }
        DataType::UInt => {
            u32::from_ne_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]) as f64
        }
        DataType::Float => {
            f32::from_ne_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]) as f64
        }
        DataType::Double => f64::from_ne_bytes([
            out[off],
            out[off + 1],
            out[off + 2],
            out[off + 3],
            out[off + 4],
            out[off + 5],
            out[off + 6],
            out[off + 7],
        ]),
    }
}

/// Copy the previous depth-slice value into element `m` (diff-encoding const 0).
fn copy_prev_depth(out: &mut [u8], m: usize, dt: DataType) {
    let v = read_value(out, m - 1, dt);
    write_value(out, m, dt, v);
}

/// Undo GDAL's optional "additional compression" wrapper around the LERC2 blob,
/// sniffed from the leading bytes.
fn strip_add_compression(blob: &[u8]) -> TiffResult<std::borrow::Cow<'_, [u8]>> {
    use std::borrow::Cow;

    if blob.starts_with(LERC2_MAGIC) {
        return Ok(Cow::Borrowed(blob));
    }
    if blob.starts_with(LERC1_MAGIC) {
        return Err(TiffError::UnsupportedError(
            TiffUnsupportedError::UnsupportedCompressionMethod(CompressionMethod::from_u16_exhaustive(
                34887,
            )),
        ));
    }

    // zstd frame magic 0x28 0xB5 0x2F 0xFD.
    if blob.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return Ok(Cow::Owned(zstd_decode_all(blob)?));
    }

    // zlib/deflate header (0x78 ..).
    if blob.first() == Some(&0x78) {
        return Ok(Cow::Owned(deflate_decode_all(blob)?));
    }

    Err(lerc_err("unrecognized LERC add-compression wrapper"))
}

#[cfg(feature = "zstd")]
fn zstd_decode_all(blob: &[u8]) -> TiffResult<Vec<u8>> {
    zstd::stream::decode_all(blob).map_err(|e| lerc_err(&format!("LERC zstd wrapper: {e}")))
}

#[cfg(all(not(feature = "zstd"), feature = "zstd-safe-rust"))]
fn zstd_decode_all(blob: &[u8]) -> TiffResult<Vec<u8>> {
    let mut out = Vec::new();
    zrip_decode::streaming::FrameDecoder::new(blob)
        .read_to_end(&mut out)
        .map_err(|e| lerc_err(&format!("LERC zstd wrapper: {e}")))?;
    Ok(out)
}

#[cfg(not(any(feature = "zstd", feature = "zstd-safe-rust")))]
fn zstd_decode_all(_blob: &[u8]) -> TiffResult<Vec<u8>> {
    Err(lerc_err(
        "LERC blob is zstd-wrapped but the `zstd` feature is disabled",
    ))
}

#[cfg(feature = "deflate")]
fn deflate_decode_all(blob: &[u8]) -> TiffResult<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(blob)
        .read_to_end(&mut out)
        .map_err(|e| lerc_err(&format!("LERC deflate wrapper: {e}")))?;
    Ok(out)
}

#[cfg(not(feature = "deflate"))]
fn deflate_decode_all(_blob: &[u8]) -> TiffResult<Vec<u8>> {
    Err(lerc_err(
        "LERC blob is deflate-wrapped but the `deflate` feature is disabled",
    ))
}

/// Decode a full LERC tile blob to raw pixel bytes in native byte order.
fn decode_lerc_blob(blob: &[u8]) -> TiffResult<Vec<u8>> {
    let stripped = strip_add_compression(blob)?;
    let mut r = ByteReader::new(&stripped);

    let hd = Lerc2Decoder::read_header(&mut r)?;
    if r.buf.len() < hd.blob_size as usize {
        return Err(lerc_err("LERC2 blob shorter than declared size"));
    }

    let n_depth = hd.n_depth as usize;
    let out_elems = hd.n_rows as usize * hd.n_cols as usize * n_depth;
    let mut out = vec![0u8; out_elems * hd.dt.size()];

    let mut dec = Lerc2Decoder {
        hd,
        mask: BitMask {
            bits: Vec::new(),
            all_valid: false,
        },
        z_min_vec: Vec::new(),
        z_max_vec: Vec::new(),
    };

    dec.read_mask(&mut r)?;

    if dec.hd.num_valid_pixel == 0 {
        return Ok(out); // all zero
    }

    if dec.hd.z_min == dec.hd.z_max {
        dec.fill_const_image(&mut out);
        return Ok(out);
    }

    if dec.hd.version >= 4 {
        dec.read_min_max_ranges(&mut r)?;
        if dec.check_min_max_equal() {
            dec.fill_const_image(&mut out);
            return Ok(out);
        }
    }

    let read_data_one_sweep = r.read_u8()?;

    if read_data_one_sweep != 0 {
        dec.read_data_one_sweep(&mut r, &mut out)?;
        return Ok(out);
    }

    // Huffman / delta-Huffman paths (byte data at maxZError 0.5, or lossless
    // float) are the only cases that inject a flag byte before the tiles; we
    // don't implement them.
    let try_huffman_int = dec.hd.version >= 2
        && (dec.hd.dt == DataType::Byte || dec.hd.dt == DataType::Char)
        && dec.hd.max_z_error == 0.5;
    let try_huffman_flt = dec.hd.version >= 6
        && (dec.hd.dt == DataType::Float || dec.hd.dt == DataType::Double)
        && dec.hd.max_z_error == 0.0;

    if try_huffman_int || try_huffman_flt {
        let flag = r.read_u8()?;
        // flag != IEM_Tiling (0) means a Huffman-coded image.
        if flag != 0 {
            return Err(lerc_err(
                "LERC2 Huffman-coded images are not supported by this decoder",
            ));
        }
    }

    dec.read_tiles(&mut r, &mut out)?;
    Ok(out)
}

/// A `Read` adapter that decodes an entire LERC tile eagerly and then serves the
/// decoded native-endian bytes, matching the interface of the other codecs.
pub struct LercReader {
    inner: Cursor<Vec<u8>>,
}

impl LercReader {
    pub fn new<R: Read>(reader: R, compressed_length: u64) -> TiffResult<Self> {
        let mut blob = Vec::with_capacity(compressed_length as usize);
        reader.take(compressed_length).read_to_end(&mut blob)?;
        let decoded = decode_lerc_blob(&blob)?;
        Ok(LercReader {
            inner: Cursor::new(decoded),
        })
    }
}

impl Read for LercReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A lossless (maxZError = 0) 8x8 float32 LERC2 v4 blob whose valid pixels
    // form the ramp 0.0, 1.0, ..., 63.0 in row-major order, produced by GDAL:
    //   gdal_translate -of GTiff -co COMPRESS=LERC -co MAX_Z_ERROR=0 ramp.raw t.tif
    // Embedded so the codec is covered without any external fixtures.
    const TINY_RAMP_BLOB: &[u8] = &[
        76, 101, 114, 99, 50, 32, 4, 0, 0, 0, 212, 170, 227, 218, 8, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0,
        0, 64, 0, 0, 0, 8, 0, 0, 0, 79, 1, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 128, 79, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 124, 66, 1, 0, 0, 0, 0,
        0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64, 0, 0, 128, 64, 0, 0, 160, 64, 0, 0, 192, 64, 0,
        0, 224, 64, 0, 0, 0, 65, 0, 0, 16, 65, 0, 0, 32, 65, 0, 0, 48, 65, 0, 0, 64, 65, 0, 0, 80,
        65, 0, 0, 96, 65, 0, 0, 112, 65, 0, 0, 128, 65, 0, 0, 136, 65, 0, 0, 144, 65, 0, 0, 152,
        65, 0, 0, 160, 65, 0, 0, 168, 65, 0, 0, 176, 65, 0, 0, 184, 65, 0, 0, 192, 65, 0, 0, 200,
        65, 0, 0, 208, 65, 0, 0, 216, 65, 0, 0, 224, 65, 0, 0, 232, 65, 0, 0, 240, 65, 0, 0, 248,
        65, 0, 0, 0, 66, 0, 0, 4, 66, 0, 0, 8, 66, 0, 0, 12, 66, 0, 0, 16, 66, 0, 0, 20, 66, 0, 0,
        24, 66, 0, 0, 28, 66, 0, 0, 32, 66, 0, 0, 36, 66, 0, 0, 40, 66, 0, 0, 44, 66, 0, 0, 48, 66,
        0, 0, 52, 66, 0, 0, 56, 66, 0, 0, 60, 66, 0, 0, 64, 66, 0, 0, 68, 66, 0, 0, 72, 66, 0, 0,
        76, 66, 0, 0, 80, 66, 0, 0, 84, 66, 0, 0, 88, 66, 0, 0, 92, 66, 0, 0, 96, 66, 0, 0, 100,
        66, 0, 0, 104, 66, 0, 0, 108, 66, 0, 0, 112, 66, 0, 0, 116, 66, 0, 0, 120, 66, 0, 0, 124,
        66,
    ];

    #[test]
    fn decode_tiny_float_ramp() {
        let bytes = decode_lerc_blob(TINY_RAMP_BLOB).expect("decode");
        assert_eq!(bytes.len(), 64 * 4);
        let pixels: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for (i, &p) in pixels.iter().enumerate() {
            assert_eq!(p, i as f32, "pixel {i}");
        }
    }

    #[test]
    fn rejects_non_lerc_blob() {
        assert!(decode_lerc_blob(b"not a lerc blob at all").is_err());
    }

    #[test]
    fn rle_roundtrip_simple() {
        // 5 literal bytes then a run of 3 zeros: counts are little-endian i16.
        let mut src = Vec::new();
        src.extend_from_slice(&5i16.to_le_bytes());
        src.extend_from_slice(&[1, 2, 3, 4, 5]);
        src.extend_from_slice(&(-3i16).to_le_bytes());
        src.push(0);
        src.extend_from_slice(&(-32768i16).to_le_bytes());
        let mut out = [0u8; 8];
        rle_decompress(&src, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 0, 0, 0]);
    }
}
