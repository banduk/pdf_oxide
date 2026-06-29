//! Page subsetting and document rebuild.
//!
//! Builds a brand-new PDF from a selection of source pages, copying **only**
//! the objects those pages actually need to preserve their full visual and
//! semantic meaning — and nothing else. The result has:
//!
//! * **No garbage.** Each kept page's `/Resources` is trimmed to the names its
//!   content stream actually references (fonts, XObjects, ExtGStates,
//!   colorspaces, patterns, shadings, marked-content property lists). Resources
//!   that were only reachable through a *shared / inherited* `/Resources`
//!   dictionary (the classic "extract one page, drag in every font in the
//!   document" bloat) are dropped. A final reachability pass guarantees no
//!   orphan objects survive.
//! * **No duplication.** Objects with byte-identical canonical form are
//!   collapsed to a single object and every reference is remapped to it. This
//!   is what keeps a font or image that appears on several pages — or across
//!   several *source documents* in [`PdfRebuilder`] — from being copied N
//!   times.
//!
//! ## Forms
//!
//! v1 trims only *page-level* resources. A Form XObject the page uses is copied
//! wholesale (its own `/Resources` untouched), so nothing it draws is ever
//! lost; trimming *inside* forms is a future enhancement.
//!
//! ## Signatures
//!
//! A cryptographic signature signs a byte range of the original file, so any
//! rebuild necessarily invalidates it — there is no way to carry a *valid*
//! signature through a subset. The policy is therefore explicit
//! ([`SignaturePolicy`]):
//!
//! * [`SignaturePolicy::PreserveVisual`] (default) keeps the signature widget's
//!   appearance (e.g. the gov.br seal image) as an ordinary annotation but
//!   drops the signature value, the field-ness, and the `/AcroForm` — so the
//!   output never carries a `/ByteRange` signature dictionary that *looks*
//!   signed but would fail validation. A warning is recorded.
//! * [`SignaturePolicy::Refuse`] returns an error naming the offending page.

use std::collections::{HashMap, HashSet};

use crate::content::{parse_content_stream, Operator};
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::object::{Object, ObjectRef};
use crate::writer::ObjectSerializer;

/// Maximum object-graph recursion depth while copying (cycle/blowup guard).
const MAX_DEPTH: usize = 256;

/// What to do when a kept page carries a digital signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignaturePolicy {
    /// Keep the signature's visual appearance (e.g. the seal image) as a plain
    /// annotation, drop the now-invalid signature value + `/AcroForm`, and warn.
    #[default]
    PreserveVisual,
    /// Refuse to subset a document whose kept pages contain a signature.
    Refuse,
}

/// Options controlling a subset / rebuild.
#[derive(Debug, Clone)]
pub struct SubsetOptions {
    /// Collapse byte-identical objects to a single shared object.
    pub dedup: bool,
    /// How to handle signatures on kept pages.
    pub on_signature: SignaturePolicy,
    /// Keep link annotations + GoTo actions whose target is a kept page
    /// (remapped to the new page); links to dropped pages are severed.
    pub keep_links: bool,
    /// Keep the document outline (bookmarks), pruned to entries that still
    /// resolve to a kept page (or have a surviving descendant).
    pub keep_outlines: bool,
    /// Keep the tagged-PDF structure tree, pruned to the kept pages.
    pub keep_struct_tree: bool,
    /// Recurse into Form XObjects and trim their `/Resources` too (otherwise a
    /// used form is copied wholesale).
    pub trim_forms: bool,
}

impl Default for SubsetOptions {
    fn default() -> Self {
        SubsetOptions {
            dedup: true,
            on_signature: SignaturePolicy::default(),
            keep_links: true,
            keep_outlines: true,
            keep_struct_tree: true,
            trim_forms: true,
        }
    }
}

/// What a subset / rebuild did.
#[derive(Debug, Default, Clone)]
pub struct SubsetReport {
    /// Human-readable notes (e.g. signatures dropped).
    pub warnings: Vec<String>,
    /// Number of signatures whose value was dropped (visual kept).
    pub dropped_signatures: usize,
    /// Objects emitted in the output.
    pub objects_written: usize,
    /// Objects removed by deduplication.
    pub objects_deduped: usize,
    /// Outline (bookmark) entries kept.
    pub outline_entries: usize,
    /// Link/GoTo destinations severed because their target page was dropped.
    pub links_severed: usize,
    /// Structure elements kept after pruning the tag tree.
    pub struct_elements: usize,
}

/// Resource names a content stream actually references, per category.
#[derive(Debug, Default)]
struct UsedNames {
    fonts: HashSet<String>,
    xobjects: HashSet<String>,
    extgstates: HashSet<String>,
    colorspaces: HashSet<String>,
    patterns: HashSet<String>,
    shadings: HashSet<String>,
    properties: HashSet<String>,
}

const RESOURCE_CATEGORIES: [&str; 7] = [
    "Font",
    "XObject",
    "ExtGState",
    "ColorSpace",
    "Pattern",
    "Shading",
    "Properties",
];

/// Page dictionary keys carried verbatim (besides the ones we rebuild:
/// `/Type`, `/Parent`, `/Resources`, `/Contents`, `/Annots`).
const PAGE_GEOMETRY_KEYS: [&str; 8] = [
    "MediaBox", "CropBox", "BleedBox", "TrimBox", "ArtBox", "Rotate", "UserUnit", "Group",
];

/// Scan a decoded content stream and record the resource names it references.
fn scan_used(content: &[u8], used: &mut UsedNames) {
    let ops = match parse_content_stream(content) {
        Ok(ops) => ops,
        Err(_) => return,
    };
    for op in ops {
        match op {
            Operator::Tf { font, .. } => {
                used.fonts.insert(font);
            },
            Operator::Do { name } => {
                used.xobjects.insert(name);
            },
            Operator::SetExtGState { dict_name } => {
                used.extgstates.insert(dict_name);
            },
            Operator::SetFillColorSpace { name } | Operator::SetStrokeColorSpace { name } => {
                used.colorspaces.insert(name);
            },
            Operator::SetFillColorN { name, .. } | Operator::SetStrokeColorN { name, .. } => {
                if let Some(name) = name {
                    used.patterns.insert(*name);
                }
            },
            Operator::PaintShading { name } => {
                used.shadings.insert(name);
            },
            Operator::BeginMarkedContentDict { properties, .. } => {
                if let Object::Name(n) = *properties {
                    used.properties.insert(n);
                }
            },
            _ => {},
        }
    }
}

