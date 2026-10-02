# Pika Spec v0 - Variation 0: Object-Oriented

Status: **not adopted** (2026-10-02). Kept for reference; Pika follows [v0](../v0.md). Originally exploratory. This document describes an object-oriented variant of [Pika v0](../v0.md) so the two designs can be compared. **Everything in v0 applies unless this document changes it.**

The short version: Pika-OO adds **classes**, which are reference types managed by **automatic reference counting (ARC)**. Classes have single inheritance, overridable methods with dynamic dispatch, abstract classes, and interfaces. Structs, enums and the ownership rules of v0 remain for value types. The model is close to Swift and Kotlin, written in Pika's command syntax.

---

## 1. A taste

```pika
:interface Speaker {
    :fn speak self -> String
}

:class Animal abstract impl=Speaker,Display {
    :const name:String
    age:int=0

    :init name:String do={
        :set ($self->name) $name
    }

    :fn speak self -> String abstract

    :fn birthday self do={
        :set ($self->age) ($self->age + 1)
    }

    :fn describe self -> String open do={
        :return "$($self->name) ($($self->age)) says $([$self->speak])"
    }
}

:class Dog extends=Animal {
    tricks:List<String>

    :init name:String do={
        :set ($self->tricks) {}        # phase 1: own fields first
        $super->init $name             # then the parent initializer
    }

    :fn speak self -> String override do={ :return "woof" }

    :fn learn self trick:String do={ $self->tricks->push $trick }
}

:class Cat extends=Animal {
    :init name:String do={ $super->init $name }
    :fn speak self -> String override do={ :return "meow" }
}

:fn main do={
    :local rex [Dog->new "Rex"]
    :local pets:List<Animal> {$rex; [Cat->new "Tom"]}

    $rex->learn "sit"                  # rex and pets->0 are the same object
    $rex->birthday

    :foreach p in=$pets do={
        :put [$p->describe]            # dynamic dispatch
        :match $p {
            d:Dog do={ :put "  knows $([:len $d->tricks]) trick(s)" }
            _ do={}
        }
    }
}
```

Output:

```
Rex (1) says woof
  knows 1 trick(s)
Tom (0) says meow
```

---

## 2. What changes from v0

| Area | v0 | Variation 0 |
|---|---|---|
| Main abstraction | structs + traits (values) | classes (references) + structs (values) |
| Memory | ownership only | ownership for values, ARC for class instances |
| Copying a variable | moves, or copies if `Copy` | class instances are **shared**: assigning copies the reference |
| Polymorphism | generics (static) only | generics + virtual methods + interface types (dynamic) |
| Inheritance | none | single class inheritance |
| Traits | `:trait` | renamed `:interface`; usable as types |
| Aliasing safety | checked at compile time | compile time for values, **runtime checks** for class fields |
| Root type | none | every class implicitly extends `Object` |

New commands: `:class`, `:interface` (replaces `:trait`), `:init`, `:deinit`, `:static`, `:cast`, `:same`.
New keywords: `super`, `is`, `weak`.
New contextual flags (only in declaration position, see 3.4): `abstract`, `open`, `override`.

---

## 3. Classes

### 3.1 Declaration

```
:class Name<Generics>? flags? ("extends=" Path)? ("impl=" Path ("," Path)*)? {
    members
}
```

Class body members:

| Member | Syntax | Meaning |
|---|---|---|
| Mutable field | `name:Type` or `name:Type=default` | instance field |
| Immutable field | `:const name:Type` or `:const name:Type=default` | set once, in `:init` or by default |
| Weak field | `name:weak Type` | does not keep the object alive (section 6.3) |
| Static field | `:static name:Type=value` | one per class, not per instance |
| Initializer | `:init params do={...}` | constructor (section 4) |
| Deinitializer | `:deinit do={...}` | runs when the last reference goes away |
| Method | `:fn name self params ... do={...}` | instance method |
| Static method | `:fn name params ... do={...}` (no `self`) | called as `[Name->name ...]` |

Fields are accessed through `$self->field`. There is no implicit `self`.

### 3.2 Final by default

