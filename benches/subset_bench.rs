//! Benchmarks for page subsetting / rebuild (`pdf_oxide::editor::subset`).
//!
//! Builds a synthetic document that stresses the two costly parts of the
//! pipeline — the object-graph deep copy and the content-hash deduplication —
//! by giving it many large, partly-duplicated image streams:
//!
//!  * a pool of `IMG_COUNT` image XObjects, where the second half are
//!    byte-identical copies of the first half (dedup targets), each `IMG_KB`
//!    of stream data;
//!  * `PAGES` pages, each referencing a rotating handful of pool images.
//!
//! We then subset the first half of the pages.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use pdf_oxide::editor::{subset_to_bytes, SubsetOptions};
use pdf_oxide::PdfDocument;
use std::hint::black_box;

const PAGES: usize = 120;
const IMG_COUNT: usize = 80; // 40 unique + 40 duplicates
const IMG_KB: usize = 16;
const IMGS_PER_PAGE: usize = 4;

fn build_doc() -> Vec<u8> {
    // Object id layout:
    //   1 Catalog, 2 Pages, 3 Font,
    //   image pool: ids 10 .. 10+IMG_COUNT,
    //   pages + contents: after the pool, two ids each.
    let img_base = 10u32;
    let page_base = img_base + IMG_COUNT as u32;

    let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
    objects.push((3, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()));

    // Image pool: second half duplicates the first half byte-for-byte.
    let unique = IMG_COUNT / 2;
    for i in 0..IMG_COUNT {
        let seed = (i % unique) as u8;
        let data = vec![seed; IMG_KB * 1024];
        let dict = format!(
            "<< /Type /XObject /Subtype /Image /Width 64 /Height {} \
             /ColorSpace /DeviceGray /BitsPerComponent 8 /Length {} >>\nstream\n",
            (IMG_KB * 1024) / 64,
            data.len()
        );
        let mut body = dict.into_bytes();
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\nendstream");
        objects.push((img_base + i as u32, body));
    }

    // Pages, each with its own /Resources referencing IMGS_PER_PAGE pool images.
    let mut kids = String::new();
    for p in 0..PAGES {
        let pid = page_base + (p as u32) * 2;
        let cid = pid + 1;
        kids.push_str(&format!("{pid} 0 R "));

        let mut xobj = String::new();
        let mut content = String::from("BT /F1 12 Tf 20 700 Td (Page) Tj ET\n");
        for k in 0..IMGS_PER_PAGE {
            let img_idx = (p * IMGS_PER_PAGE + k) % IMG_COUNT;
            let name = format!("Im{k}");
            xobj.push_str(&format!("/{name} {} 0 R ", img_base + img_idx as u32));
            content.push_str(&format!("q 64 0 0 64 {} 100 cm /{name} Do Q\n", 20 + k * 70));
        }
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {cid} 0 R \
             /Resources << /Font << /F1 3 0 R >> /XObject << {xobj}>> >> >>"
        );
        objects.push((pid, page.into_bytes()));

        let mut cbody =
            format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        cbody.extend_from_slice(content.as_bytes());
        cbody.extend_from_slice(b"\nendstream");
        objects.push((cid, cbody));
    }

    objects.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
    objects.push((
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {PAGES} >>").into_bytes(),
    ));

    // Serialize with an xref (object ids are sparse-but-bounded).
    let max_id = objects.iter().map(|(id, _)| *id).max().unwrap() as usize;
    let present: std::collections::HashSet<u32> = objects.iter().map(|(id, _)| *id).collect();
    let mut offsets = vec![0usize; max_id + 1];
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
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
        if present.contains(&(id as u32)) {
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

fn bench_subset(c: &mut Criterion) {
    let bytes = build_doc();
    let doc = PdfDocument::from_bytes(bytes).expect("parse synthetic doc");
    let keep: Vec<(usize, usize)> = (0..PAGES / 2).map(|p| (0usize, p)).collect();

    let mut group = c.benchmark_group("subset");
    group.sample_size(20);

    group.bench_function(BenchmarkId::new("half_pages", "dedup_on"), |b| {
        b.iter(|| {
            let opts = SubsetOptions::default();
            let (out, _) = subset_to_bytes(black_box(&[&doc]), black_box(&keep), opts).unwrap();
            black_box(out);
        });
    });

    group.bench_function(BenchmarkId::new("half_pages", "dedup_off"), |b| {
        b.iter(|| {
            let opts = SubsetOptions { dedup: false, ..SubsetOptions::default() };
            let (out, _) = subset_to_bytes(black_box(&[&doc]), black_box(&keep), opts).unwrap();
            black_box(out);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_subset);
criterion_main!(benches);
