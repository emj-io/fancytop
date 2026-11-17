//! Drawing primitives: colour gradients, a clipped canvas over ratatui's buffer, and widgets
//! (boxes, gradient bars, braille area graphs, sparklines).

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::theme::{THEMES, Theme};

pub type Rgb = (u8, u8, u8);

// ---------------------------------------------------------------------------- gradients

pub struct Gradient {
    lut: [Rgb; 65],
}

impl Gradient {
    pub fn new(stops: &[(f64, Rgb)]) -> Gradient {
        let mut lut = [(0, 0, 0); 65];
        for (i, slot) in lut.iter_mut().enumerate() {
            let t = i as f64 / 64.0;
            let mut c = stops[stops.len() - 1].1;
            for w in stops.windows(2) {
                let ((t0, c0), (t1, c1)) = (w[0], w[1]);
                if t <= t1 {
                    let u = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
                    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * u) as u8;
                    c = (mix(c0.0, c1.0), mix(c0.1, c1.1), mix(c0.2, c1.2));
                    break;
                }
            }
            *slot = c;
        }
        Gradient { lut }
    }

    pub fn at(&self, t: f64) -> Rgb {
        let i = if t.is_nan() || t <= 0.0 {
            0
        } else if t >= 1.0 {
            64
        } else {
            (t * 64.0) as usize
        };
        self.lut[i]
    }
}

// ---------------------------------------------------------------------------- canvas

/// A rectangle of cells, with signed coordinates like the canvas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Area {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Area {
        Area { x, y, w, h }
    }

    /// One past the last column.
    pub fn right(self) -> i32 {
        self.x + self.w
    }

    /// One past the last row.
    pub fn bottom(self) -> i32 {
        self.y + self.h
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A view over the frame buffer with a clip rectangle, so panel contents never spill out of
/// their box. Coordinates are signed and anything off-canvas is silently dropped.
pub struct Canvas<'a> {
    buf: &'a mut Buffer,
    pub w: i32,
    pub h: i32,
    clip: Area,
    truecolor: bool,
    pub theme: &'static Theme,
    /// Colour `draw_box` uses for the border.
    pub border: Rgb,
}

fn cube(v: u8) -> u8 {
    match v {
        0..48 => 0,
        48..115 => 1,
        _ => (v - 35) / 40,
    }
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut Buffer, truecolor: bool) -> Canvas<'a> {
        let (w, h) = (buf.area.width as i32, buf.area.height as i32);
        Canvas {
            buf,
            w,
            h,
            clip: Area::new(0, 0, w, h),
            truecolor,
            theme: &THEMES[0],
            border: THEMES[0].border,
        }
    }

    pub fn clip(&mut self, area: Area) {
        self.clip = area;
    }

    pub fn unclip(&mut self) {
        self.clip(Area::new(0, 0, self.w, self.h));
    }

    /// Blank an area back to the theme's background, ignoring the clip.
    pub fn clear(&mut self, a: Area) {
        let (ox, oy) = (self.buf.area.x as i32, self.buf.area.y as i32);
        let bg = self.theme.bg.map(|c| self.color(c));
        for y in a.y.max(0)..a.bottom().min(self.h) {
            for x in a.x.max(0)..a.right().min(self.w) {
                if let Some(cell) = self.buf.cell_mut(((ox + x) as u16, (oy + y) as u16)) {
                    cell.reset();
                    if let Some(bg) = bg {
                        cell.set_bg(bg);
                    }
                }
            }
        }
    }

    fn color(&self, c: Rgb) -> Color {
        if self.truecolor {
            Color::Rgb(c.0, c.1, c.2)
        } else {
            Color::Indexed(16 + 36 * cube(c.0) + 6 * cube(c.1) + cube(c.2))
        }
    }

    pub fn styled(&mut self, x: i32, y: i32, text: &str, fg: Rgb, bg: Option<Rgb>, bold: bool) {
        if y < self.clip.y.max(0) || y >= self.clip.bottom().min(self.h) {
            return;
        }
        let (fg, bg) = (self.color(fg), bg.map(|b| self.color(b)));
        let (ox, oy) = (self.buf.area.x as i32, self.buf.area.y as i32);
        let (left, right) = (self.clip.x.max(0), self.clip.right().min(self.w));
        for (i, ch) in text.chars().enumerate() {
            let xx = x + i as i32;
            if xx < left || xx >= right {
                continue;
            }
            if let Some(cell) = self.buf.cell_mut(((ox + xx) as u16, (oy + y) as u16)) {
                cell.set_char(ch).set_fg(fg);
                if let Some(bg) = bg {
                    cell.set_bg(bg);
                }
                cell.modifier = if bold { Modifier::BOLD } else { Modifier::empty() };
            }
        }
    }

    pub fn put(&mut self, x: i32, y: i32, text: &str, fg: Rgb) {
        self.styled(x, y, text, fg, None, false);
    }

    pub fn bold(&mut self, x: i32, y: i32, text: &str, fg: Rgb) {
        self.styled(x, y, text, fg, None, true);
    }

    pub fn ch(&mut self, x: i32, y: i32, ch: char, fg: Rgb) {
        let mut b = [0u8; 4];
        self.styled(x, y, ch.encode_utf8(&mut b), fg, None, false);
    }

    pub fn fill_bg(&mut self, y: i32, bg: Rgb) {
        let blank = " ".repeat(self.w.max(0) as usize);
        self.styled(0, y, &blank, self.theme.fg, Some(bg), false);
    }
}

