//! Ownership analysis and drop elaboration.
//!
//! Three forward dataflow analyses run over each body, on its local slots that own their
//! values:
//!
//! - *definitely initialized* (meet: intersection): a read of a local that is not definitely
//!   initialized is an error, either "read before assigned" or "used after move";
//! - *maybe initialized* (meet: union): a drop of a local that is not even maybe initialized
//!   is removed;
//! - *maybe moved* (meet: union): decides which of the two errors to report.
//!
//! A drop of a local that is maybe but not definitely initialized becomes conditional on a
//! drop flag, a `bool` local that the code keeps up to date on every assignment and move.
//!
//! Only whole locals are tracked: a field cannot be moved out of its struct on its own, so
//! a field is initialized exactly when its struct is. Reading, assigning or dropping a field
//! uses the struct. The exceptions are compiler-generated: a `:match` on a temporary moves
//! the bound parts out of it, destroys the rest piece by piece, and then marks the whole
//! temporary as moved ([`Statement::MarkMoved`]); moving out of a part of a place is a use of
//! the place, and the place is still initialized afterwards as far as the analysis knows.

use la_arena::ArenaMap;
use pika_diagnostics::{Diagnostic, Span};
use pika_types::Ty;

use crate::{
    BlockId, Body, CallArg, LocalDecl, LocalId, LocalMode, Operand, Place, PlaceRoot, PrintPart,
    Rvalue, Statement, Terminator, Value, codes,
};

/// Checks that values are assigned before use and not used after being moved, and makes the
/// drops of `body` exact. Returns the errors found.
pub fn analyze(body: &mut Body) -> Vec<Diagnostic> {
    let tracked = Tracked::new(body);
    if tracked.count == 0 {
        return Vec::new();
    }
    let states = solve(body, &tracked);
    let diagnostics = report_uses(body, &tracked, &states);
    elaborate_drops(body, &tracked, &states);
    diagnostics
}

/// The locals the analysis follows: those that own their value.
struct Tracked {
    index: ArenaMap<LocalId, usize>,
    count: usize,
}

impl Tracked {
    fn new(body: &Body) -> Self {
        let mut index = ArenaMap::default();
        let mut count = 0;
        for (local, decl) in body.locals.iter() {
            if decl.mode == LocalMode::Value && local != body.return_local {
                index.insert(local, count);
                count += 1;
            }
        }
        Self { index, count }
    }

    /// The index of a place that is a whole tracked local.
    fn get(&self, place: &Place) -> Option<usize> {
        place
            .as_local()
            .and_then(|local| self.index.get(local).copied())
    }

    /// The tracked local at the root of a place, with its index.
    fn root(&self, place: &Place) -> Option<(LocalId, usize)> {
        match place.root() {
            PlaceRoot::Local(local) => self.index.get(local).map(|&index| (local, index)),
            PlaceRoot::Global(_) | PlaceRoot::Capture(_) => None,
        }
    }
}

/// A set of tracked locals.
#[derive(Clone, PartialEq, Eq)]
struct Bits(Vec<u64>);

impl Bits {
    fn empty(count: usize) -> Self {
        Self(vec![0; count.div_ceil(64)])
    }

    fn full(count: usize) -> Self {
        let mut bits = Self(vec![u64::MAX; count.div_ceil(64)]);
        if !count.is_multiple_of(64)
            && let Some(last) = bits.0.last_mut()
        {
            *last = (1u64 << (count % 64)) - 1;
        }
        bits
    }

    fn contains(&self, index: usize) -> bool {
        self.0[index / 64] >> (index % 64) & 1 == 1
    }

    fn insert(&mut self, index: usize) {
        self.0[index / 64] |= 1 << (index % 64);
    }

    fn remove(&mut self, index: usize) {
        self.0[index / 64] &= !(1 << (index % 64));
    }

    fn intersect(&mut self, other: &Self) {
        for (word, other) in self.0.iter_mut().zip(&other.0) {
            *word &= other;
        }
    }

