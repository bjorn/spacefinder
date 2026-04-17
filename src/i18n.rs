//! Tiny gettext-style translator for Rust-side user-visible strings.
//!
//! The Slint side already loads bundled `.po` catalogs via `slint-build`'s
//! `with_bundled_translations`, but that infrastructure only resolves the
//! specific `@tr(...)` call sites baked into the generated UI code; it does
//! not expose a generic runtime lookup. Rather than pulling in `gettext-rs`
//! (which depends on system libgettext) or maintaining a second catalog, we
//! parse the same `.po` files at build time via `include_str!` and serve
//! lookups from a small in-memory map.
//!
//! All Rust-side entries live under `msgctxt "Rust"` to keep them in their
//! own namespace, well clear of the Slint widget contexts.
//!
//! Plural forms follow the gettext convention: `msgid_plural` declares the
//! plural form and `msgstr[N]` provides each variant. We hard-code the
//! "n != 1" rule, which matches both English and German (and what the
//! existing `Plural-Forms` headers in our `.po` files already declare).

use std::collections::HashMap;
use std::sync::OnceLock;

const RUST_CONTEXT: &str = "Rust";

/// Embedded catalogs. Add new languages here as the project grows; the
/// English source strings are intentionally NOT translated (the file just
/// pins the canonical msgids).
const CATALOGS: &[(&str, &str)] = &[
    ("de", include_str!("../lang/de/LC_MESSAGES/space.po")),
    ("en", include_str!("../lang/en/LC_MESSAGES/space.po")),
];

#[derive(Default)]
struct Catalog {
    /// Singular: msgid -> msgstr (only entries with non-empty msgstr).
    singular: HashMap<String, String>,
    /// Plural: msgid -> (msgstr[0], msgstr[1]).
    plural: HashMap<String, (String, String)>,
}

static ACTIVE: OnceLock<Catalog> = OnceLock::new();

/// Initialise the translator from the system locale (LC_ALL > LC_MESSAGES >
/// LANG). Returns the chosen language code, or "en" if no catalog matched.
/// Safe to call exactly once; subsequent calls are no-ops.
pub fn init() -> &'static str {
    let locale = detect_locale();
    let lang = pick_language(&locale);
    let catalog = CATALOGS
        .iter()
        .find(|(code, _)| *code == lang)
        .map(|(_, src)| parse_po(src))
        .unwrap_or_default();
    let _ = ACTIVE.set(catalog);
    // Leak a static str by matching against the known set, so callers can
    // log it without lifetime games.
    CATALOGS
        .iter()
        .find(|(code, _)| *code == lang)
        .map(|(code, _)| *code)
        .unwrap_or("en")
}

fn detect_locale() -> String {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return v;
            }
        }
    }
    String::new()
}

fn pick_language(locale: &str) -> String {
    // Strip encoding (".UTF-8") and region ("_DE", "@euro").
    let base = locale.split(['.', '@']).next().unwrap_or("");
    let lang = base.split('_').next().unwrap_or("");
    lang.to_ascii_lowercase()
}

/// Translate a single fixed string. Falls back to the source string if no
/// translation is registered (the catalog is missing or the entry is empty).
pub fn tr(s: &str) -> String {
    match ACTIVE.get().and_then(|c| c.singular.get(s)) {
        Some(t) => t.clone(),
        None => s.to_owned(),
    }
}

/// Translate a plural pair, picking the form for `n` ("n != 1" rule).
pub fn tr_n(singular: &str, plural: &str, n: usize) -> String {
    if let Some((s_form, p_form)) = ACTIVE.get().and_then(|c| c.plural.get(singular)) {
        return if n == 1 { s_form.clone() } else { p_form.clone() };
    }
    if n == 1 { singular.to_owned() } else { plural.to_owned() }
}

/// Convenience: translate then substitute `{}` placeholders, in order.
pub fn tr_fmt(s: &str, args: &[&dyn std::fmt::Display]) -> String {
    let template = tr(s);
    interpolate(&template, args)
}

/// Convenience: plural-translate then substitute.
pub fn tr_n_fmt(singular: &str, plural: &str, n: usize, args: &[&dyn std::fmt::Display]) -> String {
    let template = tr_n(singular, plural, n);
    interpolate(&template, args)
}

fn interpolate(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    let mut next_arg = 0;
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'}') {
            chars.next();
            if let Some(a) = args.get(next_arg) {
                use std::fmt::Write;
                let _ = write!(out, "{}", a);
            }
            next_arg += 1;
        } else {
            out.push(c);
        }
    }
    out
}

