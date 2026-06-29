//! Preserving interactive form fields (`/AcroForm`) whose widgets land on kept
//! pages, with the `/Fields` tree pruned and relinked.
//!
//! Widgets are kept by [`super::pages`] (they carry the appearance); this module
//! reconnects them to a rebuilt field tree. The common case — a *merged*
//! field/widget (one object that is both the field and its widget) — becomes a
//! top-level field directly; a *pure* widget with a `/Parent` field imports that
//! field (and its ancestors), pruning each field's `/Kids` to the widgets that
//! actually survived.

use std::collections::HashMap;

use crate::object::{Object, ObjectRef};

use super::{Builder, MAX_DEPTH};

/// Field attributes copied verbatim (remapped). `/Kids` and `/Parent` are
/// rebuilt, not copied.
const FIELD_KEYS: [&str; 11] = [
    "FT", "T", "TU", "TM", "Ff", "V", "DV", "DA", "Q", "Opt", "MaxLen",
];

impl Builder<'_> {
    /// Register a kept widget (new id `widget_id`) into the form-field tree.
    /// `orig_parent` is the widget's source `/Parent` (field) ref, if any.
    /// Returns the new `/Parent` id to set on the widget, or `None` for a
    /// top-level merged field/widget.
    pub(super) fn register_form_widget(
        &mut self,
        src: usize,
        annot: &HashMap<String, Object>,
        widget_id: u32,
        orig_parent: Option<ObjectRef>,
    ) -> Option<u32> {
        if let Some(pref) = orig_parent {
            let pfid = self.import_form_field(src, pref, 0);
            self.field_kids.entry(pfid).or_default().push(widget_id);
            Some(pfid)
        } else {
            // No parent: a merged field/widget is itself a top-level field.
            if annot.contains_key("FT") {
                self.top_fields.push(widget_id);
            }
            None
        }
    }

    /// Import a form field object (without its `/Kids` — those are rebuilt) and
    /// its ancestor chain. Cached per source field.
    fn import_form_field(&mut self, src: usize, field_ref: ObjectRef, depth: usize) -> u32 {
        if let Some(&n) = self.field_map.get(&(src, field_ref.id)) {
            return n;
        }
        let new_id = self.alloc();
        self.pinned.insert(new_id);
        self.field_map.insert((src, field_ref.id), new_id);

        let fd = self.sources[src]
            .load_object(field_ref)
            .ok()
            .and_then(|o| o.as_dict().cloned())
            .unwrap_or_default();

        let mut nd: HashMap<String, Object> = HashMap::new();
        for key in FIELD_KEYS {
            if let Some(v) = fd.get(key) {
                nd.insert(key.to_string(), self.remap(src, v, depth + 1));
            }
        }
        match fd.get("Parent").and_then(|p| p.as_reference()) {
            Some(pref) if depth < MAX_DEPTH => {
                let pfid = self.import_form_field(src, pref, depth + 1);
                nd.insert("Parent".to_string(), Object::Reference(ObjectRef::new(pfid, 0)));
                self.field_kids.entry(pfid).or_default().push(new_id);
            },
            _ => self.top_fields.push(new_id),
        }
        self.objects.insert(new_id, Object::Dictionary(nd));
        new_id
    }

    /// Assemble each field's `/Kids` and build the `/AcroForm` dictionary,
    /// carrying `/DR` `/DA` `/Q` `/NeedAppearances` from the source. Returns its
    /// new id, or `None` if no fields survived.
    pub(super) fn build_acroform(&mut self, src: usize) -> Option<u32> {
        if self.top_fields.is_empty() {
            return None;
        }
        let kids = std::mem::take(&mut self.field_kids);
        for (fid, children) in kids {
            if children.is_empty() {
                continue;
            }
            if let Some(Object::Dictionary(d)) = self.objects.get_mut(&fid) {
                let arr = children
                    .iter()
                    .map(|&c| Object::Reference(ObjectRef::new(c, 0)))
                    .collect();
                d.insert("Kids".to_string(), Object::Array(arr));
            }
        }

        let mut af: HashMap<String, Object> = HashMap::new();
        let fields: Vec<Object> = self
            .top_fields
            .iter()
            .map(|&f| Object::Reference(ObjectRef::new(f, 0)))
            .collect();
        self.report.form_fields = self.top_fields.len();
        af.insert("Fields".to_string(), Object::Array(fields));

        if let Some(src_af) = self
            .source_catalog(src)
            .and_then(|cat| cat.get("AcroForm").and_then(|a| self.resolve(src, a)))
            .and_then(|o| o.as_dict().cloned())
        {
            for key in ["DR", "DA", "Q", "NeedAppearances"] {
                if let Some(v) = src_af.get(key) {
                    let r = self.remap(src, v, 1);
                    af.insert(key.to_string(), r);
                }
            }
        }

        let af_id = self.alloc();
        self.pinned.insert(af_id);
        self.objects.insert(af_id, Object::Dictionary(af));
        Some(af_id)
    }
}
