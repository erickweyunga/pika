//! Differential testing of the code generator: random well-typed programs (arithmetic, strings,
//! structs, enums with options and boxes, and collections) are run compiled to native code
//! and interpreted, and must behave identically (output, panics and
//! exit status). The interpreter implements the reference semantics of every operation.
//!
//! Each case starts two processes, so the default number of cases is modest; set
//! `PROPTEST_CASES` for a deeper run. Compiled programs also run under the leak check, so a
//! program that does not free all of its heap memory fails.

use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Num {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

const ALL: [Num; 10] = [
    Num::I8,
    Num::I16,
    Num::I32,
    Num::I64,
    Num::U8,
    Num::U16,
    Num::U32,
    Num::U64,
    Num::F32,
    Num::F64,
];

/// Number of named values of each type that expressions can use.
const LEAVES: usize = 3;

impl Num {
    fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    fn is_signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    fn range(self) -> (i128, i128) {
        let bits = match self {
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 | Self::F32 => 32,
            Self::I64 | Self::U64 | Self::F64 => 64,
        };
        if self.is_signed() {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (0, (1i128 << bits) - 1)
        }
    }

    /// A literal of this type: edge values are likely, to exercise overflow checks.
    fn literal(self) -> BoxedStrategy<String> {
        if self == Self::F32 {
            let special = prop::sample::select(vec![
                "0.0",
                "-0.0",
                "1.0",
                "-1.0",
                "0.5",
                "3.0e38",
                "-3.0e38",
                "1e-40",
                "255.5",
                "-129.0",
                "4294967296.0",
                "16777217.0",
            ])
            .prop_map(str::to_owned);
            let finite = any::<f32>()
                .prop_filter("finite", |v| v.is_finite())
                .prop_map(|v| format!("{v:?}"));
            let small = (-1000.0..1000.0f32).prop_map(|v| format!("{v:?}"));
            return prop_oneof![special, finite, small].boxed();
        }
        if self == Self::F64 {
            let special = prop::sample::select(vec![
                "0.0",
                "-0.0",
                "1.0",
                "-1.0",
                "0.5",
                "1e300",
                "-1e300",
                "1e-300",
                "255.5",
                "-129.0",
                "4294967296.0",
                "1e20",
                "9.3e18",
            ])
            .prop_map(str::to_owned);
            let finite = any::<f64>()
                .prop_filter("finite", |v| v.is_finite())
                .prop_map(|v| format!("{v:?}"));
            let small = (-1000.0..1000.0f64).prop_map(|v| format!("{v:?}"));
            return prop_oneof![special, finite, small].boxed();
        }
        let (min, max) = self.range();
        let edges = prop::sample::select(vec![min, max, 0, 1, min + 1, max - 1, 2, 7]);
        let any_value = any::<u64>().prop_map(move |v| min + i128::from(v) % (max - min + 1));
        prop_oneof![
            edges,
            any_value,
            (-20i128..20).prop_map(move |v| v.clamp(min, max))
        ]
        .prop_map(|v| v.to_string())
        .boxed()
    }
}

/// A random expression of type `ty`, built from the named leaves `$v_<type>_<n>`.
fn expr(ty: Num, depth: u32) -> BoxedStrategy<String> {
    let leaf = (0..LEAVES)
        .prop_map(move |i| format!("$v_{}_{i}", ty.name()))
        .boxed();
    if depth == 0 {
        return leaf;
    }
    let sub = move || expr(ty, depth - 1);
    let any_numeric = prop::sample::select(ALL.to_vec());
    let cast = any_numeric
        .prop_flat_map(move |from| expr(from, depth - 1))
        .prop_map(move |inner| format!("({inner} as {})", ty.name()));
    if ty.is_float() {
        let binary = (
            sub(),
            prop::sample::select(vec!["+", "-", "*", "/", "%"]),
            sub(),
        )
            .prop_map(|(a, op, b)| format!("({a} {op} {b})"));
        let negate = sub().prop_map(|a| format!("(-{a})"));
        return prop_oneof![2 => leaf, 3 => binary, 1 => negate, 1 => cast].boxed();
    }
    let binary = (
        sub(),
        prop::sample::select(vec!["+", "-", "*", "/", "%", "&", "|", "^"]),
        sub(),
    )
        .prop_map(|(a, op, b)| format!("({a} {op} {b})"));
    let int_types: Vec<Num> = ALL.iter().copied().filter(|t| !t.is_float()).collect();
    let shift = (
        sub(),
        prop::sample::select(vec!["<<", ">>"]),
        prop::sample::select(int_types).prop_flat_map(move |t| expr(t, 0)),
    )
        .prop_map(|(a, op, amount)| format!("({a} {op} {amount})"));
    let bit_not = sub().prop_map(|a| format!("(~{a})"));
    let mut options = vec![
        (2, leaf),
        (4, binary.boxed()),
        (1, shift.boxed()),
        (1, bit_not.boxed()),
        (1, cast.boxed()),
    ];
    if ty.is_signed() {
        options.push((1, sub().prop_map(|a| format!("(-{a})")).boxed()));
    }
    proptest::strategy::Union::new_weighted(options).boxed()
}

/// A program printing a few random expressions and comparisons.
fn program() -> impl Strategy<Value = String> {
    let leaves: Vec<BoxedStrategy<String>> = ALL
        .iter()
        .flat_map(|&ty| {
            (0..LEAVES).map(move |i| {
                ty.literal()
                    .prop_map(move |v| {
                        format!("    :const v_{}_{i}:{} {v}\n", ty.name(), ty.name())
                    })
                    .boxed()
            })
        })
        .collect();
    let statement = prop::sample::select(ALL.to_vec()).prop_flat_map(|ty| {
        prop_oneof![
            3 => expr(ty, 3).prop_map(|e| format!("    :put {e}\n")),
            1 => (expr(ty, 2), prop::sample::select(vec!["=", "!=", "<", "<=", ">", ">="]), expr(ty, 2))
                .prop_map(|(a, op, b)| format!("    :put ({a} {op} {b})\n")),
        ]
    });
    (leaves, prop::collection::vec(statement, 1..6)).prop_map(|(leaves, statements)| {
        let mut source = String::from(":fn main do={\n");
        for line in leaves.iter().chain(&statements) {
            source.push_str(line);
        }
        source.push_str("}\n");
        source
    })
}

fn scratch_file(source: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("differential_{}_{index}.pk", std::process::id()));
    std::fs::write(&path, source).expect("scratch directory is writable");
    path
}

