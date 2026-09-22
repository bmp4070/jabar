//! Writes one synthetic SCIP shard, so the benchmark harness can be validated
//! end to end without Bazel, a JDK, or `scip-java`.
//!
//! Usage: `jabar-gen-shard <out-dir> [ClassName ...]`. Each name becomes a
//! class definition in `src/<Name>.java`, emitted into `<out-dir>/gen.scip`.
//! This mirrors the server's own test helper; it exists only to feed a real
//! `*.scip` file into `jabar` for a fixture-scale smoke test.

use protobuf::Message as _;

fn main() {
    let mut args = std::env::args().skip(1);
    let out_dir = args.next().unwrap_or_else(|| {
        eprintln!("usage: jabar-gen-shard <out-dir> [ClassName ...]");
        std::process::exit(2);
    });
    let names: Vec<String> = args.collect();
    let names = if names.is_empty() { vec!["Widget".to_owned()] } else { names };

    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let mut index = scip::types::Index::new();
    for name in &names {
        let symbol = format!("semanticdb maven . . example/{name}#");

        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, 6, 6 + i32::try_from(name.len()).unwrap()];
        occurrence.symbol = symbol.clone();
        occurrence.symbol_roles = scip::types::SymbolRole::Definition as i32;

        let mut information = scip::types::SymbolInformation::new();
        information.symbol = symbol;
        information.display_name = name.clone();
        information.kind = scip::types::symbol_information::Kind::Class.into();

        let mut document = scip::types::Document::new();
        document.language = "java".to_owned();
        document.relative_path = format!("src/{name}.java");
        document.occurrences.push(occurrence);
        document.symbols.push(information);
        index.documents.push(document);
    }

    let path = std::path::Path::new(&out_dir).join("gen.scip");
    std::fs::write(&path, index.write_to_bytes().expect("encode scip")).expect("write shard");
    println!("wrote {} definitions to {}", names.len(), path.display());
}