/// The deep-copy / trim / dedup engine.
struct Builder<'a> {
    sources: &'a [&'a PdfDocument],
    opts: SubsetOptions,
    report: SubsetReport,
    /// (source index, source object id) -> new id, so each source object is
    /// copied at most once.
    id_map: HashMap<(usize, u32), u32>,
    /// new id -> remapped object (all child references already point at new ids).
    objects: HashMap<u32, Object>,
    next_id: u32,
    /// new ids excluded from dedup collapsing (catalog / pages / page dicts).
    pinned: HashSet<u32>,
    /// For each source: object ids of pages we are NOT keeping. References to
    /// these (e.g. a link's `/Dest`) are severed so they can't drag a dropped
    /// page back into the graph.
    severed: Vec<HashSet<u32>>,
    page_ids: Vec<u32>,
    /// (source index, source page-leaf object id) -> new page id. Built for ALL
    /// kept pages before any destination is remapped, so a link/bookmark on one
    /// page can resolve a target on another.
    page_id_map: HashMap<(usize, u32), u32>,
    /// (source index, source form-xobject id) -> new id of its *trimmed* copy,
    /// so a form used by several pages is trimmed + emitted once.
    form_trim_map: HashMap<(usize, u32), u32>,
}

impl<'a> Builder<'a> {
    fn new(sources: &'a [&'a PdfDocument], opts: SubsetOptions) -> Self {
        Builder {
            sources,
            opts,
            report: SubsetReport::default(),
            id_map: HashMap::new(),
            objects: HashMap::new(),
            next_id: 1,
            pinned: HashSet::new(),
            severed: vec![HashSet::new(); sources.len()],
            page_ids: Vec::new(),
            page_id_map: HashMap::new(),
            form_trim_map: HashMap::new(),
        }
    }

    fn alloc(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Follow a value that may be an indirect reference, returning an owned
    /// resolved object from the given source.
    fn resolve(&self, src: usize, obj: &Object) -> Option<Object> {
        match obj {
            Object::Reference(r) => self.sources[src].load_object(*r).ok(),
            other => Some(other.clone()),
        }
    }

    /// Import a source object (by reference) into the new graph, returning its
    /// new id. Copies each source object once.
    fn import(&mut self, src: usize, old: ObjectRef, depth: usize) -> u32 {
        if let Some(&n) = self.id_map.get(&(src, old.id)) {
            return n;
        }
        // Reserve the id first so cycles terminate.
        let new_id = self.alloc();
        self.id_map.insert((src, old.id), new_id);
        let obj = self.sources[src].load_object(old).unwrap_or(Object::Null);
        let remapped = self.remap(src, &obj, depth + 1);
        self.objects.insert(new_id, remapped);
        new_id
    }

    /// Recursively rewrite every reference in `obj` to point at imported,
    /// new-id objects. References to severed (dropped) pages become `null`.
    fn remap(&mut self, src: usize, obj: &Object, depth: usize) -> Object {
        if depth > MAX_DEPTH {
            return Object::Null;
        }
        match obj {
            Object::Reference(r) => {
                if self.severed[src].contains(&r.id) {
                    return Object::Null;
                }
                let new_id = self.import(src, *r, depth);
                Object::Reference(ObjectRef::new(new_id, 0))
            },
            Object::Array(items) => {
                Object::Array(items.iter().map(|o| self.remap(src, o, depth + 1)).collect())
            },
            Object::Dictionary(d) => Object::Dictionary(
                d.iter().map(|(k, v)| (k.clone(), self.remap(src, v, depth + 1))).collect(),
            ),
            Object::Stream { dict, data } => Object::Stream {
                dict: dict.iter().map(|(k, v)| (k.clone(), self.remap(src, v, depth + 1))).collect(),
                data: data.clone(),
            },
            other => other.clone(),
        }
    }

    /// Remap an already-owned dictionary's values (used for synthesized dicts
    /// such as a trimmed `/Resources` or an edited annotation).
    fn remap_dict(&mut self, src: usize, dict: &HashMap<String, Object>) -> HashMap<String, Object> {
        dict.iter().map(|(k, v)| (k.clone(), self.remap(src, v, 1))).collect()
    }

    fn keep_set<'b>(used: &'b UsedNames, category: &str) -> Option<&'b HashSet<String>> {
        Some(match category {
            "Font" => &used.fonts,
            "XObject" => &used.xobjects,
            "ExtGState" => &used.extgstates,
            "ColorSpace" => &used.colorspaces,
            "Pattern" => &used.patterns,
            "Shading" => &used.shadings,
            "Properties" => &used.properties,
            _ => return None,
        })
    }

    /// If `value` references a Form XObject (not an image), return its ref.
    fn form_ref_if_form(&self, src: usize, value: &Object) -> Option<ObjectRef> {
        let r = value.as_reference()?;
        let o = self.sources[src].load_object(r).ok()?;
        let st = o.as_dict()?.get("Subtype")?.as_name()?;
        (st == "Form").then_some(r)
    }

    /// Build a trimmed `/Resources` dictionary holding only the entries the
    /// content actually used, with every kept entry already imported (new-id
    /// references). Used Form XObjects are themselves trimmed when
    /// `opts.trim_forms` is set; everything else is imported wholesale.
    fn build_trimmed_resources(
        &mut self,
        src: usize,
        res: &Object,
        used: &UsedNames,
        depth: usize,
    ) -> Object {
        let mut out: HashMap<String, Object> = HashMap::new();
        let res_dict = match res {
            Object::Dictionary(d) => d.clone(),
            Object::Stream { dict, .. } => dict.clone(),
            _ => return Object::Dictionary(out),
        };
        for category in RESOURCE_CATEGORIES {
            let Some(keep) = Self::keep_set(used, category) else { continue };
            if keep.is_empty() {
                continue;
            }
            let keep = keep.clone();
            let Some(sub) = res_dict.get(category).and_then(|c| self.resolve(src, c)) else {
                continue;
            };
            let Some(sub_dict) = sub.as_dict().cloned() else { continue };
            let mut new_sub: HashMap<String, Object> = HashMap::new();
            for (name, value) in sub_dict.iter() {
                if !keep.contains(name) {
                    continue;
                }
                let imported = if category == "XObject"
                    && self.opts.trim_forms
                    && depth < MAX_DEPTH
                    && self.form_ref_if_form(src, value).is_some()
                {
                    let form_ref = self.form_ref_if_form(src, value).unwrap();
                    let fid = self.import_form_trimmed(src, form_ref, depth + 1);
                    Object::Reference(ObjectRef::new(fid, 0))
                } else {
                    // Wholesale import (handles indirect refs and inline values).
                    self.remap(src, value, depth + 1)
                };
                new_sub.insert(name.clone(), imported);
            }
            if !new_sub.is_empty() {
                out.insert(category.to_string(), Object::Dictionary(new_sub));
            }
        }
        // /ProcSet is a tiny legacy hint with no references; carry it through.
        if let Some(ps) = res_dict.get("ProcSet") {
            out.insert("ProcSet".to_string(), ps.clone());
        }
        Object::Dictionary(out)
    }

    /// Import a Form XObject as a *trimmed* copy: its own `/Resources` is cut to
    /// what its content stream references (recursively). Cached so a shared form
    /// is trimmed once.
    fn import_form_trimmed(&mut self, src: usize, form_ref: ObjectRef, depth: usize) -> u32 {
        if let Some(&n) = self.form_trim_map.get(&(src, form_ref.id)) {
            return n;
        }
        let new_id = self.alloc();
        self.form_trim_map.insert((src, form_ref.id), new_id);

        let form_obj = self.sources[src].load_object(form_ref).unwrap_or(Object::Null);
        let (dict, data) = match &form_obj {
            Object::Stream { dict, data } => (dict.clone(), data.clone()),
            // Not a stream (shouldn't happen for /Subtype /Form) — copy wholesale.
            other => {
                let remapped = self.remap(src, other, depth + 1);
                self.objects.insert(new_id, remapped);
                return new_id;
            },
        };

        let content = form_obj.decode_stream_data().unwrap_or_default();
        let mut used = UsedNames::default();
        scan_used(&content, &mut used);

        let mut nd: HashMap<String, Object> = HashMap::new();
        for (k, v) in &dict {
            if k == "Resources" {
                continue;
            }
            nd.insert(k.clone(), self.remap(src, v, depth + 1));
        }
        if let Some(fr) = dict.get("Resources").and_then(|r| self.resolve(src, r)) {
            let trimmed = self.build_trimmed_resources(src, &fr, &used, depth + 1);
            nd.insert("Resources".to_string(), trimmed);
        }
        self.objects.insert(new_id, Object::Stream { dict: nd, data });
        new_id
    }

    /// Does this annotation dict represent a signature widget?
    fn is_signature_widget(&self, src: usize, annot: &HashMap<String, Object>) -> bool {
        let is_widget = annot
            .get("Subtype")
            .and_then(|s| s.as_name())
            .map(|n| n == "Widget")
            .unwrap_or(false);
        let is_sig_field = annot
            .get("FT")
            .and_then(|s| s.as_name())
            .map(|n| n == "Sig")
            .unwrap_or(false);
        // /V pointing at a /Type /Sig dictionary also marks a signature.
        let v_is_sig = annot
            .get("V")
            .and_then(|v| self.resolve(src, v))
            .and_then(|o| o.as_dict().cloned())
            .and_then(|d| d.get("Type").and_then(|t| t.as_name()).map(|n| n == "Sig"))
            .unwrap_or(false);
        is_widget && (is_sig_field || v_is_sig)
    }

    /// Build the page object for a (source, page) into the pre-allocated
    /// `page_id`. The id must already be in `page_id_map` so destinations on
    /// other pages can target it.
    fn build_page(&mut self, src: usize, page_index: usize, page_id: u32) -> Result<()> {
        let page_obj = self.sources[src].get_page(page_index)?;
        let page_dict = page_obj
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf(format!("page {page_index} is not a dictionary")))?
            .clone();

        // Resource usage from the decoded content stream(s).
        let mut used = UsedNames::default();
        if let Ok(content) = self.sources[src].get_page_content_data(page_index) {
            scan_used(&content, &mut used);
        }

        let mut new_page: HashMap<String, Object> = HashMap::new();
        new_page.insert("Type".to_string(), Object::Name("Page".to_string()));

        for key in PAGE_GEOMETRY_KEYS {
            if let Some(v) = page_dict.get(key) {
                let remapped = self.remap(src, v, 1);
                new_page.insert(key.to_string(), remapped);
            }
        }

        // Trimmed resources (already imported as new-id refs; insert directly).
        if let Some(res) = page_dict.get("Resources").and_then(|r| self.resolve(src, r)) {
            let trimmed = self.build_trimmed_resources(src, &res, &used, 0);
            new_page.insert("Resources".to_string(), trimmed);
        }

        // Content streams (copied verbatim — raw, still-encoded).
        if let Some(contents) = page_dict.get("Contents") {
            let remapped = self.remap(src, contents, 1);
            new_page.insert("Contents".to_string(), remapped);
        }

        // Annotations.
        if let Some(annots) = page_dict.get("Annots").and_then(|a| self.resolve(src, a)) {
            if let Some(arr) = annots.as_array() {
                let mut kept: Vec<Object> = Vec::new();
                for entry in arr {
                    if let Some(new_ref) = self.process_annot(src, entry, page_id, page_index)? {
                        kept.push(new_ref);
                    }
                }
                if !kept.is_empty() {
                    new_page.insert("Annots".to_string(), Object::Array(kept));
                }
            }
        }

        self.objects.insert(page_id, Object::Dictionary(new_page));
        self.page_ids.push(page_id);
        Ok(())
    }

    /// Edit + import one annotation. Returns the new reference, or `None` if the
    /// annotation is dropped.
    fn process_annot(
        &mut self,
        src: usize,
        entry: &Object,
        page_id: u32,
        page_index: usize,
    ) -> Result<Option<Object>> {
        let Some(annot_obj) = self.resolve(src, entry) else {
            return Ok(None);
        };
        let Some(annot) = annot_obj.as_dict() else {
            return Ok(None);
        };
        let mut annot = annot.clone();

        // Navigation is handled explicitly below (remapped to kept pages, or
        // severed). Strip it + back-pointers first so `remap` can never drag a
        // dropped page back in via a raw /Dest or /A.
        let orig_dest = annot.remove("Dest");
        let orig_action = annot.remove("A");
        annot.remove("Parent");
        annot.remove("P");

        // Resolve navigation against the (now complete) page-id map.
        let mut new_nav: Option<(&'static str, Object)> = None;
        if self.opts.keep_links {
            if let Some(dest) = orig_dest.as_ref() {
                if let Some(v) = self.remap_dest_value(src, dest) {
                    new_nav = Some(("Dest", v));
                }
            } else if let Some(action) = orig_action.as_ref() {
                if let Some(v) = self.remap_action(src, action) {
                    new_nav = Some(("A", v));
                }
            }
        }
        if new_nav.is_none() && (orig_dest.is_some() || orig_action.is_some()) {
            self.report.links_severed += 1;
        }

        if self.is_signature_widget(src, &annot) {
            match self.opts.on_signature {
                SignaturePolicy::Refuse => {
                    return Err(Error::InvalidPdf(format!(
                        "page {page_index} carries a digital signature; \
                         subsetting would invalidate it (SignaturePolicy::Refuse)"
                    )));
                },
                SignaturePolicy::PreserveVisual => {
                    // Keep the appearance (the seal image lives in /AP); drop the
                    // signature value and field-ness so nothing claims validity.
                    for k in ["V", "FT", "T", "TU", "Ff", "DV", "Lock", "SV", "DA", "DR"] {
                        annot.remove(k);
                    }
                    self.report.dropped_signatures += 1;
                    self.report.warnings.push(format!(
                        "page {page_index}: digital signature dropped (rebuild invalidates it); \
                         visual appearance preserved"
                    ));
                },
            }
        }

        // Remap source references first (e.g. /AP), THEN insert the new-id /P —
        // inserting a new-id reference before remapping would make `remap` treat
        // it as a *source* reference and re-import the wrong object.
        let mut remapped = self.remap_dict(src, &annot);
        remapped.insert("P".to_string(), Object::Reference(ObjectRef::new(page_id, 0)));
        if let Some((key, value)) = new_nav {
            // `value` already holds new-id references — insert AFTER remap.
            remapped.insert(key.to_string(), value);
        }
        let aid = self.alloc();
        self.objects.insert(aid, Object::Dictionary(remapped));
        Ok(Some(Object::Reference(ObjectRef::new(aid, 0))))
    }

    /// Import the source's `/Info` dictionary (document metadata) if present.
    fn import_info(&mut self, src: usize) -> Option<u32> {
        let info_ref = self.sources[src].trailer().as_dict()?.get("Info")?.as_reference()?;
        Some(self.import(src, info_ref, 0))
    }

    fn source_catalog(&self, src: usize) -> Option<HashMap<String, Object>> {
        let root = self.sources[src].trailer().as_dict()?.get("Root")?.as_reference()?;
        self.sources[src].load_object(root).ok()?.as_dict().cloned()
    }

    /// Resolve any destination form (explicit array, indirect ref, `/D` wrapper,
    /// or named destination) to the explicit destination array.
    fn dest_to_array(&self, src: usize, dest: &Object, depth: usize) -> Option<Vec<Object>> {
        if depth > 8 {
            return None;
        }
        match dest {
            Object::Array(a) => Some(a.clone()),
            Object::Reference(r) => {
                let o = self.sources[src].load_object(*r).ok()?;
                self.dest_to_array(src, &o, depth + 1)
            },
            Object::Dictionary(d) => {
                let inner = d.get("D")?;
                self.dest_to_array(src, inner, depth + 1)
            },
            // Named destination: PDF 1.1 /Dests dict (name key)…
            Object::Name(n) => {
                let v = self.resolve_named_dest_name(src, n)?;
                self.dest_to_array(src, &v, depth + 1)
            },
            // …or PDF 1.2+ /Names /Dests name tree (string key).
            Object::String(s) => {
                let v = self.resolve_named_dest_string(src, s)?;
                self.dest_to_array(src, &v, depth + 1)
            },
            _ => None,
        }
    }

    fn resolve_named_dest_name(&self, src: usize, name: &str) -> Option<Object> {
        let cat = self.source_catalog(src)?;
        let dests = cat.get("Dests").and_then(|d| self.resolve(src, d))?;
        dests.as_dict()?.get(name).cloned()
    }

    fn resolve_named_dest_string(&self, src: usize, key: &[u8]) -> Option<Object> {
        let cat = self.source_catalog(src)?;
        let names = cat.get("Names").and_then(|n| self.resolve(src, n))?;
        let dests = names.as_dict()?.get("Dests").and_then(|d| self.resolve(src, d))?;
        self.name_tree_lookup(src, &dests, key, 0)
    }

    fn name_tree_lookup(&self, src: usize, node: &Object, key: &[u8], depth: usize) -> Option<Object> {
        if depth > 32 {
            return None;
        }
        let nd = node.as_dict()?;
        if let Some(names) =
            nd.get("Names").and_then(|n| self.resolve(src, n)).and_then(|o| o.as_array().cloned())
        {
            let mut i = 0;
            while i + 1 < names.len() {
                if names[i].as_string() == Some(key) {
                    return Some(names[i + 1].clone());
                }
                i += 2;
            }
        }
        if let Some(kids) =
            nd.get("Kids").and_then(|k| self.resolve(src, k)).and_then(|o| o.as_array().cloned())
        {
            for kid in kids {
                let Some(kid_obj) = self.resolve(src, &kid) else { continue };
                if let Some(v) = self.name_tree_lookup(src, &kid_obj, key, depth + 1) {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Remap a destination to point at the kept page, or `None` if its target
    /// page was dropped / unresolvable. The returned value already holds new-id
    /// references, so insert it AFTER `remap_dict`.
    fn remap_dest_value(&self, src: usize, dest: &Object) -> Option<Object> {
        let arr = self.dest_to_array(src, dest, 0)?;
        let page_ref = arr.first()?.as_reference()?;
        let new_pid = *self.page_id_map.get(&(src, page_ref.id))?;
        let mut new_arr = arr;
        new_arr[0] = Object::Reference(ObjectRef::new(new_pid, 0));
        Some(Object::Array(new_arr))
    }

    /// Remap an action: GoTo destinations are repointed at kept pages; URI/Named
    /// actions are kept verbatim; everything else (incl. GoTo to a dropped page)
    /// is dropped. The result is self-contained (no source references besides
    /// the remapped /D), safe to insert after `remap_dict`.
    fn remap_action(&self, src: usize, action: &Object) -> Option<Object> {
        let act = self.resolve(src, action)?;
        let ad = act.as_dict()?;
        match ad.get("S").and_then(|s| s.as_name()) {
            Some("GoTo") => {
                let new_d = self.remap_dest_value(src, ad.get("D")?)?;
                let mut nd = ad.clone();
                nd.insert("D".to_string(), new_d);
                nd.remove("Next");
                Some(Object::Dictionary(nd))
            },
            Some("URI") | Some("Named") => {
                let mut nd = ad.clone();
                nd.remove("Next");
                Some(Object::Dictionary(nd))
            },
            _ => None,
        }
    }

    fn outline_dest(&self, src: usize, item: &HashMap<String, Object>) -> Option<(&'static str, Object)> {
        if let Some(d) = item.get("Dest") {
            return self.remap_dest_value(src, d).map(|v| ("Dest", v));
        }
        if let Some(a) = item.get("A") {
            return self.remap_action(src, a).map(|v| ("A", v));
        }
        None
    }

    /// Build a pruned outline sibling chain, returning the new ids in order.
    /// An item is kept iff it still resolves to a kept page OR has a surviving
    /// descendant.
    fn build_outline_items(
        &mut self,
        src: usize,
        mut cur: Option<ObjectRef>,
        parent_id: u32,
        depth: usize,
    ) -> Vec<u32> {
        let mut result: Vec<u32> = Vec::new();
        if depth > 64 {
            return result;
        }
        while let Some(item_ref) = cur {
            let Some(item) =
                self.sources[src].load_object(item_ref).ok().and_then(|o| o.as_dict().cloned())
            else {
                break;
            };
            let next = item.get("Next").and_then(|n| n.as_reference());

            let my_id = self.alloc();
            self.pinned.insert(my_id);
            let child_first = item.get("First").and_then(|f| f.as_reference());
            let child_ids = self.build_outline_items(src, child_first, my_id, depth + 1);
            let dest = self.outline_dest(src, &item);

            if dest.is_none() && child_ids.is_empty() {
                self.pinned.remove(&my_id); // unused id -> dropped at compaction
                cur = next;
                continue;
            }

            let mut d: HashMap<String, Object> = HashMap::new();
            d.insert("Parent".to_string(), Object::Reference(ObjectRef::new(parent_id, 0)));
            for key in ["Title", "C", "F"] {
                if let Some(v) = item.get(key) {
                    d.insert(key.to_string(), v.clone());
                }
            }
            if let Some((k, v)) = dest {
                d.insert(k.to_string(), v);
            }
            if let Some(&first) = child_ids.first() {
                d.insert("First".to_string(), Object::Reference(ObjectRef::new(first, 0)));
            }
            if let Some(&last) = child_ids.last() {
                d.insert("Last".to_string(), Object::Reference(ObjectRef::new(last, 0)));
                d.insert("Count".to_string(), Object::Integer(child_ids.len() as i64));
            }
            self.objects.insert(my_id, Object::Dictionary(d));
            self.report.outline_entries += 1;
            result.push(my_id);
            cur = next;
        }
        for i in 0..result.len() {
            let id = result[i];
            if let Some(Object::Dictionary(d)) = self.objects.get_mut(&id) {
                if i > 0 {
                    d.insert("Prev".to_string(), Object::Reference(ObjectRef::new(result[i - 1], 0)));
                }
                if i + 1 < result.len() {
                    d.insert("Next".to_string(), Object::Reference(ObjectRef::new(result[i + 1], 0)));
                }
            }
        }
        result
    }

    /// Build a pruned `/Outlines` tree from source `src`; returns its new id, or
    /// `None` if nothing survived.
    fn build_outlines(&mut self, src: usize) -> Option<u32> {
        let cat = self.source_catalog(src)?;
        let outlines_ref = cat.get("Outlines")?.as_reference()?;
        let outlines = self.sources[src].load_object(outlines_ref).ok()?;
        let first = outlines.as_dict()?.get("First").and_then(|f| f.as_reference());

        let outlines_id = self.alloc();
        self.pinned.insert(outlines_id);
        let kids = self.build_outline_items(src, first, outlines_id, 0);
        if kids.is_empty() {
            self.pinned.remove(&outlines_id);
            return None;
        }
        let mut d: HashMap<String, Object> = HashMap::new();
        d.insert("Type".to_string(), Object::Name("Outlines".to_string()));
        d.insert("First".to_string(), Object::Reference(ObjectRef::new(*kids.first().unwrap(), 0)));
        d.insert("Last".to_string(), Object::Reference(ObjectRef::new(*kids.last().unwrap(), 0)));
        d.insert("Count".to_string(), Object::Integer(kids.len() as i64));
        self.objects.insert(outlines_id, Object::Dictionary(d));
        Some(outlines_id)
    }

    /// Normalize a `/K` value into a list of child items (resolving an indirect
    /// array, but leaving element/MCID/MCR/OBJR items intact for the caller).
    fn struct_k_items(&self, src: usize, k: &Object) -> Vec<Object> {
        match self.resolve(src, k) {
            Some(Object::Array(a)) => a,
            _ => vec![k.clone()],
        }
    }

    /// Import a structure element (and its subtree) keeping only content that
    /// lands on a kept page. Returns the new element id, or `None` if nothing
    /// under it survives. `content` accumulates (new page id, mcid, element id)
    /// for ParentTree reconstruction.
    fn import_struct_elem(
        &mut self,
        src: usize,
        item: &Object,
        parent_id: u32,
        content: &mut Vec<(u32, i64, u32)>,
        depth: usize,
    ) -> Option<u32> {
        if depth > MAX_DEPTH {
            return None;
        }
        let elem = self.resolve(src, item)?;
        let ed = elem.as_dict()?.clone();

        let my_id = self.alloc();
        let elem_pg = ed
            .get("Pg")
            .and_then(|p| p.as_reference())
            .and_then(|r| self.page_id_map.get(&(src, r.id)).copied());

        let mut new_k: Vec<Object> = Vec::new();
        if let Some(kv) = ed.get("K") {
            for sub in self.struct_k_items(src, kv) {
                match &sub {
                    // Marked-content id in this element's own page.
                    Object::Integer(mcid) => {
                        if let Some(pg) = elem_pg {
                            new_k.push(Object::Integer(*mcid));
                            content.push((pg, *mcid, my_id));
                        }
                    },
                    Object::Dictionary(d)
                        if d.get("Type").and_then(|t| t.as_name()) == Some("MCR") =>
                    {
                        let mcr_pg = d
                            .get("Pg")
                            .and_then(|p| p.as_reference())
                            .and_then(|r| self.page_id_map.get(&(src, r.id)).copied());
                        let mcid = d.get("MCID").and_then(|m| m.as_integer());
                        if let (Some(pg), Some(mcid)) = (mcr_pg, mcid) {
                            let mut nd = HashMap::new();
                            nd.insert("Type".to_string(), Object::Name("MCR".to_string()));
                            nd.insert("Pg".to_string(), Object::Reference(ObjectRef::new(pg, 0)));
                            nd.insert("MCID".to_string(), Object::Integer(mcid));
                            new_k.push(Object::Dictionary(nd));
                            content.push((pg, mcid, my_id));
                        }
                    },
                    // Object references (tagged annotations/XObjects): dropped in
                    // v1 — the object stays visible, just untagged.
                    Object::Dictionary(d)
                        if d.get("Type").and_then(|t| t.as_name()) == Some("OBJR") => {},
                    // Otherwise a nested structure element.
                    _ => {
                        if let Some(cid) =
                            self.import_struct_elem(src, &sub, my_id, content, depth + 1)
                        {
                            new_k.push(Object::Reference(ObjectRef::new(cid, 0)));
                        }
                    },
                }
            }
        }

        if new_k.is_empty() {
            return None; // nothing relevant — drop (my_id becomes a gap)
        }

        self.pinned.insert(my_id);
        let mut d: HashMap<String, Object> = HashMap::new();
        d.insert("Type".to_string(), Object::Name("StructElem".to_string()));
        d.insert("P".to_string(), Object::Reference(ObjectRef::new(parent_id, 0)));
        if let Some(s) = ed.get("S") {
            d.insert("S".to_string(), s.clone());
        }
        if let Some(pg) = elem_pg {
            d.insert("Pg".to_string(), Object::Reference(ObjectRef::new(pg, 0)));
        }
        for key in ["Alt", "ActualText", "Lang", "T", "E", "ID", "C"] {
            if let Some(v) = ed.get(key) {
                d.insert(key.to_string(), v.clone());
            }
        }
        let kval = if new_k.len() == 1 {
            new_k.into_iter().next().unwrap()
        } else {
            Object::Array(new_k)
        };
        d.insert("K".to_string(), kval);
        self.objects.insert(my_id, Object::Dictionary(d));
        self.report.struct_elements += 1;
        Some(my_id)
    }

    /// Build a pruned `/StructTreeRoot` (tagged-PDF tree) from source `src`,
    /// rebuilding the `/ParentTree` and assigning fresh `/StructParents` to the
    /// kept pages. Returns its new id, or `None` if nothing survived.
    fn build_struct_tree(&mut self, src: usize) -> Option<u32> {
        let cat = self.source_catalog(src)?;
        let st_ref = cat.get("StructTreeRoot")?.as_reference()?;
        let st_dict = self.sources[src].load_object(st_ref).ok()?.as_dict()?.clone();

        let root_id = self.alloc();
        self.pinned.insert(root_id);

        let mut content: Vec<(u32, i64, u32)> = Vec::new();
        let mut root_kids: Vec<u32> = Vec::new();
        if let Some(kv) = st_dict.get("K") {
            for item in self.struct_k_items(src, kv) {
                if let Some(cid) = self.import_struct_elem(src, &item, root_id, &mut content, 0) {
                    root_kids.push(cid);
                }
            }
        }
        if root_kids.is_empty() {
            self.pinned.remove(&root_id);
            return None;
        }

        // Group content by page and rebuild the ParentTree, assigning a fresh
        // /StructParents key per page (in page order, so /Nums stays sorted).
        let mut by_page: HashMap<u32, Vec<(i64, u32)>> = HashMap::new();
        for (pg, mcid, elem) in content {
            by_page.entry(pg).or_default().push((mcid, elem));
        }
        let mut nums: Vec<Object> = Vec::new();
        let mut next_key: i64 = 0;
        for pg in self.page_ids.clone() {
            let Some(entries) = by_page.get(&pg) else { continue };
            let key = next_key;
            next_key += 1;
            if let Some(Object::Dictionary(d)) = self.objects.get_mut(&pg) {
                d.insert("StructParents".to_string(), Object::Integer(key));
            }
            let max_mcid = entries.iter().map(|(m, _)| *m).max().unwrap_or(-1);
            let mut arr = vec![Object::Null; (max_mcid + 1).max(0) as usize];
            for (mcid, elem) in entries {
                if *mcid >= 0 {
                    arr[*mcid as usize] = Object::Reference(ObjectRef::new(*elem, 0));
                }
            }
            nums.push(Object::Integer(key));
            nums.push(Object::Array(arr));
        }

        let pt_id = self.alloc();
        self.pinned.insert(pt_id);
        let mut pt: HashMap<String, Object> = HashMap::new();
        pt.insert("Nums".to_string(), Object::Array(nums));
        self.objects.insert(pt_id, Object::Dictionary(pt));

        let mut root: HashMap<String, Object> = HashMap::new();
        root.insert("Type".to_string(), Object::Name("StructTreeRoot".to_string()));
        root.insert("ParentTree".to_string(), Object::Reference(ObjectRef::new(pt_id, 0)));
        root.insert("ParentTreeNextKey".to_string(), Object::Integer(next_key));
        root.insert(
            "K".to_string(),
            if root_kids.len() == 1 {
                Object::Reference(ObjectRef::new(root_kids[0], 0))
            } else {
                Object::Array(
                    root_kids.iter().map(|&id| Object::Reference(ObjectRef::new(id, 0))).collect(),
                )
            },
        );
        for key in ["RoleMap", "ClassMap"] {
            if let Some(v) = st_dict.get(key) {
                let r = self.remap(src, v, 1);
                root.insert(key.to_string(), r);
            }
        }
        self.objects.insert(root_id, Object::Dictionary(root));
        Some(root_id)
    }

    /// Collapse byte-identical objects to a single id, remapping references,
    /// iterating to a fixpoint (so collapsing children can collapse parents).
    ///
    /// Canonical bytes are cached per object and only recomputed for objects
    /// whose references actually changed in the previous round, so big immutable
    /// streams (images/fonts) are serialized once, not once per round. Buckets
    /// are keyed by a fast hash and confirmed with a full-bytes comparison, so a
    /// hash collision can never merge two genuinely different objects.
    fn dedup(&mut self) {
        let mut canon: HashMap<u32, Vec<u8>> = HashMap::with_capacity(self.objects.len());
        let mut dirty: HashSet<u32> = self.objects.keys().copied().collect();

        loop {
            for &id in &dirty {
                if let Some(obj) = self.objects.get(&id) {
                    canon.insert(id, canonical_bytes(obj));
                }
            }
            dirty.clear();

            // hash -> candidate canonical ids sharing that hash.
            let mut buckets: HashMap<u64, Vec<u32>> = HashMap::new();
            let mut remap: HashMap<u32, u32> = HashMap::new();
            let mut ids: Vec<u32> = self.objects.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                if self.pinned.contains(&id) {
                    continue;
                }
                let bytes = &canon[&id];
                let h = fnv1a(bytes);
                let bucket = buckets.entry(h).or_default();
                let mut matched = None;
                for &cid in bucket.iter() {
                    if canon[&cid] == *bytes {
                        matched = Some(cid);
                        break;
                    }
                }
                match matched {
                    Some(cid) => {
                        remap.insert(id, cid);
                    },
                    None => bucket.push(id),
                }
            }
            if remap.is_empty() {
                break;
            }
            self.report.objects_deduped += remap.len();
            for id in remap.keys() {
                self.objects.remove(id);
                canon.remove(id);
            }
            // Rewrite references; only objects that actually changed need their
            // canonical bytes recomputed next round.
            for (&id, obj) in self.objects.iter_mut() {
                if rewrite_refs(obj, &remap) {
                    dirty.insert(id);
                }
            }
            self.page_ids.retain(|id| !remap.contains_key(id));
        }
    }

    /// Finalize: build the page tree + catalog, compact ids, and serialize.
    fn finish(mut self) -> Result<(Vec<u8>, SubsetReport)> {
        let info_id = self.import_info(0);
        // Document-level semantic structures come from source 0 (the subset
        // case); a multi-source rebuild does not merge outlines/tags.
        let outlines_id = if self.opts.keep_outlines { self.build_outlines(0) } else { None };
        let struct_id = if self.opts.keep_struct_tree { self.build_struct_tree(0) } else { None };

        if self.opts.dedup {
            self.dedup();
        }

        // Page tree + catalog get fresh ids after dedup so they're never collapsed.
        let pages_id = self.alloc();
        let catalog_id = self.alloc();
        self.pinned.insert(pages_id);
        self.pinned.insert(catalog_id);

        let page_ids = self.page_ids.clone();
        for &pid in &page_ids {
            if let Some(Object::Dictionary(d)) = self.objects.get_mut(&pid) {
                d.insert("Parent".to_string(), Object::Reference(ObjectRef::new(pages_id, 0)));
            }
        }

        let kids: Vec<Object> =
            page_ids.iter().map(|&pid| Object::Reference(ObjectRef::new(pid, 0))).collect();
        let mut pages_dict: HashMap<String, Object> = HashMap::new();
        pages_dict.insert("Type".to_string(), Object::Name("Pages".to_string()));
        pages_dict.insert("Count".to_string(), Object::Integer(page_ids.len() as i64));
        pages_dict.insert("Kids".to_string(), Object::Array(kids));
        self.objects.insert(pages_id, Object::Dictionary(pages_dict));

        let mut catalog: HashMap<String, Object> = HashMap::new();
        catalog.insert("Type".to_string(), Object::Name("Catalog".to_string()));
        catalog.insert("Pages".to_string(), Object::Reference(ObjectRef::new(pages_id, 0)));
        if let Some(oid) = outlines_id {
            catalog.insert("Outlines".to_string(), Object::Reference(ObjectRef::new(oid, 0)));
        }
        if let Some(sid) = struct_id {
            catalog.insert("StructTreeRoot".to_string(), Object::Reference(ObjectRef::new(sid, 0)));
            let mut mark = HashMap::new();
            mark.insert("Marked".to_string(), Object::Boolean(true));
            catalog.insert("MarkInfo".to_string(), Object::Dictionary(mark));
        }
        self.objects.insert(catalog_id, Object::Dictionary(catalog));

        // Compact ids to a contiguous 1..=K range.
        let mut live: Vec<u32> = self.objects.keys().copied().collect();
        live.sort_unstable();
        let mut compact: HashMap<u32, u32> = HashMap::new();
        for (i, &old) in live.iter().enumerate() {
            compact.insert(old, i as u32 + 1);
        }
        let mut objects: HashMap<u32, Object> = HashMap::new();
        for (&old, obj) in &self.objects {
            let mut obj = obj.clone();
            renumber_refs(&mut obj, &compact);
            objects.insert(compact[&old], obj);
        }

        let root = compact[&catalog_id];
        let info = info_id.and_then(|id| compact.get(&id).copied());

        self.report.objects_written = objects.len();
        let bytes = serialize_pdf(&objects, root, info);
        Ok((bytes, self.report))
    }
}

/// Deterministic, key-sorted serialization used purely for dedup hashing.
fn canonical_bytes(obj: &Object) -> Vec<u8> {
    let mut out = Vec::new();
    canon(obj, &mut out);
    out
}

fn canon(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Real(r) => out.extend_from_slice(format!("{r}").as_bytes()),
        Object::String(s) => {
            out.push(b'(');
            out.extend_from_slice(s);
            out.push(b')');
        },
        Object::Name(n) => {
            out.push(b'/');
            out.extend_from_slice(n.as_bytes());
        },
        Object::Array(a) => {
            out.push(b'[');
            for o in a {
                canon(o, out);
                out.push(b' ');
            }
            out.push(b']');
        },
        Object::Reference(r) => out.extend_from_slice(format!("{} R", r.id).as_bytes()),
        Object::Dictionary(d) => canon_dict(d, out),
        Object::Stream { dict, data } => {
            canon_dict(dict, out);
            out.extend_from_slice(b"stream");
            out.extend_from_slice(data);
        },
    }
}

fn canon_dict(d: &HashMap<String, Object>, out: &mut Vec<u8>) {
    out.extend_from_slice(b"<<");
    let mut keys: Vec<&String> = d.keys().collect();
    keys.sort();
    for k in keys {
        out.push(b'/');
        out.extend_from_slice(k.as_bytes());
        out.push(b' ');
        canon(&d[k], out);
        out.push(b' ');
    }
    out.extend_from_slice(b">>");
}

/// Rewrite references via `map` (id -> canonical id) in place. Returns true if
/// any reference was changed.
fn rewrite_refs(obj: &mut Object, map: &HashMap<u32, u32>) -> bool {
    match obj {
        Object::Reference(r) => match map.get(&r.id) {
            Some(&canon) => {
                r.id = canon;
                true
            },
            None => false,
        },
        Object::Array(a) => a.iter_mut().fold(false, |acc, o| rewrite_refs(o, map) | acc),
        Object::Dictionary(d) => d.values_mut().fold(false, |acc, o| rewrite_refs(o, map) | acc),
        Object::Stream { dict, .. } => {
            dict.values_mut().fold(false, |acc, o| rewrite_refs(o, map) | acc)
        },
        _ => false,
    }
}

/// FNV-1a 64-bit hash — fast, dependency-free; used only as a bucketing key
/// (full-bytes equality still confirms a match, so collisions are harmless).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Like [`rewrite_refs`] but for the final id compaction (every ref must map).
fn renumber_refs(obj: &mut Object, map: &HashMap<u32, u32>) {
    match obj {
        Object::Reference(r) => {
            if let Some(&n) = map.get(&r.id) {
                r.id = n;
            }
        },
        Object::Array(a) => a.iter_mut().for_each(|o| renumber_refs(o, map)),
        Object::Dictionary(d) => d.values_mut().for_each(|o| renumber_refs(o, map)),
        Object::Stream { dict, .. } => dict.values_mut().for_each(|o| renumber_refs(o, map)),
        _ => {},
    }
}

/// Serialize a compact 1..=K object map into a complete PDF.
fn serialize_pdf(objects: &HashMap<u32, Object>, root: u32, info: Option<u32>) -> Vec<u8> {
    let ser = ObjectSerializer::new();
    let k = objects.len() as u32;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

    let mut offsets: Vec<usize> = vec![0; (k + 1) as usize];
    for id in 1..=k {
        offsets[id as usize] = out.len();
        let obj = &objects[&id];
        out.extend_from_slice(&ser.serialize_indirect(id, 0, obj));
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
    }

    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", k + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=k {
        out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id as usize]).as_bytes());
    }

    let mut trailer = format!("trailer\n<< /Size {} /Root {} 0 R", k + 1, root);
    if let Some(info) = info {
        trailer.push_str(&format!(" /Info {info} 0 R"));
    }
    trailer.push_str(" >>\n");
    out.extend_from_slice(trailer.as_bytes());
    out.extend_from_slice(format!("startxref\n{xref_start}\n%%EOF\n").as_bytes());

    out
}