    fn union(&mut self, other: &Self) {
        for (word, other) in self.0.iter_mut().zip(&other.0) {
            *word |= other;
        }
    }
}

/// The state of the analyses at one point.
#[derive(Clone, PartialEq, Eq)]
struct State {
    definitely_init: Bits,
    maybe_init: Bits,
    maybe_moved: Bits,
}

impl State {
    fn assign(&mut self, index: usize) {
        self.definitely_init.insert(index);
        self.maybe_init.insert(index);
        self.maybe_moved.remove(index);
    }

    fn moved(&mut self, index: usize) {
        self.definitely_init.remove(index);
        self.maybe_init.remove(index);
        self.maybe_moved.insert(index);
    }

    fn dropped(&mut self, index: usize) {
        self.definitely_init.remove(index);
        self.maybe_init.remove(index);
    }

    fn join(&mut self, other: &Self) {
        self.definitely_init.intersect(&other.definitely_init);
        self.maybe_init.union(&other.maybe_init);
        self.maybe_moved.union(&other.maybe_moved);
    }
}

/// The operands a statement reads, in evaluation order.
fn statement_operands(statement: &Statement) -> Vec<&Operand> {
    match statement {
        Statement::Assign { value, .. } => rvalue_operands(value),
        Statement::Print { parts, .. } | Statement::Append { parts, .. } => print_operands(parts),
        Statement::ListPush { value, .. } => vec![value],
        Statement::ListInsert { index, value, .. } => vec![index, value],
        Statement::ListSwap { first, second, .. } => vec![first, second],
        Statement::Drop { .. }
        | Statement::BindRef { .. }
        | Statement::MarkMoved(_)
        | Statement::Clear(_) => Vec::new(),
    }
}

