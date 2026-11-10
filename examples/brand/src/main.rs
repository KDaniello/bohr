//! Writes the brand's SVG files: the mark, the wordmark, the favicon, the README banner, the
//! repository preview, the section divider and a release banner.
//!
//! A banner draws Bohr's orbits about the word, orbit n at n² times the radius of the o's ring. The
//! seed places one electron on each orbit from bohr's own tape, `Tape::from_seed`, so the same seed
//! sets the same electrons on any machine, which is the claim the banner illustrates.
//!
//! The orbits are masked off the text by its own glyphs, fattened into a halo, so the gap follows
//! whatever font the viewer has. An electron keeps clear of a box set by hand around each line for
//! its width in Segoe UI; a font wider than that by more than the box's margin can reach an
//! electron.
//!
//! `cargo run --manifest-path examples/brand/Cargo.toml -- docs/assets [version name]`

use std::{f64::consts::PI, fmt::Write as _, path::Path};

use bohr_tape::Tape;
use bohr_types::Seed;

/// One palette: the ground, ink, a muted grey for captions, and the Hα red of the o's electron.
struct Palette {
    bg: &'static str,
    ink: &'static str,
    muted: &'static str,
    ha: &'static str,
}

// The grounds are GitHub's own page colours, so the two files that carry a plate, the preview and
// the favicon, show no box edge where GitHub sets them on its page.
const LIGHT: Palette = Palette { bg: "#FFFFFF", ink: "#0F1522", muted: "#59636E", ha: "#E0393B" };
const DARK: Palette = Palette { bg: "#0D1117", ink: "#E8EDF4", muted: "#9198A1", ha: "#FF5B55" };

/// The banner's seed: 1913, the year of Bohr's model.
const SEED: u64 = 1913;

const SANS: &str = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif";
const MONO: &str =
    "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace";

const TAGLINE: &str = "Deterministic simulation testing for unmodified Linux programs.";
const LABEL: &str = "bohr — deterministic simulation testing for unmodified Linux programs";

/// The wordmark's box in its own units; the letters stand `WM_INSET`, half a stroke, inside it.
const WM_W: f64 = 276.0;
const WM_H: f64 = 112.0;
const WM_INSET: f64 = 6.0;
/// The middle of the word's ink in the box, from the b's stem to the end of the r's tail.
const WM_MIDDLE: f64 = 135.0;
/// The o's centre in the letters' frame, and the radius of its ring and of every round letter.
const O: (f64, f64) = (108.0, 70.0);
const O_R: f64 = 30.0;
/// The mark's box in the o's units: the ring's outer edge at 36 and the electron's at 39 fit.
const MARK_BOX: f64 = 80.0;

/// The b, the h and the r, in the letters' frame.
const LETTERS: &str = "<path d=\"M0 0V100\"/><circle cx=\"30\" cy=\"70\" r=\"30\"/>\
                       <path d=\"M156 0V100M156 70A30 30 0 0 1 216 70V100\"/>\
                       <path d=\"M234 40V100M234 70A30 30 0 0 1 264 40\"/>";

/// Angles an orbit reads from the tape before it is left empty.
const TRIES: usize = 8;
/// How far an electron's centre stays inside the frame.
const PAD: f64 = 28.0;
/// How far an electron's centre stays outside a box of text.
const CLEAR: f64 = 18.0;
/// An orbit's stroke, its electron, and the gap the electron breaks in it.
const ORBIT_STROKE: f64 = 3.0;
const ELECTRON_R: f64 = 7.0;
const ELECTRON_GAP: f64 = 14.0;
/// How far the orbits stay off the wordmark's strokes, in canvas units.
const WORD_HALO: f64 = 10.0;

/// A number with at most two decimals and no trailing zeros, as the kit writes them.
fn num(x: f64) -> String {
    places(x, 2)
}

