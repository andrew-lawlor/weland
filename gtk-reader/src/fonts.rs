//! Loads the vendored reading fonts (self-hosted, no runtime network calls
//! — CLAUDE.md) into this process's font list, per-process only, never
//! installed system-wide.
//!
//! This took two real, non-obvious findings to get working, both confirmed
//! empirically (not assumed) via an isolated probe binary before touching
//! this file:
//!
//! 1. **Pango rejects WOFF2 outright**, regardless of any Fontconfig/font-map
//!    wiring. `pango_fc_is_supported_font_format()` (pangofc-fontmap.c) only
//!    accepts `FC_FONT_WRAPPER == "SFNT"`; every vendored `.woff2` reports
//!    `fontwrapper=WOFF2`, so Pango's own family enumeration and font
//!    matching silently exclude them — even though FreeType itself decodes
//!    WOFF2 fine (confirmed via `fc-query`, and raw `FcFontMatch` resolves
//!    them correctly too) and even though `pango_font_map_list_families()`/
//!    `context.load_font()` report zero errors, just a fallback font. The
//!    fix is what the rewrite plan's own worst-case fallback already
//!    anticipated: re-vendor as plain SFNT (`.ttf`, via `woff2_decompress`,
//!    a 1:1 table decompression — no re-encoding, so family/style naming is
//!    unchanged from the original `.woff2`).
//! 2. **`FcConfigAppFontAddFile` fonts (`FcSetApplication`) never reach
//!    Pango's own font map**, no matter how the config/map is wired
//!    (verified: fresh `PangoCairoFontMap`, explicit `pango_fc_font_map_
//!    set_config`, `pango_context_set_font_map` on the widget's own
//!    context — all confirmed bound via pointer-identity readback, all
//!    still failed). What *does* work: build a from-scratch `FcConfig`
//!    (starting from `FcInitLoadConfigAndFonts()` so cache dirs are already
//!    valid — an empty `FcConfigCreate()` produced "no writable cache
//!    directories" and only 4 generic families), merge in one `<dir>`
//!    pointing at our fonts directory via `FcConfigParseAndLoadFromMemory`
//!    + `FcConfigBuildFonts` (this puts our fonts in `FcSetSystem`, which
//!    Pango's font map *does* read), then bind that config to a **fresh**
//!    `PangoCairoFontMap` via `pango_fc_font_map_set_config` and install it
//!    as the process default. Do **not** call `pango_fc_font_map_cache_
//!    clear()` after `set_config()` — its fini()+init() cycle resets
//!    `priv->config` back to NULL, silently undoing the binding.

use std::os::raw::c_int;
use std::path::PathBuf;

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};

static FONTS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/resources/fonts");

#[allow(non_camel_case_types)]
type FcBool = c_int;
#[allow(non_camel_case_types)]
enum FcConfig {}
#[allow(non_camel_case_types)]
enum PangoCairoFontMap {}

// libfontconfig/libpangocairo/libpangoft2 are already transitive runtime
// dependencies of gtk4/cairo/pango on Linux; linking them explicitly here
// (rather than relying on those crates' transitive links) keeps this
// correct regardless of how those crates happen to link them.
#[link(name = "fontconfig")]
extern "C" {
    fn FcInitLoadConfigAndFonts() -> *mut FcConfig;
    fn FcConfigParseAndLoadFromMemory(config: *mut FcConfig, buffer: *const u8, complain: FcBool) -> FcBool;
    fn FcConfigBuildFonts(config: *mut FcConfig) -> FcBool;
    fn FcConfigSetCurrent(config: *mut FcConfig) -> FcBool;
}
#[link(name = "pangocairo-1.0")]
extern "C" {
    fn pango_cairo_font_map_new_for_font_type(fonttype: c_int) -> *mut PangoCairoFontMap;
    fn pango_cairo_font_map_set_default(fontmap: *mut PangoCairoFontMap);
}
// pango_fc_font_map_set_config lives in libpangoft2, not libpangocairo, on
// this system (confirmed via `nm -D` across every installed pango lib) even
// though the object it operates on is a PangoCairoFontMap — the Fc font-map
// API is shared/exported from the ft2 backend lib.
#[link(name = "pangoft2-1.0")]
extern "C" {
    fn pango_fc_font_map_set_config(fcfontmap: *mut PangoCairoFontMap, config: *mut FcConfig);
}