fn rvalue_operands(value: &Rvalue) -> Vec<&Operand> {
    match value {
        Rvalue::Use(operand)
        | Rvalue::Unary { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::StringLen(operand)
        | Rvalue::BoxNew(operand)
        | Rvalue::Unbox(operand)
        | Rvalue::MapFind { key: operand, .. }
        | Rvalue::ListRemove { index: operand, .. }
        | Rvalue::MapRemove { key: operand, .. }
        | Rvalue::Contains { value: operand, .. } => vec![operand],
        Rvalue::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        Rvalue::Interpolate(parts) => print_operands(parts),
        Rvalue::Struct { fields, .. }
        | Rvalue::Variant { fields, .. }
        | Rvalue::List {
            elements: fields, ..
        } => fields.iter().collect(),
        Rvalue::Map { entries, .. } => entries.iter().flat_map(|(k, v)| [k, v]).collect(),
        Rvalue::MapInsert { key, value, .. } => vec![key, value],
        Rvalue::Closure { captures, .. } => captures.iter().collect(),
        Rvalue::Intrinsic { args, .. } => args.iter().collect(),
        Rvalue::Clone(_)
        | Rvalue::Discriminant(_)
        | Rvalue::Len(_)
        | Rvalue::ListPop(_)
        | Rvalue::MapKeys(_)
        | Rvalue::MapValues(_) => Vec::new(),
    }
}

/// The places an rvalue reads or changes without an operand: they must hold a value.
fn rvalue_places(value: &Rvalue) -> Vec<&Place> {
    match value {
        Rvalue::Clone(place)
        | Rvalue::Discriminant(place)
        | Rvalue::Len(place)
        | Rvalue::ListPop(place)
        | Rvalue::MapKeys(place)
        | Rvalue::MapValues(place)
        | Rvalue::MapFind { map: place, .. }
        | Rvalue::ListRemove { list: place, .. }
        | Rvalue::MapInsert { map: place, .. }
        | Rvalue::MapRemove { map: place, .. }
        | Rvalue::Contains {
            collection: place, ..
        } => vec![place],
        _ => Vec::new(),
    }
}

fn print_operands(parts: &[PrintPart]) -> Vec<&Operand> {
    parts
        .iter()
        .filter_map(|part| match part {
            PrintPart::Value(operand) => Some(operand),
            PrintPart::Text(_) => None,
        })
        .collect()
}

/// A place read, moved or borrowed, with where in the source.
struct Use {
    place: Place,
    span: Span,
    moves: bool,
}

fn operand_use(operand: &Operand) -> Option<Use> {
    match operand {
        Operand::Copy { place, span } => Some(Use {
            place: place.clone(),
            span: *span,
            moves: false,
        }),
        // Moving out of a part of a place uses the place (see the module documentation).
        Operand::Move { place, span } => Some(Use {
            place: place.clone(),
            span: *span,
            moves: place.as_local().is_some(),
        }),
        Operand::Const(_) => None,
    }
}

/// The uses of a statement, in evaluation order.
fn statement_uses(statement: &Statement) -> Vec<Use> {
    let mut uses: Vec<Use> = statement_operands(statement)
        .into_iter()
        .filter_map(operand_use)
        .collect();
    let use_of = |place: &Place, span: Span| Use {
        place: place.clone(),
        span,
        moves: false,
    };
    match statement {
        Statement::BindRef { place, .. }
        | Statement::Clear(place)
        | Statement::Append { target: place, .. }
        | Statement::ListPush { list: place, .. }
        | Statement::ListInsert { list: place, .. }
        | Statement::ListSwap { list: place, .. } => {
            uses.push(use_of(place, Span::empty(0)));
        }
        Statement::Assign { place, value, span } => {
            for source in rvalue_places(value) {
                uses.push(use_of(source, *span));
            }
            // Assigning a part of a variable modifies the variable.
            if place.as_local().is_none() {
                uses.push(use_of(place, *span));
            }
        }
        Statement::Drop { .. } | Statement::MarkMoved(_) | Statement::Print { .. } => {}
    }
    uses
}

fn terminator_uses(terminator: &Terminator) -> Vec<Use> {
    match terminator {
        Terminator::If { cond, .. } => operand_use(cond).into_iter().collect(),
        Terminator::Call { args, .. } => args
            .iter()
            .filter_map(|arg| match arg {
                CallArg::Value(operand) => operand_use(operand),
                CallArg::Ref { place, span, .. } => Some(Use {
                    place: place.clone(),
                    span: *span,
                    moves: false,
                }),
            })
            .collect(),
        Terminator::Panic { message, .. } => print_operands(message)
            .into_iter()
            .filter_map(operand_use)
            .collect(),
        Terminator::Raise(error) => operand_use(error).into_iter().collect(),
        Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => Vec::new(),
    }
}

/// Applies a statement to the state.
fn apply_statement(state: &mut State, tracked: &Tracked, statement: &Statement) {
    for used in statement_uses(statement) {
        if used.moves
            && let Some(index) = tracked.get(&used.place)
        {
            state.moved(index);
        }
    }
    match statement {
        Statement::Assign { place, .. } => {
            if let Some(index) = tracked.get(place) {
                state.assign(index);
            }
        }
        Statement::Drop { place, .. } => {
            if let Some(index) = tracked.get(place) {
                state.dropped(index);
            }
        }
        Statement::MarkMoved(local) => {
            if let Some(index) = tracked.get(&Place::Local(*local)) {
                state.moved(index);
            }
        }
        Statement::Print { .. }
        | Statement::Append { .. }
        | Statement::BindRef { .. }
        | Statement::ListPush { .. }
        | Statement::ListInsert { .. }
        | Statement::ListSwap { .. }
        | Statement::Clear(_) => {}
    }
}

/// Applies a terminator's effects before control leaves the block.
fn apply_terminator(state: &mut State, tracked: &Tracked, terminator: &Terminator) {
    for used in terminator_uses(terminator) {
        if used.moves
            && let Some(index) = tracked.get(&used.place)
        {
            state.moved(index);
        }
    }
}

/// Applies what holds on the edge of `terminator` to `successor`: the result of a call is
/// assigned on the way to its target, and its error on the way to its error target.
fn apply_edge(state: &mut State, tracked: &Tracked, terminator: &Terminator, successor: BlockId) {
    let Terminator::Call {
        destination,
        target,
        on_error,
        ..
    } = terminator
    else {
        return;
    };
    let assigned = if *target == Some(successor) {
        Some(destination)
    } else {
        on_error
            .as_ref()
            .filter(|e| e.block == successor)
            .map(|e| &e.place)
    };
    if let Some(index) = assigned.and_then(|place| tracked.get(place)) {
        state.assign(index);
    }
}

/// The state at the entry of every block.
fn solve(body: &Body, tracked: &Tracked) -> ArenaMap<BlockId, State> {
    let count = tracked.count;
    // Blocks not reached yet start at the neutral element of each meet.
    let unreached = State {
        definitely_init: Bits::full(count),
        maybe_init: Bits::empty(count),
        maybe_moved: Bits::empty(count),
    };
    let mut states: ArenaMap<BlockId, State> = body
        .blocks
        .iter()
        .map(|(id, _)| (id, unreached.clone()))
        .collect();
    let mut entry = State {
        definitely_init: Bits::empty(count),
        maybe_init: Bits::empty(count),
        maybe_moved: Bits::empty(count),
    };
    for &param in &body.params {
        if let Some(index) = tracked.get(&Place::Local(param)) {
            entry.assign(index);
        }
    }
    states.insert(body.entry, entry);

    let mut changed = true;
    while changed {
        changed = false;
        for (id, block) in body.blocks.iter() {
            let mut state = states[id].clone();
            for statement in &block.statements {
                apply_statement(&mut state, tracked, statement);
            }
            apply_terminator(&mut state, tracked, &block.terminator);
            for successor in block.terminator.successors() {
                let mut edge = state.clone();
                apply_edge(&mut edge, tracked, &block.terminator, successor);
                let mut next = states[successor].clone();
                next.join(&edge);
                if next != states[successor] {
                    states.insert(successor, next);
                    changed = true;
                }
            }
        }
    }
    states
}

/// Reports reads of locals that may be unassigned or moved, once per variable.
fn report_uses(
    body: &Body,
    tracked: &Tracked,
    states: &ArenaMap<BlockId, State>,
) -> Vec<Diagnostic> {
    // Every move of each local, to point at in the error.
    let mut moves: ArenaMap<LocalId, Vec<Span>> = ArenaMap::default();
    for (_, block) in body.blocks.iter() {
        let uses = block
            .statements
            .iter()
            .flat_map(statement_uses)
            .chain(terminator_uses(&block.terminator));
        for used in uses.filter(|u| u.moves) {
            if let Place::Local(local) = used.place {
                moves.entry(local).or_default().push(used.span);
            }
        }
    }

    let mut reported: ArenaMap<LocalId, ()> = ArenaMap::default();
    let mut diagnostics = Vec::new();
    let mut check = |state: &State, used: &Use, diagnostics: &mut Vec<Diagnostic>| {
        let Some((local, index)) = tracked.root(&used.place) else {
            return;
        };
        if state.definitely_init.contains(index) || reported.get(local).is_some() {
            return;
        }
        reported.insert(local, ());
        let decl = &body.locals[local];
        let Some(user) = &decl.user else {
            debug_assert!(false, "a temporary is read before it is assigned: {decl:?}");
            return;
        };
        if state.maybe_moved.contains(index) {
            let move_span = moves.get(local).and_then(|spans| {
                spans
                    .iter()
                    .copied()
                    .filter(|span| span.start < used.span.start)
                    .max_by_key(|span| span.start)
                    .or_else(|| spans.first().copied())
            });
            let diagnostic = if move_span == Some(used.span) {
                // The only move is this use itself, reached again by a loop.
                Diagnostic::error(
                    codes::USE_AFTER_MOVE,
                    format!("`{}` is moved here in an earlier iteration of the loop", user.name),
                    used.span,
                )
                .with_help(format!(
                    "move a copy (`[${}->clone]`), or assign `{}` a new value before the next iteration",
                    user.name, user.name
                ))
            } else {
                let mut diagnostic = Diagnostic::error(
                    codes::USE_AFTER_MOVE,
                    format!("`{}` is used after its value was moved", user.name),
                    used.span,
                )
                .with_help(format!(
                    "copy the value before moving it: `[${}->clone]`",
                    user.name
                ));
                if let Some(move_span) = move_span {
                    diagnostic = diagnostic.with_secondary(move_span, "the value is moved here");
                }
                diagnostic
            };
            diagnostics.push(diagnostic);
        } else {
            diagnostics.push(
                Diagnostic::error(
                    codes::USE_BEFORE_ASSIGNMENT,
                    format!("`{}` is read before it is assigned", user.name),
                    used.span,
                )
                .with_secondary(user.span, "declared here without a value")
                .with_help(format!(
                    "assign it with `:set {} ...` on every path before this",
                    user.name
                )),
            );
        }
    };

    // The uses of one statement or terminator happen in order: an operand moved earlier in
    // it, as in `{$x; $x}`, cannot be used by a later one.
    let mut check_all = |state: &State, uses: Vec<Use>, diagnostics: &mut Vec<Diagnostic>| {
        let mut state = state.clone();
        for used in uses {
            check(&state, &used, diagnostics);
            if used.moves
                && let Some(index) = tracked.get(&used.place)
            {
                state.moved(index);
            }
        }
    };
    for (id, block) in body.blocks.iter() {
        let mut state = states[id].clone();
        for statement in &block.statements {
            check_all(&state, statement_uses(statement), &mut diagnostics);
            apply_statement(&mut state, tracked, statement);
        }
        check_all(&state, terminator_uses(&block.terminator), &mut diagnostics);
    }
    diagnostics
}

/// What to do with one drop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DropKind {
    Always,
    Never,
    Flagged,
}

