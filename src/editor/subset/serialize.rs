//! Writing the final, compacted object graph out as a complete PDF.

use std::collections::HashMap;

use crate::object::Object;
use crate::writer::ObjectSerializer;

/// Rewrite references for the final id compaction (every ref must map).
pub(super) fn renumber_refs(obj: &mut Object, map: &HashMap<u32, u32>) {
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
pub(super) fn serialize_pdf(
    objects: &HashMap<u32, Object>,
    root: u32,
    info: Option<u32>,
) -> Vec<u8> {
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
