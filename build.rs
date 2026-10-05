use std::path::PathBuf;
use std::process::Command;

const RESOURCES: &str = "data/resources";
const UI: &str = "data/resources/ui";

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut blueprints: Vec<PathBuf> = std::fs::read_dir(UI)
        .expect("data/resources/ui is missing")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "blp"))
        .collect();
    blueprints.sort();
    println!("cargo:rerun-if-changed={UI}");
    for path in &blueprints {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let status = Command::new("blueprint-compiler")
        .arg("batch-compile")
        .arg(out.join("ui"))
        .arg(UI)
        .args(&blueprints)
        .status()
        .expect("blueprint-compiler is required to build the interface");
    assert!(status.success(), "blueprint-compiler failed");

    // only for running from the source tree; Meson installs the schema
    let schemas = out.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    std::fs::copy(
        "data/io.github.mehmetnuri.Ferry.gschema.xml",
        schemas.join("io.github.mehmetnuri.Ferry.gschema.xml"),
    )
    .unwrap();
    println!("cargo:rerun-if-changed=data/io.github.mehmetnuri.Ferry.gschema.xml");
    let status = Command::new("glib-compile-schemas").arg(&schemas).status().expect("glib-compile-schemas is required");
    assert!(status.success(), "glib-compile-schemas failed");

    println!("cargo:rerun-if-changed=po");
    for entry in std::fs::read_dir("po").into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "po") {
            let lang = path.file_stem().unwrap().to_string_lossy().into_owned();
            let dir = out.join("locale").join(&lang).join("LC_MESSAGES");
            std::fs::create_dir_all(&dir).unwrap();
            let status = Command::new("msgfmt")
                .arg("-o")
                .arg(dir.join("ferry.mo"))
                .arg(&path)
                .status()
                .expect("msgfmt is required");
            assert!(status.success(), "msgfmt failed for {}", path.display());
        }
    }

    println!("cargo:rerun-if-changed={RESOURCES}");
    glib_build_tools::compile_resources(
        &[out.to_str().unwrap(), RESOURCES],
        &format!("{RESOURCES}/resources.gresource.xml"),
        "ferry.gresource",
    );
}