/// Decides what to do with every drop of `body`, in order within each block, and which locals
/// need a drop flag.
fn classify_drops(
    body: &Body,
    tracked: &Tracked,
    states: &ArenaMap<BlockId, State>,
) -> (ArenaMap<BlockId, Vec<DropKind>>, ArenaMap<LocalId, ()>) {
    let mut kinds: ArenaMap<BlockId, Vec<DropKind>> = ArenaMap::default();
    let mut flagged: ArenaMap<LocalId, ()> = ArenaMap::default();
    for (id, block) in body.blocks.iter() {
        let mut state = states[id].clone();
        let mut block_kinds = Vec::new();
        for statement in &block.statements {
            if let Statement::Drop { place, flag: None } = statement {
                let kind = match tracked.get(place) {
                    None => DropKind::Always,
                    Some(index) if state.definitely_init.contains(index) => DropKind::Always,
                    Some(index) if !state.maybe_init.contains(index) => DropKind::Never,
                    Some(_) => {
                        if let Place::Local(local) = place {
                            flagged.insert(*local, ());
                        }
                        DropKind::Flagged
                    }
                };
                block_kinds.push(kind);
            }
            apply_statement(&mut state, tracked, statement);
        }
        kinds.insert(id, block_kinds);
    }

    (kinds, flagged)
}