// ---------------------------------------------------------------------------- widgets

/// One piece of a box title: text, colour, bold.
pub type Seg = (String, Rgb, bool);

pub fn seg(text: impl Into<String>, color: Rgb, bold: bool) -> Seg {
    (text.into(), color, bold)
}

/// Draw a rounded box in `cv.border`, then clip the canvas to its interior. Title groups are
/// all-or-nothing: a group that doesn't fit, and everything after it, is dropped.
pub fn draw_box(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, title: &[Vec<Seg>]) {
    cv.clip(Area::new(x, y, w, h));
    if w < 4 || h < 2 {
        cv.clip(Area::default());
        return;
    }
    let border = cv.border;
    let line = "─".repeat((w - 2) as usize);
    cv.put(x, y, &format!("╭{line}╮"), border);
    for yy in y + 1..y + h - 1 {
        cv.put(x, yy, "│", border);
        cv.put(x + w - 1, yy, "│", border);
    }
    cv.put(x, y + h - 1, &format!("╰{line}╯"), border);

    let (mut tx, limit) = (x + 3, x + w - 3);
    for (i, group) in title.iter().enumerate() {
        let len: i32 = group.iter().map(|(t, _, _)| t.chars().count() as i32).sum();
        if tx + len > limit {
            if i == 0
                && let Some((t, c, b)) = group.first()
            {
                let room = (limit - tx).max(0) as usize;
                cv.styled(tx, y, &t.chars().take(room).collect::<String>(), *c, None, *b);
                tx = limit;
            }
            break;
        }
        for (t, c, b) in group {
            cv.styled(tx, y, t, *c, None, *b);
            tx += t.chars().count() as i32;
        }
    }
    cv.put(x + 2, y, " ", border);
    cv.put(tx, y, " ", border);
    cv.clip(Area::new(x + 1, y + 1, w - 2, h - 2));
}

const EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BRAILLE_L: [u32; 5] = [0, 0x40, 0x44, 0x46, 0x47];
const BRAILLE_R: [u32; 5] = [0, 0x80, 0xA0, 0xB0, 0xB8];

/// Horizontal bar with eighth-cell precision, coloured along its length, with an optional
/// peak-hold marker.
pub fn draw_bar(cv: &mut Canvas, x: i32, y: i32, w: i32, frac: f64, grad: &Gradient, peak: Option<f64>) {
    if w <= 0 {
        return;
    }
    let filled = frac.clamp(0.0, 1.0) * w as f64;
    let peak_cell = peak.map(|p| ((p.clamp(0.0, 1.0) * w as f64) as i32).min(w - 1));
    for i in 0..w {
        let f = filled - i as f64;
        let color = grad.at((i as f64 + 0.5) / w as f64);
        if f >= 1.0 {
            cv.ch(x + i, y, '█', color);
        } else if f > 0.0625 {
            cv.ch(x + i, y, EIGHTHS[((f * 8.0) as usize).max(1)], color);
        } else if Some(i) == peak_cell {
            cv.ch(x + i, y, '▏', cv.theme.hi);
        } else {
            cv.ch(x + i, y, '─', cv.theme.faint);
        }
    }
}

