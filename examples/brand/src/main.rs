//! Writes the brand's SVG files: the mark, the wordmark, the favicon, the README banner, the
//! repository preview, the section divider and a release banner.
//!
//! A banner's spectrum is bohr's own tape for the banner's seed, `Tape::from_seed`: three bytes a
//! line, the first two its wavelength and the third its strength. The four Balmer lines of
//! hydrogen stand fixed in colour; every line the tape draws is ink. The same seed draws the same
//! lines on any machine, which is the claim the banner illustrates.
//!
//! `cargo run --manifest-path examples/brand/Cargo.toml -- docs/assets [version name]`

use std::{f64::consts::PI, fmt::Write as _, path::Path};

use bohr_tape::Tape;
use bohr_types::Seed;

/// One palette: the plate, ink, and the four Balmer lines in the order of `BALMER`.
struct Palette {
    bg: &'static str,
    faint: &'static str,
    ink: &'static str,
    muted: &'static str,
    rule: &'static str,
    lines: [&'static str; 4],
}

const LIGHT: Palette = Palette {
    bg: "#F3F5F8",
    faint: "#E6EAF0",
    ink: "#0F1522",
    muted: "#5A6478",
    rule: "#CDD3DD",
    lines: ["#E0393B", "#0E98AE", "#4A4FDB", "#8434CC"],
};

const DARK: Palette = Palette {
    bg: "#0B0F17",
    faint: "#141A26",
    ink: "#E8EDF4",
    muted: "#8B95A9",
    rule: "#263043",
    lines: ["#FF5B55", "#3CCFE2", "#7F85FF", "#B26CFF"],
};

/// Hydrogen's Balmer lines, in air: the name and the wavelength in nanometres.
const BALMER: [(&str, f64); 4] =
    [("Hα", 656.28), ("Hβ", 486.13), ("Hγ", 434.05), ("Hδ", 410.17)];

/// The banner's seed: 1913, the year of Bohr's model.
const SEED: u64 = 1913;

const SANS: &str = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif";
const MONO: &str =
    "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace";

/// The wordmark's own units.
const WM_W: f64 = 276.0;
const WM_H: f64 = 112.0;

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

/// The electron's angle: up and to the right, at −40° from the horizontal.
fn angle() -> (f64, f64) {
    let a = -40.0 * PI / 180.0;
    (a.cos(), a.sin())
}

/// How a piece of text is set.
struct Set {
    font: &'static str,
    size: f64,
    weight: u32,
    fill: &'static str,
    anchor: &'static str,
    spacing: Option<f64>,
}

impl Set {
    const fn sans(size: f64, weight: u32, fill: &'static str) -> Self {
        Self { font: SANS, size, weight, fill, anchor: "start", spacing: None }
    }

    const fn mono(size: f64, fill: &'static str) -> Self {
        Self { font: MONO, size, weight: 400, fill, anchor: "start", spacing: None }
    }

