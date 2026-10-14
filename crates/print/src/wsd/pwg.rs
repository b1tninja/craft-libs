//! PWG raster (`image/pwg-raster`) writer for 8-bit grey and RGB pages.
//!
//! The stream is the sync word `RaS2`, then per page a 1796-byte big-endian header followed by the
//! pixel rows. Each row is preceded by a byte saying how many extra times the same row repeats
//! (up to 255) and is packed in runs: a control byte `c` of 0 to 127 repeats the next pixel `c + 1`
//! times, and 129 to 255 introduces `257 - c` literal pixels (byte 128 is reserved and never
//! written). Written from the public format description; it has been checked here only against
//! this crate's own reader, so a first run against a printer or a third-party decoder is still to
//! be done (see the README follow-ups).

use super::error::{Result, WsdError};

/// Pages in one stream.
pub const MAX_PAGES: usize = 500;
/// The widest or tallest page in pixels.
pub const MAX_SIDE: u32 = 20_000;
/// The most raw bytes in one page.
pub const MAX_PAGE_BYTES: usize = 384 << 20;
/// The most bytes in the whole stream.
pub const MAX_STREAM_BYTES: usize = 1 << 30;

const SYNC: &[u8; 4] = b"RaS2";
const HEADER: usize = 1796;

/// One page of 8-bit pixels, top row first, rows tightly packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RasterPage {
    pub width: u32,
    pub height: u32,
    /// `true`: one byte per pixel (sGray); `false`: three (sRGB).
    pub gray: bool,
    pub pixels: Vec<u8>,
}

impl RasterPage {
    fn bytes_per_pixel(&self) -> usize {
        if self.gray { 1 } else { 3 }
    }
}

/// Two-sided printing as the raster header states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duplex {
    Off,
    LongEdge,
    ShortEdge,
}

#[derive(Clone, Copy, Debug)]
pub struct PwgOptions {
    pub dpi: (u32, u32),
    pub duplex: Duplex,
}

fn invalid<T>(why: impl Into<String>) -> Result<T> {
    Err(WsdError::Encode(why.into()))
}

fn set_u32(header: &mut [u8], at: usize, v: u32) {
    if let Some(slot) = header.get_mut(at..at + 4) {
        slot.copy_from_slice(&v.to_be_bytes());
    }
}

fn set_str(header: &mut [u8], at: usize, s: &str) {
    if let Some(slot) = header.get_mut(at..at + s.len().min(63)) {
        slot.copy_from_slice(s.as_bytes().get(..slot.len()).unwrap_or(&[]));
    }
}

fn page_header(page: &RasterPage, total: u32, opts: &PwgOptions) -> Vec<u8> {
    let mut h = vec![0u8; HEADER];
    let bpp = page.bytes_per_pixel() as u32;
    set_str(&mut h, 0, "PwgRaster");
    let (dx, dy) = (opts.dpi.0.max(1), opts.dpi.1.max(1));
    let points = |px: u32, dpi: u32| ((u64::from(px) * 72 + u64::from(dpi) / 2) / u64::from(dpi)) as u32;
    set_u32(&mut h, 272, u32::from(opts.duplex != Duplex::Off)); // Duplex
    set_u32(&mut h, 276, dx); // HWResolution
    set_u32(&mut h, 280, dy);
    set_u32(&mut h, 340, 1); // NumCopies: copies are a ticket matter
    set_u32(&mut h, 352, points(page.width, dx)); // PageSize in points
    set_u32(&mut h, 356, points(page.height, dy));
    set_u32(&mut h, 368, u32::from(opts.duplex == Duplex::ShortEdge)); // Tumble
    set_u32(&mut h, 372, page.width);
    set_u32(&mut h, 376, page.height);
    set_u32(&mut h, 384, 8); // bits per colour
    set_u32(&mut h, 388, 8 * bpp); // bits per pixel
    set_u32(&mut h, 392, page.width.saturating_mul(bpp)); // bytes per line
    set_u32(&mut h, 396, 0); // chunky
    set_u32(&mut h, 400, if page.gray { 18 } else { 19 }); // sGray / sRGB
    set_u32(&mut h, 420, bpp); // colours
    set_u32(&mut h, 452, total); // TotalPageCount
    set_u32(&mut h, 456, 1); // CrossFeedTransform
    set_u32(&mut h, 460, 1); // FeedTransform
    set_u32(&mut h, 472, page.width); // ImageBoxRight
    set_u32(&mut h, 476, page.height); // ImageBoxBottom
    h
}