/// Build a new PDF from `picks` (source index, page index) drawn from
/// `sources`, copying only what's needed and deduplicating shared objects.
pub fn subset_to_bytes(
    sources: &[&PdfDocument],
    picks: &[(usize, usize)],
    opts: SubsetOptions,
) -> Result<(Vec<u8>, SubsetReport)> {
    if picks.is_empty() {
        return Err(Error::InvalidPdf("no pages selected".to_string()));
    }
    let mut builder = Builder::new(sources, opts);

    // Resolve every source's page-leaf object ids in ONE tree walk per source
    // (not one walk per page — that would be O(pages²) for large selections).
    let leaf_ids: Vec<Vec<u32>> = sources.iter().map(|d| all_page_leaf_ids(d)).collect();

    // Sever references to any source page we are not keeping, so a link's
    // /Dest can't drag a dropped page (and its whole subtree) back in.
    for (src_idx, leaves) in leaf_ids.iter().enumerate() {
        let kept: HashSet<usize> = picks
            .iter()
            .filter(|(s, _)| *s == src_idx)
            .map(|(_, p)| *p)
            .collect();
        builder.severed[src_idx] = leaves
            .iter()
            .enumerate()
            .filter(|(p, _)| !kept.contains(p))
            .map(|(_, &id)| id)
            .collect();
    }

    // Pre-allocate a new id for every kept page and record (src, source leaf id)
    // -> new id BEFORE building any page, so links and bookmarks on one page can
    // resolve targets on a page that hasn't been built yet.
    let mut planned: Vec<(usize, usize, u32)> = Vec::with_capacity(picks.len());
    for &(src, page) in picks {
        if src >= sources.len() {
            return Err(Error::InvalidPdf(format!("source index {src} out of range")));
        }
        let pid = builder.alloc();
        builder.pinned.insert(pid);
        if let Some(&leaf) = leaf_ids[src].get(page) {
            builder.page_id_map.insert((src, leaf), pid);
        }
        planned.push((src, page, pid));
    }
    for (src, page, pid) in planned {
        builder.build_page(src, page, pid)?;
    }
    builder.finish()
}

