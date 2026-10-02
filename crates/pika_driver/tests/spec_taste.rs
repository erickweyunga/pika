//! The program of section 2 of the spec, "A taste", must compile without errors or warnings.

#[test]
fn the_taste_of_the_spec_compiles() {
    let spec = include_str!("../../../docs/spec/v0.md");
    let section = spec
        .split("## 2. A taste")
        .nth(1)
        .expect("the spec has section 2");
    let program = section
        .split("```pika\n")
        .nth(1)
        .and_then(|block| block.split("```").next())
        .expect("section 2 has a Pika program");
    let analysis = pika_driver::check_source(program);
    let rendered = pika_diagnostics::render_map(
        &analysis.diagnostics,
        &analysis.sources.map,
        pika_diagnostics::RenderOptions::default(),
    );
    assert!(analysis.diagnostics.is_empty(), "{rendered}");
    assert!(pika_driver::runnable(&analysis).is_ok());
}
