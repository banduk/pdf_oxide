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
//! ## Module layout
//!
//! The pipeline is split by concern (mirroring `redaction/` and `signatures/`):
//! [`pages`] builds each kept page, [`resources`] trims `/Resources` (and Form
//! XObjects), [`dests`] remaps link/GoTo destinations, [`outlines`] prunes the
//! bookmark tree, [`struct_tree`] prunes the tagged-PDF tree, [`dedup`]
//! collapses duplicates, and [`serialize`] writes the final PDF. The shared
//! deep-copy engine (`Builder`) lives here.
//!
//! ## Forms
//!
//! A Form XObject the page uses is copied with its own `/Resources` trimmed to
//! what the form draws (`SubsetOptions::trim_forms`).
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

use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::object::{Object, ObjectRef};

mod dedup;
mod dests;
mod outlines;
mod pages;
mod resources;
mod serialize;
mod struct_tree;

/// Maximum object-graph recursion depth while copying (cycle/blowup guard).
pub(super) const MAX_DEPTH: usize = 256;

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

/// The deep-copy / trim / dedup engine. Methods are implemented across the
/// submodules by concern; this file holds the struct + the core graph copy.
pub(super) struct Builder<'a> {
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
        // A broken xref entry degrades to null rather than aborting the whole
        // subset — the surrounding object graph is still recoverable.
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
            Object::Array(items) => Object::Array(
                items
                    .iter()
                    .map(|o| self.remap(src, o, depth + 1))
                    .collect(),
            ),
            Object::Dictionary(d) => Object::Dictionary(
                d.iter()
                    .map(|(k, v)| (k.clone(), self.remap(src, v, depth + 1)))
                    .collect(),
            ),
            Object::Stream { dict, data } => Object::Stream {
                dict: dict
                    .iter()
                    .map(|(k, v)| (k.clone(), self.remap(src, v, depth + 1)))
                    .collect(),
                data: data.clone(),
            },
            other => other.clone(),
        }
    }

    /// Remap an already-owned dictionary's values (used for synthesized dicts
    /// such as a trimmed `/Resources` or an edited annotation).
    fn remap_dict(
        &mut self,
        src: usize,
        dict: &HashMap<String, Object>,
    ) -> HashMap<String, Object> {
        dict.iter()
            .map(|(k, v)| (k.clone(), self.remap(src, v, 1)))
            .collect()
    }

    /// Import the source's `/Info` dictionary (document metadata) if present.
    fn import_info(&mut self, src: usize) -> Option<u32> {
        let info_ref = self.sources[src]
            .trailer()
            .as_dict()?
            .get("Info")?
            .as_reference()?;
        Some(self.import(src, info_ref, 0))
    }

    fn source_catalog(&self, src: usize) -> Option<HashMap<String, Object>> {
        let root = self.sources[src]
            .trailer()
            .as_dict()?
            .get("Root")?
            .as_reference()?;
        self.sources[src].load_object(root).ok()?.as_dict().cloned()
    }

    /// Finalize: build the page tree + catalog, compact ids, and serialize.
    fn finish(mut self) -> Result<(Vec<u8>, SubsetReport)> {
        let info_id = self.import_info(0);
        // Document-level semantic structures come from source 0 (the subset
        // case); a multi-source rebuild does not merge outlines/tags.
        let outlines_id = if self.opts.keep_outlines {
            self.build_outlines(0)
        } else {
            None
        };
        let struct_id = if self.opts.keep_struct_tree {
            self.build_struct_tree(0)
        } else {
            None
        };

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

        let kids: Vec<Object> = page_ids
            .iter()
            .map(|&pid| Object::Reference(ObjectRef::new(pid, 0)))
            .collect();
        let mut pages_dict: HashMap<String, Object> = HashMap::new();
        pages_dict.insert("Type".to_string(), Object::Name("Pages".to_string()));
        pages_dict.insert("Count".to_string(), Object::Integer(page_ids.len() as i64));
        pages_dict.insert("Kids".to_string(), Object::Array(kids));
        self.objects
            .insert(pages_id, Object::Dictionary(pages_dict));

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
            serialize::renumber_refs(&mut obj, &compact);
            objects.insert(compact[&old], obj);
        }

        let root = compact[&catalog_id];
        let info = info_id.and_then(|id| compact.get(&id).copied());

        self.report.objects_written = objects.len();
        let bytes = serialize::serialize_pdf(&objects, root, info);
        Ok((bytes, self.report))
    }
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
    // (reusing PdfDocument::all_page_refs — not one walk per page).
    let leaf_ids: Vec<Vec<u32>> = sources
        .iter()
        .map(|d| {
            d.all_page_refs()
                .map(|refs| refs.iter().map(|r| r.id).collect())
                .unwrap_or_default()
        })
        .collect();

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