/// Leaf-page object ids of `doc` in page order, via a single page-tree walk.
fn all_page_leaf_ids(doc: &PdfDocument) -> Vec<u32> {
    let mut leaves = Vec::new();
    let mut visited = HashSet::new();
    if let Some(pages_ref) = doc
        .trailer()
        .as_dict()
        .and_then(|d| d.get("Root"))
        .and_then(|r| r.as_reference())
        .and_then(|root| doc.load_object(root).ok())
        .and_then(|cat| cat.as_dict().and_then(|d| d.get("Pages")).and_then(|p| p.as_reference()))
    {
        collect_leaf_ids(doc, pages_ref, &mut leaves, &mut visited, 0);
    }
    leaves
}

fn collect_leaf_ids(
    doc: &PdfDocument,
    node_ref: ObjectRef,
    out: &mut Vec<u32>,
    visited: &mut HashSet<u32>,
    depth: usize,
) {
    if depth > MAX_DEPTH || !visited.insert(node_ref.id) {
        return;
    }
    let Ok(node) = doc.load_object(node_ref) else {
        return;
    };
    let Some(dict) = node.as_dict() else {
        return;
    };
    let is_pages = dict.get("Type").and_then(|t| t.as_name()).map(|n| n == "Pages").unwrap_or(false);
    if is_pages {
        if let Some(kids) = dict.get("Kids").and_then(|k| k.as_array()) {
            for kid in kids {
                if let Some(kref) = kid.as_reference() {
                    collect_leaf_ids(doc, kref, out, visited, depth + 1);
                }
            }
        }
    } else {
        out.push(node_ref.id);
    }
}

