//! Content-hash deduplication: collapse byte-identical objects to one.

use std::collections::{HashMap, HashSet};

use crate::object::Object;

use super::Builder;

impl Builder<'_> {
    /// Collapse byte-identical objects to a single id, remapping references,
    /// iterating to a fixpoint (so collapsing children can collapse parents).
    ///
    /// Canonical bytes are cached per object and only recomputed for objects
    /// whose references actually changed in the previous round, so big immutable
    /// streams (images/fonts) are serialized once, not once per round. Buckets
    /// are keyed by a fast hash and confirmed with a full-bytes comparison, so a
    /// hash collision can never merge two genuinely different objects.
    pub(super) fn dedup(&mut self) {
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
        Object::Array(a) => a
            .iter_mut()
            .fold(false, |acc, o| rewrite_refs(o, map) | acc),
        Object::Dictionary(d) => d
            .values_mut()
            .fold(false, |acc, o| rewrite_refs(o, map) | acc),
        Object::Stream { dict, .. } => dict
            .values_mut()
            .fold(false, |acc, o| rewrite_refs(o, map) | acc),
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
