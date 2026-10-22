//! 二维码渲染：终端字符画 + 手写的最小 PNG 编码（不引入图像库）。

use qrcode::{Color, QrCode};

/// 二维码矩阵，`true` 表示深色模块。
pub struct Matrix {
    pub width: usize,
    pub dark: Vec<bool>,
}

pub fn encode(content: &str) -> Result<Matrix, qrcode::types::QrError> {
    let code = QrCode::new(content.as_bytes())?;
    let width = code.width();
    let dark = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    Ok(Matrix { width, dark })
}

impl Matrix {
    fn is_dark(&self, x: isize, y: isize) -> bool {
        let w = self.width as isize;
        x >= 0 && y >= 0 && x < w && y < w && self.dark[(y * w + x) as usize]
    }
}

/// 终端字符画，每个字符用上下半块表示两行模块，四周留 2 格静区。
/// 默认「浅色填充」：适合深色终端。`invert` 为 true 时改为深色填充，适合浅色终端。
pub fn render_ascii(m: &Matrix, invert: bool) -> String {
    const QUIET: isize = 2;
    let w = m.width as isize;
    let filled = |x: isize, y: isize| m.is_dark(x, y) == invert;
    let mut out = String::new();
    let mut y = -QUIET;
    while y < w + QUIET {
        for x in -QUIET..w + QUIET {
            out.push(match (filled(x, y), filled(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    out
}

/// 编码为灰度 PNG：每模块 `scale` 像素，四周留 4 格静区，深色 0、浅色 255。
pub fn render_png(m: &Matrix, scale: usize) -> Vec<u8> {
    const QUIET: isize = 4;
    let side = (m.width + 2 * QUIET as usize) * scale;
    let mut raw = Vec::with_capacity((side + 1) * side);
    for py in 0..side {
        raw.push(0); // 每行滤波类型 None
        let my = (py / scale) as isize - QUIET;
        for px in 0..side {
            let mx = (px / scale) as isize - QUIET;
            raw.push(if m.is_dark(mx, my) { 0 } else { 255 });
        }
    }

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8 位灰度，无压缩外的特殊设置
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// zlib 封装的「仅存储」deflate 流。
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_structure() {
        let m = encode("https://example.invalid/x").unwrap();
        let png = render_png(&m, 4);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let side = ((m.width + 8) * 4) as u32;
        assert_eq!(&png[16..20], &side.to_be_bytes());
        assert!(png.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn ascii_has_expected_rows() {
        let m = encode("hello").unwrap();
        let art = render_ascii(&m, false);
        let rows = art.lines().count();
        assert_eq!(rows, (m.width + 4).div_ceil(2));
        assert!(art.lines().all(|l| l.chars().count() == m.width + 4));
    }
}
