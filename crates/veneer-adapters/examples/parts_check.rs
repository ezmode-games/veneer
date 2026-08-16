//! Run the class-parts reader over a real installed component directory.
use std::collections::BTreeMap;
use veneer_adapters::read_class_parts_for;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: parts_check <ui-dir>");
    let mut with_parts = 0;
    let mut without = Vec::new();
    let mut unresolved_total = 0;
    let mut root_missing = Vec::new();
    let mut files = 0;

    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".classes.ts") {
            continue;
        }
        files += 1;
        let source = std::fs::read_to_string(&path).unwrap();
        match read_class_parts_for(
            name.strip_suffix(".classes.ts").unwrap_or(&name),
            &source,
            &BTreeMap::new(),
        ) {
            Some(parts) => {
                with_parts += 1;
                unresolved_total += parts.unresolved.len();
                if parts.root().is_none() {
                    root_missing.push(format!(
                        "{name} (parts: {})",
                        parts.parts.keys().cloned().collect::<Vec<_>>().join(",")
                    ));
                }
                if name.starts_with("avatar")
                    || name.starts_with("table")
                    || name.starts_with("card")
                {
                    println!("--- {name}");
                    for (part, classes) in &parts.parts {
                        println!("    {part:10} {}", &classes[..classes.len().min(72)]);
                    }
                }
            }
            None => without.push(name),
        }
    }
    println!(
        "\nfiles: {files}   read a part map: {with_parts}   no classes function: {}",
        without.len()
    );
    println!("unresolved part expressions: {unresolved_total}");
    println!("no root part ({}): {:?}", root_missing.len(), root_missing);
    println!("no classes function: {without:?}");
}