fn places(x: f64, digits: i32) -> String {
    let scale = 10f64.powi(digits);
    let r = (x * scale).round() / scale;
    if r == 0.0 {
        return "0".to_owned();
    }
    if r == r.trunc() { format!("{}", r as i64) } else { format!("{r}") }
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The Hα electron's direction: up and to the right, at −40° from the horizontal.
fn angle() -> (f64, f64) {
    let a = -40.0 * PI / 180.0;
    (a.cos(), a.sin())
}

#[derive(Clone, Copy)]
enum Tone {
    Ink,
    Muted,
}

/// How a piece of text is set.
#[derive(Clone, Copy)]
struct Set {
    font: &'static str,
    size: f64,
    weight: u32,
    tone: Tone,
    anchor: &'static str,
    spacing: Option<f64>,
}

impl Set {
    const fn sans(size: f64, weight: u32) -> Self {
        Self { font: SANS, size, weight, tone: Tone::Ink, anchor: "start", spacing: None }
    }

    const fn mono(size: f64, weight: u32) -> Self {
        Self { font: MONO, size, weight, tone: Tone::Ink, anchor: "start", spacing: None }
    }

    const fn centred(self) -> Self {
        Self { anchor: "middle", ..self }
    }

    const fn muted(self) -> Self {
        Self { tone: Tone::Muted, ..self }
    }

    const fn spaced(self, spacing: f64) -> Self {
        Self { spacing: Some(spacing), ..self }
    }
}

fn text(p: &Palette, x: f64, y: f64, words: &str, set: &Set) -> String {
    let fill = match set.tone {
        Tone::Ink => p.ink,
        Tone::Muted => p.muted,
    };
    setting(x, y, words, set, &format!("fill=\"{fill}\""))
}

/// The text's silhouette for an orbit mask: black, every glyph fattened by `halo` units a side.
fn text_halo(x: f64, y: f64, words: &str, set: &Set, halo: f64) -> String {
    let paint = format!(
        "fill=\"#000\" stroke=\"#000\" stroke-width=\"{}\" stroke-linejoin=\"round\"",
        num(2.0 * halo)
    );
    setting(x, y, words, set, &paint)
}

fn setting(x: f64, y: f64, words: &str, set: &Set, paint: &str) -> String {
    let spacing =
        set.spacing.map(|s| format!(" letter-spacing=\"{}\"", num(s))).unwrap_or_default();
    format!(
        "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\" font-weight=\"{}\" {paint} \
         text-anchor=\"{}\"{spacing}>{}</text>",
        num(x),
        num(y),
        set.font,
        num(set.size),
        set.weight,
        set.anchor,
        esc(words)
    )
}

fn svg(w: f64, h: f64, inner: &str, label: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" \
         role=\"img\" aria-label=\"{}\">{inner}</svg>\n",
        esc(label),
        w = num(w),
        h = num(h)
    )
}

fn plate(w: f64, h: f64, p: &Palette, radius: f64) -> String {
    format!(
        "<rect width=\"{}\" height=\"{}\" rx=\"{}\" fill=\"{}\"/>",
        num(w),
        num(h),
        num(radius),
        p.bg
    )
}

/// A ring of radius 30 centred on (`cx`, `cy`), broken by a gap of radius 15 around the Hα
/// electron. It strokes in the enclosing group's ink; `id` names the gap's mask and is unique per
/// document.
fn ring(p: &Palette, cx: f64, cy: f64, id: &str) -> String {
    let (co, si) = angle();
    let (ex, ey) = (num(cx + O_R * co), num(cy + O_R * si));
    format!(
        "<mask id=\"{id}\" maskUnits=\"userSpaceOnUse\" stroke=\"none\" x=\"{x}\" y=\"{y}\" \
         width=\"100\" height=\"100\"><rect x=\"{x}\" y=\"{y}\" width=\"100\" height=\"100\" \
         fill=\"#fff\"/><circle cx=\"{ex}\" cy=\"{ey}\" r=\"15\" fill=\"#000\"/></mask>\
         <circle cx=\"{cx}\" cy=\"{cy}\" r=\"30\" mask=\"url(#{id})\"/>\
         <circle cx=\"{ex}\" cy=\"{ey}\" r=\"9\" fill=\"{ha}\" stroke=\"none\"/>",
        x = num(cx - 50.0),
        y = num(cy - 50.0),
        cx = num(cx),
        cy = num(cy),
        ha = p.ha,
    )
}

