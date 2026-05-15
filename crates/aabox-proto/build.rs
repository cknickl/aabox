// Compiles aasdk's .proto files (vendored under proto/) into Rust types via prost.
// When the aasdk submodule is added under references/aasdk, point PROTO_DIR
// at references/aasdk/aasdk_proto and re-run.

use std::path::{Path, PathBuf};

fn main() {
    let proto_dir: PathBuf = std::env::var("AABOX_PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Default: ../../references/aasdk/aasdk_proto relative to the crate dir
            let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            crate_dir
                .join("../../references/aasdk/aasdk_proto")
                .canonicalize()
                .unwrap_or_else(|_| {
                    // Fallback: empty placeholder so the crate still builds before the
                    // submodule has been added.
                    crate_dir.join("proto-stub")
                })
        });

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AABOX_PROTO_DIR");

    if !proto_dir.exists() {
        eprintln!(
            "aabox-proto: PROTO_DIR {} does not exist; building empty crate.",
            proto_dir.display()
        );
        return;
    }

    // Collect all .proto files in proto_dir.
    let mut protos: Vec<PathBuf> = Vec::new();
    collect_protos(&proto_dir, &mut protos);

    if protos.is_empty() {
        eprintln!(
            "aabox-proto: no .proto files found under {}; building empty crate.",
            proto_dir.display()
        );
        return;
    }

    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    prost_build::Config::new()
        .compile_protos(&protos, &[&proto_dir])
        .expect("prost compile failed");
}

fn collect_protos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_protos(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("proto") {
            out.push(path);
        }
    }
}