/// Removes drops of values that are never initialized there, and guards drops of values
/// that may be moved with a drop flag maintained alongside every assignment and move.
fn elaborate_drops(body: &mut Body, tracked: &Tracked, states: &ArenaMap<BlockId, State>) {
    let (kinds, flagged) = classify_drops(body, tracked, states);

    // One flag per local with a conditional drop.
    let mut flags: ArenaMap<LocalId, LocalId> = ArenaMap::default();
    let flagged: Vec<LocalId> = flagged.iter().map(|(local, ())| local).collect();
    for local in flagged {
        let flag = body.locals.alloc(LocalDecl {
            ty: Ty::Bool,
            mode: LocalMode::Value,
            user: None,
        });
        flags.insert(local, flag);
    }

    let block_ids: Vec<BlockId> = body.blocks.iter().map(|(id, _)| id).collect();
    // Call destinations become initialized at the start of the call's target block, and
    // errors at the start of its error target.
    let mut initialized_at_start: ArenaMap<BlockId, Vec<LocalId>> = ArenaMap::default();
    for &id in &block_ids {
        let Terminator::Call {
            destination,
            target,
            on_error,
            ..
        } = &body.blocks[id].terminator
        else {
            continue;
        };
        let edges = target
            .map(|target| (destination, target))
            .into_iter()
            .chain(on_error.as_ref().map(|e| (&e.place, e.block)));
        for (place, block) in edges {
            if let Place::Local(local) = place
                && flags.get(*local).is_some()
            {
                initialized_at_start.entry(block).or_default().push(*local);
            }
        }
    }

    for &id in &block_ids {
        let block = &mut body.blocks[id];
        let old = std::mem::take(&mut block.statements);
        let mut statements = Vec::with_capacity(old.len());
        if id == body.entry {
            for (local, &flag) in flags.iter() {
                let is_param = body.params.contains(&local);
                statements.push(set_flag(flag, is_param, Span::empty(0)));
            }
        }
        for local in initialized_at_start.get(id).into_iter().flatten() {
            statements.push(set_flag(flags[*local], true, Span::empty(0)));
        }
        let mut drop_kinds = kinds.get(id).cloned().unwrap_or_default().into_iter();
        for statement in old {
            rewrite_statement(statement, &flags, &mut drop_kinds, &mut statements);
        }
        // Arguments moved by the terminator: the flag is cleared just before it.
        for used in terminator_uses(&block.terminator) {
            if used.moves
                && let Place::Local(local) = used.place
                && let Some(&flag) = flags.get(local)
            {
                statements.push(set_flag(flag, false, used.span));
            }
        }
        block.statements = statements;
    }
}