/// The mark: the wordmark's o alone, `size` across, centred on (`cx`, `cy`).
fn mark(p: &Palette, cx: f64, cy: f64, size: f64) -> String {
    format!(
        "<g transform=\"translate({} {}) scale({})\" fill=\"none\" stroke=\"{}\" stroke-width=\"12\">\
         {}</g>",
        num(cx),
        num(cy),
        places(size / MARK_BOX, 4),
        p.ink,
        ring(p, 0.0, 0.0, "mark-gap")
    )
}

/// The radius of the mark's ring when the mark is `size` across.
fn mark_ring(size: f64) -> f64 {
    size * O_R / MARK_BOX
}

fn wordmark_frame(x: f64, y: f64, h: f64) -> String {
    format!(
        "translate({} {}) scale({}) translate({WM_INSET} {WM_INSET})",
        num(x),
        num(y),
        places(h / WM_H, 4)
    )
}

/// The wordmark with its top-left corner at (`x`, `y`), `h` tall: rings of radius 30 and strokes
/// 12 wide, the o broken around the Hα electron.
fn wordmark(p: &Palette, x: f64, y: f64, h: f64) -> String {
    format!(
        "<g transform=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"12\">{LETTERS}{}</g>",
        wordmark_frame(x, y, h),
        p.ink,
        ring(p, O.0, O.1, "o-gap")
    )
}

/// The wordmark's silhouette for an orbit mask: black, every stroke fattened by `halo` canvas units
/// a side, and the o filled.
fn wordmark_halo(x: f64, y: f64, h: f64, halo: f64) -> String {
    format!(
        "<g transform=\"{}\" fill=\"none\" stroke=\"#000\" stroke-width=\"{}\" stroke-linecap=\"round\" \
         stroke-linejoin=\"round\">{LETTERS}<circle cx=\"{}\" cy=\"{}\" r=\"{O_R}\" fill=\"#000\"/></g>",
        wordmark_frame(x, y, h),
        num(12.0 + 2.0 * halo * WM_H / h),
        num(O.0),
        num(O.1)
    )
}

fn wordmark_width(h: f64) -> f64 {
    WM_W * h / WM_H
}

/// Orbits about a centre, the seed that places their electrons, and the text they keep clear of.
struct Atom {
    cx: f64,
    cy: f64,
    /// The radius of orbit 1; orbit n stands at n²·r.
    r: f64,
    orbits: Vec<u32>,
    seed: u64,
    w: f64,
    h: f64,
    /// The text's silhouette, already fattened by its halo; the orbits are masked off it.
    halo: String,
    /// Boxes `[x0, y0, x1, y1]` an electron's centre stays `CLEAR` units outside of.
    avoid: Vec<[f64; 4]>,
}

#[derive(Debug, PartialEq)]
struct Electron {
    orbit: u32,
    x: f64,
    y: f64,
}

/// The tape's first `n` bytes for `seed`.
fn tape(seed: u64, n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    Tape::from_seed(Seed(seed)).read_at(0, &mut bytes);
    bytes
}

/// The angle two tape bytes give: a big-endian 16-bit fraction of a full turn.
fn turn(hi: u8, lo: u8) -> f64 {
    f64::from(u16::from_be_bytes([hi, lo])) / 65536.0 * 2.0 * PI
}

/// Whether an electron centred at (`x`, `y`) stands inside the frame and clear of the text.
fn fits(a: &Atom, x: f64, y: f64) -> bool {
    let inside = (PAD..=a.w - PAD).contains(&x) && (PAD..=a.h - PAD).contains(&y);
    inside
        && a.avoid.iter().all(|&[x0, y0, x1, y1]| {
            x <= x0 - CLEAR || x >= x1 + CLEAR || y <= y0 - CLEAR || y >= y1 + CLEAR
        })
}