const CAIRO_FONT_TYPE_FT: c_int = 1;

pub struct ReadingFont {
    pub id: &'static str,
    pub label: &'static str,
    // The family name Fontconfig actually indexes the file under — not
    // always the "obvious" name. Source Sans 3 and Libre Franklin are
    // variable fonts whose vendored files report their first named
    // instance's name as the family (e.g. "Source Sans 3 ExtraLight"), not
    // the plain "Source Sans 3" the web CSS aliases to via @font-face — a
    // trick with no raw-Fontconfig equivalent used here. Verified per file
    // with `fc-query`, not assumed.
    pub family: &'static str,
}

pub const READING_FONTS: &[ReadingFont] = &[
    ReadingFont { id: "literata", label: "Literata", family: "Literata" },
    ReadingFont { id: "lora", label: "Lora", family: "Lora" },
    ReadingFont { id: "crimson-pro", label: "Crimson Pro", family: "Crimson Pro" },
    ReadingFont { id: "spectral", label: "Spectral", family: "Spectral" },
    ReadingFont { id: "im-fell-english", label: "IM Fell English", family: "IM FELL English" },
    ReadingFont { id: "unifraktur-maguntia", label: "Unifraktur", family: "UnifrakturMaguntia" },
    ReadingFont { id: "source-sans-3", label: "Source Sans 3", family: "Source Sans 3 ExtraLight" },
    ReadingFont { id: "libre-franklin", label: "Libre Franklin", family: "Libre Franklin Thin" },
];

pub fn family_for(font_id: &str) -> &'static str {
    READING_FONTS.iter().find(|f| f.id == font_id).map(|f| f.family).unwrap_or("Literata")
}

/// Materializes every vendored font file to disk (once), builds a
/// from-scratch Fontconfig config whose only extra `<dir>` is that
/// directory, and installs a fresh Pango font map bound to it as the
/// process default. Per-process only — never touches the user's real
/// Fontconfig config or `~/.local/share/fonts`.
pub fn load_reading_fonts() -> Result<()> {
    let dir = materialize_fonts()?;
    unsafe { install_font_map(&dir) }
}

fn materialize_fonts() -> Result<PathBuf> {
    let dir = crate::persistence::data_dir()?.join("fonts");
    std::fs::create_dir_all(&dir)?;
    for file in FONTS_DIR.files() {
        let name = file.path().file_name().context("font resource has no filename")?;
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(&path, file.contents())?;
        }
    }
    Ok(dir)
}

unsafe fn install_font_map(fonts_dir: &std::path::Path) -> Result<()> {
    let xml = format!(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n<fontconfig>\n<dir>{}</dir>\n</fontconfig>\n",
        fonts_dir.display()
    );
    let mut xml_bytes = xml.into_bytes();
    xml_bytes.push(0);

    let cfg = FcInitLoadConfigAndFonts();
    if cfg.is_null() {
        anyhow::bail!("FcInitLoadConfigAndFonts returned NULL");
    }
    FcConfigParseAndLoadFromMemory(cfg, xml_bytes.as_ptr(), 1);
    FcConfigBuildFonts(cfg);
    FcConfigSetCurrent(cfg);

    let map = pango_cairo_font_map_new_for_font_type(CAIRO_FONT_TYPE_FT);
    if map.is_null() {
        anyhow::bail!("pango_cairo_font_map_new_for_font_type returned NULL");
    }
    pango_fc_font_map_set_config(map, cfg);
    pango_cairo_font_map_set_default(map);

    Ok(())
}