/// Pack `pages` into a PWG raster stream.
pub fn encode(pages: &[RasterPage], opts: &PwgOptions) -> Result<Vec<u8>> {
    if pages.is_empty() {
        return invalid("there are no pages");
    }
    if pages.len() > MAX_PAGES {
        return invalid(format!("more than {MAX_PAGES} pages"));
    }
    let mut out = Vec::new();
    out.extend_from_slice(SYNC);
    for page in pages {
        check_page(page)?;
        out.extend_from_slice(&page_header(page, pages.len() as u32, opts));
        let stride = page.width as usize * page.bytes_per_pixel();
        let mut rows = page.pixels.chunks_exact(stride.max(1)).peekable();
        while let Some(row) = rows.next() {
            let mut repeats = 0u8;
            while repeats < 255 && rows.peek().is_some_and(|next| *next == row) {
                rows.next();
                repeats += 1;
            }
            out.push(repeats);
            pack_row(row, page.bytes_per_pixel(), &mut out);
        }
        if out.len() > MAX_STREAM_BYTES {
            return Err(WsdError::TooLarge("the raster stream", MAX_STREAM_BYTES));
        }
    }
    Ok(out)
}

fn check_page(p: &RasterPage) -> Result<()> {
    if p.width == 0 || p.height == 0 || p.width > MAX_SIDE || p.height > MAX_SIDE {
        return invalid(format!("page size {}x{} is outside 1 to {MAX_SIDE} pixels", p.width, p.height));
    }
    let need = (p.width as usize).checked_mul(p.height as usize).and_then(|n| n.checked_mul(p.bytes_per_pixel()));
    match need {
        Some(n) if n <= MAX_PAGE_BYTES && n == p.pixels.len() => Ok(()),
        Some(n) if n > MAX_PAGE_BYTES => Err(WsdError::TooLarge("a raster page", MAX_PAGE_BYTES)),
        _ => invalid("the pixel buffer does not match the page size"),
    }
}

fn pack_row(row: &[u8], bpp: usize, out: &mut Vec<u8>) {
    let pixels: Vec<&[u8]> = row.chunks_exact(bpp).collect();
    let n = pixels.len();
    let mut i = 0;
    while i < n {
        let mut run = 1;
        while run < 128 && i + run < n && pixels.get(i + run) == pixels.get(i) {
            run += 1;
        }
        if run >= 2 {
            out.push((run - 1) as u8);
            out.extend_from_slice(pixels.get(i).copied().unwrap_or(&[]));
            i += run;
            continue;
        }
        // A literal stretch: up to 128 pixels, stopping before the next run of two or more.
        let mut m = 1;
        while m < 128 && i + m < n && pixels.get(i + m) != pixels.get(i + m + 1) {
            m += 1;
        }
        if i + m >= n {
            m = n - i;
        }
        if m == 1 {
            out.push(0);
        } else {
            out.push((257 - m) as u8);
        }
        for p in pixels.get(i..i + m).unwrap_or(&[]) {
            out.extend_from_slice(p);
        }
        i += m;
    }
}

