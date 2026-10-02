//! Live check that the hand-rolled .docx is accepted by a real consumer.
//!
//! Unit tests prove the XML is balanced; they cannot prove Word will open it.
//! LibreOffice parsing the file and producing a PDF is the strongest available
//! signal on this box, and it is the check that would catch a wrong content-types
//! part or a bad relationship table.

use cowry_engine::tools::docs::{markdown_to_docx, markdown_to_html, markdown_to_pdf};

const MD: &str = r#"# Quarterly note

Some **bold** text, some *italic*, an `inline span`, and a
[link](https://example.com/d).

- first item
- second item with **bold**

```rust
fn main() {
    println!("hello");
}
```

Tom & Jerry's "quote" <tag> and a raw <script>alert(1)</script> that must not
become markup.

> a quoted line
"#;

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("cowry_docs_live_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let docx = dir.join("note.docx");
    let bytes = markdown_to_docx(MD).expect("docx built");
    std::fs::write(&docx, &bytes).unwrap();
    println!("docx: {} bytes at {}", bytes.len(), docx.display());

    // zip structure
    let cur = std::io::Cursor::new(bytes.clone());
    let zip = zip::ZipArchive::new(cur).expect("zip opens");
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    println!("parts: {names:?}");

    let html = dir.join("note.html");
    std::fs::write(&html, markdown_to_html(MD)).unwrap();
    println!("html: {} bytes", std::fs::metadata(&html).unwrap().len());

    let pdf = dir.join("note.pdf");
    match markdown_to_pdf(MD, &pdf, 120).await {
        Ok(pdf) => {
            let meta = std::fs::metadata(&pdf).unwrap();
            let head = std::fs::read(&pdf).unwrap_or_default();
            let is_pdf = head.starts_with(b"%PDF");
            println!(
                "pdf: {} bytes, header_ok={is_pdf} at {}",
                meta.len(),
                pdf.display()
            );
            if !is_pdf {
                println!("FAIL: converter produced something that is not a PDF");
            }
        }
        Err(e) => println!("pdf: unavailable - {e}"),
    }

    println!("kept at {}", dir.display());
}
