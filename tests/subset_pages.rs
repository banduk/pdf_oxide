//! Page subsetting: keep only the selected pages, carrying only the objects
//! they truly need (no garbage) and collapsing duplicates (no duplication).
//!
//! The fixture is built by hand so it exhibits the two things the naive
//! `extract_pages_to_bytes` path cannot handle:
//!
//!  * a **shared / inherited** `/Resources` dictionary on the `/Pages` node that
//!    lists fonts and images *not used* by every page (the classic "extract one
//!    page, drag in the whole document's fonts" bloat), and
//!  * two **byte-identical** image objects used on different pages (a dedup
//!    target), alongside a genuinely unused image (garbage).

use std::collections::{BTreeSet, HashSet};

use pdf_oxide::editor::{DocumentEditor, SignaturePolicy, SubsetOptions};
use pdf_oxide::PdfDocument;

/// Build a 3-page PDF with inherited shared resources containing garbage and
/// duplicate images.
///
/// Object map:
///  1 Catalog, 2 Pages(+inherited /Resources 9, /MediaBox), 3/5/7 Pages,
///  4/6/8 content streams, 9 shared Resources,
///  10 FUsed(Helvetica), 11 FUnused(Courier — garbage),
///  12 ImA, 13 ImB (byte-identical to 12 — dedup), 14 ImUnused (garbage).
///
///  Page1 uses FUsed+ImA, Page2 uses FUsed+ImB, Page3 uses FUsed only.
fn build_bloated_fixture() -> Vec<u8> {
    fn stream_obj(dict_inner: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {} /Length {} >>\nstream\n", dict_inner, data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }

    let content1 =
        b"BT /FUsed 12 Tf 20 100 Td (Page1) Tj ET\nq 50 0 0 50 20 20 cm /ImA Do Q".to_vec();
    let content2 =
        b"BT /FUsed 12 Tf 20 100 Td (Page2) Tj ET\nq 50 0 0 50 20 20 cm /ImB Do Q".to_vec();
    let content3 = b"BT /FUsed 12 Tf 20 100 Td (Page3) Tj ET".to_vec();

    // A 1x1 gray pixel — ImA and ImB share these exact bytes.
    let img_shared = [0xFFu8];
    // A different image — never referenced by any kept page.
    let img_unused = [0x00u8, 0x00u8];

    let img_dict = "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
                    /ColorSpace /DeviceGray /BitsPerComponent 8";
    let img_unused_dict = "/Type /XObject /Subtype /Image /Width 2 /Height 1 \
                           /ColorSpace /DeviceGray /BitsPerComponent 8";

    let objects: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 5 0 R 7 0 R] /Count 3 \
              /MediaBox [0 0 200 200] /Resources 9 0 R >>"
                .to_vec(),
        ),
        (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec()),
        (4, stream_obj("", &content1)),
        (5, b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_vec()),
        (6, stream_obj("", &content2)),
        (7, b"<< /Type /Page /Parent 2 0 R /Contents 8 0 R >>".to_vec()),
        (8, stream_obj("", &content3)),
        (
            9,
            b"<< /Font << /FUsed 10 0 R /FUnused 11 0 R >> \
              /XObject << /ImA 12 0 R /ImB 13 0 R /ImUnused 14 0 R >> >>"
                .to_vec(),
        ),
        (10, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (11, b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_vec()),
        (12, stream_obj(img_dict, &img_shared)),
        (13, stream_obj(img_dict, &img_shared)),
        (14, stream_obj(img_unused_dict, &img_unused)),
    ];

    let max_id = 14usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

/// Resolve a value that may be an indirect reference.
fn resolve(
    doc: &PdfDocument,
    obj: &pdf_oxide::object::Object,
) -> Option<pdf_oxide::object::Object> {
    match obj {
        pdf_oxide::object::Object::Reference(r) => doc.load_object(*r).ok(),
        other => Some(other.clone()),
    }
}