/// One electron per orbit: the i-th orbit reads tape bytes 16·i to 16·i + 15 as eight angles, and
/// the first whose point `fits` wins. An orbit none of them fits stays empty. Each orbit owns its
/// bytes, so a change to one orbit's rules never moves another orbit's electron.
fn electrons(a: &Atom) -> Vec<Electron> {
    let t = tape(a.seed, a.orbits.len() * TRIES * 2);
    a.orbits
        .iter()
        .zip(t.chunks(TRIES * 2))
        .filter_map(|(&n, bytes)| {
            let radius = a.r * f64::from(n * n);
            bytes
                .chunks(2)
                .map(|b| turn(b[0], b[1]))
                .map(|th| (a.cx + radius * th.cos(), a.cy + radius * th.sin()))
                .find(|&(x, y)| fits(a, x, y))
                .map(|(x, y)| Electron { orbit: n, x, y })
        })
        .collect()
}

/// Orbits fade outward, from 0.36 on the second to 0.14 from the sixth on.
fn opacity(n: u32) -> f64 {
    match n {
        2 => 0.36,
        3 => 0.27,
        4 => 0.2,
        5 => 0.16,
        _ => 0.14,
    }
}

/// The orbits in ink, masked off the halo and broken around each electron, then the electrons.
fn atom(p: &Palette, a: &Atom) -> String {
    let placed = electrons(a);
    let (w, h) = (num(a.w), num(a.h));
    let mut s = format!(
        "<defs><mask id=\"orbits\" maskUnits=\"userSpaceOnUse\" stroke=\"none\" x=\"0\" y=\"0\" \
         width=\"{w}\" height=\"{h}\"><rect width=\"{w}\" height=\"{h}\" fill=\"#fff\"/>{}",
        a.halo
    );
    for e in &placed {
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"{ELECTRON_GAP}\" fill=\"#000\"/>",
            num(e.x),
            num(e.y)
        );
    }
    let _ = write!(
        s,
        "</mask></defs><g mask=\"url(#orbits)\" fill=\"none\" stroke=\"{}\" stroke-width=\"{ORBIT_STROKE}\">",
        p.ink
    );
    for &n in &a.orbits {
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"{}\" opacity=\"{}\"/>",
            num(a.cx),
            num(a.cy),
            num(a.r * f64::from(n * n)),
            opacity(n)
        );
    }
    s.push_str("</g>");
    for e in &placed {
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"{ELECTRON_R}\" fill=\"{}\"/>",
            num(e.x),
            num(e.y),
            p.ink
        );
    }
    s
}

/// One centred line of a hero: its baseline, its words, how it is set, and its halo.
struct Line {
    y: f64,
    words: &'static str,
    set: Set,
    halo: f64,
}

/// A banner with the wordmark and its lines centred on the canvas and orbits 2–4 about the word.
struct Hero {
    w: f64,
    h: f64,
    /// The wordmark's height and its top edge.
    wh: f64,
    wy: f64,
    lines: &'static [Line],
    /// Boxes electrons keep clear of, set by hand around the lines for their width in Segoe UI.
    clear: &'static [[f64; 4]],
}

const BANNER: Hero = Hero {
    w: 1600.0,
    h: 440.0,
    wh: 170.0,
    wy: 56.0,
    lines: &[Line { y: 396.0, words: TAGLINE, set: Set::sans(40.0, 600).centred(), halo: 12.0 }],
    clear: &[[170.0, 362.0, 1430.0, 408.0]],
};

const SOCIAL: Hero = Hero {
    w: 1280.0,
    h: 640.0,
    wh: 200.0,
    wy: 104.0,
    lines: &[
        Line {
            y: 500.0,
            words: "Deterministic simulation testing",
            set: Set::sans(46.0, 600).centred(),
            halo: 12.0,
        },
        Line {
            y: 556.0,
            words: "for unmodified Linux programs.",
            set: Set::sans(46.0, 600).centred(),
            halo: 12.0,
        },
        Line {
            y: 606.0,
            words: "github.com/KDaniello/bohr",
            set: Set::mono(20.0, 400).centred().muted(),
            halo: 10.0,
        },
    ],
    clear: &[[250.0, 462.0, 1030.0, 570.0], [480.0, 588.0, 800.0, 612.0]],
};