/// `flag = value`
fn set_flag(flag: LocalId, value: bool, span: Span) -> Statement {
    Statement::Assign {
        place: Place::Local(flag),
        value: Rvalue::Use(Operand::Const(Value::Bool(value))),
        span,
    }
}

/// Copies one statement into `out`, applying its drop decision and keeping drop flags up to
/// date after assignments, moves and drops.
fn rewrite_statement(
    statement: Statement,
    flags: &ArenaMap<LocalId, LocalId>,
    drop_kinds: &mut impl Iterator<Item = DropKind>,
    out: &mut Vec<Statement>,
) {
    let moved: Vec<(LocalId, Span)> = statement_uses(&statement)
        .into_iter()
        .filter(|u| u.moves)
        .filter_map(|u| match u.place {
            Place::Local(local) if flags.get(local).is_some() => Some((local, u.span)),
            _ => None,
        })
        .collect();
    let flag_of = |place: &Place| place.as_local().and_then(|local| flags.get(local).copied());
    match statement {
        Statement::Drop { place, flag: None } => {
            match drop_kinds.next().expect("one kind per drop") {
                DropKind::Never => {}
                DropKind::Always => {
                    let flag = flag_of(&place);
                    out.push(Statement::Drop { place, flag: None });
                    if let Some(flag) = flag {
                        out.push(set_flag(flag, false, Span::empty(0)));
                    }
                }
                DropKind::Flagged => {
                    let flag = flag_of(&place).expect("flagged drops have a flag");
                    out.push(Statement::Drop {
                        place,
                        flag: Some(flag),
                    });
                    out.push(set_flag(flag, false, Span::empty(0)));
                }
            }
        }
        Statement::Assign { place, value, span } => {
            let flag = flag_of(&place);
            out.push(Statement::Assign { place, value, span });
            for (local, move_span) in moved {
                out.push(set_flag(flags[local], false, move_span));
            }
            if let Some(flag) = flag {
                out.push(set_flag(flag, true, span));
            }
        }
        Statement::MarkMoved(local) => {
            out.push(Statement::MarkMoved(local));
            if let Some(&flag) = flags.get(local) {
                out.push(set_flag(flag, false, Span::empty(0)));
            }
        }
        other => {
            out.push(other);
            for (local, move_span) in moved {
                out.push(set_flag(flags[local], false, move_span));
            }
        }
    }
}
