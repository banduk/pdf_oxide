//! Real-CMS variant of the signature-subset test (gov.br-style visible
//! signature). Signs a fixture for real with the project's signer, asserts the
//! original signature cryptographically verifies, then asserts that subsetting
//! preserves the seal image but drops the (now-invalid) signature.
//!
//! Run with `--features signatures`.
#![cfg(feature = "signatures")]

use pdf_oxide::editor::{subset_to_bytes, SubsetOptions};
use pdf_oxide::object::{Object, ObjectRef};
use pdf_oxide::signatures::{
    sign_pdf_bytes, verify_signer_detached, ByteRangeCalculator, SignOptions, SignerVerify,
    SigningCredentials,
};
use pdf_oxide::PdfDocument;

fn resolve(doc: &PdfDocument, obj: &Object) -> Option<Object> {
    match obj {
        Object::Reference(r) => doc.load_object(*r).ok(),
        other => Some(other.clone()),
    }
}

/// Total length of the leading DER SEQUENCE (the CMS ContentInfo), used to trim
/// the zero-padding `/Contents` carries from its fixed-width placeholder.
fn der_total_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 || buf[0] != 0x30 {
        return None;
    }
    let b1 = buf[1];
    if b1 & 0x80 == 0 {
        Some(2 + b1 as usize)
    } else {
        let n = (b1 & 0x7f) as usize;
        if buf.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for &byte in &buf[2..2 + n] {
            len = (len << 8) | byte as usize;
        }
        Some(2 + n + len)
    }
}

/// Base PDF (object ids 1..=9, /Size 10) with a visible /FT /Sig widget whose
/// /AP draws a seal image, an /AcroForm, and `/V` pre-pointed at object 10 —
/// the id `sign_pdf_bytes` will give the appended signature dictionary.
fn build_base() -> Vec<u8> {
    fn stream_obj(dict_inner: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {} /Length {} >>\nstream\n", dict_inner, data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let seal = [0xFFu8, 0x00, 0x00];
    let objects: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R /AcroForm 7 0 R >>".to_vec()),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 300] \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
        ),
        (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [8 0 R] >>".to_vec()),
        (4, stream_obj("", b"BT /F1 12 Tf 20 250 Td (Signed Page) Tj ET")),
        (5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()),
        (
            6,
            stream_obj(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8",
                &seal,
            ),
        ),
        (7, b"<< /Fields [8 0 R] /SigFlags 3 >>".to_vec()),
        (
            8,
            b"<< /Type /Annot /Subtype /Widget /FT /Sig /Rect [20 20 120 80] /P 3 0 R \
              /T (Signature1) /V 10 0 R /AP << /N 9 0 R >> >>"
                .to_vec(),
        ),
        (
            9,
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 60] \
                 /Resources << /XObject << /Seal 6 0 R >> >>",
                b"q 100 0 0 60 0 0 cm /Seal Do Q",
            ),
        ),
    ];
    let max_id = 9usize;
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
    // /Size 10 -> sign_pdf_bytes appends the signature dict as object 10.
    out.extend_from_slice(
        format!("trailer\n<< /Size 10 /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF\n").as_bytes(),
    );
    out
}

fn seal_present(doc: &PdfDocument) -> bool {
    let page = doc.get_page(0).unwrap();
    let Some(annots) = page
        .as_dict()
        .and_then(|d| d.get("Annots"))
        .and_then(|a| resolve(doc, a))
    else {
        return false;
    };
    for a in annots.as_array().cloned().unwrap_or_default() {
        let Some(annot) = resolve(doc, &a) else {
            continue;
        };
        let Some(n) = annot
            .as_dict()
            .and_then(|d| d.get("AP"))
            .and_then(|ap| resolve(doc, ap))
            .and_then(|ap| {
                ap.as_dict()
                    .and_then(|d| d.get("N"))
                    .and_then(|n| resolve(doc, n))
            })
        else {
            continue;
        };
        if let Some(xo) = n
            .as_dict()
            .and_then(|d| d.get("Resources"))
            .and_then(|r| resolve(doc, r))
            .and_then(|r| {
                r.as_dict()
                    .and_then(|d| d.get("XObject"))
                    .and_then(|x| resolve(doc, x))
            })
        {
            if let Some(xd) = xo.as_dict() {
                if xd.values().any(|v| {
                    resolve(doc, v)
                        .and_then(|o| {
                            o.as_dict()
                                .and_then(|d| d.get("Subtype"))
                                .and_then(|s| s.as_name())
                                .map(String::from)
                        })
                        .as_deref()
                        == Some("Image")
                }) {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn subset_drops_a_cryptographically_valid_signature_but_keeps_the_seal() {
    let cert = std::fs::read_to_string("tests/fixtures/test_signing_cert.pem")
        .expect("cert fixture must exist");
    let key = std::fs::read_to_string("tests/fixtures/test_signing_key.pem")
        .expect("key fixture must exist");
    let creds = SigningCredentials::from_pem(&cert, &key).expect("load credentials");

    let signed = sign_pdf_bytes(&build_base(), &creds, SignOptions::default()).expect("sign");
    assert!(
        signed
            .windows(b"/ByteRange".len())
            .any(|w| w == b"/ByteRange"),
        "signed PDF carries a signature"
    );

    // --- The original signature really verifies (trust-free CMS check) ---
    let signed_doc = PdfDocument::from_bytes(signed.clone()).expect("parse signed");
    // Reach the signature dict through the visible field's /V.
    let widget = signed_doc.load_object(ObjectRef::new(8, 0)).unwrap();
    let sig_ref = widget
        .as_dict()
        .and_then(|d| d.get("V"))
        .and_then(|v| v.as_reference())
        .unwrap();
    let sig = signed_doc.load_object(sig_ref).unwrap();
    let sig_d = sig.as_dict().unwrap();
    let br: Vec<i64> = sig_d
        .get("ByteRange")
        .and_then(|b| b.as_array())
        .unwrap()
        .iter()
        .filter_map(|x| x.as_integer())
        .collect();
    let byte_range: [i64; 4] = [br[0], br[1], br[2], br[3]];
    let signed_content = ByteRangeCalculator::extract_signed_bytes(&signed, &byte_range).unwrap();
    let contents_padded = sig_d.get("Contents").and_then(|c| c.as_string()).unwrap();
    let der_len = der_total_len(contents_padded).expect("CMS DER header");
    let contents = &contents_padded[..der_len]; // strip placeholder zero-padding
    assert_eq!(
        verify_signer_detached(contents, &signed_content).unwrap(),
        SignerVerify::Valid,
        "the original signature must cryptographically verify before we drop it"
    );

    // --- Subset: seal kept, signature dropped, nothing claims to be signed ---
    let (subset, report) =
        subset_to_bytes(&[&signed_doc], &[(0, 0)], SubsetOptions::default()).expect("subset");
    let out = PdfDocument::from_bytes(subset.clone()).expect("parse subset");

    assert!(out.extract_text(0).unwrap().contains("Signed Page"));
    assert!(seal_present(&out), "the bound seal image is preserved");
    assert!(
        !subset
            .windows(b"/ByteRange".len())
            .any(|w| w == b"/ByteRange"),
        "no /ByteRange survives the rebuild"
    );
    assert_eq!(report.dropped_signatures, 1);
}