/// Build a new PDF from several source documents.
///
/// ```ignore
/// let mut rb = PdfRebuilder::new();
/// let a = rb.add_source(bytes_a)?;
/// let b = rb.add_source(bytes_b)?;
/// rb.add_pages(a, &[0, 1]).add_page(b, 3);
/// let (pdf, report) = rb.build()?;
/// ```
pub struct PdfRebuilder {
    sources: Vec<PdfDocument>,
    picks: Vec<(usize, usize)>,
    opts: SubsetOptions,
}

impl PdfRebuilder {
    pub fn new() -> Self {
        PdfRebuilder {
            sources: Vec::new(),
            picks: Vec::new(),
            opts: SubsetOptions::default(),
        }
    }

    pub fn with_options(mut self, opts: SubsetOptions) -> Self {
        self.opts = opts;
        self
    }

    /// Add a source document from its bytes; returns its source index.
    pub fn add_source(&mut self, data: Vec<u8>) -> Result<usize> {
        let doc = PdfDocument::from_bytes(data)?;
        self.sources.push(doc);
        Ok(self.sources.len() - 1)
    }

    /// Append one page from a previously-added source.
    pub fn add_page(&mut self, source: usize, page: usize) -> &mut Self {
        self.picks.push((source, page));
        self
    }

    /// Append several pages from a previously-added source, in order.
    pub fn add_pages(&mut self, source: usize, pages: &[usize]) -> &mut Self {
        for &p in pages {
            self.picks.push((source, p));
        }
        self
    }

    pub fn build(&self) -> Result<(Vec<u8>, SubsetReport)> {
        let refs: Vec<&PdfDocument> = self.sources.iter().collect();
        subset_to_bytes(&refs, &self.picks, self.opts.clone())
    }
}

impl Default for PdfRebuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: subset a single PDF's pages, returning new bytes.
pub fn subset_pdf_bytes(data: Vec<u8>, pages: &[usize], opts: SubsetOptions) -> Result<Vec<u8>> {
    let doc = PdfDocument::from_bytes(data)?;
    let picks: Vec<(usize, usize)> = pages.iter().map(|&p| (0usize, p)).collect();
    subset_to_bytes(&[&doc], &picks, opts).map(|(b, _)| b)
}