Classes and methods are **final unless marked**:

- A class can be extended only if it is marked `open` or `abstract`.
- A method can be overridden only if it is marked `open` or `abstract`.
- An overriding method must be marked `override`, and its signature must match exactly.

This avoids accidental "fragile base class" designs: the author of a class decides what subclasses may change.

### 3.3 Abstract classes and methods

- `:class Shape abstract { ... }` cannot be instantiated.
- `:fn area self -> f64 abstract` has no body and must be overridden by every concrete subclass.
- A class with any abstract method must itself be `abstract`.

### 3.4 Flags

Flags are bare words written after a signature and before its body: after the parameter list and return type of a `:fn`, or after the name of a `:class`. Because every parameter is written `name:Type`, a bare word in that position is never a parameter, so the flags are only keywords there. `open` or `override` remain valid names elsewhere (for example a method called `open`).

```pika
:fn area self -> f64 abstract
:fn describe self -> String open do={ ... }
:fn speak self -> String override do={ ... }
:fn parse s:String -> Config raises do={ ... }   # `raises` is now one flag among others
:class Shape abstract { ... }
:class Widget open impl=Display { ... }
```

### 3.5 The `Object` root class

Every class implicitly extends `Object`, which provides overridable defaults:

| Method | Default behavior |
|---|---|
| `:fn eq self other:Object -> bool open` | identity (`[:same $self $other]`) |
| `:fn hash self -> u64 open` | identity-based hash |
| `:fn fmt self mut out:Formatter open` | `<ClassName>` |

So every class instance supports `=`, can be a `Map` key, and can be printed with `:put`. Override them for value-like equality or nicer output.

---

## 4. Initialization

### 4.1 Constructors

`:init` defines how instances are created. The default initializer is called with `[Name->new ...]`; its arguments are the `:init` parameters.

```pika
:class Point2 {
    x:f64
    y:f64
    :init x:f64 y:f64 do={
        :set ($self->x) $x
        :set ($self->y) $y
    }
}

:local p [Point2->new 1.0 2.0]
:local q [Point2->new x=1.0 y=2.0]
```

- A class without `:init` gets an implicit one that takes every field without a default, in declaration order, as named or positional parameters.
- Additional named initializers are declared `:init name params do={...}` and called `[Name->name ...]`:
  ```pika
  :init origin do={ :set ($self->x) 0.0; :set ($self->y) 0.0 }
  # [Point2->origin]
  ```
- Initializers may be `raises`. If one raises, the partially built object is released and its already-initialized fields are destroyed.

### 4.2 Two-phase initialization (subclasses)

To guarantee that no method ever sees an uninitialized field, initialization runs in two phases (the Swift rule):

1. **Phase 1.** The initializer sets every field declared by its own class, then calls `$super->init ...` (or another initializer of the parent). In phase 1 it may not read `$self`, call methods on it, or pass it anywhere.
2. **Phase 2.** After the parent initializer returns, the object is fully initialized and `$self` can be used freely.

The compiler checks this with definite-initialization analysis. Calling `$super->init` exactly once on every path is required when the parent has fields without defaults.

### 4.3 Deinitialization

`:deinit do={ ... }` runs when the last strong reference to an instance is released. Subclass `:deinit` runs first, then the parent's, then fields are destroyed. `:deinit` cannot raise and cannot let `$self` escape.

---

## 5. Inheritance and dispatch

### 5.1 Single inheritance

```pika
:class Dog extends=Animal impl=Pet { ... }
```

A class extends at most one class and may implement any number of interfaces. A subclass inherits all fields and methods of its parent. Private members (names starting with `_`) are visible only within the module, as in v0; there is no `protected`.

### 5.2 Calling the parent

`$super->method args` calls the parent class's implementation directly, without dynamic dispatch. It is only valid inside a method or initializer of a subclass.

### 5.3 Dispatch rules

| Call | Dispatch |
|---|---|
| final method on a class type | static (direct call) |
| `open`/`abstract`/`override` method | dynamic, through the class vtable |
| method on an interface-typed value | dynamic, through the interface witness table |
| method on a generic `T: Interface` | static (monomorphized), as in v0 |
| `$super->method` | static |

