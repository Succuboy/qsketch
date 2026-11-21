//! Encapsulated PostScript (`.eps`) import.
//!
//! An EPS file is a PostScript program, not a picture, so drawing it properly
//! takes a PostScript interpreter. qsketch doesn't ship one; instead:
//!
//! 1. If Ghostscript is installed (free, `gswin64c.exe` on Windows, `gs`
//!    elsewhere), the file is rendered with it into a sharp, transparent
//!    image sized from the file's bounding box.
//! 2. Otherwise the preview image many EPS files carry is used: a TIFF inside
//!    a "DOS EPS" binary header (Illustrator, CorelDRAW, stock sites), or an
//!    EPSI hex bitmap (`%%BeginPreview`). Usually low resolution; a warning
//!    says so.
//! 3. With neither, opening fails with a message saying how to fix it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};

use crate::raster::Raster;

pub const EXTENSIONS: &[&str] = &["eps", "epsf", "epsi"];

/// Longest side, in pixels, an EPS is rendered at with Ghostscript.
const TARGET_PX: f64 = 3000.0;

pub fn is_eps(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Load an EPS as one raster, plus notes about how it was opened.
pub fn load_with_warnings(path: &Path) -> anyhow::Result<(Raster, Vec<String>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (ps, tiff) = split_dos_eps(&bytes);
    let mut why_not_gs = None;
    if let Some(gs) = find_ghostscript() {
        match render_with_ghostscript(&gs, path, ps) {
            Ok((r, dpi)) => return Ok((r, vec![format!("Rendered with Ghostscript at {dpi} dpi.")])),
            Err(e) => why_not_gs = Some(e),
        }
    }
    let preview = match tiff {
        Some(t) => Some(decode_tiff_preview(t)),
        None => decode_epsi_preview(ps).map(Ok),
    };
    match (preview, why_not_gs) {
        (Some(Ok(r)), None) => Ok((
            r,
            vec![
                "Opened the small preview image stored in this EPS. Install Ghostscript (free, ghostscript.com) and reopen it to get the full-quality vector art."
                    .into(),
            ],
        )),
        (Some(Ok(r)), Some(e)) => {
            Ok((r, vec![format!("Ghostscript couldn't render this EPS ({e}), so the small preview image stored in it was opened instead.")]))
        }
        (Some(Err(pe)), Some(e)) => Err(anyhow!("Ghostscript couldn't render this EPS ({e}) and its preview image couldn't be read ({pe})")),
        (Some(Err(pe)), None) => Err(anyhow!(
            "This EPS's preview image couldn't be read ({pe}). Install Ghostscript (free, ghostscript.com) to open EPS files at full quality."
        )),
        (None, Some(e)) => Err(anyhow!("Ghostscript couldn't render this EPS: {e}")),
        (None, None) => bail!(
            "This EPS has no preview image, and drawing its vector art needs Ghostscript. Install Ghostscript (free, ghostscript.com), then open the file again."
        ),
    }
}

/// A "DOS EPS" file starts with C5 D0 D3 C6 and a table pointing at the
/// PostScript and an optional TIFF preview. Returns (PostScript, TIFF).
fn split_dos_eps(b: &[u8]) -> (&[u8], Option<&[u8]>) {
    if b.len() < 30 || b[..4] != [0xC5, 0xD0, 0xD3, 0xC6] {
        return (b, None);
    }
    let u = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
    let slice = |start: usize, len: usize| -> Option<&[u8]> {
        if len == 0 {
            return None;
        }
        b.get(start..start.checked_add(len)?)
    };
    let ps = slice(u(4), u(8)).unwrap_or(&[]);
    (ps, slice(u(20), u(24)))
}

fn decode_tiff_preview(t: &[u8]) -> anyhow::Result<Raster> {
    let img = image::load_from_memory_with_format(t, image::ImageFormat::Tiff).context("decoding the TIFF preview")?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Raster::from_rgba(w, h, rgba.as_raw()))
}

/// EPSI preview: `%%BeginPreview: w h depth lines` then hex rows, each line
/// starting with `%`. 0 is white, the top value black; rows run top to bottom.
/// White becomes transparent so line art drops onto a layer cleanly.
fn decode_epsi_preview(ps: &[u8]) -> Option<Raster> {
    let text = String::from_utf8_lossy(ps);
    let mut lines = text.lines();
    let head = lines.by_ref().find(|l| l.starts_with("%%BeginPreview:"))?;
    let nums: Vec<u32> = head["%%BeginPreview:".len()..].split_whitespace().filter_map(|s| s.parse().ok()).collect();
    let (w, h, depth) = (*nums.first()?, *nums.get(1)?, *nums.get(2)?);
    if w == 0 || h == 0 || w > 20000 || h > 20000 || !matches!(depth, 1 | 2 | 4 | 8) {
        return None;
    }
    let mut hex = Vec::new();
    for l in lines {
        if l.starts_with("%%EndPreview") {
            break;
        }
        let l = l.trim_start_matches('%');
        hex.extend(l.bytes().filter(|c| c.is_ascii_hexdigit()));
    }
    let data: Vec<u8> = hex.chunks_exact(2).map(|p| (hexval(p[0]) << 4) | hexval(p[1])).collect();
    let row_bytes = (w as usize * depth as usize).div_ceil(8);
    if data.len() < row_bytes * h as usize {
        return None;
    }
    let max = (1u32 << depth) - 1;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &data[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..w as usize {
            let bit = x * depth as usize;
            let v = (row[bit / 8] >> (8 - depth as usize - bit % 8)) as u32 & max;
            let ink = (v * 255 / max) as u8; // 0 = white, max = black
            rgba.extend_from_slice(&[0, 0, 0, ink]);
        }
    }
    Some(Raster::from_rgba(w, h, &rgba))
}

fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// `%%HiResBoundingBox` (or `%%BoundingBox`) in points: (width, height).
fn bbox_size(ps: &[u8]) -> Option<(f64, f64)> {
    let head = String::from_utf8_lossy(&ps[..ps.len().min(64 * 1024)]);
    let mut best = None;
    for key in ["%%HiResBoundingBox:", "%%BoundingBox:"] {
        if let Some(l) = head.lines().find(|l| l.starts_with(key) && !l.contains("(atend)")) {
            let v: Vec<f64> = l[key.len()..].split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if v.len() == 4 && v[2] > v[0] && v[3] > v[1] {
                best = Some((v[2] - v[0], v[3] - v[1]));
                break;
            }
        }
    }
    best
}

fn render_with_ghostscript(gs: &Path, path: &Path, ps: &[u8]) -> anyhow::Result<(Raster, u32)> {
    let (wpt, hpt) = bbox_size(ps).unwrap_or((612.0, 792.0));
    let dpi = (TARGET_PX / wpt.max(hpt) * 72.0).clamp(72.0, 1200.0).round() as u32;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = std::env::temp_dir();
    let out = tmp.join(format!("qsketch-eps-{}-{stamp}.png", std::process::id()));
    // A DOS EPS wraps the PostScript in a binary header; hand Ghostscript just the PostScript.
    let src_tmp;
    let src: &Path = if std::fs::read(path).map(|b| b.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6])).unwrap_or(false) {
        src_tmp = tmp.join(format!("qsketch-eps-{}-{stamp}.ps", std::process::id()));
        std::fs::write(&src_tmp, ps)?;
        &src_tmp
    } else {
        src_tmp = PathBuf::new();
        path
    };
    let mut cmd = Command::new(gs);
    cmd.args([
        "-q",
        "-dSAFER",
        "-dBATCH",
        "-dNOPAUSE",
        "-dEPSCrop",
        "-sDEVICE=pngalpha",
        "-dTextAlphaBits=4",
        "-dGraphicsAlphaBits=4",
        &format!("-r{dpi}"),
    ])
    .arg(format!("-sOutputFile={}", out.display()))
    .arg(src);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
    }
    let result = cmd.output();
    if !src_tmp.as_os_str().is_empty() {
        let _ = std::fs::remove_file(&src_tmp);
    }
    let o = result.context("starting Ghostscript")?;
    if !o.status.success() || !out.exists() {
        let _ = std::fs::remove_file(&out);
        let msg = String::from_utf8_lossy(&o.stderr)
            .lines()
            .chain(String::from_utf8_lossy(&o.stdout).lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_string();
        bail!("{msg}");
    }
    let r = super::image_io::import(&out);
    let _ = std::fs::remove_file(&out);
    Ok((r?, dpi))
}

/// Ghostscript's command-line program: on PATH, or in its usual Windows install folder.
pub fn find_ghostscript() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["gswin64c.exe", "gswin32c.exe"] } else { &["gs"] };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for n in names {
                let p = dir.join(n);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    if cfg!(windows) {
        for root in [std::env::var_os("ProgramFiles"), std::env::var_os("ProgramFiles(x86)")].into_iter().flatten() {
            let base = Path::new(&root).join("gs");
            let Ok(rd) = std::fs::read_dir(&base) else { continue };
            let mut vers: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            vers.sort();
            for v in vers.iter().rev() {
                for n in names {
                    let p = v.join("bin").join(n);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epsi_preview_decodes_black_on_transparent() {
        // 8x2, depth 1: first row all ink, second row none.
        let ps =
            b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 8 2\n%%BeginPreview: 8 2 1 2\n% FF\n% 00\n%%EndPreview\n";
        let r = decode_epsi_preview(ps).unwrap();
        assert_eq!((r.width(), r.height()), (8, 2));
    }

    #[test]
    fn dos_header_is_split() {
        let mut f = vec![0xC5, 0xD0, 0xD3, 0xC6];
        let ps = b"%!PS";
        let start = 30u32;
        f.extend(start.to_le_bytes());
        f.extend((ps.len() as u32).to_le_bytes());
        f.extend([0u8; 16]);
        f.extend([0xFF, 0xFF]);
        f.extend(ps);
        let (p, t) = split_dos_eps(&f);
        assert_eq!(p, ps);
        assert!(t.is_none());
    }

    #[test]
    fn bbox_is_read() {
        assert_eq!(bbox_size(b"%!PS\n%%BoundingBox: 10 20 110 70\n"), Some((100.0, 50.0)));
    }
}