impl Hero {
    fn scale(&self) -> f64 {
        self.wh / WM_H
    }

    /// The wordmark's left edge, which puts the middle of its ink on the middle of the canvas.
    fn wx(&self) -> f64 {
        self.w / 2.0 - WM_MIDDLE * self.scale()
    }

    fn atom(&self, seed: u64) -> Atom {
        let (k, wx, wy) = (self.scale(), self.wx(), self.wy);
        let mut halo = wordmark_halo(wx, wy, self.wh, WORD_HALO);
        for line in self.lines {
            halo.push_str(&text_halo(self.w / 2.0, line.y, line.words, &line.set, line.halo));
        }
        let mut avoid = vec![[wx, wy, wx + wordmark_width(self.wh), wy + self.wh]];
        avoid.extend_from_slice(self.clear);
        // Orbits about the o make the word look set to the right, orbits about the word's middle
        // make it look set to the left; their centre stands halfway, 10.5 units left of the middle.
        let ox = WM_INSET + O.0;
        Atom {
            cx: wx + (ox + WM_MIDDLE) / 2.0 * k,
            cy: wy + (WM_INSET + O.1) * k,
            r: O_R * k,
            orbits: vec![2, 3, 4],
            seed,
            w: self.w,
            h: self.h,
            halo,
            avoid,
        }
    }

    fn draw(&self, p: &Palette, seed: u64, ground: bool, label: &str) -> String {
        let mut s = if ground { plate(self.w, self.h, p, 0.0) } else { String::new() };
        s.push_str(&atom(p, &self.atom(seed)));
        s.push_str(&wordmark(p, self.wx(), self.wy, self.wh));
        for line in self.lines {
            s.push_str(&text(p, self.w / 2.0, line.y, line.words, &line.set));
        }
        svg(self.w, self.h, &s, label)
    }
}

/// The README banner: no ground, so it stands on GitHub's page in either theme.
fn banner(p: &Palette, seed: u64) -> String {
    BANNER.draw(p, seed, false, &format!("{LABEL} (seed {seed})"))
}

/// The repository preview, on GitHub's ground: link cards show it on grounds of their own.
fn social(p: &Palette, seed: u64) -> String {
    SOCIAL.draw(p, seed, true, LABEL)
}