/// Minimal `.po` parser. Recognises:
///   - `msgctxt "..."`
///   - `msgid "..."`
///   - `msgid_plural "..."`
///   - `msgstr "..."`  or  `msgstr[N] "..."`
/// Continuation lines (`"..."` on the line below) are concatenated.
/// Comments (`#...`) and the empty header entry are skipped.
fn parse_po(src: &str) -> Catalog {
    let mut cat = Catalog::default();
    let mut lines = src.lines().peekable();

    // Current entry being assembled.
    let mut ctx: Option<String> = None;
    let mut id: Option<String> = None;
    let mut id_plural: Option<String> = None;
    let mut strs: Vec<(usize, String)> = Vec::new();
    let mut singular_str: Option<String> = None;

    fn flush(
        cat: &mut Catalog,
        ctx: &mut Option<String>,
        id: &mut Option<String>,
        id_plural: &mut Option<String>,
        singular_str: &mut Option<String>,
        strs: &mut Vec<(usize, String)>,
    ) {
        if let Some(id) = id.take() {
            let in_rust = ctx.as_deref() == Some(RUST_CONTEXT);
            if in_rust && !id.is_empty() {
                if let Some(p) = id_plural.take() {
                    let _ = p;
                    let mut s0 = String::new();
                    let mut s1 = String::new();
                    for (n, s) in strs.drain(..) {
                        match n {
                            0 => s0 = s,
                            1 => s1 = s,
                            _ => {}
                        }
                    }
                    if !s0.is_empty() && !s1.is_empty() {
                        cat.plural.insert(id, (s0, s1));
                    }
                } else if let Some(s) = singular_str.take() {
                    if !s.is_empty() {
                        cat.singular.insert(id, s);
                    }
                }
            }
        }
        ctx.take();
        id_plural.take();
        singular_str.take();
        strs.clear();
    }

    enum Last {
        None,
        Ctx,
        Id,
        IdPlural,
        Str(usize),
    }
    let mut last = Last::None;

    while let Some(raw) = lines.next() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            if line.is_empty() {
                flush(
                    &mut cat,
                    &mut ctx,
                    &mut id,
                    &mut id_plural,
                    &mut singular_str,
                    &mut strs,
                );
                last = Last::None;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("msgctxt ") {
            flush(
                &mut cat,
                &mut ctx,
                &mut id,
                &mut id_plural,
                &mut singular_str,
                &mut strs,
            );
            ctx = Some(unquote(rest));
            last = Last::Ctx;
        } else if let Some(rest) = line.strip_prefix("msgid_plural ") {
            id_plural = Some(unquote(rest));
            last = Last::IdPlural;
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            // A bare msgid without preceding msgctxt starts a new entry.
            if matches!(last, Last::Str(_)) {
                flush(
                    &mut cat,
                    &mut ctx,
                    &mut id,
                    &mut id_plural,
                    &mut singular_str,
                    &mut strs,
                );
            }
            id = Some(unquote(rest));
            last = Last::Id;
        } else if let Some(rest) = line.strip_prefix("msgstr[") {
            // msgstr[N] "..."
            let close = rest.find(']');
            if let Some(close) = close {
                let n: usize = rest[..close].parse().unwrap_or(0);
                let after = rest[close + 1..].trim_start();
                strs.push((n, unquote(after)));
                last = Last::Str(n);
            }
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            singular_str = Some(unquote(rest));
            last = Last::Str(0);
        } else if line.starts_with('"') {
            // Continuation of the previous field.
            let extra = unquote(line);
            match last {
                Last::Ctx => {
                    if let Some(c) = ctx.as_mut() {
                        c.push_str(&extra);
                    }
                }
                Last::Id => {
                    if let Some(c) = id.as_mut() {
                        c.push_str(&extra);
                    }
                }
                Last::IdPlural => {
                    if let Some(c) = id_plural.as_mut() {
                        c.push_str(&extra);
                    }
                }
                Last::Str(0) if singular_str.is_some() => {
                    if let Some(c) = singular_str.as_mut() {
                        c.push_str(&extra);
                    }
                }
                Last::Str(n) => {
                    if let Some((_, last_s)) =
                        strs.iter_mut().rev().find(|(idx, _)| *idx == n)
                    {
                        last_s.push_str(&extra);
                    }
                }
                Last::None => {}
            }
        }
    }
    // Flush trailing entry (file may not end with blank line).
    flush(
        &mut cat,
        &mut ctx,
        &mut id,
        &mut id_plural,
        &mut singular_str,
        &mut strs,
    );

    cat
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_singular_with_rust_context() {
        let src = r#"
msgctxt "Rust"
msgid "Open"
msgstr "Öffnen"
"#;
        let cat = parse_po(src);
        assert_eq!(cat.singular.get("Open").map(String::as_str), Some("Öffnen"));
    }

    #[test]
    fn skips_other_contexts() {
        let src = r#"
msgctxt "MainWindow"
msgid "Confirm"
msgstr "Bestätigen"
"#;
        let cat = parse_po(src);
        assert!(cat.singular.is_empty());
    }

    #[test]
    fn parses_plurals() {
        let src = r#"
msgctxt "Rust"
msgid "{} item"
msgid_plural "{} items"
msgstr[0] "{} Eintrag"
msgstr[1] "{} Einträge"
"#;
        let cat = parse_po(src);
        let (s, p) = cat.plural.get("{} item").unwrap();
        assert_eq!(s, "{} Eintrag");
        assert_eq!(p, "{} Einträge");
    }

    #[test]
    fn pick_language_strips_region_and_encoding() {
        assert_eq!(pick_language("de_DE.UTF-8"), "de");
        assert_eq!(pick_language("en"), "en");
        assert_eq!(pick_language(""), "");
    }
}
