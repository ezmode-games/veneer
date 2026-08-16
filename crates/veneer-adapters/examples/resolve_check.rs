//! One-off cross-check: run the element resolver over a real installed
//! component directory and print the buckets, so the Rust implementation can
//! be compared against the independent TypeScript-AST enumeration.
use std::collections::BTreeMap;
use std::path::PathBuf;
use veneer_adapters::{resolve_root_element, ElementSource};

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: resolve_check <ui-dir> <name>..."),
    );
    let names: Vec<String> = std::env::args().skip(2).collect();
    let mut ok: BTreeMap<String, String> = BTreeMap::new();
    let mut err: BTreeMap<String, String> = BTreeMap::new();

    for name in &names {
        // the .tsx that declares it: try every file, take the one that resolves
        let mut resolved = None;
        let mut last_err = None;
        for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("tsx") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            match resolve_root_element(name, &src) {
                Ok(found) => {
                    resolved = Some(found);
                    break;
                }
                Err(e) => {
                    if !matches!(e, veneer_adapters::ElementError::NoDeclaration { .. }) {
                        last_err = Some(e.to_string());
                    }
                }
            }
        }
        match resolved {
            Some(found) => {
                let via = match found.source {
                    ElementSource::JsxRoot => "jsx",
                    ElementSource::ForwardRefGeneric => "generic",
                };
                ok.insert(name.clone(), format!("<{}> via {}", found.tag, via));
            }
            None => {
                err.insert(
                    name.clone(),
                    last_err.unwrap_or_else(|| "no declaration found".into()),
                );
            }
        }
    }
    println!("RESOLVED {} / {}", ok.len(), names.len());
    for (k, v) in &ok {
        println!("  {k:14} {v}");
    }
    println!("\nUNRESOLVED {}", err.len());
    for (k, v) in &err {
        println!("  {k:14} {}", &v[..v.len().min(96)]);
    }
}
