use std::{
    env, fs,
    path::{Path, PathBuf},
};

const MIGRATIONS: &str = "migrations";

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    println!("cargo:rerun-if-changed={MIGRATIONS}");

    let mut names: Vec<String> = fs::read_dir(Path::new(&manifest_dir).join(MIGRATIONS))
        .unwrap_or_else(|error| panic!("cannot read {MIGRATIONS}: {error}"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sql"))
        .collect();
    names.sort();

    // `sqlx::migrate!` embeds the files it finds when it runs, and `include_str!`
    // makes the compiler track edits to files that already existed. Adding a new
    // migration file changes neither, so the embedded set would go stale. Emitting
    // the file names into OUT_DIR makes any change to the set force a recompile,
    // which reruns the macro.
    let listed = names
        .iter()
        .map(|name| format!("    \"{name}\","))
        .collect::<Vec<_>>()
        .join("\n");
    let generated = format!(
        "/// Migration file names embedded at compile time.\npub const MIGRATION_FILES: &[&str] = &[\n{listed}\n];\n"
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    fs::write(out_dir.join("migrations.rs"), generated).expect("cannot write migrations.rs");
}
