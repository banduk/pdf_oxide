//! Separation plate renderer.
//!
//! Renders individual ink separation plates as grayscale images where
//! pixel intensity represents the tint percentage of that ink at each point.
//! Used in prepress workflows, ink coverage analysis, and ML pipelines
//! that process packaging/label PDFs.
//!
//! # ICCBased heuristic
//!
//! When a fill color space resolves to an `ICCBased` array, this renderer
//! does **not** parse the embedded ICC profile. Instead it inspects the
//! component count of the current fill/stroke color: a 4-component
//! `ICCBased` space is treated as CMYK (component order C, M, Y, K), a
//! 3-component space is treated as RGB (skipped — no separation routing),
//! and a 1-component space is treated as Gray (skipped). This matches
//! the convention used by Adobe Illustrator and InDesign when exporting
//! to PDF/X-1a and PDF/X-4 with CMYK working spaces. PDFs that rely on
//! lab-CMYK profile interpretation for separation routing are out of
//! scope for this renderer; they are rare in prepress workflows that
//! ship separated artwork.
//!
//! # Limitations
//!
//! The following classes of content are recognised by the operator
//! walker but not actually painted into the plate:
//!
//! - **Raster image XObjects** (`Do` with `Subtype /Image`), including
//!   DeviceN / Separation-encoded TIFFs and CMYK photographs. The
//!   sample data is dropped. Vector artwork inside Form XObjects is
//!   recursed into and rendered normally.
//! - **Shading patterns** (`sh` operator) — gradients used as fills.
//! - **Tiling and shading patterns** invoked via `scn` / `SCN` with a
//!   `/Pattern` colour space.
//! - **Inline images** (`BI` / `ID` / `EI`).
//! - **Page annotations.** [`render_separations`] renders only the
//!   page's content stream; annotation appearance streams are not
//!   walked, in contrast to [`super::page_renderer`] which composites
//!   annotation appearances on top of the page.
//!
//! These are intentional v1 omissions: the primary use case is
//! vector-based prepress artwork (dielines, varnish layers, spot-PMS
//! text and shapes). PDFs that rely on raster spot-channel data will
//! produce incomplete plates and should be flagged at the caller.
#![allow(
    clippy::field_reassign_with_default,
    clippy::ptr_arg,
    clippy::only_used_in_recursion
)]

use std::collections::HashMap;
use std::sync::Arc;

use tiny_skia::{FillRule, Mask, PathBuilder, Pixmap, Transform};

use crate::content::graphics_state::{GraphicsState, GraphicsStateStack, Matrix};
use crate::content::operators::{Operator, TextElement};
use crate::content::parser::parse_content_stream;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::fonts::FontInfo;
use crate::object::Object;

use super::ext_gstate::{parse_ext_g_state_inner, ParsedExtGState};
use super::text_rasterizer::TextRasterizer;