    const fn anchored(self, anchor: &'static str) -> Self {
        Self { anchor, ..self }
    }
}

fn text(x: f64, y: f64, words: &str, set: &Set) -> String {
    let spacing = set.spacing.map(|s| format!(" letter-spacing=\"{}\"", num(s))).unwrap_or_default();
    format!(
        "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\" font-weight=\"{}\" fill=\"{}\" \
         text-anchor=\"{}\"{spacing}>{}</text>",
        num(x),
        num(y),
        set.font,
        num(set.size),
        set.weight,
        set.fill,
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

/// The mark: orbits at radii 5·n² — 5, 20 and 45 — and the electron just after the 3 → 2
/// transition that emits Hα. Below 32 px the inner orbit and the transition go.
fn mark(p: &Palette, cx: f64, cy: f64, size: f64, small: bool) -> String {
    let (co, si) = angle();
    let mut s = format!(
        "<g transform=\"translate({} {}) scale({})\" fill=\"none\" stroke=\"{}\">",
        num(cx),
        num(cy),
        num(size / 100.0),
        p.ink
    );
    if small {
        s.push_str("<circle r=\"18.67\" stroke-width=\"7\"/><circle r=\"42\" stroke-width=\"7\"/>");
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"9\" fill=\"{}\" stroke=\"none\"/>",
            num(18.67 * co),
            num(18.67 * si),
            p.lines[0]
        );
    } else {
        s.push_str(
            "<circle r=\"5\" stroke-width=\"2.4\"/><circle r=\"20\" stroke-width=\"2.6\"/>\
             <circle r=\"45\" stroke-width=\"2.6\"/>",
        );
        let _ = write!(
            s,
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"1.8\" \
             stroke-dasharray=\"2.6 2.4\"/>",
            num(40.4 * co),
            num(40.4 * si),
            num(25.4 * co),
            num(25.4 * si),
            p.lines[0]
        );
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"4.2\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.8\"/>",
            num(45.0 * co),
            num(45.0 * si),
            p.bg,
            p.lines[0]
        );
        let _ = write!(
            s,
            "<circle cx=\"{}\" cy=\"{}\" r=\"4.8\" fill=\"{}\" stroke=\"none\"/>",
            num(20.0 * co),
            num(20.0 * si),
            p.lines[0]
        );
        let _ = write!(s, "<circle r=\"1.9\" fill=\"{}\" stroke=\"none\"/>", p.ink);
    }
    s.push_str("</g>");
    s
}

/// The wordmark, from circles of radius 30 and strokes 11 units wide; its o is the atom.
fn wordmark(p: &Palette, x: f64, y: f64, h: f64) -> String {
    let (co, si) = angle();
    let (ox, oy, r2) = (108.0, 70.0, 13.33);
    format!(
        "<g transform=\"translate({} {}) scale({}) translate(6 6)\" fill=\"none\" stroke=\"{ink}\" \
         stroke-width=\"11\"><path d=\"M0 0V100\"/><circle cx=\"30\" cy=\"70\" r=\"30\"/>\
         <circle cx=\"{ox}\" cy=\"{oy}\" r=\"30\"/><circle cx=\"{ox}\" cy=\"{oy}\" r=\"{r2}\" \
         stroke-width=\"3\"/><circle cx=\"{ox}\" cy=\"{oy}\" r=\"3.2\" fill=\"{ink}\" stroke=\"none\"/>\
         <circle cx=\"{ex}\" cy=\"{ey}\" r=\"5.6\" fill=\"{ha}\" stroke=\"none\"/>\
         <path d=\"M156 0V100M156 70A30 30 0 0 1 216 70V100\"/>\
         <path d=\"M234 40V100M234 70A30 30 0 0 1 264 40\"/></g>",
        num(x),
        num(y),
        places(h / WM_H, 4),
        ink = p.ink,
        ox = num(ox),
        oy = num(oy),
        r2 = num(r2),
        ex = num(ox + r2 * co),
        ey = num(oy + r2 * si),
        ha = p.lines[0],
    )
}

fn wordmark_width(h: f64) -> f64 {
    WM_W * h / WM_H
}

/// What a spectrum labels its Balmer lines with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Labels {
    None,
    Short,
    Full,
}

/// Where and how a spectrum is drawn.
struct Spectrum {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    seed: u64,
    count: usize,
    labels: Labels,
    axis: bool,
    size: f64,
    reach: f64,
    band: bool,
}

/// The tape's first `n` bytes for `seed`.
fn tape(seed: u64, n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    Tape::from_seed(Seed(seed)).read_at(0, &mut bytes);
    bytes
}

