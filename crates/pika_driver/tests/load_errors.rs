//! Packages that cannot be loaded: each directory in `load_errors/` is a package with a
//! problem, and the error message is compared with the stored snapshot.

use std::path::Path;

#[test]
fn load_errors() {
    insta::glob!("load_errors/*", |dir| {
        // Paths in messages are relative to the crate, where tests run. Both paths are
        // resolved, since they can be written differently (on Windows, with `\\?\`).
        let canonical = |path: &Path| path.canonicalize().expect("the path exists");
        let crate_dir = canonical(Path::new(env!("CARGO_MANIFEST_DIR")));
        let dir = canonical(dir);
        let relative = dir
            .strip_prefix(&crate_dir)
            .expect("fixtures are in the crate");
        let error = pika_driver::load_package(relative)
            .expect_err("the package has a problem")
            .to_string();
        insta::assert_snapshot!(error);
    });
}

#[test]
fn loads_dependencies_before_their_dependents() {
    let dir = Path::new("tests/check_packages/ok_modules");
    let sources = pika_driver::load_package(dir).expect("a valid package");
    let names: Vec<&str> = sources.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["std", "shapes", "app"]);
    assert_eq!(sources.root, 2);
    assert!(sources.root_package().binary);
    let app = &sources.packages[2];
    assert_eq!(app.dependencies, ["std", "shapes"]);
    assert_eq!(app.version.as_deref(), Some("0.1.0"));
    let paths: Vec<&[String]> = app
        .modules
        .iter()
        .map(|(path, _)| path.as_slice())
        .collect();
    assert_eq!(paths, [&[][..], &["util".to_owned()][..]]);
}
