//! Per-page `/Resources` trimming, including recursion into Form XObjects.

use std::collections::HashSet;

use crate::content::{parse_content_stream, Operator};
use crate::object::{Object, ObjectRef};

use super::{Builder, ResourceTrim, MAX_DEPTH};

/// Resource names a content stream actually references, per category.
#[derive(Debug, Default)]
pub(super) struct UsedNames {
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

/// Scan a decoded content stream and record the resource names it references.
pub(super) fn scan_used(content: &[u8], used: &mut UsedNames) {
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

impl Builder<'_> {
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
    /// `opts.resources` is `Forms`; everything else is imported wholesale.
    pub(super) fn build_trimmed_resources(
        &mut self,
        src: usize,
        res: &Object,
        used: &UsedNames,
        depth: usize,
    ) -> Object {
        let mut out = std::collections::HashMap::new();
        let res_dict = match res {
            Object::Dictionary(d) => d.clone(),
            Object::Stream { dict, .. } => dict.clone(),
            _ => return Object::Dictionary(out),
        };
        for category in RESOURCE_CATEGORIES {
            let Some(keep) = Self::keep_set(used, category) else {
                continue;
            };
            if keep.is_empty() {
                continue;
            }
            let keep = keep.clone();
            let Some(sub) = res_dict.get(category).and_then(|c| self.resolve(src, c)) else {
                continue;
            };
            let Some(sub_dict) = sub.as_dict().cloned() else {
                continue;
            };
            let mut new_sub = std::collections::HashMap::new();
            for (name, value) in sub_dict.iter() {
                if !keep.contains(name) {
                    continue;
                }
                // Trim used Form XObjects; import everything else wholesale.
                let trim_form = category == "XObject"
                    && self.opts.resources == ResourceTrim::Forms
                    && depth < MAX_DEPTH;
                let form_ref = if trim_form {
                    self.form_ref_if_form(src, value)
                } else {
                    None
                };
                let imported = match form_ref {
                    Some(fr) => {
                        let fid = self.import_form_trimmed(src, fr, depth + 1);
                        Object::Reference(ObjectRef::new(fid, 0))
                    },
                    None => self.remap(src, value, depth + 1),
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

        let form_obj = self.sources[src]
            .load_object(form_ref)
            .unwrap_or(Object::Null);
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

        let mut nd = std::collections::HashMap::new();
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
        self.objects
            .insert(new_id, Object::Stream { dict: nd, data });
        new_id
    }
}
