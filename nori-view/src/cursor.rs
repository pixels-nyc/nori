//! Procedural cursor composer: base arrow + head numeral + context badges.
//!
//! Simplest methodology: one built-in pack, no assets. Identity encoding is
//! fixed: the numeral is the head number, the accent color is the node (with
//! explicit override). Everything is pure pixels so agents never see it --
//! captures stay `overlay_cursor=0`.

use std::collections::HashMap;

pub const ICON_SIZE: i32 = 64;
pub const ICON_STRIDE: i32 = ICON_SIZE * 4;

/// Byte order is BGRA (Argb8888 on little-endian).
pub const ACCENTS: &[(&str, [u8; 4])] = &[
    ("blue", [255, 130, 70, 255]),
    ("green", [60, 200, 60, 255]),
    ("orange", [0, 165, 255, 255]),
    ("purple", [200, 80, 160, 255]),
    ("red", [60, 60, 220, 255]),
];

pub const WHITE: [u8; 4] = [255, 255, 255, 255];
pub const BLACK: [u8; 4] = [0, 0, 0, 255];
pub const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

/// Accent for a node name. Explicit `--accent` overrides this; unknown nodes
/// hash onto the palette so every node is stable and distinct-ish.
pub fn accent_for_node(node: &str) -> [u8; 4] {
    if node == "local" || node.is_empty() {
        return ACCENTS[0].1;
    }
    let hash = node
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    ACCENTS[(hash as usize % (ACCENTS.len() - 1)) + 1].1
}

/// Parse `--accent`: palette name or `#rrggbb`.
pub fn parse_accent(text: &str) -> Option<[u8; 4]> {
    if let Some((_, color)) = ACCENTS.iter().find(|(name, _)| *name == text) {
        return Some(*color);
    }
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([
        (v & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        ((v >> 16) & 0xff) as u8,
        255,
    ])
}

pub fn put_pixel(buf: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= ICON_SIZE || y >= ICON_SIZE {
        return;
    }
    let px = &mut buf[(y * ICON_STRIDE + x * 4) as usize..][..4];
    if color[3] == 255 {
        px.copy_from_slice(&color);
    } else if color[3] != 0 {
        let a = color[3] as u32;
        for i in 0..3 {
            px[i] = ((color[i] as u32 * a + px[i] as u32 * (255 - a)) / 255) as u8;
        }
        px[3] = px[3].max(color[3]);
    }
}

pub fn fill_rect(buf: &mut [u8], x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
    for dy in 0..h {
        for dx in 0..w {
            put_pixel(buf, x + dx, y + dy, color);
        }
    }
}

pub fn fill_circle(buf: &mut [u8], cx: i32, cy: i32, r: i32, color: [u8; 4]) {
    for y in (cy - r - 1)..=(cy + r + 1) {
        for x in (cx - r - 1)..=(cx + r + 1) {
            let d = (((x - cx).pow(2) + (y - cy).pow(2)) as f64).sqrt();
            if d <= r as f64 {
                put_pixel(buf, x, y, color);
            } else if d <= r as f64 + 1.0 {
                let mut soft = color;
                soft[3] = (soft[3] as f64 * (r as f64 + 1.0 - d)) as u8;
                put_pixel(buf, x, y, soft);
            }
        }
    }
}

pub fn stroke_line(
    buf: &mut [u8],
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    w: i32,
    color: [u8; 4],
) {
    let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * 2).max(1);
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = (x0 as f64 + (x1 - x0) as f64 * t).round() as i32;
        let y = (y0 as f64 + (y1 - y0) as f64 * t).round() as i32;
        for dy in -w..=w {
            for dx in -w..=w {
                if dx * dx + dy * dy <= w * w + w {
                    put_pixel(buf, x + dx, y + dy, color);
                }
            }
        }
    }
}