/// Distinct image-XObject object ids and the set of /BaseFont names that the
/// given pages reference through their (possibly inherited) /Resources.
fn analyze(bytes: &[u8], pages: &[usize]) -> (HashSet<u32>, BTreeSet<String>) {
    let doc = PdfDocument::from_bytes(bytes.to_vec()).expect("parse subset output");
    let mut image_ids = HashSet::new();
    let mut basefonts = BTreeSet::new();

    for &p in pages {
        let page = doc.get_page(p).expect("get_page");
        let page_dict = page.as_dict().expect("page dict");
        let Some(res) = page_dict.get("Resources").and_then(|r| resolve(&doc, r)) else {
            continue;
        };
        let Some(res_dict) = res.as_dict() else {
            continue;
        };

        if let Some(xo) = res_dict.get("XObject").and_then(|x| resolve(&doc, x)) {
            if let Some(xo_dict) = xo.as_dict() {
                for v in xo_dict.values() {
                    if let Some(r) = v.as_reference() {
                        if let Ok(obj) = doc.load_object(r) {
                            let is_image = obj
                                .as_dict()
                                .and_then(|d| d.get("Subtype"))
                                .and_then(|s| s.as_name())
                                == Some("Image");
                            if is_image {
                                image_ids.insert(r.id);
                            }
                        }
                    }
                }
            }
        }

        if let Some(fo) = res_dict.get("Font").and_then(|f| resolve(&doc, f)) {
            if let Some(fo_dict) = fo.as_dict() {
                for v in fo_dict.values() {
                    if let Some(obj) = resolve(&doc, v) {
                        if let Some(name) = obj
                            .as_dict()
                            .and_then(|d| d.get("BaseFont"))
                            .and_then(|b| b.as_name())
                        {
                            basefonts.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    (image_ids, basefonts)
}

#[test]
fn subset_preserves_text_drops_garbage_and_dedups() {
    let fixture = build_bloated_fixture();

    // Sanity: the fixture parses and has 3 pages.
    let probe = PdfDocument::from_bytes(fixture.clone()).expect("fixture parses");
    assert_eq!(probe.page_count().unwrap(), 3);

    // Subset to pages 1 and 2 (0-indexed 0 and 1).
    let mut editor = DocumentEditor::from_bytes(fixture.clone()).expect("open editor");
    let (subset, report) = editor
        .subset_pages_with_options(&[0, 1], SubsetOptions::default())
        .expect("subset");

    // --- Visual / semantic meaning preserved ---
    let out = PdfDocument::from_bytes(subset.clone()).expect("subset parses");
    assert_eq!(out.page_count().unwrap(), 2, "kept exactly the two pages");
    assert!(out.extract_text(0).unwrap().contains("Page1"), "page 1 text preserved");
    assert!(out.extract_text(1).unwrap().contains("Page2"), "page 2 text preserved");

    // --- No garbage, no duplication ---
    let (images, fonts) = analyze(&subset, &[0, 1]);
    assert_eq!(
        images.len(),
        1,
        "ImA and ImB are byte-identical -> one shared image object; ImUnused dropped (got {images:?})"
    );
    assert_eq!(
        fonts,
        BTreeSet::from(["Helvetica".to_string()]),
        "only the used font survives; the unused Courier is dropped (got {fonts:?})"
    );
    assert!(report.objects_deduped >= 1, "the duplicate image was deduplicated");
    assert_eq!(report.dropped_signatures, 0);

    // --- Contrast: the existing extract path keeps the inherited garbage ---
    let mut editor2 = DocumentEditor::from_bytes(fixture).expect("open editor 2");
    let naive = editor2
        .extract_pages_to_bytes(&[0, 1])
        .expect("extract_pages_to_bytes");
    let (naive_images, naive_fonts) = analyze(&naive, &[0, 1]);
    assert!(
        naive_images.len() > images.len() || naive_fonts.len() > fonts.len(),
        "the naive extract still drags in unused inherited resources \
         (images {} vs {}, fonts {:?} vs {:?})",
        naive_images.len(),
        images.len(),
        naive_fonts,
        fonts
    );
}

/// Build a 2-page PDF whose first page carries a gov.br-style *visible* digital
/// signature: an `/AcroForm` `/FT /Sig` widget whose `/AP /N` form XObject draws
/// a seal image, with `/V` pointing at a `/Type /Sig` dictionary (`/ByteRange`,
/// `/Contents`). The signature is structurally faithful (what subsetting must
/// handle); its CMS is a placeholder — cryptographic validity is irrelevant to
/// how a rebuild treats it.
///
/// Object map:
///  1 Catalog(+/AcroForm 20), 2 Pages, 3 Page1(+/Annots 21), 4 content1,
///  5 Page2, 6 content2, 10 Font, 12 seal image,
///  20 AcroForm, 21 Sig widget, 22 /Type /Sig dict, 23 /AP form XObject.
fn build_signed_fixture() -> Vec<u8> {
    fn stream_obj(dict_inner: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {} /Length {} >>\nstream\n", dict_inner, data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }

    let content1 = b"BT /F1 12 Tf 20 250 Td (Signed Page) Tj ET".to_vec();
    let content2 = b"BT /F1 12 Tf 20 250 Td (Plain Page) Tj ET".to_vec();
    // The "seal": a red pixel standing in for the gov.br stamp image.
    let seal = [0xFFu8, 0x00, 0x00];
    let ap_stream = b"q 100 0 0 60 0 0 cm /Seal Do Q".to_vec();
    // Placeholder CMS contents for the signature dict (not a real signature).
    let sig_contents = "<0000000000000000>";

    let objects: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R /AcroForm 20 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 300 300] >>".to_vec(),
        ),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R \
              /Resources << /Font << /F1 10 0 R >> >> /Annots [21 0 R] >>"
                .to_vec(),
        ),
        (4, stream_obj("", &content1)),
        (
            5,
            b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R \
              /Resources << /Font << /F1 10 0 R >> >> >>"
                .to_vec(),
        ),
        (6, stream_obj("", &content2)),
        (10, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (
            12,
            stream_obj(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8",
                &seal,
            ),
        ),
        (20, b"<< /Fields [21 0 R] /SigFlags 3 >>".to_vec()),
        (
            21,
            b"<< /Type /Annot /Subtype /Widget /FT /Sig /Rect [20 20 120 80] \
              /P 3 0 R /T (Signature1) /V 22 0 R /AP << /N 23 0 R >> >>"
                .to_vec(),
        ),
        (
            22,
            format!(
                "<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached \
                 /ByteRange [0 100 200 100] /Contents {sig_contents} /M (D:20240101000000Z) >>"
            )
            .into_bytes(),
        ),
        (
            23,
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 60] \
                 /Resources << /XObject << /Seal 12 0 R >> >>",
                &ap_stream,
            ),
        ),
    ];

    let ids: Vec<u32> = objects.iter().map(|(id, _)| *id).collect();
    let max_id = *ids.iter().max().unwrap() as usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        // Free entry for object numbers we never allocated (7,8,9,11,13..19).
        if ids.contains(&(id as u32)) {
            out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \r\n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

/// Does page 0's signature widget still carry its seal image through `/AP`?
fn seal_image_count(doc: &PdfDocument) -> usize {
    let mut count = 0;
    let page = doc.get_page(0).expect("get_page 0");
    let Some(annots) = page
        .as_dict()
        .and_then(|d| d.get("Annots"))
        .and_then(|a| resolve(doc, a))
    else {
        return 0;
    };
    let Some(arr) = annots.as_array() else {
        return 0;
    };
    for a in arr {
        let Some(annot) = resolve(doc, a) else {
            continue;
        };
        let Some(ap) = annot
            .as_dict()
            .and_then(|d| d.get("AP"))
            .and_then(|x| resolve(doc, x))
        else {
            continue;
        };
        let Some(n) = ap
            .as_dict()
            .and_then(|d| d.get("N"))
            .and_then(|x| resolve(doc, x))
        else {
            continue;
        };
        // Walk the appearance form's resources for image XObjects.
        let Some(res) = n
            .as_dict()
            .and_then(|d| d.get("Resources"))
            .and_then(|r| resolve(doc, r))
        else {
            continue;
        };
        if let Some(xo) = res
            .as_dict()
            .and_then(|d| d.get("XObject"))
            .and_then(|x| resolve(doc, x))
        {
            if let Some(xo_dict) = xo.as_dict() {
                for v in xo_dict.values() {
                    if let Some(obj) = resolve(doc, v) {
                        let is_image = obj
                            .as_dict()
                            .and_then(|d| d.get("Subtype"))
                            .and_then(|s| s.as_name())
                            == Some("Image");
                        if is_image {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    count
}

#[test]
fn subset_preserves_signature_seal_but_drops_invalid_signature() {
    let fixture = build_signed_fixture();

    // Sanity: the fixture itself contains a /ByteRange signature dict.
    assert!(
        fixture
            .windows(b"/ByteRange".len())
            .any(|w| w == b"/ByteRange"),
        "fixture should contain a signature"
    );

    // Subset the parsed fixture directly (no editor round-trip) to isolate.
    let doc = PdfDocument::from_bytes(fixture).expect("parse fixture");
    let (subset, report) =
        pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], SubsetOptions::default())
            .expect("subset signed page");

    let out = PdfDocument::from_bytes(subset.clone()).expect("subset parses");

    // Text + the bound seal image survive.
    assert!(out.extract_text(0).unwrap().contains("Signed Page"), "page text preserved");
    assert_eq!(seal_image_count(&out), 1, "the seal image is preserved (and not duplicated)");

    // Nothing claims to be a valid signature any more.
    assert!(
        !subset
            .windows(b"/ByteRange".len())
            .any(|w| w == b"/ByteRange"),
        "no /ByteRange signature dict survives a rebuild"
    );
    assert!(
        !subset.windows(b"/Sig".len()).any(|w| w == b"/Sig"),
        "no leftover signature dictionary"
    );
    assert_eq!(report.dropped_signatures, 1, "one signature was dropped");
    assert!(!report.warnings.is_empty(), "a warning was recorded");
}

#[test]
fn subset_refuses_signed_page_when_policy_is_refuse() {
    let fixture = build_signed_fixture();
    let mut editor = DocumentEditor::from_bytes(fixture).expect("open editor");
    let opts = SubsetOptions {
        on_signature: SignaturePolicy::Refuse,
        ..SubsetOptions::default()
    };
    let result = editor.subset_pages_with_options(&[0], opts);
    assert!(result.is_err(), "Refuse policy must reject subsetting a signed page");

    // The unsigned page (1) is fine under Refuse.
    let mut editor2 = DocumentEditor::from_bytes(build_signed_fixture()).expect("open editor 2");
    let opts2 = SubsetOptions {
        on_signature: SignaturePolicy::Refuse,
        ..SubsetOptions::default()
    };
    let ok = editor2.subset_pages_with_options(&[1], opts2);
    assert!(ok.is_ok(), "an unsigned page can still be subset under Refuse");
}

/// Build a 3-page PDF with an outline (bookmarks) and link annotations that
/// point at both kept and dropped pages, to exercise destination remapping +
/// pruning.
///
///  Outline: "Chapter 1"->page0, "Chapter 2"->page2(dropped),
///           "Chapter 3"->page1 with child "Section 3.1"->page2(dropped).
///  Links:   page0 -> page1 (kept), page1 -> page2 (dropped).
fn build_outline_fixture() -> Vec<u8> {
    fn stream_obj(data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let objects: Vec<(u32, Vec<u8>)> =
        vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R /Outlines 30 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 5 0 R 7 0 R] /Count 3 /MediaBox [0 0 200 200] \
              /Resources << /Font << /F1 10 0 R >> >> >>"
                .to_vec(),
        ),
        (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [20 0 R] >>".to_vec()),
        (4, stream_obj(b"BT /F1 12 Tf 20 100 Td (P0) Tj ET")),
        (5, b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Annots [21 0 R] >>".to_vec()),
        (6, stream_obj(b"BT /F1 12 Tf 20 100 Td (P1) Tj ET")),
        (7, b"<< /Type /Page /Parent 2 0 R /Contents 8 0 R >>".to_vec()),
        (8, stream_obj(b"BT /F1 12 Tf 20 100 Td (P2) Tj ET")),
        (10, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (20, b"<< /Type /Annot /Subtype /Link /Rect [10 10 50 50] /Dest [5 0 R /Fit] >>".to_vec()),
        (21, b"<< /Type /Annot /Subtype /Link /Rect [10 10 50 50] /Dest [7 0 R /Fit] >>".to_vec()),
        (30, b"<< /Type /Outlines /First 31 0 R /Last 33 0 R /Count 3 >>".to_vec()),
        (
            31,
            b"<< /Title (Chapter 1) /Parent 30 0 R /Dest [3 0 R /Fit] /Next 32 0 R >>".to_vec(),
        ),
        (
            32,
            b"<< /Title (Chapter 2) /Parent 30 0 R /Prev 31 0 R /Next 33 0 R /Dest [7 0 R /Fit] >>"
                .to_vec(),
        ),
        (
            33,
            b"<< /Title (Chapter 3) /Parent 30 0 R /Prev 32 0 R /Dest [5 0 R /Fit] \
              /First 34 0 R /Last 34 0 R /Count 1 >>"
                .to_vec(),
        ),
        (34, b"<< /Title (Section 3.1) /Parent 33 0 R /Dest [7 0 R /Fit] >>".to_vec()),
    ];
    let ids: Vec<u32> = objects.iter().map(|(id, _)| *id).collect();
    let max_id = *ids.iter().max().unwrap() as usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        if ids.contains(&(id as u32)) {
            out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \r\n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

/// Outline titles in /First.. /Next order, and the /Type of each /Dest target.
fn outline_titles_and_targets(doc: &PdfDocument) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(catalog) = doc.catalog() else {
        return out;
    };
    let Some(outlines) = catalog
        .as_dict()
        .and_then(|d| d.get("Outlines"))
        .and_then(|o| resolve(doc, o))
    else {
        return out;
    };
    let mut cur = outlines
        .as_dict()
        .and_then(|d| d.get("First"))
        .and_then(|f| f.as_reference());
    while let Some(item_ref) = cur {
        let Ok(item) = doc.load_object(item_ref) else {
            break;
        };
        let Some(d) = item.as_dict() else { break };
        let title = d
            .get("Title")
            .and_then(|t| t.as_string())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let target = d
            .get("Dest")
            .and_then(|de| de.as_array())
            .and_then(|a| a.first())
            .and_then(|f| f.as_reference())
            .and_then(|r| doc.load_object(r).ok())
            .and_then(|o| {
                o.as_dict()
                    .and_then(|dd| dd.get("Type"))
                    .and_then(|t| t.as_name())
                    .map(String::from)
            })
            .unwrap_or_default();
        out.push((title, target));
        cur = d.get("Next").and_then(|n| n.as_reference());
    }
    out
}

#[test]
fn subset_remaps_links_and_prunes_outlines() {
    let fixture = build_outline_fixture();
    let doc = PdfDocument::from_bytes(fixture).expect("parse fixture");

    // Keep pages 0 and 1; drop page 2.
    let (subset, report) =
        pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0), (0, 1)], SubsetOptions::default())
            .expect("subset");
    let out = PdfDocument::from_bytes(subset).expect("subset parses");

    // --- Outlines pruned to entries that still resolve to a kept page ---
    let titles = outline_titles_and_targets(&out);
    let kept: Vec<String> = titles.iter().map(|(t, _)| t.clone()).collect();
    assert_eq!(
        kept,
        vec!["Chapter 1".to_string(), "Chapter 3".to_string()],
        "Chapter 2 (-> dropped page) is pruned; Chapter 1 and 3 survive (got {kept:?})"
    );
    for (title, target) in &titles {
        assert_eq!(target, "Page", "outline '{title}' must point at a real kept page");
    }
    assert_eq!(report.outline_entries, 2);

    // --- Links: kept-page target preserved, dropped-page target severed ---
    let p0 = out.get_page(0).unwrap();
    let p0_link_has_dest = p0
        .as_dict()
        .and_then(|d| d.get("Annots"))
        .and_then(|a| resolve(&out, a))
        .and_then(|a| a.as_array().and_then(|arr| arr.first().cloned()))
        .and_then(|e| resolve(&out, &e))
        .map(|annot| {
            annot
                .as_dict()
                .map(|d| d.contains_key("Dest"))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    assert!(p0_link_has_dest, "page 0's link to a kept page keeps its /Dest");

    let p1 = out.get_page(1).unwrap();
    let p1_link_has_dest = p1
        .as_dict()
        .and_then(|d| d.get("Annots"))
        .and_then(|a| resolve(&out, a))
        .and_then(|a| a.as_array().and_then(|arr| arr.first().cloned()))
        .and_then(|e| resolve(&out, &e))
        .map(|annot| {
            annot
                .as_dict()
                .map(|d| d.contains_key("Dest"))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    assert!(!p1_link_has_dest, "page 1's link to a dropped page is severed");
    assert!(report.links_severed >= 1, "at least one link was severed");
}

/// Build a 1-page PDF whose page uses a Form XObject; the form's OWN
/// /Resources list a used image and an unused image.
fn build_form_fixture() -> Vec<u8> {
    fn stream_obj(dict_inner: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {} /Length {} >>\nstream\n", dict_inner, data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let img = "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
               /ColorSpace /DeviceGray /BitsPerComponent 8";
    let img2 = "/Type /XObject /Subtype /Image /Width 2 /Height 1 \
                /ColorSpace /DeviceGray /BitsPerComponent 8";
    let objects: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 200] \
              /Resources << /XObject << /Fm 20 0 R >> >> >>"
                .to_vec(),
        ),
        (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec()),
        (4, stream_obj("", b"q 100 0 0 100 0 0 cm /Fm Do Q")),
        (
            20,
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                 /Resources << /XObject << /ImUsed 21 0 R /ImUnused 22 0 R >> >>",
                b"q 50 0 0 50 0 0 cm /ImUsed Do Q",
            ),
        ),
        (21, stream_obj(img, &[0xFFu8])),
        (22, stream_obj(img2, &[0x00u8, 0x00u8])),
    ];
    let ids: Vec<u32> = objects.iter().map(|(id, _)| *id).collect();
    let max_id = *ids.iter().max().unwrap() as usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        if ids.contains(&(id as u32)) {
            out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \r\n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

/// Count image XObjects inside the page's form `/Fm`'s own /Resources.
fn form_internal_image_count(doc: &PdfDocument) -> usize {
    let page = doc.get_page(0).expect("page");
    let res = page
        .as_dict()
        .and_then(|d| d.get("Resources"))
        .and_then(|r| resolve(doc, r))
        .unwrap();
    let xo = res
        .as_dict()
        .and_then(|d| d.get("XObject"))
        .and_then(|x| resolve(doc, x))
        .unwrap();
    let fm = xo
        .as_dict()
        .and_then(|d| d.get("Fm"))
        .and_then(|f| resolve(doc, f))
        .unwrap();
    let fres = fm
        .as_dict()
        .and_then(|d| d.get("Resources"))
        .and_then(|r| resolve(doc, r))
        .unwrap();
    let fxo = fres
        .as_dict()
        .and_then(|d| d.get("XObject"))
        .and_then(|x| resolve(doc, x));
    let Some(fxo) = fxo else { return 0 };
    fxo.as_dict()
        .map(|d| {
            d.values()
                .filter(|v| {
                    resolve(doc, v).and_then(|o| {
                        o.as_dict()
                            .and_then(|dd| dd.get("Subtype"))
                            .and_then(|s| s.as_name())
                            .map(String::from)
                    }) == Some("Image".to_string())
                })
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn subset_trims_form_internal_resources() {
    let fixture = build_form_fixture();
    let doc = PdfDocument::from_bytes(fixture).expect("parse");

    // trim_forms ON (default): the form's unused image is dropped.
    let (trimmed, _) =
        pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], SubsetOptions::default()).unwrap();
    let out = PdfDocument::from_bytes(trimmed).expect("parse trimmed");
    assert_eq!(form_internal_image_count(&out), 1, "form keeps only its used image");

    // trim_forms OFF: the form is copied wholesale, unused image retained.
    let opts = SubsetOptions {
        trim_forms: false,
        ..SubsetOptions::default()
    };
    let (whole, _) = pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], opts).unwrap();
    let out2 = PdfDocument::from_bytes(whole).expect("parse whole");
    assert_eq!(form_internal_image_count(&out2), 2, "wholesale form keeps both images");
}

/// Build a 2-page tagged PDF: a /Document structure element with one /P
/// (paragraph) per page, marked content (MCID 0) on each page, a ParentTree,
/// and per-page /StructParents.
fn build_tagged_fixture() -> Vec<u8> {
    fn stream_obj(data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let c0 = b"/P <</MCID 0>> BDC BT /F1 12 Tf 20 100 Td (Page0) Tj ET EMC".to_vec();
    let c1 = b"/P <</MCID 0>> BDC BT /F1 12 Tf 20 100 Td (Page1) Tj ET EMC".to_vec();
    let objects: Vec<(u32, Vec<u8>)> = vec![
        (
            1,
            b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 30 0 R /MarkInfo << /Marked true >> >>"
                .to_vec(),
        ),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 200 200] \
              /Resources << /Font << /F1 10 0 R >> >> >>"
                .to_vec(),
        ),
        (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /StructParents 0 >>".to_vec()),
        (4, stream_obj(&c0)),
        (5, b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R /StructParents 1 >>".to_vec()),
        (6, stream_obj(&c1)),
        (10, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (
            30,
            b"<< /Type /StructTreeRoot /K 31 0 R /ParentTree 40 0 R /ParentTreeNextKey 2 \
              /RoleMap << >> >>"
                .to_vec(),
        ),
        (31, b"<< /Type /StructElem /S /Document /P 30 0 R /K [32 0 R 33 0 R] >>".to_vec()),
        (32, b"<< /Type /StructElem /S /P /P 31 0 R /Pg 3 0 R /K 0 >>".to_vec()),
        (33, b"<< /Type /StructElem /S /P /P 31 0 R /Pg 5 0 R /K 0 >>".to_vec()),
        (40, b"<< /Nums [0 [32 0 R] 1 [33 0 R]] >>".to_vec()),
    ];
    let ids: Vec<u32> = objects.iter().map(|(id, _)| *id).collect();
    let max_id = *ids.iter().max().unwrap() as usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        if ids.contains(&(id as u32)) {
            out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \r\n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

#[test]
fn subset_prunes_structure_tree_to_kept_pages() {
    let fixture = build_tagged_fixture();
    let doc = PdfDocument::from_bytes(fixture).expect("parse");

    // Keep page 0 only; the /P element for page 1 must be pruned.
    let (subset, report) =
        pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], SubsetOptions::default())
            .expect("subset");
    let out = PdfDocument::from_bytes(subset).expect("subset parses");

    // Document + one surviving /P.
    assert_eq!(
        report.struct_elements, 2,
        "Document + the kept page's P survive (P for page 1 pruned)"
    );

    let cat = out.catalog().unwrap();
    let cat_d = cat.as_dict().unwrap();
    assert!(cat_d.contains_key("MarkInfo"), "MarkInfo present");
    let st = cat_d
        .get("StructTreeRoot")
        .and_then(|s| resolve(&out, s))
        .expect("StructTreeRoot");
    let st_d = st.as_dict().unwrap();
    assert_eq!(st_d.get("Type").and_then(|t| t.as_name()), Some("StructTreeRoot"));

    // Root /K -> Document element.
    let docel = st_d
        .get("K")
        .and_then(|k| resolve(&out, k))
        .expect("doc elem");
    let docel_d = docel.as_dict().unwrap();
    assert_eq!(docel_d.get("S").and_then(|s| s.as_name()), Some("Document"));

    // Document has exactly one surviving child P, pointing at a real kept page.
    let children: Vec<pdf_oxide::object::Object> = match docel_d.get("K") {
        Some(pdf_oxide::object::Object::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
        None => vec![],
    };
    assert_eq!(children.len(), 1, "only the kept page's paragraph remains");
    let p = resolve(&out, &children[0]).unwrap();
    let p_d = p.as_dict().unwrap();
    assert_eq!(p_d.get("S").and_then(|s| s.as_name()), Some("P"));
    let pg = p_d.get("Pg").and_then(|g| resolve(&out, g)).unwrap();
    assert_eq!(
        pg.as_dict()
            .and_then(|d| d.get("Type"))
            .and_then(|t| t.as_name()),
        Some("Page")
    );

    // ParentTree maps the kept page's StructParents key to the P element.
    let pt = st_d
        .get("ParentTree")
        .and_then(|p| resolve(&out, p))
        .expect("ParentTree");
    let nums = pt
        .as_dict()
        .and_then(|d| d.get("Nums"))
        .and_then(|n| n.as_array())
        .expect("Nums");
    assert!(nums.len() >= 2, "at least one (key, array) pair");
    let arr = resolve(&out, &nums[1]).unwrap();
    let first = arr
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| resolve(&out, e))
        .unwrap();
    assert_eq!(
        first
            .as_dict()
            .and_then(|d| d.get("S"))
            .and_then(|s| s.as_name()),
        Some("P")
    );

    // Text still extracts.
    assert!(out.extract_text(0).unwrap().contains("Page0"));
}

/// Build a 2-page PDF with an interactive text field on each page, an optional
/// content layer used by page 0, and catalog metadata (/Lang, /Metadata,
/// /ViewerPreferences). Page 0's field is "name1"; page 1's is "name2".
fn build_form_doc_fixture() -> Vec<u8> {
    fn stream_obj(dict_inner: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {} /Length {} >>\nstream\n", dict_inner, data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let xmp = b"<?xpacket?><x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta><?xpacket end='r'?>";
    let objects: Vec<(u32, Vec<u8>)> = vec![
        (
            1,
            b"<< /Type /Catalog /Pages 2 0 R /AcroForm 20 0 R /OCProperties 30 0 R \
              /Lang (en-US) /Metadata 35 0 R /ViewerPreferences << /HideToolbar true >> >>"
                .to_vec(),
        ),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 300 300] >>".to_vec(),
        ),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R \
              /Resources << /Font << /F1 10 0 R >> /Properties << /MC0 31 0 R >> >> \
              /Annots [21 0 R] >>"
                .to_vec(),
        ),
        (4, stream_obj("", b"/OC /MC0 BDC BT /F1 12 Tf 20 250 Td (Page0) Tj ET EMC")),
        (
            5,
            b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R \
              /Resources << /Font << /F1 10 0 R >> >> /Annots [22 0 R] >>"
                .to_vec(),
        ),
        (6, stream_obj("", b"BT /F1 12 Tf 20 250 Td (Page1) Tj ET")),
        (10, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (
            20,
            b"<< /Fields [21 0 R 22 0 R] /DA (/Helv 0 Tf 0 g) \
              /DR << /Font << /Helv 10 0 R >> >> /NeedAppearances true >>"
                .to_vec(),
        ),
        (
            21,
            b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (name1) /V (Alice) \
              /Rect [20 20 120 50] /P 3 0 R /AP << /N 23 0 R >> >>"
                .to_vec(),
        ),
        (
            22,
            b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (name2) /V (Bob) \
              /Rect [20 20 120 50] /P 5 0 R /AP << /N 24 0 R >> >>"
                .to_vec(),
        ),
        (23, stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 100 30]", b"q Q")),
        (24, stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 100 30]", b"q Q")),
        (30, b"<< /OCGs [31 0 R] /D << /Order [31 0 R] /ON [31 0 R] >> >>".to_vec()),
        (31, b"<< /Type /OCG /Name (Layer1) >>".to_vec()),
        (35, stream_obj("/Type /Metadata /Subtype /XML", xmp)),
    ];
    let ids: Vec<u32> = objects.iter().map(|(id, _)| *id).collect();
    let max_id = *ids.iter().max().unwrap() as usize;
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; max_id + 1];
    for (id, body) in &objects {
        offsets[*id as usize] = out.len();
        out.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_start = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", max_id + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \r\n");
    for id in 1..=max_id {
        if ids.contains(&(id as u32)) {
            out.extend_from_slice(format!("{:010} 00000 n \r\n", offsets[id]).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \r\n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            max_id + 1,
            xref_start
        )
        .as_bytes(),
    );
    out
}

fn catalog_of(doc: &PdfDocument) -> std::collections::HashMap<String, pdf_oxide::object::Object> {
    doc.catalog().unwrap().as_dict().unwrap().clone()
}

#[test]
fn subset_preserves_acroform_layers_and_metadata() {
    let fixture = build_form_doc_fixture();
    let doc = PdfDocument::from_bytes(fixture).expect("parse");

    // Keep page 0 (field "name1" + the layer); drop page 1 (field "name2").
    let (subset, report) =
        pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], SubsetOptions::default())
            .expect("subset");
    let out = PdfDocument::from_bytes(subset).expect("subset parses");
    let cat = catalog_of(&out);

    // --- AcroForm: only the kept page's field survives, still interactive ---
    let acro = cat
        .get("AcroForm")
        .and_then(|a| resolve(&out, a))
        .expect("AcroForm kept");
    let fields = acro
        .as_dict()
        .and_then(|d| d.get("Fields"))
        .and_then(|f| f.as_array())
        .unwrap();
    assert_eq!(fields.len(), 1, "only the kept page's field survives (got {})", fields.len());
    assert_eq!(report.form_fields, 1);
    let field = resolve(&out, &fields[0]).unwrap();
    let fd = field.as_dict().unwrap();
    assert_eq!(fd.get("FT").and_then(|v| v.as_name()), Some("Tx"), "field type preserved");
    assert_eq!(
        fd.get("T").and_then(|v| v.as_string()),
        Some(&b"name1"[..]),
        "the kept field is name1 (not the dropped name2)"
    );
    assert_eq!(
        fd.get("V").and_then(|v| v.as_string()),
        Some(&b"Alice"[..]),
        "field value preserved (interactive form intact)"
    );

    // --- Optional content layer carried ---
    let oc = cat
        .get("OCProperties")
        .and_then(|o| resolve(&out, o))
        .expect("OCProperties kept");
    assert!(oc.as_dict().and_then(|d| d.get("OCGs")).is_some(), "OCGs carried");

    // --- Catalog metadata carried ---
    assert_eq!(
        cat.get("Lang")
            .and_then(|v| resolve(&out, v))
            .and_then(|v| v.as_string().map(<[u8]>::to_vec)),
        Some(b"en-US".to_vec()),
        "/Lang carried"
    );
    assert!(
        cat.get("Metadata").and_then(|m| resolve(&out, m)).is_some(),
        "/Metadata carried"
    );
    assert!(cat.contains_key("ViewerPreferences"), "/ViewerPreferences carried");
}

#[test]
fn subset_keep_acroform_false_drops_the_form() {
    let fixture = build_form_doc_fixture();
    let doc = PdfDocument::from_bytes(fixture).expect("parse");
    let opts = SubsetOptions {
        keep_acroform: false,
        ..SubsetOptions::default()
    };
    let (subset, _) = pdf_oxide::editor::subset_to_bytes(&[&doc], &[(0, 0)], opts).expect("subset");
    let out = PdfDocument::from_bytes(subset).expect("parses");
    assert!(
        !catalog_of(&out).contains_key("AcroForm"),
        "no AcroForm when keep_acroform=false"
    );
    // The widget's appearance is still on the page (visual preserved).
    let page = out.get_page(0).unwrap();
    assert!(
        page.as_dict().and_then(|d| d.get("Annots")).is_some(),
        "widget annotation still present visually"
    );
}
