//! The standard library, checked as the package compiled: every function in it is built, and
//! it must have no errors and no warnings.

use pika_diagnostics::{RenderOptions, render_map};
use pika_driver::Sources;

#[test]
fn standard_library_checks_cleanly() {
    let analysis = pika_driver::check(Sources::standard_library());
    let rendered = render_map(
        &analysis.diagnostics,
        &analysis.sources.map,
        RenderOptions::default(),
    );
    assert!(analysis.diagnostics.is_empty(), "{rendered}");
    assert!(analysis.mir.is_some());
}

#[test]
fn standard_library_is_type_checked() {
    let analysis = pika_driver::check(Sources::standard_library());
    let described = pika_driver::describe_types(&analysis);
    assert!(described.contains("module /std/collections"), "{described}");
    assert!(
        described.contains("fn sort<T: Ord>(items: List<T>) -> nothing"),
        "{described}"
    );
}

/// A standard library with the given root module, to check how declarations of runtime
/// functions are verified.
fn fake_standard_library(text: &str) -> Sources {
    let mut map = pika_diagnostics::SourceMap::default();
    let file = map.add("std/lib.pk", text);
    Sources {
        map,
        packages: vec![pika_driver::Package {
            name: "std".to_owned(),
            version: None,
            binary: false,
            dependencies: Vec::new(),
            modules: vec![(Vec::new(), file)],
        }],
        root: 0,
    }
}

/// The diagnostics of a standard library written as `text`, rendered.
fn diagnostics_of(text: &str) -> String {
    let analysis = pika_driver::check(fake_standard_library(text));
    render_map(
        &analysis.diagnostics,
        &analysis.sources.map,
        RenderOptions::default(),
    )
}

#[test]
fn runtime_functions_must_match_intrinsics() {
    insta::assert_snapshot!(diagnostics_of(
        ":extern lib=\"pika\" {\n    :fn sqrt x:f32 -> f64\n    :fn _no_such_thing\n    :fn _print mut text:String\n    :fn _arg_count -> i64 raises\n}\n",
    ));
}

#[test]
fn runtime_functions_have_no_body_or_type_parameters() {
    insta::assert_snapshot!(diagnostics_of(
        ":extern lib=\"pika\" {\n    :fn pow<T> x:T y:T -> T\n    :fn sqrt x:f64=1.0 -> f64 do={ :return $x }\n}\n",
    ));
}

#[test]
fn impls_are_checked() {
    insta::assert_snapshot!(diagnostics_of(
        ":struct Point {\n    x:i64\n}\n:impl Point {\n    :fn origin -> Point do={ :return Point{x=0} }\n}\n:impl<T> String {\n}\n:impl<T> List<T> {\n    :fn push mut self owned value:T do={}\n    :fn clone self -> i64 do={ :return 0 }\n    :fn twice self -> i64 do={ :return 2 }\n}\n:impl<T> List<T> {\n    :fn twice self -> i64 do={ :return 2 }\n}\n:fn main do={\n    :impl i64 {\n    }\n}\n",
    ));
}
