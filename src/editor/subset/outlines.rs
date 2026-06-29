//! Pruning the document outline (bookmarks) to entries that still resolve to a
//! kept page (or have a surviving descendant).

use std::collections::HashMap;

use crate::object::{Object, ObjectRef};

use super::Builder;

impl Builder<'_> {
    fn outline_dest(
        &self,
        src: usize,
        item: &HashMap<String, Object>,
    ) -> Option<(&'static str, Object)> {
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
            let Some(item) = self.sources[src]
                .load_object(item_ref)
                .ok()
                .and_then(|o| o.as_dict().cloned())
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
                    d.insert(
                        "Prev".to_string(),
                        Object::Reference(ObjectRef::new(result[i - 1], 0)),
                    );
                }
                if i + 1 < result.len() {
                    d.insert(
                        "Next".to_string(),
                        Object::Reference(ObjectRef::new(result[i + 1], 0)),
                    );
                }
            }
        }
        result
    }

    /// Build a pruned `/Outlines` tree from source `src`; returns its new id, or
    /// `None` if nothing survived.
    pub(super) fn build_outlines(&mut self, src: usize) -> Option<u32> {
        let cat = self.source_catalog(src)?;
        let outlines_ref = cat.get("Outlines")?.as_reference()?;
        let outlines = self.sources[src].load_object(outlines_ref).ok()?;
        let first = outlines
            .as_dict()?
            .get("First")
            .and_then(|f| f.as_reference());

        let outlines_id = self.alloc();
        self.pinned.insert(outlines_id);
        let kids = self.build_outline_items(src, first, outlines_id, 0);
        if kids.is_empty() {
            self.pinned.remove(&outlines_id);
            return None;
        }
        let mut d: HashMap<String, Object> = HashMap::new();
        d.insert("Type".to_string(), Object::Name("Outlines".to_string()));
        d.insert(
            "First".to_string(),
            Object::Reference(ObjectRef::new(*kids.first().unwrap(), 0)),
        );
        d.insert("Last".to_string(), Object::Reference(ObjectRef::new(*kids.last().unwrap(), 0)));
        d.insert("Count".to_string(), Object::Integer(kids.len() as i64));
        self.objects.insert(outlines_id, Object::Dictionary(d));
        Some(outlines_id)
    }
}