/// FNV-1a over a string, the seed of a release banner's electrons.
fn fnv(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

const RELEASE_W: f64 = 1600.0;
const RELEASE_H: f64 = 360.0;
/// The release banner's mark: its centre and its size.
const RELEASE_MARK: (f64, f64, f64) = (1300.0, 180.0, 96.0);
/// The release banner's text stands left of x = 800; the orbits about the mark end right of it.
const RELEASE_TEXT: [f64; 4] = [0.0, 0.0, 800.0, RELEASE_H];

fn release_atom(version: &str, name: &str) -> Atom {
    let (mx, my, size) = RELEASE_MARK;
    Atom {
        cx: mx,
        cy: my,
        r: mark_ring(size),
        orbits: vec![2, 3],
        seed: fnv(&format!("{version} {name}")),
        w: RELEASE_W,
        h: RELEASE_H,
        halo: String::new(),
        avoid: vec![RELEASE_TEXT],
    }
}

/// A release banner: the version and its name on clear ground at the left, the mark with orbits 2
/// and 3 at the right.
fn release(p: &Palette, version: &str, name: &str) -> String {
    let (mx, my, size) = RELEASE_MARK;
    let (wx, wy, wh) = (96.0, 44.0, 64.0);
    let mut s = atom(p, &release_atom(version, name));
    s.push_str(&mark(p, mx, my, size));
    s.push_str(&wordmark(p, wx, wy, wh));
    let caption = Set::mono(22.0, 400).muted();
    s.push_str(&text(p, wx + wordmark_width(wh) + 22.0, 96.0, "release", &caption));
    s.push_str(&text(p, 90.0, 254.0, version, &Set::mono(120.0, 500).spaced(-3.0)));
    s.push_str(&text(p, 96.0, 318.0, name, &Set::sans(42.0, 500)));
    svg(RELEASE_W, RELEASE_H, &s, &format!("bohr {version} — {name}"))
}

/// The section divider: an ink hairline broken once, at its centre, by the Hα electron.
fn divider(p: &Palette) -> String {
    let inner = format!(
        "<path d=\"M0 8H780M820 8H1600\" stroke=\"{}\" stroke-opacity=\".22\" stroke-width=\"2\"/>\
         <circle cx=\"800\" cy=\"8\" r=\"6\" fill=\"{}\"/>",
        p.ink, p.ha
    );
    svg(1600.0, 16.0, &inner, "")
}

fn mark_file(p: &Palette) -> String {
    svg(200.0, 200.0, &mark(p, 100.0, 100.0, 160.0), "bohr")
}

fn favicon(p: &Palette) -> String {
    let inner = plate(64.0, 64.0, p, 14.0) + &mark(p, 32.0, 32.0, 46.0);
    svg(64.0, 64.0, &inner, "bohr")
}

fn wordmark_file(p: &Palette) -> String {
    let h = 150.0;
    svg((wordmark_width(h) + 80.0).ceil(), h + 80.0, &wordmark(p, 40.0, 40.0, h), "bohr")
}

/// Every file `make brand` writes except the release banner, by name.
fn assets() -> Vec<(String, String)> {
    // A favicon cannot switch with the theme the way a README `<picture>` does, so it is one file
    // that carries its own ground.
    let mut files = vec![("favicon-dark.svg".to_owned(), favicon(&DARK))];
    for (theme, p) in [("light", &LIGHT), ("dark", &DARK)] {
        files.extend([
            (format!("banner-{theme}.svg"), banner(p, SEED)),
            (format!("social-{theme}.svg"), social(p, SEED)),
            (format!("divider-{theme}.svg"), divider(p)),
            (format!("mark-{theme}.svg"), mark_file(p)),
            (format!("wordmark-{theme}.svg"), wordmark_file(p)),
        ]);
    }
    files
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: brand <dir> [version name]");
        std::process::exit(2);
    };
    let out = Path::new(out);
    std::fs::create_dir_all(out).expect("the output directory");
    let release_named = match (args.get(1), args.get(2)) {
        (Some(version), Some(name)) => Some((version.as_str(), name.as_str())),
        _ => None,
    };
    let mut files = assets();
    if let Some((version, name)) = release_named {
        for (theme, p) in [("light", &LIGHT), ("dark", &DARK)] {
            files.push((format!("release-{version}-{theme}.svg"), release(p, version, name)));
        }
    }
    for (name, body) in files {
        std::fs::write(out.join(&name), body).expect("an asset written");
        println!("{}", out.join(name).display());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn numbers_are_written_as_the_kit_writes_them() {
        assert_eq!(num(96.0), "96");
        assert_eq!(num(12.345), "12.35");
        assert_eq!(num(-0.001), "0");
        assert_eq!(places(128.0 / 112.0, 4), "1.1429");
    }

    #[test]
    fn the_committed_assets_are_the_bytes_the_seed_draws() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/assets");
        for (name, body) in assets() {
            let committed = std::fs::read_to_string(dir.join(&name)).expect("a committed asset");
            assert!(committed == body, "{name} differs from what `make brand` draws");
        }
        assert_eq!(fnv(""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn the_seed_moves_the_electrons() {
        assert_ne!(electrons(&BANNER.atom(SEED)), electrons(&BANNER.atom(SEED + 1)));
        assert_eq!(electrons(&BANNER.atom(SEED)).len(), 3, "seed 1913 sets all three electrons");
    }

    #[test]
    fn electrons_obey_the_placement_rules() {
        let mut atoms = Vec::new();
        for seed in 0..200 {
            atoms.push(BANNER.atom(seed));
            atoms.push(SOCIAL.atom(seed));
            atoms.push(release_atom(&format!("v0.{seed}.0"), "a release"));
        }
        let mut placed = 0;
        for a in &atoms {
            let t = tape(a.seed, a.orbits.len() * TRIES * 2);
            let mut expected = Vec::new();
            for (i, &n) in a.orbits.iter().enumerate() {
                let radius = a.r * f64::from(n * n);
                let first = (0..TRIES)
                    .map(|j| turn(t[16 * i + 2 * j], t[16 * i + 2 * j + 1]))
                    .map(|th| (a.cx + radius * th.cos(), a.cy + radius * th.sin()))
                    .find(|&(x, y)| {
                        x >= PAD
                            && x <= a.w - PAD
                            && y >= PAD
                            && y <= a.h - PAD
                            && a.avoid.iter().all(|b| {
                                x <= b[0] - CLEAR
                                    || x >= b[2] + CLEAR
                                    || y <= b[1] - CLEAR
                                    || y >= b[3] + CLEAR
                            })
                    });
                if let Some((x, y)) = first {
                    assert!(((x - a.cx).hypot(y - a.cy) - radius).abs() < 1e-9);
                    expected.push(Electron { orbit: n, x, y });
                }
            }
            placed += expected.len();
            assert_eq!(electrons(a), expected);
        }
        // A rule that rejected every angle would pass the comparison above with nothing placed.
        let orbits: usize = atoms.iter().map(|a| a.orbits.len()).sum();
        assert!(placed * 2 > orbits, "{placed} electrons on {orbits} orbits");
    }

    #[test]
    fn the_release_orbits_end_right_of_its_text() {
        let a = release_atom("v0.9.1", "honest findings");
        let widest = a.r * f64::from(a.orbits.iter().max().unwrap().pow(2)) + ORBIT_STROKE / 2.0;
        assert!(a.cx - widest > RELEASE_TEXT[2]);
    }

    fn every_file() -> Vec<String> {
        let mut files: Vec<String> = assets().into_iter().map(|(_, body)| body).collect();
        files.extend([&LIGHT, &DARK].map(|p| release(p, "v0.9.1", "honest findings")));
        files
    }

    #[test]
    fn only_the_preview_and_the_favicon_have_a_ground() {
        for p in [&LIGHT, &DARK] {
            let ground = format!("fill=\"{}\"", p.bg);
            assert!(social(p, SEED).contains(&ground));
            for bare in [
                banner(p, SEED),
                release(p, "v0.9.1", "honest findings"),
                divider(p),
                mark_file(p),
                wordmark_file(p),
            ] {
                assert!(!bare.contains(&ground) && !bare.contains("rx=\""));
            }
        }
        assert!(favicon(&DARK).contains("fill=\"#0D1117\""));
    }

    #[test]
    fn the_only_red_is_the_hydrogen_alpha_electron() {
        for p in [&LIGHT, &DARK] {
            for single in
                [banner(p, SEED), social(p, SEED), divider(p), mark_file(p), wordmark_file(p)]
            {
                assert_eq!(single.matches(p.ha).count(), 1);
            }
            // The release banner shows two o's: the wordmark's and the mark.
            assert_eq!(release(p, "v0.9.1", "honest findings").matches(p.ha).count(), 2);
        }
        assert_eq!(favicon(&DARK).matches(DARK.ha).count(), 1);
    }

    #[test]
    fn every_mask_id_is_unique_within_its_file() {
        for file in every_file() {
            let ids: Vec<&str> =
                file.split(" id=\"").skip(1).map(|rest| rest.split('"').next().unwrap()).collect();
            assert_eq!(ids.len(), ids.iter().collect::<BTreeSet<_>>().len(), "{ids:?}");
        }
    }
}
