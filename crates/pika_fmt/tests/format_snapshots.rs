//! Snapshot tests of the formatter: every `format/*.pk` file is formatted, and the result is
//! compared with the stored snapshot. Formatting the result again must not change it.

#[test]
fn format_snapshots() {
    insta::glob!("format/*.pk", |path| {
        let source = std::fs::read_to_string(path).expect("readable test input");
        let formatted = pika_fmt::format(&source).expect("the input can be formatted");
        let again = pika_fmt::format(&formatted).expect("formatted output can be formatted");
        assert_eq!(formatted, again, "formatting is not idempotent");
        insta::assert_snapshot!(formatted);
    });
}