/// A rendered separation plate for a single ink.
#[derive(Debug, Clone)]
pub struct SeparationPlate {
    /// Ink name (e.g., "Cyan", "PANTONE 185 C", "Dieline").
    pub ink_name: String,
    /// Grayscale pixel data, row-major, top-left origin.
    /// 0 = no ink, 255 = full tint. `data.len() == width * height`.
    pub data: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Render all separation plates for a page.
///
/// Returns one [`SeparationPlate`] per ink. Process inks (Cyan, Magenta,
/// Yellow, Black) are always emitted; if the page uses no CMYK content
/// those plates will be all-zero. Spot inks are emitted only when the
/// page's resource dictionary declares a `Separation` or `DeviceN` colour
/// space that names them.
///
/// Each plate is a grayscale image where pixel intensity equals the
/// tint percentage of that ink (255 = full tint, 0 = no ink).
pub fn render_separations(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
) -> Result<Vec<SeparationPlate>> {
    let inks = collect_page_inks(doc, page_num)?;
    if inks.is_empty() {
        return Ok(Vec::new());
    }

    // Pre-parse the content stream once to detect which inks are actually
    // referenced. Plates for unreferenced inks short-circuit to an empty
    // pixmap and skip the per-plate operator walk entirely (O6).
    let referenced = collect_referenced_inks(doc, page_num)?;

    let mut plates = Vec::with_capacity(inks.len());
    for ink in &inks {
        let plate = if referenced.contains(ink) {
            render_single_separation(doc, page_num, ink, dpi)?
        } else {
            empty_plate_for(doc, page_num, ink, dpi)?
        };
        plates.push(plate);
    }
    Ok(plates)
}

/// Render a single ink separation plate for a page.
///
/// Returns a grayscale image where pixel intensity = tint percentage
/// of the named ink. If the ink is not present on the page, the plate
/// is all zeros.
pub fn render_separation(
    doc: &PdfDocument,
    page_num: usize,
    ink_name: &str,
    dpi: u32,
) -> Result<SeparationPlate> {
    render_single_separation(doc, page_num, ink_name, dpi)
}

/// Collect all ink names present on a page.
///
/// CMYK is always returned regardless of whether the page actually
/// uses CMYK content — emitting four extra all-zero plates is much
/// cheaper than the recursive operator walk that would be required to
/// detect CMYK content nested inside Form XObjects (the previous
/// shallow scan missed those, RED #1). Unused CMYK plates are filtered
/// out by the short-circuit in [`render_separations`].
fn collect_page_inks(doc: &PdfDocument, page_num: usize) -> Result<Vec<String>> {
    let mut inks = vec![
        "Cyan".to_string(),
        "Magenta".to_string(),
        "Yellow".to_string(),
        "Black".to_string(),
    ];

    let spot_inks = doc.get_page_inks(page_num)?;
    for ink in spot_inks {
        if !inks.contains(&ink) {
            inks.push(ink);
        }
    }

    Ok(inks)
}

/// Build a (width × height) all-zero plate without walking the operator
/// stream. Used when [`collect_referenced_inks`] proves the ink is not
/// touched anywhere on the page.
fn empty_plate_for(
    doc: &PdfDocument,
    page_num: usize,
    ink_name: &str,
    dpi: u32,
) -> Result<SeparationPlate> {
    let (width, height, _) = compute_page_extent(doc, page_num, dpi)?;
    let pixel_count = (width as usize) * (height as usize);
    Ok(SeparationPlate {
        ink_name: ink_name.to_string(),
        data: vec![0u8; pixel_count],
        width,
        height,
    })
}

/// Walk the content stream (and any Form XObjects it references) and
/// collect every ink name that could possibly appear on the page.
fn collect_referenced_inks(doc: &PdfDocument, page_num: usize) -> Result<Vec<String>> {
    let resources = doc.get_page_resources(page_num)?;
    let color_spaces = load_color_spaces(doc, &resources)?;
    let content_data = doc.get_page_content_data(page_num)?;
    let operators = parse_content_stream(&content_data)?;
    let mut referenced: Vec<String> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    scan_operators_for_inks(
        &operators,
        doc,
        &resources,
        &color_spaces,
        &mut referenced,
        &mut visited,
    )?;
    Ok(referenced)
}

fn scan_operators_for_inks(
    operators: &[Operator],
    doc: &PdfDocument,
    resources: &Object,
    color_spaces: &HashMap<String, Object>,
    referenced: &mut Vec<String>,
    visited: &mut Vec<String>,
) -> Result<()> {
    let xobjects = match resources {
        Object::Dictionary(rd) => rd.get("XObject").and_then(|o| doc.resolve_object(o).ok()),
        _ => None,
    };

    let push = |list: &mut Vec<String>, name: &str| {
        if !list.iter().any(|s| s == name) {
            list.push(name.to_string());
        }
    };

    for op in operators {
        match op {
            Operator::SetFillCmyk { .. } | Operator::SetStrokeCmyk { .. } => {
                push(referenced, "Cyan");
                push(referenced, "Magenta");
                push(referenced, "Yellow");
                push(referenced, "Black");
            },
            Operator::SetFillColorSpace { name } | Operator::SetStrokeColorSpace { name } => {
                inks_from_space(name, color_spaces, resources, doc, referenced);
            },
            Operator::Do { name } => {
                if visited.iter().any(|s| s == name) {
                    continue;
                }
                visited.push(name.clone());
                if let Some(xobj_dict) = xobjects.as_ref().and_then(|o| o.as_dict()) {
                    if let Some(xobj_ref_obj) = xobj_dict.get(name) {
                        if let Ok(xobj) = doc.resolve_object(xobj_ref_obj) {
                            if let Object::Stream { ref dict, .. } = xobj {
                                let subtype = dict.get("Subtype").and_then(|o| o.as_name());
                                if subtype == Some("Form") {
                                    let stream_data = if let Some(r) = xobj_ref_obj.as_reference() {
                                        doc.decode_stream_with_encryption(&xobj, r)?
                                    } else {
                                        xobj.decode_stream_data()?
                                    };
                                    let form_resources = if let Some(res) = dict.get("Resources") {
                                        doc.resolve_object(res)?
                                    } else {
                                        resources.clone()
                                    };
                                    let form_cs = load_color_spaces(doc, &form_resources)?;
                                    let mut merged_cs = color_spaces.clone();
                                    merged_cs.extend(form_cs);
                                    if let Ok(form_ops) = parse_content_stream(&stream_data) {
                                        scan_operators_for_inks(
                                            &form_ops,
                                            doc,
                                            &form_resources,
                                            &merged_cs,
                                            referenced,
                                            visited,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            },
            _ => {},
        }
    }
    Ok(())
}

fn inks_from_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
    out: &mut Vec<String>,
) {
    // Honour DefaultCMYK/RGB/Gray remap (RED #2 — see resolve_color_space).
    let space = resolve_color_space(space_name, color_spaces, resources, doc);
    match space {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
            for ink in ["Cyan", "Magenta", "Yellow", "Black"] {
                if !out.iter().any(|s| s == ink) {
                    out.push(ink.to_string());
                }
            }
        },
        ResolvedSpace::Separation(name) => {
            if !out.iter().any(|s| s == &name) {
                out.push(name);
            }
        },
        ResolvedSpace::DeviceN(names) => {
            for n in names {
                if !out.iter().any(|s| s == &n) {
                    out.push(n);
                }
            }
        },
        ResolvedSpace::Rgb
        | ResolvedSpace::Gray
        | ResolvedSpace::IccRgb
        | ResolvedSpace::IccGray
        | ResolvedSpace::Unknown => {},
    }
}

/// Page extent computation (width/height in pixels and the base
/// transform that maps PDF user space into the pixmap).
fn compute_page_extent(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
) -> Result<(u32, u32, Transform)> {
    let page_info = doc.get_page_info(page_num)?;
    let media_box = page_info.media_box;

    let rotation = page_info.rotation % 360;
    let (page_w, page_h) = if rotation == 90 || rotation == 270 {
        (media_box.height, media_box.width)
    } else {
        (media_box.width, media_box.height)
    };
    let scale = dpi as f32 / 72.0;
    let width = (page_w * scale).ceil() as u32;
    let height = (page_h * scale).ceil() as u32;

    let base_transform = match rotation {
        90 => Transform::from_translate(-media_box.x, -media_box.y)
            .post_concat(Transform::from_row(0.0, scale, scale, 0.0, 0.0, 0.0)),
        180 => Transform::from_translate(-media_box.x, -media_box.y)
            .post_scale(-scale, scale)
            .post_translate(media_box.width * scale, 0.0),
        270 => Transform::from_translate(-media_box.x, -media_box.y).post_concat(
            Transform::from_row(0.0, scale, -scale, 0.0, media_box.height * scale, 0.0),
        ),
        _ => Transform::from_translate(-media_box.x, -media_box.y)
            .post_scale(scale, -scale)
            .post_translate(0.0, page_h * scale),
    };

    Ok((width, height, base_transform))
}

/// Core rendering logic for a single separation plate.
fn render_single_separation(
    doc: &PdfDocument,
    page_num: usize,
    ink_name: &str,
    dpi: u32,
) -> Result<SeparationPlate> {
    let (width, height, base_transform) = compute_page_extent(doc, page_num, dpi)?;

    let mut pixmap = Pixmap::new(width, height)
        .ok_or_else(|| Error::InvalidPdf("Failed to create separation pixmap".to_string()))?;

    let resources = doc.get_page_resources(page_num)?;
    let color_spaces = load_color_spaces(doc, &resources)?;
    let fonts = load_fonts(doc, &resources);
    let text_rasterizer = TextRasterizer::new();

    let content_data = doc.get_page_content_data(page_num)?;
    let operators = parse_content_stream(&content_data)?;

    let _ = page_num; // kept in the public API surface; not needed past extent computation
    let mut ctx = SeparationContext {
        doc,
        target_ink: ink_name,
        text_rasterizer: &text_rasterizer,
        fonts: &fonts,
    };

    execute_separation_operators(
        &mut pixmap,
        base_transform,
        &operators,
        &mut ctx,
        &resources,
        &color_spaces,
        None,
    )?;

    let pixel_count = (width * height) as usize;
    let mut data = vec![0u8; pixel_count];
    let rgba = pixmap.data();
    for i in 0..pixel_count {
        data[i] = rgba[i * 4];
    }

    Ok(SeparationPlate {
        ink_name: ink_name.to_string(),
        data,
        width,
        height,
    })
}

/// Resolved colour-space classification used by the separation pipeline.
#[derive(Debug, Clone)]
enum ResolvedSpace {
    Cmyk,
    Rgb,
    Gray,
    Separation(String),
    DeviceN(Vec<String>),
    /// ICCBased with a 4-component profile (treated as CMYK by heuristic).
    IccCmyk,
    /// ICCBased with 3 components (RGB).
    IccRgb,
    /// ICCBased with 1 component (Gray).
    IccGray,
    Unknown,
}

/// Resolve a colour-space name to a known classification.
///
/// Handles ISO 32000-1 §8.6.5.6: when the named space is one of the
/// Device families and the resource dictionary defines a corresponding
/// `Default*` entry, the Default mapping is consulted instead.
fn resolve_color_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> ResolvedSpace {
    // Direct Device* names — try DefaultCMYK / DefaultRGB / DefaultGray remap first.
    let default_key = match space_name {
        "DeviceCMYK" | "CMYK" => Some("DefaultCMYK"),
        "DeviceRGB" | "RGB" => Some("DefaultRGB"),
        "DeviceGray" | "G" => Some("DefaultGray"),
        _ => None,
    };
    if let Some(key) = default_key {
        if let Some(default) = color_spaces.get(key) {
            // Walk into the default array as a fresh classification.
            return classify_resolved(default, color_spaces, resources, doc);
        }
        return match key {
            "DefaultCMYK" => ResolvedSpace::Cmyk,
            "DefaultRGB" => ResolvedSpace::Rgb,
            _ => ResolvedSpace::Gray,
        };
    }

    if let Some(cs_obj) = color_spaces.get(space_name) {
        classify_resolved(cs_obj, color_spaces, resources, doc)
    } else {
        ResolvedSpace::Unknown
    }
}

/// Classify a colour-space object (either an array or a name) into a
/// [`ResolvedSpace`]. Used both as the entry point from a resource-dict
/// lookup and recursively when an array starts with a name that is
/// itself a device alias.
fn classify_resolved(
    cs_obj: &Object,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> ResolvedSpace {
    // Plain name (e.g. /DeviceCMYK as the array's tail target).
    if let Some(name) = cs_obj.as_name() {
        return match name {
            "DeviceCMYK" | "CMYK" => ResolvedSpace::Cmyk,
            "DeviceRGB" | "RGB" => ResolvedSpace::Rgb,
            "DeviceGray" | "G" => ResolvedSpace::Gray,
            _ => resolve_color_space(name, color_spaces, resources, doc),
        };
    }

    let arr = match cs_obj.as_array() {
        Some(a) => a,
        None => return ResolvedSpace::Unknown,
    };
    let type_name = match arr.first().and_then(|o| o.as_name()) {
        Some(n) => n,
        None => return ResolvedSpace::Unknown,
    };
    match type_name {
        "DeviceCMYK" | "CMYK" => ResolvedSpace::Cmyk,
        "DeviceRGB" | "RGB" => ResolvedSpace::Rgb,
        "DeviceGray" | "G" => ResolvedSpace::Gray,
        "Separation" => {
            let ink = arr
                .get(1)
                .and_then(|o| o.as_name())
                .map(|s| s.to_string())
                .unwrap_or_default();
            ResolvedSpace::Separation(ink)
        },
        "DeviceN" => {
            if let Some(Object::Array(ink_names)) = arr.get(1) {
                let names = ink_names
                    .iter()
                    .filter_map(|o| o.as_name().map(|s| s.to_string()))
                    .collect();
                ResolvedSpace::DeviceN(names)
            } else {
                ResolvedSpace::Unknown
            }
        },
        "ICCBased" => {
            // ICCBased: try to read /N from the stream dict to pick the
            // right component-count interpretation. Falls back to the
            // 4-comp = CMYK heuristic when the stream isn't reachable.
            if let Some(stream_obj) = arr.get(1) {
                if let Ok(resolved) = doc.resolve_object(stream_obj) {
                    if let Object::Stream { ref dict, .. } = resolved {
                        if let Some(n) = dict.get("N").and_then(|o| o.as_integer()) {
                            return match n {
                                4 => ResolvedSpace::IccCmyk,
                                3 => ResolvedSpace::IccRgb,
                                1 => ResolvedSpace::IccGray,
                                _ => ResolvedSpace::Unknown,
                            };
                        }
                    }
                }
            }
            // Unknown component count — caller decides via component vector length.
            ResolvedSpace::IccCmyk
        },
        _ => ResolvedSpace::Unknown,
    }
}

/// Load color space definitions from page resources.
fn load_color_spaces(doc: &PdfDocument, resources: &Object) -> Result<HashMap<String, Object>> {
    let mut color_spaces = HashMap::new();
    if let Object::Dictionary(res_dict) = resources {
        if let Some(cs_obj) = res_dict.get("ColorSpace") {
            let cs_dict_obj = doc.resolve_object(cs_obj)?;
            if let Some(cs_dict) = cs_dict_obj.as_dict() {
                for (name, o) in cs_dict {
                    if let Ok(resolved_cs) = doc.resolve_object(o) {
                        color_spaces.insert(name.clone(), resolved_cs);
                    }
                }
            }
        }
    }
    Ok(color_spaces)
}

/// Load font resources for the page. Failures are swallowed (text using
/// unloadable fonts is dropped); this matches the page renderer's
/// best-effort behaviour and keeps separation rendering robust on PDFs
/// with corrupt or missing fonts.
fn load_fonts(doc: &PdfDocument, resources: &Object) -> HashMap<String, Arc<FontInfo>> {
    let mut fonts = HashMap::new();
    if let Object::Dictionary(res_dict) = resources {
        if let Some(font_obj) = res_dict.get("Font") {
            if let Ok(font_dict_obj) = doc.resolve_object(font_obj) {
                if let Some(font_dict) = font_dict_obj.as_dict() {
                    for (name, f_obj) in font_dict {
                        if let Ok(info) = doc.get_or_load_font_for_rendering(f_obj) {
                            fonts.insert(name.clone(), info);
                        }
                    }
                }
            }
        }
    }
    fonts
}

/// Compute the tint contribution to `target_ink` for the current colour
/// state. Returns `None` if the current colour does not touch this
/// plate; `Some(tint)` otherwise, with `tint` in 0.0..=1.0.
fn tint_for_ink(
    fill: bool,
    gs: &GraphicsState,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
    target_ink: &str,
    fill_components: &[f32],
    stroke_components: &[f32],
) -> Option<f32> {
    let space_name = if fill {
        &gs.fill_color_space
    } else {
        &gs.stroke_color_space
    };
    let components = if fill {
        fill_components
    } else {
        stroke_components
    };

    let resolved = resolve_color_space(space_name, color_spaces, resources, doc);
    match resolved {
        ResolvedSpace::Cmyk => {
            let cmyk_state = if fill {
                gs.fill_color_cmyk
            } else {
                gs.stroke_color_cmyk
            };
            let (c, m, y, k) = if let Some(v) = cmyk_state {
                v
            } else if components.len() >= 4 {
                (components[0], components[1], components[2], components[3])
            } else {
                return None;
            };
            match target_ink {
                "Cyan" => Some(c),
                "Magenta" => Some(m),
                "Yellow" => Some(y),
                "Black" => Some(k),
                _ => None,
            }
        },
        ResolvedSpace::Rgb
        | ResolvedSpace::Gray
        | ResolvedSpace::IccRgb
        | ResolvedSpace::IccGray => {
            // RGB / Gray content does not contribute to ink plates.
            // Converting RGB → CMYK is intentionally not done: prepress
            // workflows want artwork that was authored in process or
            // spot colours, not synthesised process from RGB.
            None
        },
        ResolvedSpace::Separation(ink) => {
            if ink == target_ink && !components.is_empty() {
                Some(components[0])
            } else {
                None
            }
        },
        ResolvedSpace::DeviceN(names) => {
            for (i, n) in names.iter().enumerate() {
                if n == target_ink && i < components.len() {
                    return Some(components[i]);
                }
            }
            None
        },
        ResolvedSpace::IccCmyk => {
            // Heuristic: 4-component ICC space = CMYK. See module docs.
            if components.len() >= 4 {
                match target_ink {
                    "Cyan" => Some(components[0]),
                    "Magenta" => Some(components[1]),
                    "Yellow" => Some(components[2]),
                    "Black" => Some(components[3]),
                    _ => None,
                }
            } else {
                None
            }
        },
        ResolvedSpace::Unknown => None,
    }
}

/// Per-render shared context (read-only) passed through the operator
/// walk and into recursive Form XObject invocations.
struct SeparationContext<'a> {
    doc: &'a PdfDocument,
    target_ink: &'a str,
    text_rasterizer: &'a TextRasterizer,
    fonts: &'a HashMap<String, Arc<FontInfo>>,
}

/// Color state tracked alongside the graphics state for separation rendering.
#[derive(Clone, Debug)]
struct SeparationColorState {
    fill_components: Vec<f32>,
    stroke_components: Vec<f32>,
}

impl SeparationColorState {
    fn new() -> Self {
        Self {
            fill_components: Vec::new(),
            stroke_components: Vec::new(),
        }
    }
}

/// Compute the initial colour components for a colour space per
/// ISO 32000-1 §8.6.4.2. `cs`/`CS` resets the current colour to these
/// values when entering the space.
fn initial_components_for_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> (Vec<f32>, Option<(f32, f32, f32, f32)>) {
    let resolved = resolve_color_space(space_name, color_spaces, resources, doc);
    match resolved {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
            (vec![0.0, 0.0, 0.0, 1.0], Some((0.0, 0.0, 0.0, 1.0)))
        },
        ResolvedSpace::Rgb | ResolvedSpace::IccRgb => (vec![0.0, 0.0, 0.0], None),
        ResolvedSpace::Gray | ResolvedSpace::IccGray => (vec![0.0], None),
        ResolvedSpace::Separation(_) => (vec![1.0], None),
        ResolvedSpace::DeviceN(names) => {
            let n = names.len().max(1);
            (vec![1.0; n], None)
        },
        ResolvedSpace::Unknown => (Vec::new(), None),
    }
}

/// State inherited from a calling context when recursing into a Form
/// XObject (PDF §8.10.1: a Form XObject's initial graphics state is
/// the calling context's graphics state).
struct InheritedState {
    fill_color_space: String,
    stroke_color_space: String,
    fill_color_cmyk: Option<(f32, f32, f32, f32)>,
    stroke_color_cmyk: Option<(f32, f32, f32, f32)>,
    fill_components: Vec<f32>,
    stroke_components: Vec<f32>,
}

/// Execute operators for separation plate rendering.
fn execute_separation_operators(
    pixmap: &mut Pixmap,
    base_transform: Transform,
    operators: &[Operator],
    ctx: &mut SeparationContext<'_>,
    resources: &Object,
    color_spaces: &HashMap<String, Object>,
    inherited: Option<&InheritedState>,
) -> Result<()> {
    let mut gs_stack = GraphicsStateStack::new();
    {
        let gs = gs_stack.current_mut();
        if let Some(inh) = inherited {
            gs.fill_color_space = inh.fill_color_space.clone();
            gs.stroke_color_space = inh.stroke_color_space.clone();
            gs.fill_color_cmyk = inh.fill_color_cmyk;
            gs.stroke_color_cmyk = inh.stroke_color_cmyk;
        } else {
            gs.fill_color_space = "DeviceGray".to_string();
            gs.stroke_color_space = "DeviceGray".to_string();
        }
        gs.fill_color_rgb = (0.0, 0.0, 0.0);
        gs.stroke_color_rgb = (0.0, 0.0, 0.0);
    }

    let initial_cs = if let Some(inh) = inherited {
        SeparationColorState {
            fill_components: inh.fill_components.clone(),
            stroke_components: inh.stroke_components.clone(),
        }
    } else {
        SeparationColorState::new()
    };
    let mut color_state_stack: Vec<SeparationColorState> = vec![initial_cs];
    let mut current_path = PathBuilder::new();
    let mut pending_clip: Option<(tiny_skia::Path, FillRule)> = None;
    let mut clip_stack: Vec<Option<Mask>> = vec![None];
    let mut in_text_object = false;

    // Pre-resolve ExtGState for the gs cache.
    let ext_g_state_resolved: Option<Object> = match resources {
        Object::Dictionary(rd) => rd
            .get("ExtGState")
            .and_then(|o| ctx.doc.resolve_object(o).ok()),
        _ => None,
    };
    let ext_g_states: Option<&HashMap<String, Object>> =
        ext_g_state_resolved.as_ref().and_then(|o| o.as_dict());
    let mut ext_g_state_cache: HashMap<String, ParsedExtGState> = HashMap::new();

    let xobjects_resolved: Option<Object> = match resources {
        Object::Dictionary(rd) => rd
            .get("XObject")
            .and_then(|o| ctx.doc.resolve_object(o).ok()),
        _ => None,
    };

    for op in operators {
        match op {
            Operator::SaveState => {
                gs_stack.save();
                let cs = color_state_stack
                    .last()
                    .cloned()
                    .unwrap_or_else(SeparationColorState::new);
                color_state_stack.push(cs);
                clip_stack.push(clip_stack.last().cloned().unwrap_or(None));
            },
            Operator::RestoreState => {
                gs_stack.restore();
                if color_state_stack.len() > 1 {
                    color_state_stack.pop();
                }
                if clip_stack.len() > 1 {
                    clip_stack.pop();
                }
            },

            Operator::Cm { a, b, c, d, e, f } => {
                let current = gs_stack.current_mut();
                let new_matrix = Matrix {
                    a: *a,
                    b: *b,
                    c: *c,
                    d: *d,
                    e: *e,
                    f: *f,
                };
                current.ctm = new_matrix.multiply(&current.ctm);
            },

            Operator::SetFillRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (*r, *g, *b);
                gs.fill_color_space = "DeviceRGB".to_string();
                gs.fill_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![*r, *g, *b];
                }
            },
            Operator::SetStrokeRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (*r, *g, *b);
                gs.stroke_color_space = "DeviceRGB".to_string();
                gs.stroke_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![*r, *g, *b];
                }
            },
            Operator::SetFillGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (g, g, g);
                gs.fill_color_space = "DeviceGray".to_string();
                gs.fill_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![g];
                }
            },
            Operator::SetStrokeGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (g, g, g);
                gs.stroke_color_space = "DeviceGray".to_string();
                gs.stroke_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![g];
                }
            },
            Operator::SetFillCmyk { c, m, y, k } => {
                let gs = gs_stack.current_mut();
                gs.fill_color_cmyk = Some((*c, *m, *y, *k));
                gs.fill_color_space = "DeviceCMYK".to_string();
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![*c, *m, *y, *k];
                }
            },
            Operator::SetStrokeCmyk { c, m, y, k } => {
                let gs = gs_stack.current_mut();
                gs.stroke_color_cmyk = Some((*c, *m, *y, *k));
                gs.stroke_color_space = "DeviceCMYK".to_string();
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![*c, *m, *y, *k];
                }
            },
            Operator::SetFillColorSpace { name } => {
                let (components, cmyk) =
                    initial_components_for_space(name, color_spaces, resources, ctx.doc);
                let gs = gs_stack.current_mut();
                gs.fill_color_space = name.clone();
                gs.fill_color_cmyk = cmyk;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = components;
                }
            },
            Operator::SetStrokeColorSpace { name } => {
                let (components, cmyk) =
                    initial_components_for_space(name, color_spaces, resources, ctx.doc);
                let gs = gs_stack.current_mut();
                gs.stroke_color_space = name.clone();
                gs.stroke_color_cmyk = cmyk;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = components;
                }
            },
            Operator::SetFillColor { components } | Operator::SetFillColorN { components, .. } => {
                let gs = gs_stack.current_mut();
                let space = gs.fill_color_space.clone();
                match space.as_str() {
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.fill_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    },
                    _ => {},
                }
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = components.clone();
                }
            },
            Operator::SetStrokeColor { components }
            | Operator::SetStrokeColorN { components, .. } => {
                let gs = gs_stack.current_mut();
                let space = gs.stroke_color_space.clone();
                match space.as_str() {
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.stroke_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    },
                    _ => {},
                }
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = components.clone();
                }
            },

            Operator::SetLineWidth { width } => {
                gs_stack.current_mut().line_width = *width;
            },
            Operator::SetLineCap { cap_style } => {
                gs_stack.current_mut().line_cap = *cap_style;
            },
            Operator::SetLineJoin { join_style } => {
                gs_stack.current_mut().line_join = *join_style;
            },
            Operator::SetMiterLimit { limit } => {
                gs_stack.current_mut().miter_limit = *limit;
            },
            Operator::SetDash { array, phase } => {
                gs_stack.current_mut().dash_pattern = (array.clone(), *phase);
            },

            Operator::MoveTo { x, y } => {
                current_path.move_to(*x, *y);
            },
            Operator::LineTo { x, y } => {
                current_path.line_to(*x, *y);
            },
            Operator::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            } => {
                current_path.cubic_to(*x1, *y1, *x2, *y2, *x3, *y3);
            },
            Operator::CurveToV { x2, y2, x3, y3 } => {
                if let Some(last) = current_path.last_point() {
                    current_path.cubic_to(last.x, last.y, *x2, *y2, *x3, *y3);
                }
            },
            Operator::CurveToY { x1, y1, x3, y3 } => {
                current_path.cubic_to(*x1, *y1, *x3, *y3, *x3, *y3);
            },
            Operator::Rectangle {
                x,
                y,
                width,
                height,
            } => {
                let (nx, nw) = if *width < 0.0 {
                    (x + width, -width)
                } else {
                    (*x, *width)
                };
                let (ny, nh) = if *height < 0.0 {
                    (y + height, -height)
                } else {
                    (*y, *height)
                };
                if let Some(rect) = tiny_skia::Rect::from_xywh(nx, ny, nw, nh) {
                    current_path.push_rect(rect);
                }
            },
            Operator::ClosePath => {
                current_path.close();
            },

            Operator::Stroke => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    let gs = gs_stack.current();
                    let empty = SeparationColorState::new();
                    let cs = color_state_stack.last().unwrap_or(&empty);
                    if let Some(tint) = tint_for_ink(
                        false,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        stroke_separation(pixmap, &path, transform, gs, tint, clip);
                    }
                }
                current_path = PathBuilder::new();
            },
            Operator::Fill => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    let gs = gs_stack.current();
                    let empty = SeparationColorState::new();
                    let cs = color_state_stack.last().unwrap_or(&empty);
                    if let Some(tint) = tint_for_ink(
                        true,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        fill_separation(pixmap, &path, transform, tint, FillRule::Winding, clip);
                    }
                }
                current_path = PathBuilder::new();
            },
            Operator::FillEvenOdd => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    let gs = gs_stack.current();
                    let empty = SeparationColorState::new();
                    let cs = color_state_stack.last().unwrap_or(&empty);
                    if let Some(tint) = tint_for_ink(
                        true,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        fill_separation(pixmap, &path, transform, tint, FillRule::EvenOdd, clip);
                    }
                }
                current_path = PathBuilder::new();
            },
            Operator::FillStroke | Operator::CloseFillStroke => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    let gs = gs_stack.current();
                    let empty = SeparationColorState::new();
                    let cs = color_state_stack.last().unwrap_or(&empty);
                    let transform = combine_transforms(base_transform, &gs.ctm);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    if let Some(tint) = tint_for_ink(
                        true,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        fill_separation(pixmap, &path, transform, tint, FillRule::Winding, clip);
                    }
                    if let Some(tint) = tint_for_ink(
                        false,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        stroke_separation(pixmap, &path, transform, gs, tint, clip);
                    }
                }
                current_path = PathBuilder::new();
            },
            Operator::FillStrokeEvenOdd | Operator::CloseFillStrokeEvenOdd => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    let gs = gs_stack.current();
                    let empty = SeparationColorState::new();
                    let cs = color_state_stack.last().unwrap_or(&empty);
                    let transform = combine_transforms(base_transform, &gs.ctm);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    if let Some(tint) = tint_for_ink(
                        true,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        fill_separation(pixmap, &path, transform, tint, FillRule::EvenOdd, clip);
                    }
                    if let Some(tint) = tint_for_ink(
                        false,
                        gs,
                        color_spaces,
                        resources,
                        ctx.doc,
                        ctx.target_ink,
                        &cs.fill_components,
                        &cs.stroke_components,
                    ) {
                        stroke_separation(pixmap, &path, transform, gs, tint, clip);
                    }
                }
                current_path = PathBuilder::new();
            },
            Operator::EndPath => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap,
                    base_transform,
                    &gs_stack,
                );
                current_path = PathBuilder::new();
            },

            Operator::ClipNonZero => {
                if let Some(path) = current_path.clone().finish() {
                    pending_clip = Some((path, FillRule::Winding));
                }
            },
            Operator::ClipEvenOdd => {
                if let Some(path) = current_path.clone().finish() {
                    pending_clip = Some((path, FillRule::EvenOdd));
                }
            },

            // Text object
            Operator::BeginText => {
                in_text_object = true;
                let gs = gs_stack.current_mut();
                gs.text_matrix = Matrix::identity();
                gs.text_line_matrix = Matrix::identity();
            },
            Operator::EndText => {
                in_text_object = false;
            },

            // Text state
            Operator::Tc { char_space } => {
                gs_stack.current_mut().char_space = *char_space;
            },
            Operator::Tw { word_space } => {
                gs_stack.current_mut().word_space = *word_space;
            },
            Operator::Tz { scale } => {
                gs_stack.current_mut().horizontal_scaling = *scale;
            },
            Operator::TL { leading } => {
                gs_stack.current_mut().leading = *leading;
            },
            Operator::Ts { rise } => {
                gs_stack.current_mut().text_rise = *rise;
            },
            Operator::Tr { render } => {
                gs_stack.current_mut().render_mode = *render;
            },
            Operator::Tf { font, size } => {
                let gs = gs_stack.current_mut();
                gs.font_name = Some(font.clone());
                gs.font_size = *size;
            },

            // Text positioning
            Operator::Td { tx, ty } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    let translation = Matrix::translation(*tx, *ty);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            },
            Operator::TD { tx, ty } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    gs.leading = -(*ty);
                    let translation = Matrix::translation(*tx, *ty);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            },
            Operator::Tm { a, b, c, d, e, f } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    gs.text_matrix = Matrix {
                        a: *a,
                        b: *b,
                        c: *c,
                        d: *d,
                        e: *e,
                        f: *f,
                    };
                    gs.text_line_matrix = gs.text_matrix;
                }
            },
            Operator::TStar => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    let leading = gs.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            },

            // Text showing
            Operator::Tj { text } => {
                if in_text_object {
                    let advance = render_text_to_plate(
                        pixmap,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                    )?;
                    let gs_mut = gs_stack.current_mut();
                    let advance_matrix = Matrix::translation(advance, 0.0);
                    gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                }
            },
            Operator::TJ { array } => {
                if in_text_object {
                    let advance = render_tj_to_plate(
                        pixmap,
                        array,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                    )?;
                    let gs_mut = gs_stack.current_mut();
                    let advance_matrix = Matrix::translation(advance, 0.0);
                    gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                }
            },
            Operator::Quote { text } => {
                if in_text_object {
                    let gs_mut = gs_stack.current_mut();
                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    let advance = render_text_to_plate(
                        pixmap,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                    )?;
                    let gs_mut = gs_stack.current_mut();
                    let advance_matrix = Matrix::translation(advance, 0.0);
                    gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                }
            },
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            } => {
                if in_text_object {
                    let gs_mut = gs_stack.current_mut();
                    gs_mut.word_space = *word_space;
                    gs_mut.char_space = *char_space;
                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    let advance = render_text_to_plate(
                        pixmap,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                    )?;
                    let gs_mut = gs_stack.current_mut();
                    let advance_matrix = Matrix::translation(advance, 0.0);
                    gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                }
            },

            // ExtGState
            Operator::SetExtGState { dict_name } => {
                let entry = ext_g_state_cache
                    .entry(dict_name.clone())
                    .or_insert_with(|| {
                        if let Some(states) = ext_g_states {
                            if let Some(state_obj) = states.get(dict_name) {
                                return parse_ext_g_state_inner(state_obj, ctx.doc)
                                    .unwrap_or_default();
                            }
                        }
                        ParsedExtGState::default()
                    });
                entry.apply(gs_stack.current_mut());
            },

            // XObject (Form XObjects may contain ink-bearing content).
            // Image XObjects are dropped — see module-level Limitations.
            Operator::Do { name } => {
                if let Some(xobjects) = xobjects_resolved.as_ref().and_then(|o| o.as_dict()) {
                    if let Some(xobj_ref_obj) = xobjects.get(name) {
                        if let Ok(xobj) = ctx.doc.resolve_object(xobj_ref_obj) {
                            if let Object::Stream { ref dict, .. } = xobj {
                                if let Some(subtype) = dict.get("Subtype").and_then(|o| o.as_name())
                                {
                                    if subtype == "Form" {
                                        let xobj_ref = xobj_ref_obj.as_reference();
                                        let stream_data = if let Some(r) = xobj_ref {
                                            ctx.doc.decode_stream_with_encryption(&xobj, r)?
                                        } else {
                                            xobj.decode_stream_data()?
                                        };

                                        let form_resources =
                                            if let Some(res) = dict.get("Resources") {
                                                ctx.doc.resolve_object(res)?
                                            } else {
                                                resources.clone()
                                            };

                                        let form_cs = load_color_spaces(ctx.doc, &form_resources)?;
                                        let mut merged_cs = color_spaces.clone();
                                        merged_cs.extend(form_cs);

                                        let form_matrix = parse_form_matrix(dict);
                                        let gs = gs_stack.current();
                                        let combined = combine_transforms(base_transform, &gs.ctm)
                                            .pre_concat(form_matrix);

                                        // Inherit the calling context's colour state into the
                                        // form's initial graphics state (PDF §8.10.1, O5).
                                        let empty = SeparationColorState::new();
                                        let cs = color_state_stack.last().unwrap_or(&empty);
                                        let inherited = InheritedState {
                                            fill_color_space: gs.fill_color_space.clone(),
                                            stroke_color_space: gs.stroke_color_space.clone(),
                                            fill_color_cmyk: gs.fill_color_cmyk,
                                            stroke_color_cmyk: gs.stroke_color_cmyk,
                                            fill_components: cs.fill_components.clone(),
                                            stroke_components: cs.stroke_components.clone(),
                                        };

                                        let form_ops = parse_content_stream(&stream_data)?;
                                        execute_separation_operators(
                                            pixmap,
                                            combined,
                                            &form_ops,
                                            ctx,
                                            &form_resources,
                                            &merged_cs,
                                            Some(&inherited),
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            },

            _ => {},
        }
    }
    Ok(())
}

/// Render text into the separation pixmap, routing each glyph through the
/// current ink's tint. The strategy is to clone the GraphicsState, replace
/// its fill colour with a grayscale paint equal to the tint, and reuse
/// the standard [`TextRasterizer`]. This preserves glyph shape, kerning,
/// and anti-aliasing — the same fidelity as the page renderer.
#[allow(clippy::too_many_arguments)]
fn render_text_to_plate(
    pixmap: &mut Pixmap,
    text: &[u8],
    base_transform: Transform,
    gs_stack: &mut GraphicsStateStack,
    color_state_stack: &[SeparationColorState],
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &mut SeparationContext<'_>,
    clip: Option<&Mask>,
) -> Result<f32> {
    let gs = gs_stack.current();
    let empty = SeparationColorState::new();
    let cs = color_state_stack.last().unwrap_or(&empty);

    // Render mode 3 = invisible text. Still advance the text matrix but skip painting.
    if gs.render_mode == 3 {
        return measure_text_advance(text, gs, ctx.fonts);
    }

    let tint = tint_for_ink(
        true,
        gs,
        color_spaces,
        resources,
        ctx.doc,
        ctx.target_ink,
        &cs.fill_components,
        &cs.stroke_components,
    );
    if tint.is_none() {
        // Colour doesn't touch this plate — advance but don't paint.
        return measure_text_advance(text, gs, ctx.fonts);
    }
    let tint = tint.unwrap();

    // Build a faked-grayscale GraphicsState so the rasteriser paints in
    // (tint, tint, tint) which becomes the plate value in the R channel.
    let mut faux = gs.clone();
    faux.fill_color_rgb = (tint, tint, tint);
    faux.fill_alpha = 1.0;
    faux.blend_mode = "Normal".to_string();

    let transform = combine_transforms(base_transform, &gs.ctm);
    ctx.text_rasterizer
        .render_text(pixmap, text, transform, &faux, resources, ctx.doc, clip, ctx.fonts)
}

/// Render a TJ array (sequence of strings + offsets) into the plate.
/// Walks the array applying offsets between strings, painting each
/// string component via [`render_text_to_plate`].
#[allow(clippy::too_many_arguments)]
fn render_tj_to_plate(
    pixmap: &mut Pixmap,
    array: &[TextElement],
    base_transform: Transform,
    gs_stack: &mut GraphicsStateStack,
    color_state_stack: &[SeparationColorState],
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &mut SeparationContext<'_>,
    clip: Option<&Mask>,
) -> Result<f32> {
    let mut total_advance = 0.0;
    for element in array {
        match element {
            TextElement::String(text) => {
                let advance = render_text_to_plate(
                    pixmap,
                    text,
                    base_transform,
                    gs_stack,
                    color_state_stack,
                    color_spaces,
                    resources,
                    ctx,
                    clip,
                )?;
                let gs_mut = gs_stack.current_mut();
                let advance_matrix = Matrix::translation(advance, 0.0);
                gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                total_advance += advance;
            },
            TextElement::Offset(offset) => {
                let gs = gs_stack.current();
                let shift = (-*offset / 1000.0) * gs.font_size;
                let advance_matrix = Matrix::translation(shift, 0.0);
                let gs_mut = gs_stack.current_mut();
                gs_mut.text_matrix = advance_matrix.multiply(&gs_mut.text_matrix);
                total_advance += shift;
            },
        }
    }
    Ok(total_advance)
}

/// Compute the horizontal advance a [`TextRasterizer`] call would
/// produce, without painting. Used for invisible/skipped text so the
/// text matrix stays consistent with the painted ink plates.
///
/// Best-effort: when an embedded width table is unavailable we fall
/// back to `font_size * len * 0.5` — close enough to keep glyph
/// positions inside the rest of the line.
fn measure_text_advance(
    text: &[u8],
    gs: &GraphicsState,
    fonts: &HashMap<String, Arc<FontInfo>>,
) -> Result<f32> {
    let font_info = gs
        .font_name
        .as_ref()
        .and_then(|n| fonts.get(n))
        .map(Arc::clone);

    // Sum widths from the font's width table (in glyph units / 1000)
    // multiplied by font_size, plus per-char Tc spacing.
    let mut units: f32 = 0.0;
    let mut count: usize = 0;
    if let Some(info) = font_info.as_ref() {
        if info.subtype != "Type0" {
            for &b in text {
                units += info.get_glyph_width(b as u16);
                count += 1;
            }
        } else {
            // Type0: iterate 2-byte codes (approx).
            let mut i = 0;
            while i + 1 < text.len() {
                let code = ((text[i] as u16) << 8) | text[i + 1] as u16;
                units += info.get_glyph_width(code);
                count += 1;
                i += 2;
            }
        }
    } else {
        for _ in text {
            units += 500.0;
            count += 1;
        }
    }
    let advance = units * gs.font_size / 1000.0 + (count as f32) * gs.char_space;
    Ok(advance)
}

/// Fill a path into the separation pixmap with the given tint value.
fn fill_separation(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    tint: f32,
    fill_rule: FillRule,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    // SourceOver with opaque (alpha=255) source = replacement; this matches
    // PDF's opaque painting model where each new fill overwrites the pixels
    // under it within the path. Overlapping fills are *not* accumulated —
    // PDF separation semantics dictate last-writer-wins per ink at the
    // overlapping pixels, which SourceOver gives us for free.
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;

    pixmap.fill_path(path, &paint, fill_rule, transform, clip);
}

/// Stroke a path into the separation pixmap with the given tint value.
fn stroke_separation(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    gs: &GraphicsState,
    tint: f32,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

    let mut stroke = tiny_skia::Stroke::default();
    stroke.width = gs.line_width;
    stroke.line_cap = match gs.line_cap {
        1 => tiny_skia::LineCap::Round,
        2 => tiny_skia::LineCap::Square,
        _ => tiny_skia::LineCap::Butt,
    };
    stroke.line_join = match gs.line_join {
        1 => tiny_skia::LineJoin::Round,
        2 => tiny_skia::LineJoin::Bevel,
        _ => tiny_skia::LineJoin::Miter,
    };
    stroke.miter_limit = gs.miter_limit;

    if !gs.dash_pattern.0.is_empty() {
        stroke.dash = tiny_skia::StrokeDash::new(gs.dash_pattern.0.clone(), gs.dash_pattern.1);
    }

    pixmap.stroke_path(path, &paint, &stroke, transform, clip);
}

/// Apply a pending clip path to the clip stack.
fn apply_separation_clip(
    pending: &mut Option<(tiny_skia::Path, FillRule)>,
    clip_stack: &mut Vec<Option<Mask>>,
    pixmap: &Pixmap,
    base_transform: Transform,
    gs_stack: &GraphicsStateStack,
) {
    if let Some((path, fill_rule)) = pending.take() {
        let gs = gs_stack.current();
        let transform = combine_transforms(base_transform, &gs.ctm);

        if let Some(path_transformed) = path.transform(transform) {
            let mut new_mask = Mask::new(pixmap.width(), pixmap.height()).unwrap();
            new_mask.fill_path(&path_transformed, fill_rule, true, Transform::identity());

            if let Some(Some(current_mask)) = clip_stack.last() {
                let mut combined = current_mask.clone();
                let combined_data = combined.data_mut();
                let new_data = new_mask.data();
                for i in 0..combined_data.len() {
                    combined_data[i] = ((combined_data[i] as u32 * new_data[i] as u32) / 255) as u8;
                }
                *clip_stack.last_mut().unwrap() = Some(combined);
            } else {
                *clip_stack.last_mut().unwrap() = Some(new_mask);
            }
        }
    }
}

/// Parse a form XObject matrix from its dictionary.
fn parse_form_matrix(dict: &HashMap<String, Object>) -> Transform {
    if let Some(Object::Array(arr)) = dict.get("Matrix") {
        let get_f32 = |i: usize| -> f32 {
            match arr.get(i) {
                Some(Object::Real(v)) => *v as f32,
                Some(Object::Integer(v)) => *v as f32,
                _ => {
                    if i == 0 || i == 3 {
                        1.0
                    } else {
                        0.0
                    }
                },
            }
        };
        Transform::from_row(get_f32(0), get_f32(1), get_f32(2), get_f32(3), get_f32(4), get_f32(5))
    } else {
        Transform::identity()
    }
}

/// Combine two transformations (base + CTM).
fn combine_transforms(base: Transform, ctm: &Matrix) -> Transform {
    base.pre_concat(Transform::from_row(ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f))
}