/// A reader for tests: the pages of a stream as (width, height, gray, pixels).
#[cfg(test)]
pub(crate) fn decode_for_tests(data: &[u8]) -> Result<Vec<RasterPage>> {
    let be = |d: &[u8], at: usize| -> Result<u32> {
        d.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| WsdError::Encode("short header".into()))
    };
    if data.get(..4) != Some(&SYNC[..]) {
        return invalid("bad sync word");
    }
    let mut at = 4;
    let mut pages = Vec::new();
    while at < data.len() {
        let h = data.get(at..at + HEADER).ok_or_else(|| WsdError::Encode("short header".into()))?;
        at += HEADER;
        let (w, ht, bpp) = (be(h, 372)? as usize, be(h, 376)? as usize, (be(h, 388)? / 8) as usize);
        if bpp == 0 || w == 0 || ht == 0 {
            return invalid("empty page");
        }
        let mut pixels = Vec::new();
        let mut rows_done = 0;
        while rows_done < ht {
            let repeat = *data.get(at).ok_or_else(|| WsdError::Encode("short rows".into()))? as usize + 1;
            at += 1;
            let mut row = Vec::new();
            while row.len() < w * bpp {
                let c = *data.get(at).ok_or_else(|| WsdError::Encode("short row".into()))?;
                at += 1;
                if c <= 127 {
                    let px = data.get(at..at + bpp).ok_or_else(|| WsdError::Encode("short run".into()))?;
                    at += bpp;
                    for _ in 0..=c {
                        row.extend_from_slice(px);
                    }
                } else if c == 128 {
                    return invalid("reserved control byte");
                } else {
                    let n = 257 - c as usize;
                    row.extend_from_slice(data.get(at..at + n * bpp).ok_or_else(|| WsdError::Encode("short literal".into()))?);
                    at += n * bpp;
                }
            }
            if row.len() != w * bpp {
                return invalid("row overran");
            }
            for _ in 0..repeat.min(ht - rows_done) {
                pixels.extend_from_slice(&row);
            }
            rows_done += repeat;
        }
        pages.push(RasterPage { width: w as u32, height: ht as u32, gray: bpp == 1, pixels });
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> PwgOptions {
        PwgOptions { dpi: (300, 300), duplex: Duplex::Off }
    }

    fn page(w: u32, h: u32, gray: bool, f: impl Fn(u32, u32) -> u8) -> RasterPage {
        let bpp = if gray { 1 } else { 3 };
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for c in 0..bpp {
                    pixels.push(f(x, y).wrapping_add(c as u8 * 7));
                }
            }
        }
        RasterPage { width: w, height: h, gray, pixels }
    }

    #[test]
    fn pages_round_trip_through_the_reader() {
        let pages = vec![
            page(5, 4, true, |x, y| (x * 13 + y * 7) as u8),
            page(300, 3, false, |x, _| if x % 50 < 25 { 0 } else { 200 }),
            page(1, 1, true, |_, _| 5),
            page(130, 300, true, |_, _| 255),
            page(257, 2, false, |x, y| (x * y) as u8),
        ];
        let bytes = encode(&pages, &opts()).unwrap();
        assert_eq!(decode_for_tests(&bytes).unwrap(), pages);
    }

    #[test]
    fn the_header_has_the_documented_size_and_fields() {
        let p = page(300, 600, false, |_, _| 9);
        let bytes = encode(std::slice::from_ref(&p), &PwgOptions { dpi: (300, 300), duplex: Duplex::ShortEdge }).unwrap();
        assert_eq!(&bytes[..4], b"RaS2");
        let h = &bytes[4..4 + HEADER];
        assert_eq!(&h[..9], b"PwgRaster");
        let be = |at: usize| u32::from_be_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]]);
        assert_eq!((be(372), be(376), be(384), be(388), be(392), be(400), be(420)), (300, 600, 8, 24, 900, 19, 3));
        assert_eq!((be(352), be(356)), (72, 144)); // points
        assert_eq!((be(272), be(368), be(276), be(280), be(452)), (1, 1, 300, 300, 1));
    }

    #[test]
    fn identical_rows_and_pixels_compress() {
        let flat = page(1000, 1000, true, |_, _| 255);
        let bytes = encode(&[flat], &opts()).unwrap();
        // 1000 rows collapse to 4 row groups of at most 256, each a few run bytes.
        assert!(bytes.len() < 4 + HEADER + 100, "{}", bytes.len());
    }

    #[test]
    fn worst_case_literals_stay_within_one_control_byte_per_128() {
        let noisy = page(1000, 1, true, |x, _| (x * 37 % 251) as u8);
        let bytes = encode(&[noisy], &opts()).unwrap();
        assert!(bytes.len() <= 4 + HEADER + 1 + 1000 + 8 + 1);
    }

    #[test]
    fn bad_pages_are_errors() {
        assert!(encode(&[], &opts()).is_err());
        let ok = page(2, 2, true, |_, _| 1);
        assert!(encode(&[RasterPage { pixels: vec![0; 3], ..ok.clone() }], &opts()).is_err());
        assert!(encode(&[RasterPage { width: 0, ..ok.clone() }], &opts()).is_err());
        assert!(encode(&[RasterPage { width: MAX_SIDE + 1, ..ok.clone() }], &opts()).is_err());
        assert!(matches!(
            encode(&[RasterPage { width: MAX_SIDE, height: MAX_SIDE, gray: false, pixels: vec![] }], &opts()),
            Err(WsdError::TooLarge(..))
        ));
        assert!(encode(&vec![ok; MAX_PAGES + 1], &opts()).is_err());
        // A zero dpi cannot divide by zero.
        assert!(encode(&[page(2, 2, true, |_, _| 1)], &PwgOptions { dpi: (0, 0), duplex: Duplex::Off }).is_ok());
    }

    #[test]
    fn the_reader_rejects_garbage() {
        assert!(decode_for_tests(b"nope").is_err());
        assert!(decode_for_tests(b"RaS2\0\0").is_err());
        let mut bytes = encode(&[page(4, 4, true, |_, _| 3)], &opts()).unwrap();
        bytes.truncate(bytes.len() - 1);
        assert!(decode_for_tests(&bytes).is_err());
    }
}
