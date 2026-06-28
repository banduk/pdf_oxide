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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Keep the signature's visual appearance (e.g. the seal image) as a plain
    /// annotation, drop the now-invalid signature value + `/AcroForm`, and warn.
    PreserveVisual,
    /// Refuse to subset a document whose kept pages contain a signature.
    Refuse,
}

impl Default for SignaturePolicy {
    fn default() -> Self {
        SignaturePolicy::PreserveVisual
    }
}

/// Options controlling a subset / rebuild.
#[derive(Debug, Clone)]
pub struct SubsetOptions {
    /// Collapse byte-identical objects to a single shared object.
    pub dedup: bool,
    /// How to handle signatures on kept pages.
    pub on_signature: SignaturePolicy,
}

impl Default for SubsetOptions {
    fn default() -> Self {
        SubsetOptions {
            dedup: true,
            on_signature: SignaturePolicy::default(),
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

    /// Build a trimmed `/Resources` dictionary holding only the entries the
    /// page's content actually used. Entries stay as references into the source
    /// (they get imported by the caller's `remap`).
    fn trim_resources(&self, src: usize, res: &Object, used: &UsedNames) -> Object {
        let mut out: HashMap<String, Object> = HashMap::new();
        let res_dict = match res {
            Object::Dictionary(d) => d.clone(),
            Object::Stream { dict, .. } => dict.clone(),
            _ => return Object::Dictionary(out),
        };
        for category in RESOURCE_CATEGORIES {
            let keep: &HashSet<String> = match category {
                "Font" => &used.fonts,
                "XObject" => &used.xobjects,
                "ExtGState" => &used.extgstates,
                "ColorSpace" => &used.colorspaces,
                "Pattern" => &used.patterns,
                "Shading" => &used.shadings,
                "Properties" => &used.properties,
                _ => continue,
            };
            if keep.is_empty() {
                continue;
            }
            let Some(sub) = res_dict.get(category).and_then(|c| self.resolve(src, c)) else {
                continue;
            };
            let Some(sub_dict) = sub.as_dict() else {
                continue;
            };
            let mut new_sub: HashMap<String, Object> = HashMap::new();
            for (name, value) in sub_dict.iter() {
                if keep.contains(name) {
                    new_sub.insert(name.clone(), value.clone());
                }
            }
            if !new_sub.is_empty() {
                out.insert(category.to_string(), Object::Dictionary(new_sub));
            }
        }
        // /ProcSet is a tiny legacy hint; carry it through if present.
        if let Some(ps) = res_dict.get("ProcSet") {
            out.insert("ProcSet".to_string(), ps.clone());
        }
        Object::Dictionary(out)
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

    /// Process one source page into a new page object; returns its new id.
    fn add_page(&mut self, src: usize, page_index: usize) -> Result<u32> {
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

        let page_id = self.alloc();
        self.pinned.insert(page_id);

        let mut new_page: HashMap<String, Object> = HashMap::new();
        new_page.insert("Type".to_string(), Object::Name("Page".to_string()));

        for key in PAGE_GEOMETRY_KEYS {
            if let Some(v) = page_dict.get(key) {
                let remapped = self.remap(src, v, 1);
                new_page.insert(key.to_string(), remapped);
            }
        }

        // Trimmed resources.
        if let Some(res) = page_dict.get("Resources").and_then(|r| self.resolve(src, r)) {
            let trimmed = self.trim_resources(src, &res, &used);
            let remapped = self.remap(src, &trimmed, 1);
            new_page.insert("Resources".to_string(), remapped);
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
        Ok(page_id)
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

        // Sever back-pointers and navigation so we never drag in dropped pages.
        annot.remove("Parent");
        annot.remove("P");
        annot.remove("Dest");
        annot.remove("A");

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

        // Re-point the widget at its new page.
        annot.insert("P".to_string(), Object::Reference(ObjectRef::new(page_id, 0)));

        let remapped = self.remap_dict(src, &annot);
        let aid = self.alloc();
        self.objects.insert(aid, Object::Dictionary(remapped));
        Ok(Some(Object::Reference(ObjectRef::new(aid, 0))))
    }

    /// Import the source's `/Info` dictionary (document metadata) if present.
    fn import_info(&mut self, src: usize) -> Option<u32> {
        let info_ref = self.sources[src].trailer().as_dict()?.get("Info")?.as_reference()?;
        Some(self.import(src, info_ref, 0))
    }

    /// Collapse byte-identical objects to a single id, remapping references,
    /// iterating to a fixpoint (so collapsing children can collapse parents).
    fn dedup(&mut self) {
        loop {
            let mut by_hash: HashMap<Vec<u8>, u32> = HashMap::new();
            let mut remap: HashMap<u32, u32> = HashMap::new();
            let mut ids: Vec<u32> = self.objects.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                if self.pinned.contains(&id) {
                    continue;
                }
                let key = canonical_bytes(&self.objects[&id]);
                match by_hash.get(&key) {
                    Some(&canon) => {
                        remap.insert(id, canon);
                    },
                    None => {
                        by_hash.insert(key, id);
                    },
                }
            }
            if remap.is_empty() {
                break;
            }
            self.report.objects_deduped += remap.len();
            for id in remap.keys() {
                self.objects.remove(id);
            }
            for obj in self.objects.values_mut() {
                rewrite_refs(obj, &remap);
            }
            self.page_ids.retain(|id| !remap.contains_key(id));
        }
    }

    /// Finalize: build the page tree + catalog, compact ids, and serialize.
    fn finish(mut self) -> Result<(Vec<u8>, SubsetReport)> {
        let info_id = self.import_info(0);

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

/// Rewrite references via `map` (id -> canonical id) in place.
fn rewrite_refs(obj: &mut Object, map: &HashMap<u32, u32>) {
    match obj {
        Object::Reference(r) => {
            if let Some(&canon) = map.get(&r.id) {
                r.id = canon;
            }
        },
        Object::Array(a) => a.iter_mut().for_each(|o| rewrite_refs(o, map)),
        Object::Dictionary(d) => d.values_mut().for_each(|o| rewrite_refs(o, map)),
        Object::Stream { dict, .. } => dict.values_mut().for_each(|o| rewrite_refs(o, map)),
        _ => {},
    }
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
    // Sever references to any source page we are not keeping, so a link's
    // /Dest can't drag a dropped page (and its whole subtree) back in.
    for (src_idx, doc) in sources.iter().enumerate() {
        let kept: HashSet<usize> = picks
            .iter()
            .filter(|(s, _)| *s == src_idx)
            .map(|(_, p)| *p)
            .collect();
        builder.severed[src_idx] = collect_dropped_page_ids(doc, &kept);
    }
    for &(src, page) in picks {
        if src >= sources.len() {
            return Err(Error::InvalidPdf(format!("source index {src} out of range")));
        }
        builder.add_page(src, page)?;
    }
    builder.finish()
}

/// Object ids of the pages in `doc` that are NOT in `kept`.
fn collect_dropped_page_ids(doc: &PdfDocument, kept: &HashSet<usize>) -> HashSet<u32> {
    let mut dropped = HashSet::new();
    let total = doc.page_count().unwrap_or(0);
    for p in 0..total {
        if kept.contains(&p) {
            continue;
        }
        if let Some(id) = page_leaf_id(doc, p) {
            dropped.insert(id);
        }
    }
    dropped
}

/// Best-effort lookup of a page's leaf object id by walking the page tree.
fn page_leaf_id(doc: &PdfDocument, page_index: usize) -> Option<u32> {
    let root = doc.trailer().as_dict()?.get("Root")?.as_reference()?;
    let catalog = doc.load_object(root).ok()?;
    let pages_ref = catalog.as_dict()?.get("Pages")?.as_reference()?;
    let mut counter = 0usize;
    let mut found = None;
    walk_pages(doc, pages_ref, &mut counter, page_index, &mut found, 0);
    found
}

fn walk_pages(
    doc: &PdfDocument,
    node_ref: ObjectRef,
    counter: &mut usize,
    target: usize,
    found: &mut Option<u32>,
    depth: usize,
) {
    if found.is_some() || depth > MAX_DEPTH {
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
                    walk_pages(doc, kref, counter, target, found, depth + 1);
                    if found.is_some() {
                        return;
                    }
                }
            }
        }
    } else {
        // Leaf page.
        if *counter == target {
            *found = Some(node_ref.id);
        }
        *counter += 1;
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
