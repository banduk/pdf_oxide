//! Building a single kept page: trimmed resources, content, and annotations
//! (with link remapping and the signature policy).

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::object::{Object, ObjectRef};

use super::resources::{scan_used, UsedNames};
use super::{Builder, SignaturePolicy};

/// Page dictionary keys carried verbatim (besides the ones we rebuild:
/// `/Type`, `/Parent`, `/Resources`, `/Contents`, `/Annots`).
const PAGE_GEOMETRY_KEYS: [&str; 8] = [
    "MediaBox", "CropBox", "BleedBox", "TrimBox", "ArtBox", "Rotate", "UserUnit", "Group",
];

impl Builder<'_> {
    /// Build the page object for a (source, page) into the pre-allocated
    /// `page_id`. The id must already be in `page_id_map` so destinations on
    /// other pages can target it.
    pub(super) fn build_page(&mut self, src: usize, page_index: usize, page_id: u32) -> Result<()> {
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
        if let Some(res) = page_dict
            .get("Resources")
            .and_then(|r| self.resolve(src, r))
        {
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
        let orig_parent = annot.remove("Parent").and_then(|p| p.as_reference());
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

        let mut is_signature = false;
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
                    is_signature = true;
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

        // Reconnect interactive form-field widgets to a rebuilt /AcroForm tree.
        // A dropped signature is never re-registered as a field.
        let is_field = !is_signature
            && self.opts.keep_acroform
            && (annot.contains_key("FT") || orig_parent.is_some());
        if is_field {
            // Pin so two distinct fields can't be merged by dedup.
            self.pinned.insert(aid);
            if let Some(pfid) = self.register_form_widget(src, &annot, aid, orig_parent) {
                if let Some(Object::Dictionary(d)) = self.objects.get_mut(&aid) {
                    d.insert("Parent".to_string(), Object::Reference(ObjectRef::new(pfid, 0)));
                }
            }
        }
        Ok(Some(Object::Reference(ObjectRef::new(aid, 0))))
    }
}