fn run(path: &PathBuf, interpret: bool) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
    command.env("PIKA_LEAK_CHECK", "1").arg("run").arg(path);
    if interpret {
        command.arg("--interpret");
    }
    let output = command.output().expect("the pika binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(128)
}

/// Number of string variables in generated string programs.
const STRINGS: usize = 4;

/// A statement on string variables `$s0`..`$s3`. Every statement leaves every variable
/// assigned, so programs never use a moved value.
fn string_statement() -> impl Strategy<Value = String> {
    let var = || (0..STRINGS).prop_map(|i| format!("s{i}"));
    let number = prop_oneof![
        any::<i64>().prop_map(|v| v.to_string()),
        (-1000.0..1000.0f64).prop_map(|v| format!("{v:?}")),
        prop::sample::select(vec!["'x'", "true", "1m30s", "(255 as u8)", "0.1"])
            .prop_map(str::to_owned),
    ];
    let literal = prop::sample::select(vec!["", "a", "abc", "héllo", "xyz", "a b"])
        .prop_map(|t| format!("\"{t}\""));
    prop_oneof![
        (var(), var(), var()).prop_map(|(d, a, b)| format!(":set {d} (${a} . ${b})")),
        (var(), var(), number).prop_map(|(d, a, n)| format!(":set {d} \"${a}-$({n})\"")),
        (var(), literal).prop_map(|(d, t)| format!(":set {d} {t}")),
        (var(), var()).prop_map(|(d, a)| format!(":set {d} [${a}->clone]")),
        // A `mut` argument cannot also be another argument of the same call.
        (var(), var())
            .prop_filter("distinct variables", |(d, a)| d != a)
            .prop_map(|(d, a)| format!(":append ${d} ${a}")),
        (var(), var()).prop_map(|(d, a)| format!(":set {d} [:echo ${a}]")),
        (var(), var()).prop_map(|(d, a)| format!(":set {d} [:take [${a}->clone]]")),
        (var(), var()).prop_map(|(d, a)| format!(
            ":local moved_{d}_{a} ${a}\n    :set {a} \"$moved_{d}_{a}!\""
        )),
        (var(), var()).prop_map(|(a, b)| format!(
            ":put \"$(${a} = ${b}) $(${a} < ${b}) $(${a} in ${b}) $[:len ${a}]\""
        )),
        (var(), any::<bool>()).prop_map(|(a, flag)| {
            format!(":if ({flag}) do={{ :consume ${a}\n        :set {a} \"refilled\" }}")
        }),
        var().prop_map(|a| format!(":put ${a}")),
    ]
}