/// The visible spectrum, 380–700 nm: the tape's lines in ink, the Balmer lines in colour.
fn spectrum(p: &Palette, o: &Spectrum) -> String {
    let (lo, hi) = (380.0, 700.0);
    let at = |nm: f64| o.x + (nm - lo) / (hi - lo) * o.w;
    let t = tape(o.seed, o.count * 3);
    let lw = (o.w / 860.0).max(1.0);
    let mut s = String::new();
    if o.band {
        let _ = write!(
            s,
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
            num(o.x),
            num(o.y),
            num(o.w),
            num(o.h),
            p.faint
        );
    }
    for i in 0..o.count {
        let nm = lo + f64::from(u32::from(t[3 * i]) << 8 | u32::from(t[3 * i + 1])) / 65536.0
            * (hi - lo);
        if BALMER.iter().any(|(_, line)| (line - nm).abs() < 2.4) {
            continue;
        }
        let a = f64::from(t[3 * i + 2]) / 255.0;
        let _ = write!(
            s,
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\" opacity=\"{:.3}\"/>",
            num(at(nm) - lw / 2.0),
            num(o.y),
            num(lw),
            num(o.h),
            p.ink,
            0.08 + 0.6 * a * a
        );
    }
    let bw = lw * 3.2;
    for (index, (name, nm)) in BALMER.iter().enumerate() {
        let colour = p.lines[index];
        let _ = write!(
            s,
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{colour}\"/>",
            num(at(*nm) - bw / 2.0),
            num(o.y - o.reach),
            num(bw),
            num(o.h + 2.0 * o.reach)
        );
        let label = match o.labels {
            Labels::None => None,
            Labels::Short => Some((*name).to_owned()),
            Labels::Full => Some(format!("{name} {nm:.1}")),
        };
        if let Some(label) = label {
            // Hδ and Hγ stand 24 nm apart, closer than two labels are wide, so Hδ's is set to the
            // left of its line and Hγ's to the right.
            let (anchor, dx) = match *name {
                "Hδ" => ("end", -4.0),
                "Hγ" => ("start", 4.0),
                _ => ("middle", 0.0),
            };
            let set = Set::mono(o.size, colour).anchored(anchor);
            s.push_str(&text(at(*nm) + dx, o.y - o.reach - 8.0, &label, &set));
        }
    }
    if o.axis {
        let ay = o.y + o.h + o.reach + 6.0;
        let _ = write!(
            s,
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
            num(o.x),
            num(ay),
            num(o.x + o.w),
            num(ay),
            p.rule
        );
        for nm in (380..=700).step_by(10) {
            let major = nm % 50 == 0;
            let x = at(f64::from(nm));
            let _ = write!(
                s,
                "<line x1=\"{x}\" y1=\"{}\" x2=\"{x}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
                num(ay),
                num(ay + if major { 8.0 } else { 4.0 }),
                p.muted,
                x = num(x)
            );
            if major {
                let set = Set::mono(o.size, p.muted).anchored("middle");
                s.push_str(&text(x, ay + 8.0 + o.size + 3.0, &nm.to_string(), &set));
            }
        }
        // After the axis, where it cannot cover the last number.
        let set = Set::mono(o.size, p.muted);
        s.push_str(&text(o.x + o.w + o.size * 1.6, ay + 8.0 + o.size + 3.0, "nm", &set));
    }
    s
}

const TAGLINE: &str = "Deterministic simulation testing for unmodified Linux programs.";
const LABEL: &str = "bohr — deterministic simulation testing for unmodified Linux programs";

fn banner(p: &Palette) -> String {
    let mut s = plate(1600.0, 400.0, p, 18.0);
    s.push_str(&wordmark(p, 96.0, 92.0, 128.0));
    let seed = format!("seed 0x{SEED:016x} · tape 780 B");
    s.push_str(&text(1504.0, 62.0, &seed, &Set::mono(15.0, p.muted).anchored("end")));
    s.push_str(&spectrum(
        p,
        &Spectrum {
            x: 500.0,
            y: 110.0,
            w: 1004.0,
            h: 96.0,
            seed: SEED,
            count: 260,
            labels: Labels::Full,
            axis: true,
            size: 15.0,
            reach: 6.0,
            band: true,
        },
    ));
    s.push_str(&text(96.0, 330.0, TAGLINE, &Set::sans(36.0, 600, p.ink)));
    svg(1600.0, 400.0, &s, LABEL)
}

fn social(p: &Palette) -> String {
    let mut s = plate(1280.0, 640.0, p, 0.0);
    s.push_str(&wordmark(p, 96.0, 110.0, 184.0));
    s.push_str(&text(
        1184.0,
        92.0,
        "github.com/KDaniello/bohr",
        &Set::mono(20.0, p.muted).anchored("end"),
    ));
    s.push_str(&text(96.0, 388.0, "Deterministic simulation testing", &Set::sans(48.0, 600, p.ink)));
    s.push_str(&text(96.0, 446.0, "for unmodified Linux programs.", &Set::sans(48.0, 600, p.ink)));
    s.push_str(&spectrum(
        p,
        &Spectrum {
            x: 96.0,
            y: 504.0,
            w: 1088.0,
            h: 52.0,
            seed: SEED,
            count: 240,
            labels: Labels::Short,
            axis: true,
            size: 14.0,
            reach: 5.0,
            band: true,
        },
    ));
    svg(1280.0, 640.0, &s, LABEL)
}