/// 3x5 bitmap digits (plus `+`), drawn at 2x for badge numerals.
const DIGITS: [(&str, [u8; 5]); 11] = [
    ("0", [0b111, 0b101, 0b101, 0b101, 0b111]),
    ("1", [0b010, 0b010, 0b010, 0b010, 0b010]),
    ("2", [0b111, 0b001, 0b111, 0b100, 0b111]),
    ("3", [0b111, 0b001, 0b111, 0b001, 0b111]),
    ("4", [0b101, 0b101, 0b111, 0b001, 0b001]),
    ("5", [0b111, 0b100, 0b111, 0b001, 0b111]),
    ("6", [0b111, 0b100, 0b111, 0b101, 0b111]),
    ("7", [0b111, 0b001, 0b001, 0b010, 0b010]),
    ("8", [0b111, 0b101, 0b111, 0b101, 0b111]),
    ("9", [0b111, 0b101, 0b111, 0b001, 0b111]),
    ("+", [0b000, 0b010, 0b111, 0b010, 0b000]),
];

fn digit_rows(ch: char) -> Option<[u8; 5]> {
    DIGITS
        .iter()
        .find(|(s, _)| s.chars().next() == Some(ch))
        .map(|(_, rows)| *rows)
}

/// Draw 1-2 characters centered at (cx, cy); scale 2 for one char, 1 for two.
pub fn draw_numeral(buf: &mut [u8], text: &str, cx: i32, cy: i32, color: [u8; 4]) {
    let chars: Vec<char> = text.chars().take(2).collect();
    if chars.is_empty() {
        return;
    }
    let scale = if chars.len() == 1 { 2 } else { 1 };
    let glyph_w = 3 * scale;
    let total_w = chars.len() as i32 * (glyph_w + scale) - scale;
    let mut x = cx - total_w / 2;
    let y0 = cy - 5 * scale / 2;
    for ch in chars {
        if let Some(rows) = digit_rows(ch) {
            for (ry, row) in rows.iter().enumerate() {
                for rx in 0..3 {
                    if row & (1 << (2 - rx)) != 0 {
                        fill_rect(
                            buf,
                            x + rx * scale,
                            y0 + ry as i32 * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
        }
        x += glyph_w + scale;
    }
}

/// Classic pointer arrow polygon, tip near (6, 4).
fn arrow_contains(x: i32, y: i32) -> bool {
    if x < 6 || y < 4 || x > 26 || y > 36 {
        return false;
    }
    let dx = (x - 6) as f64;
    let dy = (y - 4) as f64;
    if dy < dx * 1.35 {
        return false;
    }
    if dy > 13.0 && dx > 7.0 + (dy - 13.0) * 0.25 {
        if dx < 11.0 {
            return true;
        }
        return dy < 13.0 + (dx - 7.0) * 2.2;
    }
    true
}

pub fn draw_arrow(buf: &mut [u8]) {
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            if arrow_contains(x, y) {
                put_pixel(buf, x, y, BLACK);
                for (dx, dy) in [(-2, 0), (2, 0), (0, -2), (0, 2), (-1, -1), (1, 1)] {
                    put_pixel(buf, x + dx, y + dy, BLACK);
                }
            }
        }
    }
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            if arrow_contains(x, y) {
                put_pixel(buf, x, y, WHITE);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum FileClass {
    Image,
    Video,
    Audio,
    Doc,
    Pdf,
    Archive,
    Code,
    Folder,
    Link,
    Generic,
}

/// Extension -> class. Folders (trailing `/`) and http(s) checked first.
pub fn classify_uri(uri: &str) -> FileClass {
    let uri = uri.trim();
    if uri.ends_with('/') {
        return FileClass::Folder;
    }
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return FileClass::Link;
    }
    let path = lower.rsplit(['#', '?']).next().unwrap_or(&lower);
    let ext = path.rsplit('.').next().unwrap_or("");
    // Guard: no dot, or dot is the whole segment (".bashrc" has stem).
    if !path.contains('.') || ext.contains('/') {
        return FileClass::Generic;
    }
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tiff"
        | "heic" | "avif" => FileClass::Image,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "ogv" => FileClass::Video,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "opus" => FileClass::Audio,
        "pdf" => FileClass::Pdf,
        "txt" | "md" | "rst" | "doc" | "docx" | "odt" | "rtf" | "csv" => FileClass::Doc,
        "zip" | "tar" | "gz" | "xz" | "7z" | "rar" | "bz2" | "tgz" => FileClass::Archive,
        "rs" | "py" | "js" | "ts" | "tsx" | "c" | "h" | "cpp" | "hpp" | "go" | "java"
        | "rb" | "sh" | "yaml" | "yml" | "toml" | "json" | "html" | "css" => FileClass::Code,
        _ => FileClass::Generic,
    }
}

/// First three classes plus total URI count from a text/uri-list payload.
pub fn classify_uris(text: &str) -> (Vec<FileClass>, usize) {
    let uris: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let classes = uris.iter().take(3).map(|u| classify_uri(u)).collect();
    (classes, uris.len())
}

/// Draw one file glyph: white page + class band + mini symbol, in a 30x34 box
/// at (x, y). Folders render as a tabbed folder instead of a page.
pub fn draw_file_glyph(buf: &mut [u8], class: FileClass, x: i32, y: i32) {
    if class == FileClass::Folder {
        fill_rect(buf, x + 2, y + 6, 12, 6, [40, 180, 230, 255]);
        fill_rect(buf, x, y + 10, 30, 22, [70, 200, 245, 255]);
        fill_rect(buf, x, y + 10, 30, 2, BLACK);
        return;
    }
    // Page with folded corner.
    fill_rect(buf, x + 4, y, 22, 34, WHITE);
    fill_rect(buf, x + 4, y, 22, 2, BLACK);
    fill_rect(buf, x + 4, y + 32, 22, 2, BLACK);
    fill_rect(buf, x + 4, y, 2, 34, BLACK);
    fill_rect(buf, x + 24, y, 2, 34, BLACK);
    stroke_line(buf, x + 20, y, x + 26, y + 6, 0, BLACK);
    // BGRA byte order.
    let band: [u8; 4] = match class {
        FileClass::Image => [255, 170, 90, 255],
        FileClass::Video => [200, 70, 200, 255],
        FileClass::Audio => [40, 170, 255, 255],
        FileClass::Doc => [255, 180, 120, 255],
        FileClass::Pdf => [60, 60, 220, 255],
        FileClass::Archive => [70, 110, 150, 255],
        FileClass::Code => [80, 220, 130, 255],
        FileClass::Link => [90, 220, 220, 255],
        FileClass::Generic | FileClass::Folder => [160, 160, 160, 255],
    };
    fill_rect(buf, x + 6, y + 8, 18, 6, band);
    match class {
        FileClass::Image => {
            fill_circle(buf, x + 12, y + 22, 3, [80, 220, 255, 255]);
            stroke_line(buf, x + 8, y + 28, x + 15, y + 21, 1, [80, 180, 80, 255]);
            stroke_line(buf, x + 15, y + 21, x + 23, y + 28, 1, [80, 180, 80, 255]);
        }
        FileClass::Video => {
            // Play triangle.
            for i in 0..8 {
                stroke_line(
                    buf,
                    x + 12,
                    y + 19 + i,
                    x + 12 + i,
                    y + 19 + i,
                    0,
                    WHITE,
                );
            }
        }
        FileClass::Audio => {
            for (i, h) in [6, 10, 7].iter().enumerate() {
                fill_rect(buf, x + 11 + i as i32 * 4, y + 28 - h, 2, *h, BLACK);
            }
        }
        FileClass::Doc | FileClass::Generic => {
            for i in 0..3 {
                stroke_line(buf, x + 9, y + 19 + i * 4, x + 21, y + 19 + i * 4, 0, BLACK);
            }
        }
        FileClass::Pdf => {
            // White rules on the red band (no digit: digits mean numerals).
            stroke_line(buf, x + 9, y + 10, x + 21, y + 10, 0, WHITE);
            stroke_line(buf, x + 9, y + 12, x + 18, y + 12, 0, WHITE);
        }
        FileClass::Archive => {
            fill_rect(buf, x + 11, y + 20, 8, 7, [150, 110, 70, 255]);
            stroke_line(buf, x + 11, y + 22, x + 19, y + 22, 0, BLACK);
        }
        FileClass::Code => {
            stroke_line(buf, x + 15, y + 18, x + 10, y + 23, 0, BLACK);
            stroke_line(buf, x + 10, y + 23, x + 15, y + 28, 0, BLACK);
            stroke_line(buf, x + 16, y + 18, x + 21, y + 23, 0, BLACK);
            stroke_line(buf, x + 21, y + 23, x + 16, y + 28, 0, BLACK);
        }
        FileClass::Link => {
            fill_circle(buf, x + 12, y + 24, 4, WHITE);
            fill_circle(buf, x + 19, y + 21, 4, WHITE);
            stroke_line(buf, x + 12, y + 24, x + 19, y + 21, 1, BLACK);
        }
        FileClass::Folder => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DragBadge {
    Neutral,
    Accepted,
}

/// Everything the composer needs. `files` empty + `drag` None = idle cursor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CursorSpec {
    pub numeral: u32,
    pub accent: [u8; 4],
    pub files: Vec<FileClass>,
    pub file_count: usize,
    pub drag: Option<DragBadge>,
}

impl CursorSpec {
    pub fn idle(numeral: u32, accent: [u8; 4]) -> Self {
        Self {
            numeral,
            accent,
            files: Vec::new(),
            file_count: 0,
            drag: None,
        }
    }
}

/// Layout: arrow (or file glyph) left, numeral badge top-right, state badge
/// bottom-right. Arrow tip stays near (6, 4) so the hotspot never moves.
pub fn compose_cursor(spec: &CursorSpec) -> Vec<u8> {
    let mut buf = vec![0u8; (ICON_SIZE * ICON_STRIDE) as usize];
    if spec.files.is_empty() {
        draw_arrow(&mut buf);
    } else {
        // Stacked glyphs, first on top (draw back-to-front).
        for (i, class) in spec.files.iter().rev().enumerate() {
            let off = i as i32 * 3;
            draw_file_glyph(&mut buf, *class, 2 + off, 12 + off);
        }
        if spec.file_count > 1 {
            fill_circle(&mut buf, 10, 52, 9, BLACK);
            fill_circle(&mut buf, 10, 52, 7, spec.accent);
            let label = if spec.file_count > 9 {
                "+".to_string()
            } else {
                spec.file_count.to_string()
            };
            draw_numeral(&mut buf, &label, 10, 52, WHITE);
        }
    }
    // Numeral badge top-right (head identity, always visible).
    fill_circle(&mut buf, 50, 12, 11, BLACK);
    fill_circle(&mut buf, 50, 12, 9, spec.accent);
    draw_numeral(&mut buf, &spec.numeral.to_string(), 50, 12, WHITE);
    // State badge bottom-right while dragging.
    if let Some(badge) = spec.drag {
        let (color, check) = match badge {
            DragBadge::Neutral => ([255, 130, 70, 255], false),
            DragBadge::Accepted => ([60, 200, 60, 255], true),
        };
        fill_circle(&mut buf, 47, 47, 13, BLACK);
        fill_circle(&mut buf, 47, 47, 11, color);
        if check {
            stroke_line(&mut buf, 41, 47, 45, 51, 1, WHITE);
            stroke_line(&mut buf, 45, 51, 53, 42, 1, WHITE);
        } else {
            stroke_line(&mut buf, 47, 41, 47, 53, 1, WHITE);
            stroke_line(&mut buf, 41, 47, 53, 47, 1, WHITE);
        }
    }
    buf
}

/// Distinctness helper for tests.
#[allow(dead_code)]
pub fn class_counts(classes: &[FileClass]) -> HashMap<FileClass, usize> {
    let mut map = HashMap::new();
    for class in classes {
        *map.entry(*class).or_insert(0) += 1;
    }
    map
}
