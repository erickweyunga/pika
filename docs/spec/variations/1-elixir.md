# Pika Spec v0 - Variation 1: Elixir-Style

Status: **not adopted** (2026-10-02). Kept for reference; Pika follows [v0](../v0.md). Originally exploratory. This document describes a functional, actor-based variant of [Pika v0](../v0.md), modeled on Elixir (and its typed cousin Gleam), so the designs can be compared. **Everything in v0 applies unless this document changes it.**

The short version: in Pika-Ex **all data is immutable**, everything is an expression, functions are defined by **multiple pattern-matching clauses**, code is chained with the **pipe operator** `|>`, and concurrency uses **lightweight processes** that share nothing and communicate by **message passing**, with supervisors that restart crashed processes ("let it crash"). It stays statically typed and compiled to native code.

Because data is immutable, the user-facing ownership system from v0 (moves, `mut`, `owned`, borrow rules) **disappears entirely**. Memory is managed by reference counting with in-place reuse, which is optimized by the compiler (section 7).

---

## 1. A taste

```pika
:use /std/enum
:use /std/string

## Word frequencies with a pipeline
:spec word_counts text:String -> Map<String, int>
:fn word_counts text do={
    ($text
        |> /string/downcase
        |> /string/split " "
        |> /enum/reject &([:len $1] = 0)
        |> /enum/frequencies)
}

## Multi-clause function with pattern matching and guards
:spec fib n:int -> int
:fn fib 0 do={ 0 }
:fn fib 1 do={ 1 }
:fn fib n when=($n > 1) do={ ([:fib ($n - 1)] + [:fib ($n - 2)]) }

## A process: state lives in the arguments of a recursive loop
:type CounterMsg (@inc | @add(int) | @get(Reply<int>))

:spec counter count:int -> never receives=CounterMsg
:fn counter count do={
    :receive {
        @inc do={ [:counter ($count + 1)] }
        @add(n) do={ [:counter ($count + $n)] }
        @get(from) do={
            :reply $from $count
            [:counter $count]
        }
        after=1m do={ :exit @idle }
    }
}

:fn main do={
    :local pid [:spawn [:fn do={ [:counter 0] }]]
    :send $pid @inc
    :send $pid @add(41)
    :put [:call $pid @get timeout=1s]        # 42

    :put ("the cat saw the dog" |> :word_counts)
    # {"the"=2; "cat"=1; "saw"=1; "dog"=1}
}
```

---

## 2. What changes from v0

| Area | v0 | Variation 1 |
|---|---|---|
| Data | mutable values with ownership | **immutable** values, freely shared |
| Ownership syntax | `mut`, `owned`, moves, borrow rules | none |
| Memory | ownership + deterministic drop | reference counting with in-place reuse (no cycles possible) |
| `:set` | mutates a variable or field | **rebinds** a name in the current block (section 3.3) |
| Statements | commands only | everything is an expression; a block's value is its last line |
| Functions | one body per function | multiple clauses with patterns and `when=` guards |
| Methods | `$obj->method` | none: functions live in modules, chained with `\|>` |
| Errors | `raises`, `:onerror` | tagged results (`@ok`/`@error`), `:with`, and crashing processes |
| Loops | `:while`, `:for`, `:foreach`, `:break` | recursion (guaranteed tail calls), comprehensions, `:reduce` |
| Traits | `:trait` declared on the type | `:protocol` + `:impl` (can be added for any type) |
| Concurrency | none in v0 | processes, mailboxes, links, monitors, supervisors |
| Panics | abort the program | crash **only the current process** |

New syntax: tags `@name`, tuples `(a, b)`, pipe `|>`, capture shorthand `&(...)`, struct update `$s{field=value}`, `<-` inside `:with`.
New commands: `:spec`, `:type`, `:protocol`, `:impl`, `:reduce`, `:with`, `:spawn`, `:send`, `:receive`, `:reply`, `:call`, `:self`, `:exit`, `:link`, `:monitor`.
Removed commands: `:while`, `:do`, `:break`, `:continue`, `:return`, `:onerror`, `:global`, `:trait`.

---

## 3. Expressions and bindings

### 3.1 Everything is an expression