/// FNV-1a over a string, the seed of a release's spectrum.
fn fnv(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn release(p: &Palette, version: &str, name: &str) -> String {
    let seed = fnv(&format!("{version} {name}"));
    let mut s = plate(1600.0, 360.0, p, 18.0);
    s.push_str(&wordmark(p, 96.0, 56.0, 52.0));
    s.push_str(&text(96.0 + wordmark_width(52.0) + 18.0, 98.0, "release", &Set::mono(18.0, p.muted)));
    let big = Set { weight: 500, spacing: Some(-3.0), ..Set::mono(108.0, p.ink) };
    s.push_str(&text(90.0, 240.0, version, &big));
    s.push_str(&text(96.0, 298.0, name, &Set::sans(40.0, 500, p.ink)));
    let said = format!("seed fnv1a(\"{version} {name}\")");
    s.push_str(&text(1504.0, 62.0, &said, &Set::mono(14.0, p.muted).anchored("end")));
    s.push_str(&spectrum(
        p,
        &Spectrum {
            x: 780.0,
            y: 118.0,
            w: 724.0,
            h: 120.0,
            seed,
            count: 200,
            labels: Labels::Full,
            axis: true,
            size: 14.0,
            reach: 6.0,
            band: true,
        },
    ));
    svg(1600.0, 360.0, &s, &format!("bohr {version} — {name}"))
}

fn divider(p: &Palette) -> String {
    let line = spectrum(
        p,
        &Spectrum {
            x: 0.0,
            y: 8.0,
            w: 1600.0,
            h: 8.0,
            seed: SEED,
            count: 320,
            labels: Labels::None,
            axis: false,
            size: 14.0,
            reach: 6.0,
            band: false,
        },
    );
    svg(1600.0, 24.0, &line, "")
}

fn mark_file(p: &Palette) -> String {
    svg(400.0, 400.0, &mark(p, 200.0, 200.0, 300.0, false), "bohr mark")
}

fn favicon(p: &Palette) -> String {
    let inner = plate(64.0, 64.0, p, 14.0) + &mark(p, 32.0, 32.0, 52.0, true);
    svg(64.0, 64.0, &inner, "bohr")
}

fn wordmark_file(p: &Palette) -> String {
    let h = 150.0;
    svg((wordmark_width(h) + 80.0).ceil(), h + 80.0, &wordmark(p, 40.0, 40.0, h), "bohr")
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
    for (theme, p) in [("light", &LIGHT), ("dark", &DARK)] {
        let mut files = vec![
            (format!("banner-{theme}.svg"), banner(p)),
            (format!("social-{theme}.svg"), social(p)),
            (format!("divider-{theme}.svg"), divider(p)),
            (format!("mark-{theme}.svg"), mark_file(p)),
            (format!("favicon-{theme}.svg"), favicon(p)),
            (format!("wordmark-{theme}.svg"), wordmark_file(p)),
        ];
        if let Some((version, name)) = release_named {
            files.push((format!("release-{version}-{theme}.svg"), release(p, version, name)));
        }
        for (name, body) in files {
            std::fs::write(out.join(&name), body).expect("an asset written");
            println!("{}", out.join(name).display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_as_the_kit_writes_them() {
        assert_eq!(num(96.0), "96");
        assert_eq!(num(12.345), "12.35");
        assert_eq!(num(-0.001), "0");
        assert_eq!(places(128.0 / 112.0, 4), "1.1429");
    }

    #[test]
    fn the_same_seed_draws_the_same_banner() {
        assert_eq!(banner(&LIGHT), banner(&LIGHT));
        assert_ne!(banner(&LIGHT), banner(&DARK));
        assert_eq!(fnv(""), 0xcbf2_9ce4_8422_2325);
    }
}
