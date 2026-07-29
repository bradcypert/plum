// Deliberately simpler than the AST, and deliberately span-free: this
// is the "simplified typed IR" DESIGN.md's sequencing plan calls for —
// every later pass (FBIP, eventually codegen) should only ever need to
// handle this small, uniform set of node kinds, not the full surface
// grammar. Error messages during lowering should reference the
// original AST node's span, not carry one through into the IR itself.
//
// Scope note: this currently covers literals, variables, let, unary/
// binary operators, if, calls, and now a minimal heap-shaped value
// (`Ctor`/`Match`) — just enough for the FBIP pass to have something
// real to refcount. `Ctor`/`Match` are deliberately NOT the full
// struct/enum surface grammar: fields are positional, not named
// (matching Perceus's own minimal core calculus), and `Match` only
// deconstructs by tag, with no nested patterns, guards, or or-patterns.
// Lowering the real surface syntax down to this is separate, later,
// comparatively mechanical work — this is scoped to give the algorithm
// something to work on, not to be a complete compilation target yet.

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Bool,
    Unit,
}

// What the (still-stub) FBIP pass will insert: a `dup` (increment) at
// any point a value is used more than once, a `drop` (decrement) at a
// value's last use. See fbip.rs.
#[derive(Debug, Clone, PartialEq)]
pub enum RcOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Float(f64),
    // A string LITERAL — evaluates to a heap-allocated, refcounted
    // value (`Interpreter::eval` allocates a dedicated string heap
    // cell, distinct from `Ctor`'s tag+fields shape, and returns a
    // `Value::HeapRef` into it), matching DESIGN.md's "records, variant
    // payloads, arrays, strings, closures are refcounted" — NOT an
    // inline scalar the way `Int`/`Float`/`Bool` are. See
    // `StrConcat`'s doc comment for the one operation v1 supports
    // beyond construction.
    Str(String),
    Bool(bool),
    // `()`, Unit's only value — see DESIGN.md's "Tuples and closures".
    // Non-empty tuples aren't lowered yet (they're heap-allocated, so
    // they wait for the same pass as structs/enums).
    Unit,
    Var(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Let {
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    // A heap-allocated, tagged value with positional fields — the
    // minimal representation of "a struct or an enum variant" this IR
    // needs. E.g. `Ctor("Point", [x, y])` or `Ctor("Cons", [head, tail])`.
    Ctor {
        tag: String,
        fields: Vec<Expr>,
    },
    // Deconstructs `scrutinee` by tag; the matching arm's `bindings`
    // name the fields positionally. No or-patterns yet — see this
    // file's scope note. Arms are tried in order: the first arm whose
    // tag matches AND whose `guard` (if any) evaluates truthy wins: see
    // `MatchArm`'s doc comment for why this means more than one arm can
    // share a tag.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    // A reuse-in-place candidate, inserted by fbip.rs's reuse analysis
    // — the second half of FBIP, on top of refcount insertion. Means:
    // "construct (tag, fields), but if `reuse_of`'s refcount is 1 at
    // this point, overwrite its memory in place instead of allocating
    // fresh." Codegen would turn this into a single `if rc == 1`
    // branch (see DESIGN.md's memory model section) — this IR node
    // just marks WHERE that's safe, it doesn't simulate the memory
    // itself.
    CtorReuse {
        reuse_of: String,
        tag: String,
        fields: Vec<Expr>,
    },
    RcAnnotated {
        op: RcOp,
        target: String,
        rest: Box<Expr>,
    },
    // `for var in start..end { body }`, lowered from the ONLY iterable
    // shape currently supported — a literal Range (see lower.rs's
    // `lower_for` for why: no array/list/collection type exists yet to
    // iterate over otherwise). `..` is exclusive of `end`, matching
    // Rust's `Range` — DESIGN.md now records this as Decided. The loop
    // itself always evaluates to Unit; `body`'s value is discarded each
    // iteration, same as a statement-expression inside a block.
    For {
        var: String,
        start: Box<Expr>,
        end: Box<Expr>,
        body: Box<Expr>,
    },
    // A closure LITERAL — evaluates to a first-class function value
    // that captures its defining environment. Only plain-identifier
    // params exist at the AST level for closures (unlike function
    // params, there's no destructuring-Pattern case to reject here).
    // Captured heap values are NOT refcounted by this pass yet — see
    // fbip.rs's `Closure` handling in `mark_last_uses` for why (a
    // closure can escape and be called later or multiple times, so a
    // use inside it is conservatively never treated as a last use,
    // same tradeoff as `For`'s body: a deliberate leak, not a
    // use-after-free).
    Closure {
        params: Vec<String>,
        body: Box<Expr>,
    },
    // `name = value; rest` — reassigns an EXISTING binding (never
    // introduces one; that's still `Let`) and continues into `rest`,
    // the same "statement sequencing via nested expression" shape as
    // `Let` uses for a discarded expression-statement. Evaluates
    // `value` to Unit's worth of side effect, not a value used by
    // anything. Nothing checks the target was actually declared `let
    // mut` (DESIGN.md's "non-escaping local mutation" story) — that's
    // a static mutability check that doesn't exist at any layer yet,
    // lowering included; see lower.rs's `Stmt::Assign` handling.
    // Heap-shaped reassignment isn't precisely refcounted yet either —
    // see fbip.rs's `Assign` handling for why that's an accepted leak,
    // not a soundness gap, matching `For`/`Closure`.
    Assign {
        name: String,
        value: Box<Expr>,
        rest: Box<Expr>,
    },
    // `spawn { block }` — runs `block` on a genuinely separate OS
    // thread (DESIGN.md's "OS threads first" concurrency model) with
    // its own fresh `Interpreter`/`Heap`. Evaluates to a `Value::Task`
    // handle. See DESIGN.md's "Implementation blocker: heap ownership
    // across tasks" for why this needs its own heap at all: a
    // `Value::HeapRef` is only meaningful within the `Heap` that
    // allocated it, so `block` can't simply share the spawning
    // thread's heap. Every heap-shaped value `block` can see (captured
    // locals, globals) crosses via a deep copy into the new thread's
    // OWN heap, DECIDED over a genuinely shared atomically-refcounted
    // heap specifically to keep every `Heap` single-owner and
    // non-atomic — see `Interpreter::to_portable`/`from_portable`.
    Spawn {
        block: Box<Expr>,
    },
    // `t.join()` — blocks until the task handle `task` (evaluated from
    // `task`, expected to be a `Value::Task`) finishes, and returns its
    // result deep-copied BACK into the joining thread's own heap (the
    // same crossing mechanism as `Spawn`, just in the other
    // direction). A second `.join()` on the same handle is a runtime
    // error — `JoinHandle::join` itself consumes the handle, so
    // there's nothing left to join a second time.
    TaskJoin {
        task: Box<Expr>,
    },
    // `channel[T]()` — creates a new channel and evaluates to a
    // `(Sender[T], Receiver[T])` pair (an ordinary `Ctor`-backed
    // 2-tuple, same as any other tuple; the two FIELDS are what carry
    // the actual `Value::Sender`/`Value::Receiver` runtime handles). No
    // fields here: `T` is fully erased at this layer, matching every
    // other generic in the language — the interpreter only ever needs
    // to know "make a channel," never what it carries.
    Channel,
    // `tx.send(v)` — pushes `v`, deep-copied into portable form (the
    // SAME crossing mechanism `Spawn`/`TaskJoin` use — see
    // `Interpreter::to_portable`), onto the channel `sender` (expected
    // to be a `Value::Sender`) identifies. Evaluates to `Unit`.
    // DESIGN.md's "channel send is a move" IS enforced — see
    // `plum-ir::movecheck` — as a separate AST-level static pass, not
    // by anything at this IR layer.
    ChannelSend {
        sender: Box<Expr>,
        value: Box<Expr>,
    },
    // `rx.recv()` — blocks until a value arrives on the channel
    // `receiver` identifies (expected to be a `Value::Receiver`), then
    // deep-copies it BACK into the receiving thread's own heap (the
    // same crossing mechanism as `TaskJoin`'s result). A disconnected
    // channel (every `Sender` dropped with nothing left to receive) is
    // a runtime error, not a silent hang or a `None`.
    ChannelRecv {
        receiver: Box<Expr>,
    },
    // `select { pattern = expr => body, ... }` — blocks until ONE of
    // `arms`' `receiver`s has a value ready, then evaluates that ONE
    // arm's `body`. `body` already has `pattern`'s destructuring baked
    // in via the SAME `wrap_destructure` machinery a function param
    // uses (see `lower.rs`'s `lower_select`) — it references a FIXED
    // synthetic name (`"__select_recv"`) the interpreter is
    // responsible for binding to the actually-received value right
    // before evaluating `body`. Every arm's `receiver` is evaluated
    // exactly ONCE up front (not re-evaluated on every poll — see
    // `Interpreter::eval`'s `Select` case), so an expression with side
    // effects there behaves as expected.
    Select {
        arms: Vec<SelectArm>,
    },
    // `arr[i]` — reads the field at RUNTIME index `index` out of the
    // heap value `base` (an `Array[T]`, represented as an ordinary
    // `Ctor` — see `lower.rs`'s `ARRAY_TAG` — but this node works on
    // ANY tagged heap value with enough fields; nothing about it is
    // Array-specific at this layer). A genuinely NEW node, unlike
    // reading a Struct/tuple field: `Match`'s bindings are fixed at
    // COMPILE time (one name per DECLARED field position), but an
    // array index is only known at RUNTIME. Out-of-bounds is a
    // reported runtime error, not a panic or silent wraparound.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    // `arr.len()` — the heap cell's field COUNT. Same "check the
    // callee's shape" convention as `.join()`/`.send()`/`.recv()`.
    ArrayLen {
        array: Box<Expr>,
    },
    // `arr.push(v)` — evaluates to a NEW array with `v` appended.
    // Always allocates a fresh heap cell (old fields cloned, `v`
    // appended) — genuinely, honestly NOT reuse-in-place optimized
    // yet: FBIP's existing `CtorReuse` mechanism only ever recognizes
    // SAME-arity reconstruction (a struct/tuple/enum's field COUNT
    // never changes), never a cell GROWING by one field, so making
    // `push` grow the old cell in place when uniquely owned needs
    // genuinely new analysis, not just wiring into what already
    // exists — deliberately deferred, not silently pretended-away.
    // `arr.push(v)` — evaluates to a NEW array with `v` appended.
    // Allocates a fresh heap cell UNLESS `mark_reuse` (fbip.rs) proved
    // `array` is a plain variable and rewrote this into `ArrayPushReuse`
    // — see that variant's doc comment for the actual reuse-in-place
    // mechanism. This node itself is unchanged from before that pass
    // existed: it's the "might still need a fresh allocation" fallback
    // shape, not a "never reuses" one.
    ArrayPush {
        array: Box<Expr>,
        value: Box<Expr>,
    },
    // `arr.pop()` — evaluates to a NEW array with the LAST element
    // removed. A reported runtime error on an empty array (same
    // "runtime-checked, not compile-time" convention as `Index`'s
    // out-of-bounds), not a panic. See `ArrayPush`'s doc comment for
    // the reuse-in-place relationship to `ArrayPopReuse`.
    ArrayPop {
        array: Box<Expr>,
    },
    // `arr.set(i, v)` — evaluates to a NEW array with the element at
    // RUNTIME index `index` replaced by `value`; every other element
    // unchanged. Bounds-checked at runtime, same convention as `Index`.
    // See `ArrayPush`'s doc comment for the reuse-in-place relationship
    // to `ArraySetReuse`.
    ArraySet {
        array: Box<Expr>,
        index: Box<Expr>,
        value: Box<Expr>,
    },
    // `arr.remove(i)` — evaluates to a NEW array with the element at
    // RUNTIME index `index` removed (later elements shift down by one).
    // Bounds-checked at runtime, same convention as `Index`. See
    // `ArrayPush`'s doc comment for the reuse-in-place relationship to
    // `ArrayRemoveReuse`.
    ArrayRemove {
        array: Box<Expr>,
        index: Box<Expr>,
    },
    // The reuse-in-place counterpart to `ArrayPush`/`ArrayPop`/
    // `ArraySet`/`ArrayRemove` — produced ONLY by `mark_reuse`
    // (fbip.rs), never by lowering directly, exactly mirroring how
    // `CtorReuse` relates to `Ctor`. `reuse_of` names the SAME plain
    // variable the ordinary node's `array` field held (only a bare
    // variable names a specific cell reuse can target — see
    // `mark_reuse`'s `Match` case for the identical precedent). At
    // runtime (`Interpreter::eval`), `reuse_of`'s cell has its
    // reference count decremented FIRST; if that reaches zero (nobody
    // else holds a reference), the SAME heap address is recycled with
    // the new field list instead of allocating fresh — otherwise a
    // fresh cell is allocated exactly as the ordinary node would.
    // Safety comes entirely from that runtime refcount check, not from
    // this node's mere presence: if `reuse_of` were used again
    // elsewhere in the same scope, `insert_refcount_ops` (which always
    // runs BEFORE `mark_reuse`) would already have inserted an `Inc`
    // for that other use, keeping the refcount above zero here and
    // correctly forcing the fresh-allocation path — the exact same
    // argument that already makes `CtorReuse` safe.
    ArrayPushReuse {
        reuse_of: String,
        value: Box<Expr>,
    },
    ArrayPopReuse {
        reuse_of: String,
    },
    ArraySetReuse {
        reuse_of: String,
        index: Box<Expr>,
        value: Box<Expr>,
    },
    ArrayRemoveReuse {
        reuse_of: String,
        index: Box<Expr>,
    },
    // `s.concat(other)` — evaluates to a NEW string with `other`
    // appended. Same "shape-only detection, dedicated primitive node"
    // convention as `.push()`/`ArrayPush`, and the same reuse-in-place
    // relationship to `StrConcatReuse` that `ArrayPush` has to
    // `ArrayPushReuse` — see `ArrayPushReuse`'s doc comment for the
    // full safety argument, which applies identically here (same
    // `Heap::dec_and_maybe_reuse_str` runtime refcount check).
    StrConcat {
        base: Box<Expr>,
        other: Box<Expr>,
    },
    StrConcatReuse {
        reuse_of: String,
        other: Box<Expr>,
    },
    // `s.runes()` — decodes `s` (a UTF-8 byte buffer underneath) into
    // an `Array[Int]` of Unicode codepoints, one element per character
    // rather than per byte — the answer to `s[i]`'s BYTE-indexing
    // deliberately NOT being character-aware (see `Index`'s own doc
    // comment / DESIGN.md's "Strings" section): decode ONCE, O(n), into
    // an ordinary array, then every subsequent access is an ordinary
    // O(1) `Index` into IT. A genuinely new primitive node (not
    // shape-shared with anything else, and not expressible as a
    // desugaring into existing nodes — UTF-8 decoding needs bit
    // manipulation the IR has no operators for at all yet). No reuse-
    // in-place counterpart: this always builds a brand new array of a
    // different heap shape than the string it reads from, never
    // recycling the string cell itself.
    StrRunes {
        base: Box<Expr>,
    },
    // `s.trim()` — evaluates to a NEW string with leading/trailing
    // Unicode whitespace removed (delegates directly to Rust's own
    // `str::trim()` — same "just call the underlying Rust method"
    // precedent as `.runes()`'s `str::chars()`). Same reuse-in-place
    // relationship to `StrTrimReuse` that `StrConcat` has to
    // `StrConcatReuse` — see `ArrayPushReuse`'s doc comment for the
    // full safety argument.
    StrTrim {
        base: Box<Expr>,
    },
    StrTrimReuse {
        reuse_of: String,
    },
    // `s.split(sep)` — evaluates to `Array[Str]`, one element per
    // substring, delegating directly to Rust's own `str::split()`
    // (including ITS edge-case behavior: consecutive separators yield
    // empty-string elements, an empty `sep` yields one element per
    // character plus empty leading/trailing entries — deliberately not
    // special-cased). A genuinely new primitive node: like `StrRunes`,
    // this builds a whole new differently-shaped heap value (an array
    // of freshly allocated string cells) rather than transforming one
    // existing cell, so there's no reuse-in-place counterpart.
    StrSplit {
        base: Box<Expr>,
        sep: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectArm {
    pub receiver: Expr,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub tag: String,
    pub bindings: Vec<String>,
    // `if <guard>` on a match arm — evaluated with `bindings` already
    // in scope, AFTER a tag match but BEFORE running `body`. If it
    // evaluates to `false`, this arm is skipped as though its tag
    // hadn't matched at all, and evaluation moves on to the next arm —
    // which is why more than one arm is allowed to share the same tag
    // (`Circle(r) if r > 0.0 => ..., Circle(r) => ...`), unlike the
    // guard-free case where `Expr::Match`'s scope note used to imply
    // (by "the matching arm") there'd only ever be one arm per tag.
    // Scoped to plain top-level bindings only for now: an arm whose
    // pattern needs NESTED destructuring (bindings only introduced
    // partway through evaluating `body`, via `wrap_nested_destructures`)
    // can't yet combine with a guard — see lower.rs's `lower_match`.
    pub guard: Option<Box<Expr>>,
    pub body: Expr,
}

// A named, top-level, non-closure function — see lower.rs's
// `lower_program` scope note for exactly what does and doesn't lower
// into one of these yet (only plain-identifier params; generics are
// ignored entirely, since a type parameter has no runtime effect
// without a type checker).
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Expr,
}

// A zero-parameter top-level `let` — a plain, eagerly-evaluated value,
// not a function. `Program.globals` is ORDERED: a global's `value` may
// only reference EARLIER globals (never a later one, never itself —
// there's no lazy/recursive-let story here), since a real interpreter
// evaluates them once, in this exact order, before anything else runs.
// Referencing any function is fine regardless of order, since a
// function isn't evaluated until called.
#[derive(Debug, Clone, PartialEq)]
pub struct Global {
    pub name: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub functions: Vec<Function>,
    pub globals: Vec<Global>,
}