/// A program applying random string statements, then printing every variable.
fn string_program() -> impl Strategy<Value = String> {
    prop::collection::vec(string_statement(), 1..25).prop_map(|statements| {
        let mut source = String::from(
            ":fn append mut text:String suffix:String do={ :set text ($text . $suffix) }\n\
             :fn echo text:String -> String do={ :return [$text->clone] }\n\
             :fn take owned text:String -> String do={ :return ($text . \"+\") }\n\
             :fn consume owned text:String do={ :put \"consumed $[:len $text]\" }\n\
             :fn main do={\n",
        );
        for i in 0..STRINGS {
            writeln!(source, "    :local s{i} \"init{i}\"").expect("writing to a String");
        }
        for (index, statement) in statements.iter().enumerate() {
            // Each statement in its own block, so that the variables it declares are unique.
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        for i in 0..STRINGS {
            writeln!(source, "    :put $s{i}").expect("writing to a String");
        }
        source.push_str("}\n");
        source
    })
}

/// Number of struct variables in generated struct programs.
const RECORDS: usize = 3;

/// Declarations of struct programs: fields of mixed sizes, so that layouts have padding, a
/// nested struct, owned strings and a `Copy` struct.
const STRUCT_PRELUDE: &str = "\
:struct Small impl=Copy,Eq,Ord,Display {
    x:i16
    y:u8
    z:f64
}
:struct Inner impl=Clone,Eq,Ord,Display {
    c:char
    f:f32
    t:String
}
:struct Rec impl=Clone,Eq,Ord,Display {
    a:u8
    s:String
    b:i64=-1
    inner:Inner
    small:Small=Small{x=1; y=2; z=0.5}
    level:i8=0

    :fn describe self -> String do={
        :return \"$($self->s)/$($self->inner->t):$($self->b)\"
    }

    :fn touch mut self n:i64 do={
        :set ($self->b) (($self->b / 2) + $n)
        :set ($self->level) (($self->level / 2) - 3)
    }
}
:fn make text:String -> Rec do={
    :return Rec{a=7; s=[$text->clone]; inner=Inner{c='q'; f=1.5; t=($text . \"!\")}}
}
:fn bump mut inner:Inner do={
    :set ($inner->t) ($inner->t . \"+\")
    :set ($inner->f) ($inner->f * 2.0)
}
:fn consume owned r:Rec do={ :put \"consumed $($r->s)\" }
";

/// A statement on struct variables `$r0`..`$r2`. Every statement leaves every variable
/// assigned, so programs never use a moved value.
fn struct_statement() -> impl Strategy<Value = String> {
    let var = || (0..RECORDS).prop_map(|i| format!("r{i}"));
    let small = (-50i16..50, 0u8..10).prop_map(|(x, y)| format!("Small{{x={x}; y={y}; z=0.25}}"));
    let text = prop::sample::select(vec!["", "a", "héllo", "x\\\"y"]).prop_map(str::to_owned);
    prop_oneof![
        (var(), var()).prop_map(|(d, a)| format!(":set (${d}->s) (${a}->s . \"x\")")),
        (var(), var()).prop_map(|(d, a)| format!(":set (${d}->inner->t) [${a}->inner->t->clone]")),
        (var(), var(), -1000i64..1000)
            .prop_map(|(d, a, n)| format!(":set (${d}->b) ((${a}->b / 2) + {n})")),
        (var(), var()).prop_map(|(d, a)| format!(":set (${d}->inner->f) (${a}->inner->f * 0.5)")),
        var().prop_map(|d| format!(":set (${d}->inner->f) (0.0 / 0.0)")),
        (var(), var()).prop_map(|(d, a)| format!(":set (${d}->small) ${a}->small")),
        (var(), small).prop_map(|(d, v)| format!(":set (${d}->small) {v}")),
        (var(), var()).prop_map(|(d, a)| format!(":set (${d}->small->x) (${a}->small->x / 2)")),
        (var(), var()).prop_map(|(d, a)| format!(":set {d} [${a}->clone]")),
        (var(), var(), text.clone())
            .prop_filter("distinct variables", |(d, a, _)| d != a)
            .prop_map(|(d, a, t)| format!(":set {d} ${a}\n    :set {a} [:make \"{t}\"]")),
        (var(), text).prop_map(|(d, t)| format!(":set {d} [:make \"{t}\"]")),
        (var(), -100i64..100).prop_map(|(d, n)| format!("${d}->touch {n}")),
        var().prop_map(|d| format!(":bump ${d}->inner")),
        var().prop_map(|d| format!(":put [${d}->describe]")),
        (var(), var()).prop_map(|(a, b)| format!(
            ":put \"$(${a} = ${b}) $(${a} != ${b}) $(${a} < ${b}) $(${a} >= ${b}) $(${a}->small < ${b}->small)\""
        )),
        (var(), any::<bool>()).prop_map(|(a, flag)| {
            format!(":if ({flag}) do={{ :consume ${a}\n        :set {a} [:make \"refilled\"] }}")
        }),
        var().prop_map(|a| format!(":put ${a}")),
        var().prop_map(|a| format!(":put \"<${a}>\"")),
    ]
}

/// A program applying random struct statements, then printing every variable.
fn struct_program() -> impl Strategy<Value = String> {
    prop::collection::vec(struct_statement(), 1..20).prop_map(|statements| {
        let mut source = String::from(STRUCT_PRELUDE);
        source.push_str(":fn main do={\n");
        for i in 0..RECORDS {
            writeln!(source, "    :local r{i} [:make \"init{i}\"]").expect("writing to a String");
        }
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        for i in 0..RECORDS {
            writeln!(source, "    :put $r{i}").expect("writing to a String");
        }
        source.push_str("}\n");
        source
    })
}

/// Number of enum variables in generated enum programs.
const ITEMS: usize = 3;

/// Declarations of enum programs: a recursive enum with variants of different layouts,
/// boxes, options, and functions that match on places and on temporaries.
const ENUM_PRELUDE: &str = "\
:enum Item impl=Clone,Eq,Ord,Display {
    num value:i64
    small c:char flag:u8
    text s:String
    pair left:Box<Item> right:Box<Item>?
    empty

    :fn depth self -> i64 do={
        :match $self {
            Item->pair l (some r) do={
                :const a [$l->value->depth]
                :const b [$r->value->depth]
                :if ($a > $b) do={ :return ($a + 1) }
                :return ($b + 1)
            }
            Item->pair l none do={ :return ([$l->value->depth] + 1) }
            _ do={ :return 0 }
        }
    }
}
:fn describe item:Item -> String do={
    :match $item {
        Item->num v if=($v < 0) do={ :return \"negative\" }
        Item->num 0 do={ :return \"zero\" }
        Item->num v do={ :return \"num $v\" }
        Item->small 'a' 1 do={ :return \"small a\" }
        Item->small c _ do={ :return \"small $c\" }
        Item->text \"\" do={ :return \"no text\" }
        Item->text s do={ :return \"text $s\" }
        Item->pair _ none do={ :return \"pair without right\" }
        Item->pair l r do={ :return \"pair $[$l->value->depth] $r\" }
        Item->empty do={ :return \"empty\" }
    }
}
:fn pass owned item:Item -> Item do={ :return $item }
:fn take_text owned item:Item -> String do={
    :match [:pass $item] {
        Item->text s do={ :return $s }
        Item->pair l (some r) do={
            :const inner [$l->unbox]
            :const right [$r->unbox]
            :return \"$inner/$right\"
        }
        _ do={ :return \"none\" }
    }
}
:fn first_num item:Item -> i64? do={
    :match $item {
        Item->num v do={ :return [some $v] }
        _ do={ :return none }
    }
}
:fn consume owned item:Item do={ :put \"consumed $[$item->depth]\" }
";

/// A statement on enum variables `$i0`..`$i2`. Every statement leaves every variable
/// assigned, so programs never use a moved value.
fn enum_statement() -> impl Strategy<Value = String> {
    let var = || (0..ITEMS).prop_map(|i| format!("i{i}"));
    let value = prop_oneof![
        (-5i64..5).prop_map(|v| format!("[Item->num {v}]")),
        (prop::sample::select(vec!['a', 'b', 'é']), 0u8..3)
            .prop_map(|(c, f)| format!("[Item->small '{c}' {f}]")),
        prop::sample::select(vec!["", "x", "héllo"]).prop_map(|t| format!("[Item->text \"{t}\"]")),
        Just("Item->empty".to_owned()),
    ];
    prop_oneof![
        (var(), value).prop_map(|(d, v)| format!(":set {d} {v}")),
        (var(), var()).prop_map(|(d, a)| format!(
            ":set {d} [Item->pair [Box->new [${a}->clone]] none]"
        )),
        (var(), var()).prop_map(|(d, a)| format!(
            ":set {d} [Item->pair [Box->new Item->empty] [some [Box->new [${a}->clone]]]]"
        )),
        (var(), var())
            .prop_filter("distinct variables", |(d, a)| d != a)
            .prop_map(|(d, a)| format!(":set {d} ${a}\n    :set {a} [Item->num 1]")),
        var().prop_map(|a| format!(":put [:describe ${a}]")),
        var().prop_map(|a| format!(":put [:take_text [${a}->clone]]")),
        var().prop_map(|a| format!(":put [${a}->depth]")),
        (var(), var()).prop_map(|(a, b)| format!(
            ":put \"$(${a} = ${b}) $(${a} != ${b}) $(${a} < ${b}) $(${a} >= ${b})\""
        )),
        var().prop_map(|a| format!(
            ":const n [:first_num ${a}]\n    :put \"$n $[$n->is_some] $[$n->unwrap_or -1]\""
        )),
        var().prop_map(|a| format!(
            ":local o:Item? [some [${a}->clone]]\n    :const t [$o->take]\n    :put \"$t $o\""
        )),
        var().prop_map(|a| format!(
            ":local b [Box->new [${a}->clone]]\n    :set ($b->value) [Item->num 7]\n    :set {a} [$b->unbox]"
        )),
        (var(), any::<bool>()).prop_map(|(a, flag)| format!(
            ":if ({flag}) do={{ :consume ${a}\n        :set {a} Item->empty }}"
        )),
        var().prop_map(|a| format!(
            ":match [${a}->clone] {{\n        Item->pair l r do={{ :const owned $l\n            :put \"owned $owned $r\" }}\n        other do={{ :put \"other $other\" }}\n    }}"
        )),
        var().prop_map(|a| format!(":put ${a}")),
    ]
}

/// A program applying random enum statements, then printing every variable.
fn enum_program() -> impl Strategy<Value = String> {
    prop::collection::vec(enum_statement(), 1..20).prop_map(|statements| {
        let mut source = String::from(ENUM_PRELUDE);
        source.push_str(":fn main do={\n");
        for i in 0..ITEMS {
            writeln!(source, "    :local i{i} [Item->num {i}]").expect("writing to a String");
        }
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        for i in 0..ITEMS {
            writeln!(source, "    :put $i{i}").expect("writing to a String");
        }
        source.push_str("}\n");
        source
    })
}

/// Declarations of collection programs.
const COLLECTION_PRELUDE: &str = "\
:fn total xs:List<i64> -> i64 do={
    :local sum 0
    :foreach x in=$xs do={ :set sum ($sum + $x) }
    :return $sum
}
:fn shout mut words:List<String> do={
    :foreach mut w in=$words do={ :set w ($w . \"!\") }
}
";

/// A statement on collection variables: `$l0`/`$l1` (`List<String>`), `$n`
/// (`List<i64>`), `$m` (`Map<String, i64>`), `$s` (`Set<String>`) and `$g`
/// (`Map<String, List<String>>`). Indices are checked before use, so no statement panics.
fn collection_statement() -> impl Strategy<Value = String> {
    let list = || prop::sample::select(vec!["l0", "l1"]);
    let word =
        || prop::sample::select(vec!["a", "bb", "héllo", "", "a"]).prop_map(|w| format!("\"{w}\""));
    prop_oneof![
        (list(), word()).prop_map(|(l, w)| format!("${l}->push {w}")),
        list().prop_map(|l| format!(":put \"$[${l}->pop] ${l}\"")),
        (list(), 0i64..4, word()).prop_map(|(l, i, w)| format!(
            ":if ({i} <= [:len ${l}]) do={{ ${l}->insert {i} {w} }}"
        )),
        (list(), 0i64..4)
            .prop_map(|(l, i)| format!(":if ({i} < [:len ${l}]) do={{ :put [${l}->remove {i}] }}")),
        (list(), 0i64..4, word()).prop_map(|(l, i, w)| format!(
            ":if ({i} < [:len ${l}]) do={{ :set (${l}->{i}) ({w} . ${l}->{i}) }}"
        )),
        (list(), -1i64..4).prop_map(|(l, i)| format!(":put [${l}->get {i}]")),
        (list(), list()).prop_map(|(a, b)| format!(":set {a} (${a} . ${b})")),
        (list(), list()).prop_map(|(a, b)| format!(":set {a} [${b}->clone]")),
        (list(), list()).prop_map(|(a, b)| format!(
            ":put \"$(${a} = ${b}) $(${a} < ${b}) $[${a}->contains \"a\"] $(\"bb\" in ${b})\""
        )),
        list().prop_map(|l| format!(":shout ${l}")),
        list().prop_map(|l| format!(":foreach i,w in=${l} do={{ :put \"$i=$w\" }}")),
        list().prop_map(|l| format!(":if ([:len ${l}] > 3) do={{ ${l}->clear }}")),
        (-50i64..50).prop_map(|v| format!("$n->push {v}")),
        Just(":foreach mut x in=$n do={ :set x ($x * 3) }".to_owned()),
        Just(":put \"$n $[:total $n] $[$n->pop] $n\"".to_owned()),
        (word(), -9i64..9).prop_map(|(k, v)| format!(":set ($m->{k}) {v}")),
        (word(), -9i64..9).prop_map(|(k, v)| format!(":put [$m->insert {k} {v}]")),
        word().prop_map(|k| format!(":put \"$[$m->remove {k}] $[$m->get {k}] $({k} in $m) $m\"")),
        Just(":put \"$[$m->keys] $[$m->values] $[$m->len]\"".to_owned()),
        Just(":foreach mut k,v in=$m do={ :set v ($v + 1) }".to_owned()),
        word().prop_map(|k| format!(":if ({k} in $m) do={{ :put ($m->{k}) }}")),
        word().prop_map(|w| format!(":put \"$[$s->insert {w}] $s\"")),
        word().prop_map(|w| format!(":put \"$[$s->remove {w}] $s\"")),
        (word(), list()).prop_map(|(k, l)| format!(":set ($g->{k}) [${l}->clone]")),
        (word(), word()).prop_map(|(k, w)| format!(":if ({k} in $g) do={{ $g->{k}->push {w} }}")),
        Just(":put \"$g $[$g->len]\"".to_owned()),
        Just(":set g [$g->clone]".to_owned()),
    ]
}

/// A program applying random collection statements, then printing every variable.
fn collection_program() -> impl Strategy<Value = String> {
    prop::collection::vec(collection_statement(), 1..25).prop_map(|statements| {
        let mut source = String::from(COLLECTION_PRELUDE);
        source.push_str(
            ":fn main do={\n    :local l0 {\"x\"}\n    :local l1:List<String> {}\n    :local n:List<i64> {}\n    :local m:Map<String, i64> {}\n    :local s:Set<String> {}\n    :local g:Map<String, List<String>> {}\n",
        );
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        source.push_str("    :put \"$l0 $l1 $n $m $s $g\"\n}\n");
        source
    })
}

/// Declarations of generic programs: generic structs, an enum, functions and a trait, used
/// with several type arguments each, including other instances.
const GENERIC_PRELUDE: &str = "\
:trait Measured impl=Display {
    :fn size self -> i64
    :fn unit -> String
    :fn describe self -> String do={ :return \"$self has $([$self->size]) $([Self->unit])\" }
}
:fn measure<M: Measured + Clone> m:M -> String do={
    :const copy [$m->clone]
    :return \"$([$copy->describe]) / $([M->unit])\"
}
:struct Stack<T: Clone> impl=Clone,Eq,Display,Measured {
    items:List<T>
    :fn unit -> String do={ :return \"items\" }
    :fn push mut self owned x:T do={ $self->items->push $x }
    :fn pop mut self -> T? do={ :return [$self->items->pop] }
    :fn peek self -> T? do={ :return [$self->items->get ([:len $self->items] - 1)] }
    :fn size self -> i64 do={ :return [:len $self->items] }
}
:struct Pair<A, B> impl=Clone,Eq,Ord,Display,Measured {
    first:A
    second:B
    :fn size self -> i64 do={ :return 2 }
    :fn unit -> String do={ :return \"parts\" }
    :fn describe self -> String do={ :return \"a pair\" }
}
:enum Either<L, R> impl=Clone,Eq,Display {
    left value:L
    right value:R
}
:fn pick<T: Clone> c:bool a:T b:T -> T do={
    :if ($c) do={ :return [$a->clone] }
    :return [$b->clone]
}
:fn flip<A: Clone, B: Clone> p:Pair<A, B> -> Pair<B, A> do={
    :return Pair{first=[$p->second->clone]; second=[$p->first->clone]}
}
:fn side<L: Display, R: Display> e:Either<L, R> -> String do={
    :match $e {
        Either->left v do={ :return \"L$v\" }
        Either->right v do={ :return \"R$v\" }
    }
}
:fn repeat<T: Clone> x:T n:i64 -> List<T> do={
    :local out:List<T> {}
    :for i from=1 to=$n do={ $out->push [$x->clone] }
    :return $out
}
:fn bigger<T: Ord + Clone> a:T b:T -> T do={
    :if ($a > $b) do={ :return [$a->clone] }
    :return [$b->clone]
}
";

/// A statement on generic variables: `$st` (`Stack<String>`), `$sn` (`Stack<i16>`), `$sp`
/// (`Stack<Pair<i64, String>>`), `$p` (`Pair<i8, String>`), `$q` (`Pair<String, i8>`) and
/// `$e` (`Either<i64, String>`).
fn generic_statement() -> impl Strategy<Value = String> {
    let word =
        || prop::sample::select(vec!["a", "bb", "héllo", "", "z"]).prop_map(|w| format!("\"{w}\""));
    let small = || -100i64..100;
    prop_oneof![
        word().prop_map(|w| format!("$st->push {w}")),
        Just(":put \"$[$st->pop] $[$st->peek] $[$st->size]\"".to_owned()),
        small().prop_map(|v| format!("$sn->push {v}")),
        Just(":put \"$[$sn->pop] $[$sn->peek] $sn\"".to_owned()),
        (small(), word()).prop_map(|(v, w)| format!("$sp->push Pair{{first={v}; second={w}}}")),
        Just(":put \"$[$sp->pop] $sp\"".to_owned()),
        (small(), word()).prop_map(|(v, w)| format!(":set p Pair{{first={v}; second={w}}}")),
        Just(":set q [:flip $p]".to_owned()),
        Just(":set p [:flip $q]".to_owned()),
        Just(":put \"$[:bigger $p [:flip $q]] $($p = [:flip $q]) $($p < [:flip $q])\"".to_owned()),
        (word(), word()).prop_map(|(a, b)| format!(":put [:bigger {a} {b}]")),
        (small(), small()).prop_map(|(a, b)| format!(":put [:bigger {a} {b}]")),
        small().prop_map(|v| format!(":set e [Either->left {v}]")),
        word().prop_map(|w| format!(":set e [Either->right {w}]")),
        Just(":put \"$[:side $e] $e\"".to_owned()),
        Just(":put [:side [Either<Pair<i8, String>, f32>->left [$p->clone]]]".to_owned()),
        (word(), 0i64..3).prop_map(|(w, n)| format!(":put [:repeat {w} {n}]")),
        (0i64..3).prop_map(|n| format!(":put [:repeat $p {n}]")),
        any::<bool>()
            .prop_map(|c| format!(":set st [:pick {c} $st Stack<String>{{items={{\"n\"}}}}]")),
        Just(":put \"$($st = [$st->clone]) $($sn = $sn)\"".to_owned()),
        Just(":set sp [$sp->clone]".to_owned()),
        Just(":put \"$[:measure $st] $[:measure $sn] $[:measure $p] $[$sp->describe]\"".to_owned()),
    ]
}

/// A program applying random generic statements, then printing every variable.
fn generic_program() -> impl Strategy<Value = String> {
    prop::collection::vec(generic_statement(), 1..25).prop_map(|statements| {
        let mut source = String::from(GENERIC_PRELUDE);
        source.push_str(
            ":fn main do={\n    :local st Stack<String>{items={}}\n    :local sn Stack<i16>{items={}}\n    :local sp Stack<Pair<i64, String>>{items={}}\n    :local p Pair<i8, String>{first=1; second=\"a\"}\n    :local q [:flip $p]\n    :local e:Either<i64, String> [Either->left 0]\n",
        );
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        source.push_str("    :put \"$st $sn $sp $p $q $e\"\n}\n");
        source
    })
}

/// Declarations of destruction programs: values whose `drop` functions print, inside
/// structs, enums, options, boxes, lists and maps.
const DROP_PRELUDE: &str = "\
:struct T impl=Drop,Clone,Eq,Hash,Display {
    id:i64
    :fn drop mut self do={ :put \"~$($self->id)\" }
}
:struct Holder impl=Drop,Clone,Display {
    inner:T
    tag:String
    :fn drop mut self do={ :put \"~holder $($self->tag)\" }
}
:enum Slot impl=Clone,Display {
    empty
    full item:T
}
:fn make id:i64 -> T do={ :return T{id=$id} }
:fn pass owned t:T -> T do={ :return $t }
:fn consume owned t:T do={ :put \"consumed $t\" }
";

/// A statement on variables `$a`, `$b` (`T`), `$l` (`List<T>`), `$m` (`Map<String, T>`), `$k`
/// (`Set<T>`), `$n` (`Map<T, i64>`), `$o` (`T?`), `$h` (`Holder`), `$s` (`Slot`) and `$x` (`Box<T>`). Every
/// statement leaves every variable assigned.
fn drop_statement() -> impl Strategy<Value = String> {
    let id = || 0i64..50;
    let key = || prop::sample::select(vec!["p", "q"]).prop_map(|k| format!("\"{k}\""));
    prop_oneof![
        id().prop_map(|n| format!(":set a [:make {n}]")),
        Just(":set b [$a->clone]".to_owned()),
        id().prop_map(|n| format!("$l->push [:make {n}]")),
        Just(":put [$l->pop]".to_owned()),
        (0i64..3).prop_map(|i| format!(":if ([:len $l] > {i}) do={{ :put [$l->remove {i}] }}")),
        Just(":if ([:len $l] > 2) do={ $l->clear }".to_owned()),
        (id(), id()).prop_map(|(a, b)| format!(":set l {{[:make {a}]; [:make {b}]}}")),
        (key(), id()).prop_map(|(k, n)| format!(":put [$m->insert {k} [:make {n}]]")),
        key().prop_map(|k| format!(":put [$m->remove {k}]")),
        (key(), id()).prop_map(|(k, n)| format!(":set ($m->{k}) [:make {n}]")),
        (key(), id(), id())
            .prop_map(|(k, a, b)| format!(":put {{{k}=[:make {a}]; {k}=[:make {b}]}}")),
        id().prop_map(|n| format!(":put [$k->insert [:make {n}]]")),
        id().prop_map(|n| format!(":put [$k->remove [:make {n}]]")),
        id().prop_map(|n| format!(":put \"$[$k->contains [:make {n}]] $([:make {n}] in $k)\"")),
        (id(), -9i64..9).prop_map(|(n, v)| format!(":put [$n->insert [:make {n}] {v}]")),
        id().prop_map(|n| format!(":put [$n->remove [:make {n}]]")),
        id().prop_map(|n| format!(":put \"$[$n->get [:make {n}]] $([:make {n}] in $n) $n\"")),
        id().prop_map(|n| format!(":set o [some [:make {n}]]")),
        Just(":put [$o->take]".to_owned()),
        Just(":set o none".to_owned()),
        id().prop_map(|n| format!(":set h Holder{{inner=[:make {n}]; tag=\"t{n}\"}}")),
        Just(":put [$h->clone]".to_owned()),
        id().prop_map(|n| format!(":set s [Slot->full [:make {n}]]")),
        Just(":set s Slot->empty".to_owned()),
        id().prop_map(|n| format!(":consume [:make {n}]")),
        Just(":set a [:pass [$b->clone]]".to_owned()),
        (id(), id()).prop_map(|(a, b)| format!(":put [:len {{[:make {a}]; [:make {b}]}}]")),
        id().prop_map(|n| format!(":set x [Box->new [:make {n}]]")),
        Just(":put \"$a $b $l $m $k $o $h $s $($x->value)\"".to_owned()),
    ]
}

/// A program applying random destruction statements; everything left is destroyed at the
/// end of `main`.
fn drop_program() -> impl Strategy<Value = String> {
    prop::collection::vec(drop_statement(), 1..25).prop_map(|statements| {
        let mut source = String::from(DROP_PRELUDE);
        source.push_str(
            ":fn main do={\n    :local a [:make 1]\n    :local b [:make 2]\n    :local l {[:make 3]}\n    :local m {\"p\"=[:make 4]}\n    :local k:Set<T> {}\n    :local n:Map<T, i64> {}\n    :local o:T? none\n    :local h Holder{inner=[:make 5]; tag=\"h\"}\n    :local s Slot->empty\n    :local x [Box->new [:make 6]]\n",
        );
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        source.push_str("    :put \"end\"\n}\n");
        source
    })
}

/// Declarations of error programs: functions that raise errors for some inputs, with values
/// whose `drop` prints destroyed on the way out, and functions that catch or wrap errors.
const ERROR_PRELUDE: &str = "\
:struct T impl=Drop {
    id:i64
    :fn drop mut self do={ :put \"~$($self->id)\" }
}
:fn check n:i64 -> i64 raises do={
    :const t T{id=$n}
    :if (($n % 3) = 0) do={ :error \"bad $n\" }
    :return ($n * 2)
}
:fn twice n:i64 -> i64 raises do={
    :const a [:check? $n]
    :const b [:check? ($n + 1)]
    :return ($a + $b)
}
:fn collect n:i64 -> List<i64> raises do={
    :local out:List<i64> {}
    :for i from=1 to=$n do={
        :const t T{id=(100 + $i)}
        $out->push [:check? $i]
    }
    :return $out
}
:fn safe n:i64 -> i64 do={
    :onerror e in={ :return [:twice? $n] } do={
        :put \"safe: $e\"
        :return -1
    }
}
:fn wrap n:i64 -> i64 raises do={
    :onerror e in={ :return [:check? $n] } do={ :error \"wrapped\" source=$e }
}
";

/// A statement of an error program; any error it raises is caught.
fn error_statement() -> impl Strategy<Value = String> {
    let n = || 1i64..8;
    prop_oneof![
        n().prop_map(|v| format!(":put [:safe {v}]")),
        n().prop_map(|v| format!(":onerror e in={{ :put [:twice? {v}] }} do={{ :put \"caught $e\" }}")),
        n().prop_map(|v| format!(":onerror e in={{ :put [:collect? {v}] }} do={{ :put \"caught $e\" }}")),
        (n(), n()).prop_map(|(a, b)| format!(
            ":onerror e in={{ :const t T{{id=5{a}}}; :put \"$[:check? {a}] $[:check? {b}]\" }} do={{ :put \"caught $e\" }}"
        )),
        n().prop_map(|v| format!(
            ":onerror e in={{ :put [:wrap? {v}] }} do={{ :put \"caught $e\"; :match $e->source {{ some c do={{ :put \"cause $($c->value)\" }}; none do={{}} }} }}"
        )),
        n().prop_map(|v| format!(
            ":for i from=1 to={v} do={{ :const t T{{id=(200 + $i)}}; :onerror e in={{ :put [:check? $i] }} do={{ :put \"skip $e\"; :continue }} }}"
        )),
        n().prop_map(|v| format!(
            ":onerror o in={{ :onerror i in={{ :put [:check? {v}] }} do={{ :error \"inner $i\" }} }} do={{ :put \"outer $o\" }}"
        )),
    ]
}

/// A program of error statements; `main` may end with an error of its own, which it raises.
fn error_program() -> impl Strategy<Value = String> {
    (prop::collection::vec(error_statement(), 1..12), 1i64..8).prop_map(|(statements, last)| {
        let mut source = String::from(ERROR_PRELUDE);
        source.push_str(":fn main raises do={\n    :const kept T{id=999}\n");
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        writeln!(source, "    :put [:check? {last}]\n}}").expect("writing to a String");
        source
    })
}

/// Declarations of function value programs: closures capturing values whose `drop` prints,
/// functions that take, return and compose function values, and one that raises.
const CLOSURE_PRELUDE: &str = "\
:struct T impl=Drop,Clone {
    id:i64
    :fn drop mut self do={ :put \"~$($self->id)\" }
}
:fn adder n:i64 -> fn(i64) -> i64 do={
    :const t T{id=$n}
    :return [:fn x:i64 -> i64 do={ :return ($x + $t->id) }]
}
:fn apply f:fn(i64) -> i64 x:i64 -> i64 do={ :return [$f $x] }
:fn compose f:fn(i64) -> i64 g:fn(i64) -> i64 -> fn(i64) -> i64 do={
    :const f2 [$f->clone]
    :const g2 [$g->clone]
    :return [:fn x:i64 -> i64 do={ :return [$g2 [$f2 $x]] }]
}
:fn square x:i64 -> i64 do={ :return (($x % 1000) * ($x % 1000)) }
:fn limited n:i64 -> fn(i64) -> i64 raises do={
    :const t T{id=(100 + $n)}
    :return [:fn x:i64 -> i64 raises do={
        :if ($x > $t->id) do={ :error \"$x over $($t->id)\" }
        :return $x
    }]
}
";

/// A statement on variables `$f` and `$g` (`fn(i64) -> i64`), `$fs` (a list of them) and
/// `$r` (`fn(i64) -> i64 raises`).
fn closure_statement() -> impl Strategy<Value = String> {
    let n = || 1i64..20;
    prop_oneof![
        n().prop_map(|v| format!(":set f [:adder {v}]")),
        Just(":set g [$f->clone]".to_owned()),
        Just(":set g $square".to_owned()),
        Just(":set f [:compose $f $g]".to_owned()),
        n().prop_map(|v| format!(":put \"$[$f {v}] $[$g {v}] $[:apply $f {v}]\"")),
        Just("$fs->push [$f->clone]".to_owned()),
        n().prop_map(|v| format!("$fs->push [:adder {v}]")),
        Just(":if ([:len $fs] > 0) do={ :const h [$fs->pop]; :put \"popped\" }".to_owned()),
        n().prop_map(|v| format!(":foreach h in=$fs do={{ :put [$h {v}] }}")),
        Just(":if ([:len $fs] > 2) do={ $fs->clear }".to_owned()),
        n().prop_map(|v| format!(":set r [:limited {v}]")),
        (n(), 90i64..130).prop_map(|(_, x)| format!(
            ":onerror e in={{ :put [$r? {x}] }} do={{ :put \"caught $e\" }}"
        )),
        n().prop_map(|v| format!(
            ":const k T{{id=(200 + {v})}}; :set g [:fn x:i64 -> i64 do={{ :return (($x % 1000) * $k->id) }}]"
        )),
    ]
}

/// A program of function value statements; everything left is destroyed at the end.
fn closure_program() -> impl Strategy<Value = String> {
    prop::collection::vec(closure_statement(), 1..20).prop_map(|statements| {
        let mut source = String::from(CLOSURE_PRELUDE);
        source.push_str(
            ":fn main do={\n    :local f [:adder 1]\n    :local g $square\n    :local fs:List<fn(i64) -> i64> {}\n    :local r [:limited 0]\n",
        );
        for (index, statement) in statements.iter().enumerate() {
            writeln!(source, "    {{ # {index}\n    {statement}\n    }}")
                .expect("writing to a String");
        }
        source.push_str("    :put \"end\"\n}\n");
        source
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn closure_programs_match_interpreter(source in closure_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("] Error"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn error_programs_match_interpreter(source in error_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("] Error"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        // `main` ends normally, or with the error of its last statement.
        prop_assert!(
            matches!(native.0, Some(0 | 1)),
            "program failed:\n{}\n{:?}", source, native
        );
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn drop_programs_match_interpreter(source in drop_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn generic_programs_match_interpreter(source in generic_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn collection_programs_match_interpreter(source in collection_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn enum_programs_match_interpreter(source in enum_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn struct_programs_match_interpreter(source in struct_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn string_programs_match_interpreter(source in string_program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        prop_assert_eq!(native.0, Some(0), "program failed:\n{}\n{:?}", source, native);
        prop_assert_eq!(native, interpreted, "program:\n{}", source);
    }

    #[test]
    fn native_code_matches_interpreter(source in program()) {
        let path = scratch_file(&source);
        let native = run(&path, false);
        let interpreted = run(&path, true);
        std::fs::remove_file(&path).ok();
        // Programs that do not pass `pika check` would be generator bugs.
        prop_assert!(
            !native.2.contains("Error:"),
            "generated program does not check:\n{source}\n{}", native.2
        );
        let mut report = String::new();
        writeln!(report, "program:\n{source}").expect("writing to a String");
        writeln!(report, "native: {native:?}\ninterpreted: {interpreted:?}").expect("writing to a String");
        prop_assert_eq!(native, interpreted, "{}", report);
    }
}