Every block (`do={...}`, `else={...}`, `{...}`) evaluates to the value of its **last line**. A line may be a command or, newly in this variant, a bare atom (`$x`, `"text"`, `(expr)`, `[command]`, a literal or a tag).

```pika
:local label [:if ($n < 0) do={ "negative" } else={ "non-negative" }]

:spec abs n:int -> int
:fn abs n do={
    :if ($n < 0) do={ (-$n) } else={ $n }
}
```

There is no `:return`. A function returns the value of its body.

### 3.2 Bindings are immutable and can destructure

`:local pattern value` binds names by matching. If the pattern does not match, the process crashes with a match error.

```pika
:local x 5
:local (a, b) (1, "one")                  # tuple destructuring
:local @ok(cfg) [:read_config "app.toml"] # crash unless the result is @ok
:local {first; second} $pair_list         # list of exactly two elements
```

`:const` is removed: every binding is already immutable.

### 3.3 `:set` rebinds

`:set name value` creates a new binding that shadows `name` for the rest of the current block. It never changes a value that anything else can see.

```pika
:local total 0
:set total ($total + 10)                   # same as `:local total ($total + 10)`
:set ($user->address->city) "Dar"          # rebinds `user` to an updated copy
```

- `:set` on a field or element path rebinds the root variable to an updated copy: `:set ($m->"k") 1` means `:local m [/map/put $m "k" 1]`.
- `:set` may only target a variable declared in the **same block**. Setting a variable from an enclosing block (for example inside a loop body) is a compile error: "variables are immutable; return the new value from the block instead". This removes the classic Elixir surprise where a rebinding inside `if` is silently lost.

### 3.4 Struct update

`$value{field=new; ...}` creates a copy of a struct with some fields replaced:

```pika
:local older $user{age=($user->age + 1)}
```

When `$user` is not used afterwards, the compiler reuses its memory and the update happens in place (section 7.2).

---

## 4. Data types

All v0 types remain, immutable. Additions:

### 4.1 Tags

A tag is an identifier prefixed with `@`, optionally carrying one payload joined in parentheses. Tags play the role of Elixir atoms and tagged tuples, but they are **statically typed**.

```pika
@ok           @error("not found")         @point((1, 2))       @done
```

The type of a tag value is written the same way, and tags combine into **unions** with `|`:

```pika
:type Status (@active | @suspended(String) | @deleted)
:type ReadResult (@ok(String) | @error(@enoent | @eacces | @eio))
```

- Tag unions are structural: two unions with the same tags are the same type, and a smaller union is accepted where a larger one is expected (`@ok(int)` fits `@ok(int) | @error(String)`).
- `:match` over a tag union must be exhaustive.
- `:type Name Type` declares a type alias.

v0 enums remain available for nominal sum types. Tags are the idiomatic choice for results, messages and small state machines.

### 4.2 Tuples

`(a, b, c)` is a tuple, of type `(A, B, C)`. `(a)` is just a parenthesized expression; a one-element tuple is written `(a,)`. The empty tuple `()` is the unit value and replaces `[:nothing]`.

### 4.3 Collections

`List`, `Map` and `Set` are **persistent** (immutable with structural sharing):

- Updating returns a new collection that shares most of its structure with the old one. This takes O(log n) time for `Map`/`Set` and amortized O(1) for appending to `List`.
- When the old collection is uniquely referenced, the update is done in place (section 7.2), so code that "looks like copying" usually is not.
- `List` is a relaxed radix-balanced vector, not a linked list, so indexing is fast. Prepend-and-match (`{head; ..tail}`) is supported in patterns.

---

## 5. Functions

### 5.1 Specs and clauses

A function with more than one clause is declared with a `:spec` giving its parameter names, types and return type, followed by one or more `:fn` clauses whose parameters are **patterns**:

```pika
:spec area shape:Shape -> f64
:fn area @circle(r) do={ (/math/pi * $r * $r) }
:fn area @rect((w, h)) do={ ($w * $h) }
:fn area @square(s) do={ ($s * $s) }
```

- Clauses are tried in order. The set of clauses must be exhaustive for the spec's types (checked).
- `when=(guard)` adds a guard to a clause. Guards are restricted to pure expressions (comparisons, arithmetic, `:len`, type tests, other guard-safe functions).
- A single-clause function may still use v0's inline form: `:fn add a:int b:int -> int do={ ($a + $b) }`.
- Named arguments use the parameter names from the `:spec`: `[:area shape=@square(2.0)]`.

