//! Terminal output: colors only on a terminal (anstream strips them in a pipe or a file, and
//! honors NO_COLOR and CLICOLOR_FORCE; it also turns them on in the Windows console), and a
//! closed pipe (`gpuzip l big.zip | head`) ends the program quietly instead of panicking.

use anstyle::AnsiColor;
pub use anstyle::Style;
use std::fmt::Display;
use std::io::Write;

/// println! through anstream, see the module comment
macro_rules! say {
    () => { $crate::ui::say_line(format_args!("")) };
    ($($arg:tt)*) => { $crate::ui::say_line(format_args!($($arg)*)) };
}
pub(crate) use say;

pub fn say_line(args: std::fmt::Arguments) {
    let mut out = anstream::stdout().lock();
    if let Err(e) = writeln!(out, "{args}") {
        // a reader that went away: BrokenPipe on Unix, ERROR_NO_DATA (232) on Windows
        if e.kind() == std::io::ErrorKind::BrokenPipe || e.raw_os_error() == Some(232) {
            std::process::exit(0);
        }
    }
}

pub const GOOD: Style = AnsiColor::Green.on_default().bold();
pub const WARN: Style = AnsiColor::Yellow.on_default().bold();
pub const BAD: Style = AnsiColor::Red.on_default().bold();
pub const INFO: Style = AnsiColor::Cyan.on_default().bold();
pub const BOLD: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();

/// `text` in `style`
pub fn paint(style: Style, text: impl Display) -> String {
    format!("{style}{text}{style:#}")
}

/// "N failed" in red, or "0 failed" in green
pub fn failed(n: usize) -> String {
    paint(if n == 0 { GOOD } else { BAD }, format!("{n} failed"))
}

/// A bar of `width` cells for shares that add up to at most 1, each drawn with its own glyph in
/// its own style (the glyphs tell the parts apart without color); the rest dotted
pub fn bar(parts: &[(f64, Style, char)], width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for (share, style, glyph) in parts {
        let mut n = (share * width as f64).round() as usize;
        if *share > 0.0 && n == 0 {
            n = 1; // a share too small to round to a cell still shows
        }
        n = n.min(width - used);
        used += n;
        out.push_str(&paint(*style, glyph.to_string().repeat(n)));
    }
    out + &paint(DIM, "·".repeat(width - used))
}

/// `path` as the user would type it: relative to the current folder when it lies inside it
pub fn shown_path(path: &std::path::Path) -> String {
    let rel = std::env::current_dir().ok().and_then(|cwd| path.strip_prefix(cwd).ok().map(|p| p.to_path_buf()));
    match rel {
        Some(r) if r.as_os_str().is_empty() => ".".into(),
        Some(r) => std::path::Path::new(".").join(r).display().to_string(),
        None => path.display().to_string(),
    }
}