/// Braille area chart: two samples per column, four dots per row, coloured bottom-to-top.
/// `values` are 0..1, oldest first; the newest sample is drawn at the right edge.
pub fn draw_graph<'v>(
    cv: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    values: impl DoubleEndedIterator<Item = &'v f64>,
    grad: &Gradient,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let need = (w * 2) as usize;
    let mut vals: Vec<Option<f64>> = values.rev().take(need).map(|&v| Some(v)).collect();
    vals.resize(need, None);
    vals.reverse();
    let dots = (h * 4) as f64;
    let height = |v: Option<f64>| match v {
        Some(v) if v > 0.004 => ((v * dots).round() as i32).max(1),
        _ => 0,
    };
    for col in 0..w {
        let (ha, hb) = (height(vals[2 * col as usize]), height(vals[2 * col as usize + 1]));
        for r in 0..h {
            let la = (ha - r * 4).clamp(0, 4) as usize;
            let lb = (hb - r * 4).clamp(0, 4) as usize;
            if la > 0 || lb > 0 {
                let ch = char::from_u32(0x2800 | BRAILLE_L[la] | BRAILLE_R[lb]).unwrap_or(' ');
                cv.ch(x + col, y + h - 1 - r, ch, grad.at((r as f64 + 0.5) / h as f64));
            } else if r == 0 {
                cv.ch(x + col, y + h - 1, '⣀', cv.theme.faint);
            }
        }
    }
}

pub fn draw_spark<'v>(cv: &mut Canvas, x: i32, y: i32, values: impl Iterator<Item = &'v f64>, grad: &Gradient) {
    for (i, &v) in values.enumerate() {
        let v = v.clamp(0.0, 1.0);
        cv.ch(x + i as i32, y, SPARK[((v * 8.0) as usize).min(7)], grad.at(v));
    }
}

// ---------------------------------------------------------------------------- formatting

pub fn fmt_bytes(n: f64, prec: usize) -> String {
    let mut n = n;
    for unit in ["B", "K", "M", "G", "T"] {
        if n.abs() < 1024.0 || unit == "T" {
            return if unit == "B" || unit == "K" {
                format!("{n:.0}{unit}")
            } else {
                format!("{n:.prec$}{unit}")
            };
        }
        n /= 1024.0;
    }
    unreachable!()
}

/// Bandwidth in decimal GB/s, the way memory bandwidth is usually quoted.
pub fn fmt_bw(n: f64) -> String {
    if n >= 99.5e9 {
        format!("{:.0} GB/s", n / 1e9)
    } else {
        format!("{:.1} GB/s", n / 1e9)
    }
}

pub fn fmt_rate(n: f64) -> String {
    if n >= 1.0 {
        format!("{}/s", fmt_bytes(n, 1))
    } else {
        "0".into()
    }
}

pub fn fmt_uptime(secs: i64) -> String {
    let (d, rem) = (secs / 86400, secs % 86400);
    let (h, m) = (rem / 3600, rem % 3600 / 60);
    if d > 0 { format!("{d}d {h}h") } else { format!("{h}h {m}m") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_bytes(512.0, 1), "512B");
        assert_eq!(fmt_bytes(2048.0, 1), "2K");
        assert_eq!(fmt_bytes(3.5 * 1024.0 * 1024.0 * 1024.0, 1), "3.5G");
        assert_eq!(fmt_bw(450.6e9), "451 GB/s");
        assert_eq!(fmt_bw(3.24e9), "3.2 GB/s");
        assert_eq!(fmt_uptime(2 * 86400 + 3 * 3600), "2d 3h");
        assert_eq!(fmt_uptime(3 * 3600 + 120), "3h 2m");
    }

    #[test]
    fn gradient_endpoints() {
        let heat = &THEMES[0].heat;
        assert_eq!(heat.at(0.0), (0, 220, 140));
        assert_eq!(heat.at(1.0), (255, 30, 90));
        assert_eq!(heat.at(-1.0), heat.at(0.0));
        assert_eq!(heat.at(f64::NAN), heat.at(0.0));
    }
}