### 5.2 The pipe operator

`value |> command args` calls `command` with `value` inserted as the **first** argument. It has the lowest precedence of any operator and is only valid inside `( )`.

```pika
:local result ($orders
    |> /enum/filter &($1->paid)
    |> /enum/map &($1->total)
    |> /enum/sum)
```

With pipes there are no methods. A type's functions live in its module: `/user/new`, `/user/rename`. `->` remains only for field and index access.

### 5.3 Anonymous functions and capture shorthand

```pika
:local double [:fn x do={ ($x * 2) }]       # parameter types inferred from use
:local double2 &($1 * 2)                     # capture shorthand, $1 $2 ... are positional args
:local sq &/math/pow/2                        # function reference by name and arity
:put [$double 21]
```

The shorthand reuses RouterOS's `$1`, `$2` positional argument names. Closures capture by sharing (all values are immutable).

### 5.4 Tail calls

Calls in tail position (the last expression of a function body or of a block in tail position) are **guaranteed** not to grow the stack, including mutual recursion. Recursion is the looping construct for long-running processes.

---

## 6. Control flow

### 6.1 `:if`, `:match`, `:with`

`:if` and `:match` are expressions. `:match` arms take `when=` guards instead of `if=`.

`:with` chains steps that must each match, and short-circuits to `else=` on the first mismatch (Elixir's `with`):

```pika
:spec load path:String -> (@ok(Config) | @error(String))
:fn load path do={
    :with {
        @ok(text) <- [/fs/read $path]
        @ok(raw) <- [/toml/parse $text]
        @ok(cfg) <- [/config/validate $raw]
    } do={
        @ok($cfg)
    } else={
        @error(reason) do={ @error("cannot load $path: $reason") }
    }
}
```

### 6.2 Comprehensions and reduction

`:for` becomes a comprehension that **returns a collection**. `:foreach` is kept for side effects only and returns `()`.

```pika
:local squares [:for x in=$nums when=($x > 0) do={ ($x * $x) }]           # List
:local index   [:for u in=$users into=Map do={ ($u->id, $u) }]             # Map
:local pairs   [:for x in=$xs y in=$ys do={ ($x, $y) }]                    # cartesian product
:local evens   [:for i from=0 to=10 step=2 do={ $i }]

:local sum [:reduce x in=$nums acc=0 do={ ($acc + $x) }]

:foreach line in=$lines do={ :put $line }
```

`:while`, `:do`, `:break` and `:continue` are removed. Use recursion, `:reduce`, or `/enum/reduce_while`.

---

## 7. Memory model

### 7.1 Reference counting without cycles

Values are reference counted. Because data is immutable and a value can only reference values that existed before it was created, **reference cycles cannot be created**. Reference counting therefore frees everything without a tracing garbage collector and without `weak` references.

### 7.2 Reuse when unique ("functional but in place")

The compiler inserts reference count operations precisely (Perceus-style, as in Koka and Lean 4) and performs **reuse analysis**:

- When a value's last use is an update (`:set`, struct update, `/map/put`, list append), and its count is 1 at runtime, the update is performed **in place** with no allocation.
- Borrow inference removes count operations for values that are only read.

Programs written in a purely functional style get close to imperative performance in common cases, but **not guaranteed**: a value that is unexpectedly shared will be copied. `pika build --explain-copies` reports where copies happen.

### 7.3 Per-process heaps

Each process allocates from its own heap with non-atomic reference counts. When a process exits or crashes, its whole heap is released at once, which is why crashes need no unwinding and no destructors.

---

## 8. Processes

Processes are the unit of concurrency and of failure. They are much lighter than OS threads: a few hundred bytes plus a small growable stack. Programs may run millions of them.

### 8.1 Spawning and messaging

```pika
:local pid [:spawn [:fn do={ [:counter 0] }]]      # Pid<CounterMsg>
:send $pid @inc
:local me [:self]
```

- A function that uses `:receive` declares the type of messages its process accepts with the spec flag `receives=Type`. `:spawn` infers `Pid<Type>` from it, and `:send` checks the message type at compile time.
- Messages are **copied** into the receiving process's heap (share-nothing), except `Bytes` values larger than 64 bytes, which are shared with an atomic reference count.
- Message order is preserved between any pair of processes.

### 8.2 Receiving

```pika
:receive {
    pattern do={ ... }
    pattern when=(guard) do={ ... }
    after=5s do={ ... }               # optional timeout
}
```

`:receive` takes the first message in the mailbox that matches any arm (selective receive), waiting if there is none. `after=` uses v0's duration literals.

### 8.3 Request and reply

```pika
:put [:call $pid @get timeout=1s]
```

`:call` creates a one-shot `Reply<T>` value, sends the message with it as the last payload field, and waits for the answer. The receiving process answers with `:reply $from value`. If no reply arrives within `timeout=` (default 5s), the **caller** crashes with a timeout error.

### 8.4 Scheduling

- The runtime starts one scheduler thread per CPU core and distributes processes across them with work stealing.
- Scheduling is **preemptive**: the compiler inserts cheap yield checks at function entries and loop back-edges, so a busy process cannot starve others.
- Blocking FFI calls and file I/O run on a separate pool of blocking threads so they do not stall schedulers.

---

## 9. Errors and fault tolerance

### 9.1 Two kinds of failure

1. **Expected failures** are values: functions return `@ok(value) | @error(reason)` and callers handle them with `:match` or `:with`.
2. **Unexpected failures** crash the process: a failed `:local` match, `:error "message"`, a failed `:assert`, integer overflow, division by zero, a missing map key, a `:call` timeout.

`:error` no longer has a `raises` contract; it crashes the current process. There is no try/catch. The idiom is "let it crash": write the happy path, and let a supervisor restart the process from a known-good state.

### 9.2 Links and monitors

```pika
:link $pid           # if either process crashes, the other receives an exit signal (and crashes too)
:local ref [:monitor $pid]   # one-way: receive @down((ref, pid, reason)) when pid exits
```

A process can mark itself `[:trap_exits]` to receive exit signals as `@exit((pid, reason))` messages instead of crashing.

### 9.3 Supervisors

The standard library provides supervisors and generic servers modeled on OTP:

```pika
:use /std/otp/supervisor

:fn main do={
    [/supervisor/start strategy=@one_for_one max_restarts=3 within=5s children={
        [/supervisor/worker id=@counter start=&[:counter 0]]
        [/supervisor/worker id=@web start=&[/web/serve port=8080]]
    }]
    :receive { _ do={ () } }      # keep main alive
}
```

| Strategy | On a child crash |
|---|---|
| `@one_for_one` | restart only that child |
| `@one_for_all` | restart all children |
| `@rest_for_one` | restart that child and those started after it |

If restarts exceed `max_restarts` within `within=`, the supervisor itself crashes and its own supervisor decides what to do.

---

## 10. Protocols

`:protocol` replaces `:trait`. Implementations are declared separately with `:impl`, so a protocol can be implemented for types from other modules (Elixir's `defimpl`):

```pika
:protocol Size {
    :spec size value:Self -> int
}

:impl Size for=String { :fn size s do={ [:len $s] } }
:impl<T> Size for=List<T> { :fn size l do={ [:len $l] } }

:put [:size "hello"]          # protocol functions are callable directly when the protocol is in scope
```

Rules: an implementation must live in the module of the protocol or of the type (the "orphan rule"), so two libraries can never provide conflicting implementations. Dispatch is static when the type is known, and dynamic for protocol-typed values.

---

## 11. Removed from v0

| v0 feature | Why removed | Replacement |
|---|---|---|
| `mut`, `owned`, moves, borrow checking | data is immutable | none needed |
| `:global` | shared mutable state contradicts share-nothing processes | a process holding the state, or `/std/ets` tables (later) |
| `raises`, `:onerror` | errors are values or crashes | tagged results, `:with`, supervisors |
| `:while`, `:break`, `:continue`, `:return` | require mutation or early exit | recursion, comprehensions, `:reduce` |
| methods in type bodies | Elixir keeps data and functions separate | module functions + `\|>` |
| `Drop` | no deterministic destruction order across shared values | resources owned by a process are closed when it exits |
| `Box`, `Rc` | every value is already shared and reference counted | none needed |

---

## 12. Grammar additions

```
spec        := ":spec" IDENT param* ("->" type)? specFlag*
specFlag    := "receives=" type
clause      := ":fn" IDENT pattern* ("when=" "(" expr ")")? "do=" block
typeAlias   := ":type" IDENT generics? type
protocol    := ":protocol" IDENT generics? "{" spec* "}"
impl        := ":impl" generics? path "for=" type "{" (clause | fn)* "}"

atom        += tag | tuple | capture | var "{" (IDENT "=" atom (";" | NEWLINE))* "}"
tag         := "@" IDENT ("(" expr ")")?          # "(" joined to the name
tuple       := "(" ")" | "(" expr "," ")" | "(" expr ("," expr)+ ")"
capture     := "&" "(" expr ")" | "&" "[" command "]" | "&" path "/" INT
expr        += expr "|>" head arg*                # lowest precedence
type        += tagType ("|" tagType)* | "(" type ("," type)* ")"
tagType     := "@" IDENT ("(" type ")")?
pattern     := "_" | IDENT | literal | tag-pattern | tuple-pattern | list-pattern | pattern "when=" ...
list-pattern := "{" (pattern (";" pattern)*)? (";" ".." IDENT)? "}"
with        := ":with" "{" (pattern "<-" atom NEWLINE)+ "}" "do=" block ("else=" "{" matchArm* "}")?
stmt        += atom                               # bare value line
```

---

## 13. Implementation impact

| Component | Compared with v0 |
|---|---|
| Borrow checker | **removed** |
| Type checker | adds structural tag unions with subtyping, tuples, clause exhaustiveness, `receives=` effect checking |
| Pattern compiler | multi-clause functions compiled to decision trees |
| MIR | Perceus reference counting, borrow inference, reuse analysis, guaranteed tail calls (Cranelift `tail` calling convention), yield-point insertion |
| Runtime | **much larger**: multi-core work-stealing scheduler, growable process stacks, per-process heaps, mailboxes with selective receive, timers, links/monitors, blocking-thread pool, message copying |
| Standard library | persistent collections (RRB vector, HAMT), OTP-style supervisor and server |

The runtime is the hardest part. The BEAM virtual machine has had more than thirty years of tuning, and a native-code equivalent is a substantial project of its own.

---

## 14. Assessment: v0 vs Variation 1

**What Elixir-style Pika gives you**

- **The simplest model for users.** No ownership, no borrowing, no mutation surprises, and no cycles to leak.
- **Best-in-class concurrency and fault tolerance.** Millions of processes, no data races by construction, and supervision trees. This makes it excellent for servers, network services, chat and real-time systems, and IoT gateways. That's very close to RouterOS's own world of long-running, event-driven automation.
- **Expressive code.** Pipes, multi-clause functions, `:with` and comprehensions make data transformation concise.
- **Static types on top**, where Elixir itself is only now adding them.

**What it costs**

- **A lower and less predictable performance ceiling.** Reference count traffic, copying on message send, persistent data structures and preemption checks all cost something. Reuse analysis recovers a lot, but whether an update is in place depends on sharing that isn't visible in the source. This is the opposite of v0's goal of predictable, near-metal speed.
- **Numeric and systems work gets harder.** In-place mutation of large buffers, SIMD and zero-copy FFI all fight the immutable model.
- **A very large runtime** (section 13). Elixir's best features also come from BEAM capabilities this design does not include yet: hot code reloading, distribution across machines, and a process observer.
- **Structural tag unions** make type inference and error messages harder than v0's nominal types.

**My recommendation**

This variant is a different language with a different niche: a natively compiled, statically typed Elixir, close to what Gleam is for the BEAM. It's coherent and attractive, but it gives up v0's goal of predictable systems-level performance.

If you want both, a strong combination is **v0 as the core plus Elixir's concurrency model as a library and a few syntax features**:

1. Processes, mailboxes, links and supervisors in `/std/actor`. Messages must be owned values that get **moved** between processes. v0's ownership rules make that safe with no copying.
2. The pipe operator `|>` and multi-clause functions with `when=` guards. Both fit v0 without changing its memory model.
3. Tags with payloads as lightweight enums, and `:with` for chaining `Result`-returning calls.

That combination keeps predictable performance and gains most of what makes Elixir pleasant to write.
