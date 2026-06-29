//! Remapping link / GoTo destinations onto kept pages.
//!
//! Named-destination resolution (PDF 1.1 `/Dests` dict and PDF 1.2+
//! `/Names` → `/Dests` name tree) reuses the shared helpers in
//! [`crate::outline`] rather than duplicating the name-tree walk.

use std::collections::HashMap;

use crate::object::{Object, ObjectRef};
use crate::outline::{lookup_named_dest, normalize_dest_value};

use super::Builder;

impl Builder<'_> {
    /// Resolve any destination form (explicit array, indirect ref, `/D` wrapper,
    /// or named destination) to the explicit destination array.
    fn dest_to_array(&self, src: usize, dest: &Object) -> Option<Vec<Object>> {
        let doc = self.sources[src];
        let resolve = |r: ObjectRef| doc.load_object(r).ok();
        let normalized = match dest {
            // Named destination — resolve against the catalog (PDF 1.1 /Dests
            // dict by name, or PDF 1.2+ /Names /Dests name tree by string).
            Object::Name(n) => {
                lookup_named_dest(&self.source_catalog(src)?, n.as_bytes(), &resolve, 0)
            },
            Object::String(s) => lookup_named_dest(&self.source_catalog(src)?, s, &resolve, 0),
            // Explicit array, indirect ref, or `<< /D … >>` wrapper.
            other => normalize_dest_value(other, &resolve),
        }?;
        normalized.as_array().cloned()
    }

    /// Remap a destination to point at the kept page, or `None` if its target
    /// page was dropped / unresolvable. The returned value already holds new-id
    /// references, so insert it AFTER `remap_dict`.
    pub(super) fn remap_dest_value(&self, src: usize, dest: &Object) -> Option<Object> {
        let arr = self.dest_to_array(src, dest)?;
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
    pub(super) fn remap_action(&self, src: usize, action: &Object) -> Option<Object> {
        let act = self.resolve(src, action)?;
        let ad = act.as_dict()?;
        match ad.get("S").and_then(|s| s.as_name()) {
            Some("GoTo") => {
                let new_d = self.remap_dest_value(src, ad.get("D")?)?;
                let mut nd: HashMap<String, Object> = ad.clone();
                nd.insert("D".to_string(), new_d);
                nd.remove("Next");
                Some(Object::Dictionary(nd))
            },
            Some("URI") | Some("Named") => {
                let mut nd: HashMap<String, Object> = ad.clone();
                nd.remove("Next");
                Some(Object::Dictionary(nd))
            },
            _ => None,
        }
    }
}