The compiler devirtualizes dynamic calls when it can prove the concrete class.

### 5.4 Subtyping

- A `Dog` is accepted wherever an `Animal` is expected.
- A class or struct implementing `Speaker` is accepted wherever the interface type `Speaker` is expected.
- Generic types are **invariant**: `List<Dog>` is not a `List<Animal>`. Build a `List<Animal>` explicitly.

### 5.5 Type tests and casts

```pika
:if ($a is Dog) do={
    $a->learn "roll"           # smart cast: $a has type Dog inside this block
}

:local d [:cast<Dog> $a]       # Dog? - none if $a is not a Dog

:match $a {
    d:Dog do={ ... }           # typed binding pattern
    c:Cat do={ ... }
    _ do={ ... }
}
```

- `($x is T)` tests the runtime class or interface conformance.
- Smart casts apply inside the `do=` block when `$x` is a `:const` binding or a parameter (it cannot change in between).
- `name:Type` is a new pattern form for `:match`. A `:match` over an `abstract` class's subclasses is not exhaustive unless it has `_`, since new subclasses can be added in other modules.

---

## 6. Reference semantics and memory

### 6.1 Classes are references

A class instance lives on the heap with a reference count. Variables, fields and collection slots hold **references**:

```pika
:local a [Dog->new "Rex"]
:local b $a                     # same object; reference count is now 2
$b->birthday
:put ($a->age)                  # 1
:put [:same $a $b]              # true: identity
```

- Assigning, passing or returning a class reference increments the count. Dropping a reference decrements it. At zero, `:deinit` runs and the memory is freed.
- Class references are never "moved out of": there are no use-after-move errors for class types.
- Mutating an object's fields does not require `mut` on the variable or parameter. `mut` on a class-typed parameter means only that the callee may make the caller's variable point to a different object.
- The compiler removes redundant retain/release pairs (ARC optimization).

### 6.2 Structs and enums stay values

Structs and enums keep all v0 rules: moves, `Copy`, `read`/`mut`/`owned` and compile-time exclusivity. A struct field holding a class reference shares that object when the struct is copied or cloned.

When a struct value is used as an interface type (for example passed where `Speaker` is expected), it is copied into a heap box. Mutations through the interface affect the box, not the original.

### 6.3 Cycles and weak references

Reference counting cannot free cycles. Use `weak` for back-references:

```pika
:class TreeNode {
    value:i64
    children:List<TreeNode>
    parent:weak TreeNode            # reading it yields TreeNode?

    :init value:i64 do={
        :set ($self->value) $value
        :set ($self->children) {}
        :set ($self->parent) none
    }

    :fn add self child:TreeNode do={
        :set ($child->parent) $self
        $self->children->push $child
    }
}
```

- Reading a `weak` field yields `T?`: `none` once the object has been freed.
- A strong cycle that is never broken leaks. `pika run --leak-check` reports leaked objects at exit in debug builds.

### 6.4 Exclusivity for class fields (runtime)

Because many references can reach the same object, the compiler cannot always prove that two accesses to a field do not overlap. For class fields, Pika enforces v0's exclusivity rule **at runtime**:

- Each borrow of a class field (passing it as an argument, iterating it with `:foreach`, a `mut` method call on it) marks that field as being accessed for the duration of the call or loop.
- A conflicting access during that time (a mutation while it is borrowed, or a second mutable borrow) **panics** with "simultaneous access to field `tricks` of `Dog`".
- When the compiler can prove no conflict, the check is removed.

```pika
:foreach t in=$dog->tricks do={
    $other_ref_to_same_dog->learn "x"    # panic: tricks is being iterated
}
```

This keeps memory safety without a garbage collector, at a small runtime cost on class field accesses that cannot be checked statically.

---

## 7. Interfaces

`:interface` replaces v0's `:trait` and keeps its features (required methods, default methods, `impl=` inheritance between interfaces, derivable built-ins). In addition:

- An interface can be used **as a type**: `items:List<Speaker>`. Calls go through dynamic dispatch.
- Interfaces cannot declare fields.
- Interfaces with generic methods or methods that take or return `Self` can only be used as generic bounds, not as types.
- Built-in interfaces (`Copy`, `Clone`, `Eq`, `Ord`, `Hash`, `Display`, `Default`, operator interfaces) are unchanged. Classes cannot implement `Copy`; `Clone` on a class creates a new object (shallow by default when derived).

---

## 8. Generics with classes

Classes may be generic, and generic bounds may name classes as well as interfaces:

```pika
:class Box2<T: Clone> open {
    item:T
    :fn get self -> T do={ :return [$self->item->clone] }
}

:fn loudest<T: Animal> pets:List<T> -> T? do={ ... }   # T must be Animal or a subclass
```

Generic code is still monomorphized. A class bound adds no runtime cost beyond the dispatch of the methods it calls.

---

## 9. Grammar additions

```
class       := ":class" IDENT generics? classFlag* ("extends=" path)? implList? "{" classMember* "}"
classFlag   := "open" | "abstract"
classMember := field | ":const" field | ":static" field | init | deinit | fn
field       := IDENT ":" "weak"? type ("=" atom)?
init        := ":init" IDENT? param* "raises"? "do=" block
deinit      := ":deinit" "do=" block
fn          := ":fn" IDENT generics? param* ("->" type)? fnFlag* ("do=" block)?
fnFlag      := "raises" | "open" | "override" | "abstract"
interface   := ":interface" IDENT generics? implList? "{" fn* "}"
pattern     += IDENT ":" type
expr        += expr "is" type
head        += "$" "super" "->" IDENT
```

`is` binds at the comparison level (section 7.1 of v0).

---

## 10. Implementation impact

Compared with v0, the compiler and runtime need:

| Component | Extra work |
|---|---|
| Type checker | subtyping, override checking, smart casts, `is`/`:cast`, object-safety rules |
| Init checker | two-phase definite initialization across class hierarchies |
| MIR | retain/release insertion and ARC optimization passes; runtime exclusivity checks for class fields |
| Codegen | object layout with header (refcount, class pointer), vtables, interface witness tables, existential boxes for structs |
| Runtime | refcounting, weak reference side tables, dynamic type tests, leak checker |
| Optimizer | devirtualization and retain/release elimination to recover performance |

---

## 11. Assessment: v0 vs Variation 0

**What OO Pika gives you**

- A familiar model for developers coming from Java, C#, Kotlin, Swift or Python.
- Shared, mutable object graphs (UIs, games, simulations, caches) are easy to write. You don't need `Rc` or restructuring to satisfy the borrow rules.
- No use-after-move errors for class types, so the learning curve is gentler.
- Runtime polymorphism is built in rather than deferred to v1.

**What it costs**

- **Two memory models in one language.** Users must know whether a type is a value (struct) or a reference (class) to predict what assignment does. This is Swift's most common source of confusion.
- **Runtime overhead.** ARC traffic, runtime exclusivity checks and virtual calls cost performance that v0 gets for free. The optimizer can recover much of it, but not all, and predictable performance is the main point of Pika.
- **Some safety moves from compile time to runtime.** Aliasing mistakes become panics in production instead of compile errors.
- **Inheritance problems.** Fragile base classes and deep hierarchies. Final-by-default limits this but doesn't remove it.
- **A much larger compiler.** Section 10 adds roughly a third more front-end and runtime work before the first useful release.

**My recommendation**

Keep v0 as the base. The parts of this variant that bring the most benefit for the least cost can be added to v0 later without classes or inheritance:

1. **Interface types with dynamic dispatch**: v0 already reserves `dyn Trait`. This gives runtime polymorphism.
2. **Shared mutable objects through the standard library**: `Rc<T>` plus a checked cell type gives the shared-graph use case as an explicit opt-in, so its cost is visible.
3. **Method-centric syntax**: v0 already has methods, `->` calls, `Self` and associated functions, so code already *reads* object-oriented.

If the goal is specifically to attract OO developers, Variation 0 is coherent and implementable, but it moves Pika toward Swift and away from its systems-language goals.
