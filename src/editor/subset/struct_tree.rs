//! Pruning the tagged-PDF structure tree (`/StructTreeRoot`) to the kept pages,
//! rebuilding the `/ParentTree` and per-page `/StructParents`.

use std::collections::HashMap;

use crate::object::{Object, ObjectRef};

use super::{Builder, MAX_DEPTH};

impl Builder<'_> {
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
    pub(super) fn build_struct_tree(&mut self, src: usize) -> Option<u32> {
        let cat = self.source_catalog(src)?;
        let st_ref = cat.get("StructTreeRoot")?.as_reference()?;
        let st_dict = self.sources[src]
            .load_object(st_ref)
            .ok()?
            .as_dict()?
            .clone();

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
            let Some(entries) = by_page.get(&pg) else {
                continue;
            };
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
                    root_kids
                        .iter()
                        .map(|&id| Object::Reference(ObjectRef::new(id, 0)))
                        .collect(),
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
}
