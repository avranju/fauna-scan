use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    let root = Path::new("web/dist");
    let mut assets = Vec::new();
    fn visit(path: &Path, assets: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries {
                let path = entry.expect("read frontend asset").path();
                if path.is_dir() {
                    visit(&path, assets);
                } else {
                    assets.push(path);
                }
            }
        }
    }
    visit(root, &mut assets);
    assets.sort();
    let mut code = String::from("static ASSETS: &[(&str, &[u8])] = &[\n");
    for path in assets {
        let name = format!("/{}", path.strip_prefix(root).unwrap().to_string_lossy());
        let absolute = fs::canonicalize(&path).expect("frontend asset path");
        code.push_str(&format!(
            "({name:?}, include_bytes!({:?})),\n",
            absolute.to_string_lossy()
        ));
    }
    code.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("web_assets.rs"),
        code,
    )
    .unwrap();
    if !root.join("index.html").exists() {
        println!(
            "cargo:warning=Frontend assets absent. Run cd web && npm ci && npm run build before building the release binary."
        );
    }
}
