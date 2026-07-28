mod heap;

use plum_ir::ir::{BinOp, Expr, Function, Program, RcOp, UnOp};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    HeapRef(usize),
    // An index into `Interpreter::closures`, not a heap address — kept
    // in its own id space (not `HeapRef`) so the two can never be
    // confused, since they're looked up in entirely different tables.
    Closure(usize),
    // A reference to a named TOP-LEVEL function, used as a value rather
    // than called directly — e.g. `let f = square; f(5)`, or passing
    // `square` itself as a higher-order argument. Named by String
    // (matching `Interpreter::functions`' own key type), not an id,
    // since top-level functions are looked up by name, unlike closures.
    // Calling a function DIRECTLY BY NAME (`square(5)`) still takes the
    // pre-existing fast path in `Expr::Call` and never constructs this
    // — it only ever appears when a function's name is evaluated in a
    // NON-call position (see `lookup`).
    Function(String),
    // A `spawn { .. }` task handle — see `TaskHandle`'s doc comment.
    Task(TaskHandle),
    // The two ends of a `channel[T]()` — see `SenderHandle`/
    // `ReceiverHandle`'s doc comments.
    Sender(SenderHandle),
    Receiver(ReceiverHandle),
}

/// The sending end of a `channel[T]()`. Wraps `std::sync::mpsc::
/// Sender<PortableValue>` in an `Arc` purely to give `Value::Sender`
/// an identity `PartialEq` can compare (`mpsc::Sender` has neither
/// `PartialEq` nor any exposed identity of its own) — NOT for thread-
/// safety, since `mpsc::Sender` is already `Clone`+`Send`+`Sync`-
/// capable on its own (multiple senders are a supported, intentional
/// use of `mpsc`). Values cross already-converted to portable form
/// (see `PortableValue`'s doc comment; `.send()` converts before
/// pushing), so the raw `mpsc` primitive is exactly what's needed
/// underneath.
#[derive(Debug, Clone)]
pub struct SenderHandle(Arc<mpsc::Sender<PortableValue>>);

impl PartialEq for SenderHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The receiving end of a `channel[T]()`. `mpsc::Receiver` has no
/// `Clone` at all (only one consumer can meaningfully take each
/// message) — wrapped in `Arc<Mutex<..>>` purely so `Value::Receiver`
/// can still be `Clone` like every other `Value` (cloning gives
/// another handle to the SAME receiving end, sharing the one
/// underlying consumer, the same "share a handle, not duplicate the
/// resource" shape as `TaskHandle`). `Arc`/`Mutex` (not `Rc`/
/// `RefCell`, unlike `TaskHandle`) specifically because a `Receiver`
/// needs to be `Send` to cross into a spawned task's thread — see
/// `Interpreter::to_portable`.
///
/// `PartialEq` is hand-written as `Arc::ptr_eq` (identity — the same
/// reasoning as `TaskHandle`).
#[derive(Debug, Clone)]
pub struct ReceiverHandle(Arc<Mutex<mpsc::Receiver<PortableValue>>>);

impl PartialEq for ReceiverHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A joinable handle to a task spawned on a real OS thread (see
/// DESIGN.md's concurrency model — "OS threads first"). Wraps
/// `Option` so `.join()` can `take()` the `JoinHandle` out (it's
/// consumed by `JoinHandle::join`, so a value can only be joined
/// once), and `Rc<RefCell<..>>` so `Value::Task` stays cheaply
/// `Clone` like every other `Value` — cloning a `Value::Task` gives
/// another handle to the SAME task, not a second task, matching how
/// `Value::Closure`'s id-based sharing already works.
///
/// `PartialEq` is hand-written (can't derive through `JoinHandle`,
/// which has none) as identity comparison — two `Value::Task`s are
/// equal exactly when they're handles to the SAME spawned task.
#[derive(Debug, Clone)]
pub struct TaskHandle(Rc<RefCell<Option<thread::JoinHandle<Result<PortableValue, String>>>>>);

impl PartialEq for TaskHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// A `Value` with every heap reference resolved into owned, self-
/// contained data — the deep-copy form DECIDED in DESIGN.md's
/// "Implementation blocker: heap ownership across tasks" for crossing
/// a thread boundary (`spawn`'s captured environment going IN, a
/// task's result coming back out via `.join()`, and eventually a
/// channel `send`/`recv`). A `Value::HeapRef` is only meaningful
/// within the `Heap` that allocated it, so nothing containing one can
/// cross as-is; a `PortableValue` owns its data outright instead of
/// pointing at any particular heap.
#[derive(Debug, Clone)]
pub enum PortableValue {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Unit,
    Ctor(String, Vec<PortableValue>),
    // Channel endpoints ARE genuinely portable, unlike closures/
    // functions/tasks — `mpsc::Sender`/`Arc<Mutex<mpsc::Receiver<_>>>`
    // are real cross-thread-safe primitives already (that's the whole
    // point of `mpsc`), so crossing means moving the actual handle,
    // not re-deriving anything from a heap address the way `Ctor`
    // does. See `SenderHandle`/`ReceiverHandle`'s doc comments.
    Sender(SenderHandle),
    Receiver(ReceiverHandle),
}

// A closure's defining environment, captured WHOLE at creation time
// (every binding currently in scope, not just the free variables the
// body actually uses) — the simple, obviously-correct choice; a real
// free-variable analysis to capture only what's needed is a later
// precision improvement, not a correctness requirement. Captured heap
// values are NOT refcounted (see fbip.rs's `Closure` handling) — this
// can leak a reference the closure never gives back, same deliberate
// tradeoff as `For`'s body.
#[derive(Debug, Clone, PartialEq)]
struct ClosureValue {
    params: Vec<String>,
    body: Expr,
    captured: Vec<(String, Value)>,
}

// A flat, innermost-last scope stack — `let` pushes one entry, evaluates
// its body, then pops. Variable lookup scans from the end so shadowing
// (an inner `let x` hiding an outer one) falls out for free, and
// popping on the way back out of a `let`'s body is what keeps a binding
// from leaking into code that comes lexically after it.
pub struct Interpreter {
    env: Vec<(String, Value)>,
    heap: heap::Heap,
    // Separate from `env` on purpose: a named top-level function must
    // NEVER see whatever local variables happen to exist wherever it
    // was called from — only its own parameters. Keeping functions in
    // their own table (rather than pushing them onto `env`) is what
    // makes that isolation structural rather than something `call` has
    // to remember to enforce.
    functions: HashMap<String, Function>,
    closures: HashMap<usize, ClosureValue>,
    next_closure_id: usize,
    // Zero-parameter top-level `let`s — plain values, evaluated ONCE
    // by `load_program` (never re-evaluated), and — like `functions` —
    // deliberately NOT part of `env`: `call`/`call_closure` completely
    // REPLACE `env` per invocation for isolation, so anything a
    // function body needs to see regardless of call site has to live
    // in its own permanent table instead. `lookup` falls back here
    // after scanning `env` comes up empty.
    globals: HashMap<String, Value>,
}

impl Interpreter {
    pub fn new() -> Self {
        Interpreter {
            env: Vec::new(),
            heap: heap::Heap::new(),
            functions: HashMap::new(),
            closures: HashMap::new(),
            next_closure_id: 0,
            globals: HashMap::new(),
        }
    }

    /// Registers every function, then evaluates every global's
    /// initializer ONCE, in declaration order — functions first and
    /// unconditionally, since a function isn't evaluated until called
    /// and so can be registered regardless of where it sits relative to
    /// a global that might reference it. Globals are evaluated with
    /// `self.globals` already containing every EARLIER global (each one
    /// is inserted immediately after being evaluated), so a global's
    /// initializer naturally sees prior globals via ordinary `Var`
    /// lookup — see ir.rs's `Global` doc comment for why later/self
    /// references aren't meaningful here and aren't supported.
    pub fn load_program(&mut self, program: &Program) -> Result<(), String> {
        for f in &program.functions {
            self.functions.insert(f.name.clone(), f.clone());
        }
        for g in &program.globals {
            let value = self.eval(&g.value)?;
            self.globals.insert(g.name.clone(), value);
        }
        Ok(())
    }

    /// Invokes a named top-level function with concrete argument
    /// values. Swaps in a completely FRESH environment containing only
    /// the parameters (not whatever `env` currently holds), evaluates
    /// the body, then restores the caller's environment — this swap-
    /// and-restore is what gives correct isolation, and it's also
    /// exactly what makes recursion work for free: a function calling
    /// itself just looks itself up in `functions` again, fresh.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        let func = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown function: {name}"))?;
        if func.params.len() != args.len() {
            return Err(format!(
                "{name} expects {} argument(s), found {}",
                func.params.len(),
                args.len()
            ));
        }
        let fresh_env: Vec<(String, Value)> = func.params.into_iter().zip(args).collect();
        let caller_env = std::mem::replace(&mut self.env, fresh_env);
        let result = self.eval(&func.body);
        self.env = caller_env;
        result
    }

    /// Invokes a closure value: the fresh environment is the CAPTURED
    /// environment (a snapshot from creation time) with the params
    /// pushed on top, not the caller's env — same isolation reasoning
    /// as `call`, except a closure's "outer scope" is whatever was in
    /// scope where it was DEFINED, not whatever's in scope where it's
    /// CALLED from. Pushing params after the captured bindings means a
    /// param correctly shadows a captured binding of the same name
    /// (`lookup` scans from the end).
    fn call_closure(&mut self, id: usize, args: Vec<Value>) -> Result<Value, String> {
        let closure = self
            .closures
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("invalid closure reference: {id}"))?;
        if closure.params.len() != args.len() {
            return Err(format!(
                "closure expects {} argument(s), found {}",
                closure.params.len(),
                args.len()
            ));
        }
        let mut fresh_env = closure.captured;
        fresh_env.extend(closure.params.into_iter().zip(args));
        let caller_env = std::mem::replace(&mut self.env, fresh_env);
        let result = self.eval(&closure.body);
        self.env = caller_env;
        result
    }

    fn lookup(&self, name: &str) -> Result<Value, String> {
        self.env
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.globals.get(name).cloned())
            // A local/global binding always shadows a same-named
            // top-level function, matching `Expr::Call`'s own fast-path
            // precedence (a local variable holding a closure named the
            // same as a real function still calls the closure).
            .or_else(|| self.functions.contains_key(name).then(|| Value::Function(name.to_string())))
            .ok_or_else(|| format!("unbound variable: {name}"))
    }

    /// Deep-copies `value` into a self-contained, thread-portable
    /// form — recursively resolving every heap cell it transitively
    /// reaches into owned data (see `PortableValue`'s doc comment).
    /// Closures, top-level function references, and task handles
    /// aren't portable — their identity (a captured environment, a
    /// name only meaningful in THIS interpreter's `functions` table, a
    /// `JoinHandle` tied to this specific thread) can't cross a thread
    /// boundary at all yet. A real, documented v1 limitation: a
    /// spawned task can't capture a closure, a bare function value, or
    /// another task handle.
    fn to_portable(&self, value: &Value) -> Result<PortableValue, String> {
        match value {
            Value::Int(n) => Ok(PortableValue::Int(*n)),
            Value::Float(f) => Ok(PortableValue::Float(*f)),
            Value::Str(s) => Ok(PortableValue::Str(s.clone())),
            Value::Bool(b) => Ok(PortableValue::Bool(*b)),
            Value::Unit => Ok(PortableValue::Unit),
            Value::HeapRef(addr) => {
                let (tag, fields) = self.heap.read(*addr)?;
                let tag = tag.to_string();
                let portable_fields =
                    fields.iter().map(|f| self.to_portable(f)).collect::<Result<Vec<_>, _>>()?;
                Ok(PortableValue::Ctor(tag, portable_fields))
            }
            Value::Closure(_) => {
                Err("cannot send a closure across a task boundary (not yet supported)".to_string())
            }
            Value::Function(name) => {
                Err(format!("cannot send function {name:?} across a task boundary (not yet supported)"))
            }
            Value::Task(_) => {
                Err("cannot send a task handle across a task boundary (not yet supported)".to_string())
            }
            // Genuinely portable — see `PortableValue::Sender`/
            // `Receiver`'s doc comment.
            Value::Sender(h) => Ok(PortableValue::Sender(h.clone())),
            Value::Receiver(h) => Ok(PortableValue::Receiver(h.clone())),
        }
    }

    /// The inverse of `to_portable`: materializes a portable value
    /// into THIS interpreter's own heap, allocating a FRESH cell for
    /// every `Ctor` — never reusing whatever address it happened to
    /// have in the originating thread's heap, since that address means
    /// nothing here.
    fn from_portable(&mut self, value: PortableValue) -> Value {
        match value {
            PortableValue::Int(n) => Value::Int(n),
            PortableValue::Float(f) => Value::Float(f),
            PortableValue::Str(s) => Value::Str(s),
            PortableValue::Bool(b) => Value::Bool(b),
            PortableValue::Unit => Value::Unit,
            PortableValue::Ctor(tag, fields) => {
                let values: Vec<Value> = fields.into_iter().map(|f| self.from_portable(f)).collect();
                let addr = self.heap.alloc(tag, values);
                Value::HeapRef(addr)
            }
            // No re-materialization needed — the handle itself is
            // already the real, live cross-thread-safe resource.
            PortableValue::Sender(h) => Value::Sender(h),
            PortableValue::Receiver(h) => Value::Receiver(h),
        }
    }

    /// Total number of fresh heap allocations performed so far — does
    /// NOT count `CtorReuse` overwriting an existing cell, so this is
    /// what actually demonstrates FBIP's reuse-in-place saving a real
    /// allocation, not just that a `CtorReuse` node was present.
    pub fn alloc_count(&self) -> usize {
        self.heap.alloc_count()
    }

    /// The current live refcount of a heap value — `Err` if `value`
    /// isn't a `HeapRef` or points at already-freed memory.
    pub fn refcount(&self, value: &Value) -> Result<usize, String> {
        match value {
            Value::HeapRef(addr) => self.heap.refcount(*addr),
            other => Err(format!("{other:?} is not a heap value")),
        }
    }

    pub fn eval(&mut self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Unit => Ok(Value::Unit),
            Expr::Var(name) => self.lookup(name),
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                eval_unary(op, v)
            }
            // && and || short-circuit: the right side is only
            // evaluated when the left side doesn't already determine
            // the result. This has to be handled here, before the
            // operands are evaluated, not inside a generic binary-op
            // helper that's already given both values.
            Expr::Binary(BinOp::And, l, r) => match self.eval(l)? {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => match self.eval(r)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    v => Err(format!("`&&` requires Bool operands, found {v:?}")),
                },
                v => Err(format!("`&&` requires Bool operands, found {v:?}")),
            },
            Expr::Binary(BinOp::Or, l, r) => match self.eval(l)? {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => match self.eval(r)? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    v => Err(format!("`||` requires Bool operands, found {v:?}")),
                },
                v => Err(format!("`||` requires Bool operands, found {v:?}")),
            },
            Expr::Binary(op, l, r) => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                eval_binary(op, lv, rv)
            }
            Expr::Let { name, value, body } => {
                let v = self.eval(value)?;
                self.env.push((name.clone(), v));
                let result = self.eval(body);
                self.env.pop();
                result
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => match self.eval(cond)? {
                Value::Bool(true) => self.eval(then_branch),
                Value::Bool(false) => self.eval(else_branch),
                v => Err(format!("`if` condition must be Bool, found {v:?}")),
            },
            Expr::Call { callee, args } => {
                // A bare name naming a known top-level function is the
                // fast path, unchanged from before closures existed —
                // this skips constructing a `Value::Function` just to
                // immediately unwrap it again. Only taken when NOTHING
                // local shadows the name first (a `let`/param/closure-
                // capture binding of the same name takes precedence,
                // matching ordinary lexical scoping and `lookup`'s own
                // precedence — see its doc comment). When the callee
                // ISN'T a known top-level function name (or IS shadowed)
                // we fall through to evaluating it as an ordinary
                // expression, which must then produce a Closure or
                // Function value — covers a closure literal called
                // directly, a variable/parameter holding one, a bare
                // function name used as a value, or one returned from a
                // call.
                if let Expr::Var(name) = callee.as_ref() {
                    let shadowed = self.env.iter().rev().any(|(n, _)| n == name);
                    if !shadowed && self.functions.contains_key(name) {
                        let arg_values =
                            args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
                        return self.call(name, arg_values);
                    }
                }
                let callee_v = self.eval(callee)?;
                match callee_v {
                    Value::Closure(id) => {
                        let arg_values = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
                        self.call_closure(id, arg_values)
                    }
                    Value::Function(name) => {
                        let arg_values = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
                        self.call(&name, arg_values)
                    }
                    other => Err(format!("cannot call {other:?} — not a function or closure")),
                }
            }
            Expr::Closure { params, body } => {
                let id = self.next_closure_id;
                self.next_closure_id += 1;
                self.closures.insert(
                    id,
                    ClosureValue {
                        params: params.clone(),
                        body: (**body).clone(),
                        captured: self.env.clone(),
                    },
                );
                Ok(Value::Closure(id))
            }
            Expr::Spawn { block } => {
                // Snapshot everything `block` can see and convert it
                // to portable form BEFORE crossing the thread boundary
                // — see `to_portable`'s doc comment. Fails loudly (in
                // THIS thread, before anything spawns) if the captured
                // environment contains something that can't cross yet
                // (a closure, a bare function value, a task handle).
                let portable_env = self
                    .env
                    .iter()
                    .map(|(name, v)| Ok((name.clone(), self.to_portable(v)?)))
                    .collect::<Result<Vec<(String, PortableValue)>, String>>()?;
                let portable_globals = self
                    .globals
                    .iter()
                    .map(|(name, v)| Ok((name.clone(), self.to_portable(v)?)))
                    .collect::<Result<Vec<(String, PortableValue)>, String>>()?;
                let functions = self.functions.clone();
                let block = (**block).clone();

                let join_handle = thread::spawn(move || -> Result<PortableValue, String> {
                    let mut task_interp = Interpreter::new();
                    task_interp.functions = functions;
                    for (name, pv) in portable_globals {
                        let v = task_interp.from_portable(pv);
                        task_interp.globals.insert(name, v);
                    }
                    for (name, pv) in portable_env {
                        let v = task_interp.from_portable(pv);
                        task_interp.env.push((name, v));
                    }
                    let result = task_interp.eval(&block)?;
                    task_interp.to_portable(&result)
                });

                Ok(Value::Task(TaskHandle(Rc::new(RefCell::new(Some(join_handle))))))
            }
            Expr::TaskJoin { task } => {
                let Value::Task(handle) = self.eval(task)? else {
                    return Err("`.join()` requires a Task value".to_string());
                };
                let join_handle = handle
                    .0
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| "task already joined".to_string())?;
                match join_handle.join() {
                    Ok(Ok(portable)) => Ok(self.from_portable(portable)),
                    Ok(Err(e)) => Err(format!("spawned task failed: {e}")),
                    Err(_) => Err("spawned task panicked".to_string()),
                }
            }
            Expr::Channel => {
                let (tx, rx) = mpsc::channel::<PortableValue>();
                let sender = Value::Sender(SenderHandle(Arc::new(tx)));
                let receiver = Value::Receiver(ReceiverHandle(Arc::new(Mutex::new(rx))));
                let addr = self.heap.alloc("2Tuple".to_string(), vec![sender, receiver]);
                Ok(Value::HeapRef(addr))
            }
            Expr::ChannelSend { sender, value } => {
                let Value::Sender(handle) = self.eval(sender)? else {
                    return Err("`.send()` requires a Sender value".to_string());
                };
                let v = self.eval(value)?;
                let portable = self.to_portable(&v)?;
                handle
                    .0
                    .send(portable)
                    .map_err(|_| "cannot send: the channel's receiver has been dropped".to_string())?;
                Ok(Value::Unit)
            }
            Expr::ChannelRecv { receiver } => {
                let Value::Receiver(handle) = self.eval(receiver)? else {
                    return Err("`.recv()` requires a Receiver value".to_string());
                };
                let portable = handle
                    .0
                    .lock()
                    .map_err(|_| "channel receiver lock poisoned".to_string())?
                    .recv()
                    .map_err(|_| "cannot recv: the channel's sender has been dropped".to_string())?;
                Ok(self.from_portable(portable))
            }
            Expr::Select { arms } => {
                // Every arm's channel is evaluated exactly ONCE, up
                // front — never re-evaluated on later poll sweeps (see
                // ir.rs's `Select` doc comment).
                let mut handles = Vec::with_capacity(arms.len());
                for arm in arms {
                    let Value::Receiver(handle) = self.eval(&arm.receiver)? else {
                        return Err("`select` arm requires a Receiver value".to_string());
                    };
                    handles.push(handle);
                }
                loop {
                    let mut any_still_open = false;
                    for (i, handle) in handles.iter().enumerate() {
                        let attempt = handle
                            .0
                            .lock()
                            .map_err(|_| "channel receiver lock poisoned".to_string())?
                            .try_recv();
                        match attempt {
                            Ok(portable) => {
                                let v = self.from_portable(portable);
                                self.env.push(("__select_recv".to_string(), v));
                                let result = self.eval(&arms[i].body);
                                self.env.pop();
                                return result;
                            }
                            Err(TryRecvError::Empty) => any_still_open = true,
                            Err(TryRecvError::Disconnected) => {}
                        }
                    }
                    // Every channel disconnected with nothing EVER
                    // received on any of them — blocking forever here
                    // would just hang, so this is a real, reported
                    // error instead.
                    if !any_still_open {
                        return Err("select: every channel is disconnected with nothing received".to_string());
                    }
                    // A brief, deliberately simple busy-poll — this
                    // interpreter has no native cross-channel select
                    // primitive (plain `std::sync::mpsc` doesn't offer
                    // one), and no async runtime to wait on instead.
                    // Correct (nothing is ever missed — a message
                    // sent between sweeps is caught on the NEXT one),
                    // just not maximally efficient; a real scheduler
                    // is a later performance upgrade, same status
                    // DESIGN.md already gives the rest of the
                    // concurrency model.
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Expr::Index { base, index } => {
                let Value::HeapRef(addr) = self.eval(base)? else {
                    return Err("indexing requires an Array value".to_string());
                };
                let Value::Int(i) = self.eval(index)? else {
                    return Err("array index must be an Int".to_string());
                };
                let (_, fields) = self.heap.read(addr)?;
                let Ok(i) = usize::try_from(i) else {
                    return Err(format!("array index out of bounds: {i} (len {})", fields.len()));
                };
                fields
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("array index out of bounds: {i} (len {})", fields.len()))
            }
            Expr::ArrayLen { array } => {
                let Value::HeapRef(addr) = self.eval(array)? else {
                    return Err("`.len()` requires an Array value".to_string());
                };
                let (_, fields) = self.heap.read(addr)?;
                Ok(Value::Int(fields.len() as i64))
            }
            Expr::ArrayPush { array, value } => {
                let Value::HeapRef(addr) = self.eval(array)? else {
                    return Err("`.push()` requires an Array value".to_string());
                };
                let v = self.eval(value)?;
                let (tag, fields) = self.heap.read(addr)?;
                let tag = tag.to_string();
                let mut new_fields = fields.to_vec();
                new_fields.push(v);
                let new_addr = self.heap.alloc(tag, new_fields);
                Ok(Value::HeapRef(new_addr))
            }
            Expr::ArrayPop { array } => {
                let Value::HeapRef(addr) = self.eval(array)? else {
                    return Err("`.pop()` requires an Array value".to_string());
                };
                let (tag, fields) = self.heap.read(addr)?;
                if fields.is_empty() {
                    return Err("cannot pop from an empty array".to_string());
                }
                let tag = tag.to_string();
                let mut new_fields = fields.to_vec();
                new_fields.pop();
                let new_addr = self.heap.alloc(tag, new_fields);
                Ok(Value::HeapRef(new_addr))
            }
            Expr::ArraySet { array, index, value } => {
                let Value::HeapRef(addr) = self.eval(array)? else {
                    return Err("`.set()` requires an Array value".to_string());
                };
                let Value::Int(i) = self.eval(index)? else {
                    return Err("`.set()` index must be an Int".to_string());
                };
                let v = self.eval(value)?;
                let (tag, fields) = self.heap.read(addr)?;
                let tag = tag.to_string();
                let mut new_fields = fields.to_vec();
                let Ok(i) = usize::try_from(i) else {
                    return Err(format!("array index out of bounds: {i} (len {})", new_fields.len()));
                };
                if i >= new_fields.len() {
                    return Err(format!("array index out of bounds: {i} (len {})", new_fields.len()));
                }
                new_fields[i] = v;
                let new_addr = self.heap.alloc(tag, new_fields);
                Ok(Value::HeapRef(new_addr))
            }
            Expr::ArrayRemove { array, index } => {
                let Value::HeapRef(addr) = self.eval(array)? else {
                    return Err("`.remove()` requires an Array value".to_string());
                };
                let Value::Int(i) = self.eval(index)? else {
                    return Err("`.remove()` index must be an Int".to_string());
                };
                let (tag, fields) = self.heap.read(addr)?;
                let tag = tag.to_string();
                let mut new_fields = fields.to_vec();
                let Ok(i) = usize::try_from(i) else {
                    return Err(format!("array index out of bounds: {i} (len {})", new_fields.len()));
                };
                if i >= new_fields.len() {
                    return Err(format!("array index out of bounds: {i} (len {})", new_fields.len()));
                }
                new_fields.remove(i);
                let new_addr = self.heap.alloc(tag, new_fields);
                Ok(Value::HeapRef(new_addr))
            }
            Expr::RcAnnotated { op, target, rest } => {
                // Only heap values are affected — a stray Inc/Dec on a
                // non-heap name (shouldn't happen given how fbip.rs
                // scopes its analysis, but nothing here assumes it
                // can't) is a harmless no-op.
                if let Value::HeapRef(addr) = self.lookup(target)? {
                    match op {
                        RcOp::Inc => self.heap.inc(addr)?,
                        RcOp::Dec => self.heap.dec(addr)?,
                    }
                }
                self.eval(rest)
            }
            Expr::Ctor { tag, fields } => {
                let values = fields.iter().map(|f| self.eval(f)).collect::<Result<Vec<_>, _>>()?;
                let addr = self.heap.alloc(tag.clone(), values);
                Ok(Value::HeapRef(addr))
            }
            Expr::CtorReuse { reuse_of, tag, fields } => {
                // Fields are evaluated BEFORE touching the reused
                // cell — this is symbolic (we don't manipulate real
                // memory here), but it mirrors the real constraint a
                // codegen backend has: compute the new values first,
                // then overwrite, so a partial in-place write never
                // reads back its own not-yet-written bytes.
                let values = fields.iter().map(|f| self.eval(f)).collect::<Result<Vec<_>, _>>()?;
                let Value::HeapRef(addr) = self.lookup(reuse_of)? else {
                    return Err(format!("reuse target {reuse_of:?} is not a heap value"));
                };
                let result_addr = self.heap.dec_and_maybe_reuse(addr, tag.clone(), values)?;
                Ok(Value::HeapRef(result_addr))
            }
            Expr::Match { scrutinee, arms } => {
                let Value::HeapRef(addr) = self.eval(scrutinee)? else {
                    return Err("match scrutinee must be a heap value".to_string());
                };
                let (tag, fields) = self.heap.read(addr)?;
                let tag = tag.to_string();
                let fields = fields.to_vec();
                // Arms are tried in order: the first one whose tag
                // matches AND whose guard (if any) evaluates truthy
                // wins — see ir.rs's `MatchArm` doc comment for why
                // more than one arm can share a tag.
                for arm in arms {
                    if arm.tag != tag {
                        continue;
                    }
                    if arm.bindings.len() != fields.len() {
                        return Err(format!(
                            "arm for {tag:?} expects {} field(s), found {}",
                            arm.bindings.len(),
                            fields.len()
                        ));
                    }
                    let bound: Vec<(String, Value)> =
                        arm.bindings.iter().cloned().zip(fields.iter().cloned()).collect();
                    self.env.extend(bound);
                    if let Some(guard) = &arm.guard {
                        let guard_result = self.eval(guard);
                        let passed = match guard_result {
                            Ok(Value::Bool(b)) => b,
                            Ok(other) => {
                                self.env.truncate(self.env.len() - arm.bindings.len());
                                return Err(format!("match guard must evaluate to a Bool, found {other:?}"));
                            }
                            Err(e) => {
                                self.env.truncate(self.env.len() - arm.bindings.len());
                                return Err(e);
                            }
                        };
                        if !passed {
                            self.env.truncate(self.env.len() - arm.bindings.len());
                            continue;
                        }
                    }
                    let result = self.eval(&arm.body);
                    self.env.truncate(self.env.len() - arm.bindings.len());
                    return result;
                }
                Err(format!("no match arm for tag {tag:?}"))
            }
            Expr::For { var, start, end, body } => {
                let Value::Int(start) = self.eval(start)? else {
                    return Err("`for` loop range start must be Int".to_string());
                };
                let Value::Int(end) = self.eval(end)? else {
                    return Err("`for` loop range end must be Int".to_string());
                };
                // `..` is exclusive of `end`, matching Rust's Range.
                for i in start..end {
                    self.env.push((var.clone(), Value::Int(i)));
                    let result = self.eval(body);
                    self.env.pop();
                    result?;
                }
                Ok(Value::Unit)
            }
            Expr::Assign { name, value, rest } => {
                let v = self.eval(value)?;
                // The target must already exist — `Assign` never
                // introduces a binding, only `Let` does. Search from
                // the end, same direction `lookup` scans, so shadowing
                // is respected: assigning `x` mutates the INNERMOST
                // `x` currently in scope, not some outer one.
                let slot = self
                    .env
                    .iter_mut()
                    .rev()
                    .find(|(n, _)| n == name)
                    .ok_or_else(|| format!("assignment to undefined variable: {name}"))?;
                slot.1 = v;
                self.eval(rest)
            }
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn eval_unary(op: &UnOp, v: Value) -> Result<Value, String> {
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| "integer overflow negating i64::MIN".to_string()),
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => Err(format!("type error: cannot apply {op:?} to {v:?}")),
    }
}

fn eval_binary(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => eval_arith(op, lv, rv),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => eval_order(op, lv, rv),
        BinOp::Eq => values_equal(&lv, &rv).map(Value::Bool),
        BinOp::Ne => values_equal(&lv, &rv).map(|b| Value::Bool(!b)),
        BinOp::And | BinOp::Or => {
            unreachable!("And/Or are handled with short-circuit evaluation in Interpreter::eval")
        }
    }
}

fn eval_arith(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    match (lv, rv) {
        (Value::Int(a), Value::Int(b)) => {
            let result = match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                BinOp::Div if b == 0 => return Err("division by zero".to_string()),
                BinOp::Div => a.checked_div(b),
                BinOp::Rem if b == 0 => return Err("division by zero".to_string()),
                BinOp::Rem => a.checked_rem(b),
                _ => unreachable!(),
            };
            result.map(Value::Int).ok_or_else(|| "integer overflow".to_string())
        }
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => a / b,
            BinOp::Rem => a % b,
            _ => unreachable!(),
        })),
        (a, b) => Err(format!(
            "type error: no implicit numeric coercion, cannot apply {op:?} to {a:?} and {b:?}"
        )),
    }
}

fn eval_order(op: &BinOp, lv: Value, rv: Value) -> Result<Value, String> {
    let ordering = match (&lv, &rv) {
        (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        _ => return Err(format!("type error: cannot order-compare {lv:?} and {rv:?}")),
    };
    let Some(ord) = ordering else {
        return Err("comparison produced no ordering (NaN?)".to_string());
    };
    Ok(Value::Bool(match op {
        BinOp::Lt => ord.is_lt(),
        BinOp::Gt => ord.is_gt(),
        BinOp::Le => ord.is_le(),
        BinOp::Ge => ord.is_ge(),
        _ => unreachable!(),
    }))
}

fn values_equal(a: &Value, b: &Value) -> Result<bool, String> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        (Value::Float(x), Value::Float(y)) => Ok(x == y),
        (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
        (Value::Str(x), Value::Str(y)) => Ok(x == y),
        (Value::Unit, Value::Unit) => Ok(true),
        _ => Err(format!("type error: cannot compare {a:?} and {b:?} for equality")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plum_ir::lower::{lower_expr, LoweringContext};
    use plum_syntax::lexer::Lexer;
    use plum_syntax::ast;
    use plum_syntax::parser::Parser;

    fn eval(src: &str) -> Value {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ir = lower_expr(&ast, &LoweringContext::new())
            .unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"));
        Interpreter::new()
            .eval(&ir)
            .unwrap_or_else(|e| panic!("eval error for {src:?}: {e}"))
    }

    fn eval_err(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        let ir = lower_expr(&ast, &LoweringContext::new())
            .unwrap_or_else(|e| panic!("lowering error for {src:?}: {e}"));
        Interpreter::new()
            .eval(&ir)
            .expect_err(&format!("expected eval of {src:?} to fail"))
    }

    #[test]
    fn literals() {
        assert_eq!(eval("5"), Value::Int(5));
        assert_eq!(eval("3.14"), Value::Float(3.14));
        assert_eq!(eval("\"hi\""), Value::Str("hi".to_string()));
        assert_eq!(eval("true"), Value::Bool(true));
        assert_eq!(eval("()"), Value::Unit);
    }

    #[test]
    fn integer_arithmetic() {
        assert_eq!(eval("1 + 2"), Value::Int(3));
        assert_eq!(eval("10 - 3"), Value::Int(7));
        assert_eq!(eval("4 * 5"), Value::Int(20));
        assert_eq!(eval("10 / 3"), Value::Int(3));
        assert_eq!(eval("10 % 3"), Value::Int(1));
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        eval_err("5 / 0");
        eval_err("5 % 0");
    }

    #[test]
    fn float_arithmetic() {
        assert_eq!(eval("1.5 + 2.5"), Value::Float(4.0));
    }

    #[test]
    fn mixed_int_float_arithmetic_is_a_type_error() {
        // No implicit numeric coercion — see DESIGN.md's small built-in
        // trait set (Num); this interpreter doesn't implement overload
        // resolution, just checks the operand types match.
        eval_err("1 + 1.0");
    }

    #[test]
    fn comparisons() {
        assert_eq!(eval("3 < 5"), Value::Bool(true));
        assert_eq!(eval("5 <= 5"), Value::Bool(true));
        assert_eq!(eval("5 > 3"), Value::Bool(true));
        assert_eq!(eval("5 >= 6"), Value::Bool(false));
    }

    #[test]
    fn equality() {
        assert_eq!(eval("3 == 3"), Value::Bool(true));
        assert_eq!(eval("3 != 4"), Value::Bool(true));
        assert_eq!(eval("\"a\" == \"a\""), Value::Bool(true));
        assert_eq!(eval("true == false"), Value::Bool(false));
    }

    #[test]
    fn logical_and_or_short_circuit() {
        // The right-hand side must NOT be evaluated when the left side
        // already determines the result — proven here by putting a
        // division-by-zero on the side that should never run. If
        // short-circuiting were broken, these would error instead of
        // returning cleanly.
        assert_eq!(eval("false && (1 / 0 == 0)"), Value::Bool(false));
        assert_eq!(eval("true || (1 / 0 == 0)"), Value::Bool(true));
        assert_eq!(eval("true && false"), Value::Bool(false));
        assert_eq!(eval("false || true"), Value::Bool(true));
    }

    #[test]
    fn unary_operators() {
        assert_eq!(eval("-5"), Value::Int(-5));
        assert_eq!(eval("-3.14"), Value::Float(-3.14));
        assert_eq!(eval("!true"), Value::Bool(false));
    }

    #[test]
    fn if_expression() {
        assert_eq!(eval("if true { 1 } else { 2 }"), Value::Int(1));
        assert_eq!(eval("if false { 1 } else { 2 }"), Value::Int(2));
    }

    #[test]
    fn if_condition_must_be_bool() {
        eval_err("if 5 { 1 } else { 2 }");
    }

    #[test]
    fn let_binding() {
        assert_eq!(eval("{ let x = 5; x + 1 }"), Value::Int(6));
    }

    #[test]
    fn let_shadowing_inner_wins() {
        assert_eq!(eval("{ let x = 1; let x = 2; x }"), Value::Int(2));
    }

    #[test]
    fn let_binding_does_not_leak_after_its_scope() {
        // Both `x`s are independent — the outer `x` is unreachable from
        // inside the inner block's own `x`, and once a `let`'s body
        // finishes evaluating, that binding is gone.
        assert_eq!(eval("{ let x = 1; { let x = x + 1; x } }"), Value::Int(2));
    }

    #[test]
    fn unbound_variable_is_an_error() {
        eval_err("y");
    }

    #[test]
    fn calling_an_undefined_function_is_an_error() {
        // Calls themselves work now (see the "real function
        // definitions and calls" tests below) — this specific `eval`
        // helper never loads any program, so `f` is genuinely unknown
        // here, not "calls aren't supported" in general anymore.
        eval_err("f(1)");
    }

    // --- `for` loops: real surface syntax, since lowering supports it
    // now (unlike Ctor/Match/CtorReuse below, which still need hand-
    // built IR trees).

    #[test]
    fn for_loop_evaluates_to_unit() {
        assert_eq!(eval("for i in 0..5 { i }"), Value::Unit);
    }

    #[test]
    fn for_loop_over_an_empty_range_does_not_iterate_and_is_still_unit() {
        assert_eq!(eval("for i in 5..5 { i }"), Value::Unit);
        assert_eq!(eval("for i in 5..0 { i }"), Value::Unit);
    }

    #[test]
    fn for_loop_bounds_can_be_arbitrary_expressions() {
        assert_eq!(eval("{ let n = 3; for i in 0..n { i } }"), Value::Unit);
    }

    #[test]
    fn range_is_a_first_class_value_stored_in_a_let_binding() {
        // No field-access syntax exists yet, so the result is checked
        // by using the bound range directly as a `for`-loop iterand
        // rather than reading its start/end back out.
        let src = "{ let mut sum = 0; let r = 0..3; for i in r { sum = sum + i; }; sum }";
        assert_eq!(eval(src), Value::Int(3));
    }

    #[test]
    fn for_loop_over_a_range_passed_as_a_function_argument() {
        let src = "let sum_range r = { let mut sum = 0; for i in r { sum = sum + i; }; sum }\n\
                    let use_it dummy = sum_range(0..5)";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(10));
    }

    #[test]
    fn for_loop_variable_does_not_leak_past_the_loop() {
        eval_err("{ for i in 0..3 { i }; i }");
    }

    #[test]
    fn for_loop_does_not_disturb_code_after_it() {
        assert_eq!(eval("{ for i in 0..3 { i }; 42 }"), Value::Int(42));
    }

    #[test]
    fn for_loop_bounds_must_be_int() {
        eval_err("for i in true..false { i }");
    }

    #[test]
    fn for_loop_body_error_propagates_out_of_the_loop() {
        eval_err("for i in 0..3 { 1 / 0 }");
    }

    #[test]
    fn for_loop_actually_runs_the_body_once_per_iteration() {
        // Nothing observable exists inside the language itself (no I/O,
        // no mutation) to watch a loop run — but each iteration
        // constructing a struct is a real heap allocation, and
        // `alloc_count` is exposed for exactly this kind of proof (see
        // its doc comment). Proves the loop runs exactly `n` times, not
        // once, and not "not at all."
        let src = "struct Boxed { x: Int }\n\
                    let make_n n = for i in 0..n { Boxed { x: i } }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = plum_ir::lower::lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program).unwrap_or_else(|e| panic!("load error: {e}"));
        let result = interp
            .call("make_n", vec![Value::Int(5)])
            .unwrap_or_else(|e| panic!("call error: {e}"));
        assert_eq!(result, Value::Unit);
        assert_eq!(interp.alloc_count(), 5);
    }

    // --- Zero-parameter top-level `let` (globals) ---

    #[test]
    fn a_global_is_referenced_bare() {
        assert_eq!(run("let x = 5\nlet use_it n = x + n", "use_it", vec![Value::Int(1)]), Value::Int(6));
    }

    #[test]
    fn a_global_can_reference_an_earlier_global() {
        assert_eq!(
            run("let a = 1\nlet b = a + 1\nlet use_it n = b + n", "use_it", vec![Value::Int(1)]),
            Value::Int(3)
        );
    }

    #[test]
    fn a_global_can_call_a_function_regardless_of_declaration_order() {
        // `double` is declared AFTER `x` textually — fine, since
        // functions are all registered before ANY global evaluates.
        assert_eq!(run("let x = double(5)\nlet double n = n * 2\nlet use_it dummy = x", "use_it", vec![Value::Unit]), Value::Int(10));
    }

    #[test]
    fn a_function_can_reference_a_global_declared_earlier() {
        assert_eq!(run("let pi_ish = 3\nlet area r = pi_ish * r * r", "area", vec![Value::Int(2)]), Value::Int(12));
    }

    #[test]
    fn globals_are_evaluated_exactly_once() {
        // Each call to `use_it` re-reads the SAME already-evaluated
        // global — proven by allocating a heap value as a global and
        // checking `alloc_count` stays 1 across multiple calls, not
        // growing per call.
        let src = "struct Boxed { x: Int }\n\
                    let origin = Boxed { x: 0 }\n\
                    let use_it dummy = match origin { Boxed(x) => x }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program).unwrap_or_else(|e| panic!("load error: {e}"));
        assert_eq!(interp.alloc_count(), 1);
        interp.call("use_it", vec![Value::Unit]).unwrap();
        interp.call("use_it", vec![Value::Unit]).unwrap();
        interp.call("use_it", vec![Value::Unit]).unwrap();
        assert_eq!(interp.alloc_count(), 1, "the global's struct must be allocated once, not once per call");
    }

    #[test]
    fn a_failing_global_initializer_fails_load_program() {
        let tokens = Lexer::new("let x = 1 / 0").tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap();
        let ctx = LoweringContext::new();
        let ir_program = lower_program(&program, &ctx).unwrap();
        let mut interp = Interpreter::new();
        assert!(interp.load_program(&ir_program).is_err());
    }

    // --- Mutation (`let mut` + assignment) ---

    #[test]
    fn assign_mutates_an_existing_binding() {
        assert_eq!(eval("{ let mut x = 5; x = 6; x }"), Value::Int(6));
    }

    #[test]
    fn assign_value_can_reference_the_current_binding() {
        assert_eq!(eval("{ let mut x = 5; x = x + 1; x }"), Value::Int(6));
    }

    #[test]
    fn multiple_assigns_apply_in_order() {
        assert_eq!(eval("{ let mut x = 0; x = x + 1; x = x + 1; x = x + 1; x }"), Value::Int(3));
    }

    #[test]
    fn assign_to_an_undefined_variable_is_an_error() {
        eval_err("x = 5");
    }

    #[test]
    fn assign_mutates_the_innermost_shadowed_binding_only() {
        // Two DIFFERENT `x`s — the inner block's assignment must not
        // touch the outer one at all.
        assert_eq!(
            eval("{ let mut x = 1; { let mut x = 2; x = 99; }; x }"),
            Value::Int(1)
        );
    }

    #[test]
    fn a_closure_capturing_a_mutable_variable_sees_the_snapshot_at_capture_time() {
        // Same "capture is a snapshot" behavior proven for shadowing
        // earlier — reassignment after a closure captures its
        // environment doesn't retroactively change what the closure
        // sees either, since `captured` is a plain value copy, not a
        // live reference into `env`.
        assert_eq!(
            eval("{ let mut n = 1; let f = |x| x + n; n = 99; f(0) }"),
            Value::Int(1)
        );
    }

    #[test]
    fn the_classic_for_loop_accumulator_from_design_md() {
        // DESIGN.md's own motivating example for `let mut`: "the
        // classic for-loop-with-an-accumulator case." First real proof
        // that `for` and mutation compose into something actually
        // useful — up to now `for` could only ever produce Unit.
        assert_eq!(
            eval("{ let mut sum = 0; for i in 0..5 { sum = sum + i; }; sum }"),
            Value::Int(10)
        );
    }

    // --- Closures ---

    #[test]
    fn calling_a_closure_literal_directly() {
        assert_eq!(eval("(|x| x + 1)(5)"), Value::Int(6));
    }

    #[test]
    fn calling_a_closure_stored_in_a_variable() {
        assert_eq!(eval("{ let f = |x| x + 1; f(5) }"), Value::Int(6));
    }

    #[test]
    fn closure_with_multiple_params() {
        assert_eq!(eval("{ let add = |a, b| a + b; add(2, 3) }"), Value::Int(5));
    }

    #[test]
    fn closure_with_zero_params() {
        assert_eq!(eval("{ let five = || 5; five() }"), Value::Int(5));
    }

    #[test]
    fn closure_captures_the_defining_environment() {
        assert_eq!(eval("{ let n = 10; let add_n = |x| x + n; add_n(5) }"), Value::Int(15));
    }

    #[test]
    fn closure_capture_is_a_snapshot_not_a_live_reference() {
        // `n` inside the closure is whatever it was AT CREATION time —
        // the inner shadowing `let n = 99` never reaches a closure
        // already captured before it exists.
        assert_eq!(
            eval("{ let n = 1; let f = |x| x + n; let n = 99; f(0) }"),
            Value::Int(1)
        );
    }

    #[test]
    fn closure_param_shadows_a_captured_binding_of_the_same_name() {
        assert_eq!(eval("{ let x = 100; let f = |x| x + 1; f(5) }"), Value::Int(6));
    }

    #[test]
    fn wrong_closure_argument_count_is_an_error() {
        eval_err("(|x, y| x + y)(1)");
    }

    #[test]
    fn calling_a_non_function_value_is_an_error() {
        eval_err("5(1)");
    }

    #[test]
    fn a_named_function_can_receive_a_closure_argument_and_call_it() {
        // `apply`'s param `f` is bound to a Closure value like any
        // other value — no special-casing needed there. Inside
        // `apply`'s body, `f(x)` calls it: `f` isn't a known top-level
        // function name, so the Call falls through to evaluating it as
        // an ordinary expression, finds a Closure in `env`, and calls
        // that.
        let src = "let apply f x = f(x)\n\
                    let use_it n = apply(|x| x + 1, n)";
        assert_eq!(run(src, "use_it", vec![Value::Int(5)]), Value::Int(6));
    }

    #[test]
    fn a_closure_can_call_a_named_top_level_function() {
        let src = "let double n = n * 2\n\
                    let use_it n = { let f = |x| double(x); f(n) }";
        assert_eq!(run(src, "use_it", vec![Value::Int(5)]), Value::Int(10));
    }

    // --- `spawn` / `.join()` — a real OS thread, per DESIGN.md's
    // "OS threads first" concurrency model.

    #[test]
    fn spawn_runs_the_block_on_a_real_thread_and_join_returns_its_result() {
        assert_eq!(eval("spawn { 1 + 2 }.join()"), Value::Int(3));
    }

    #[test]
    fn spawn_can_call_a_named_top_level_function() {
        let src = "let double n = n * 2\n\
                    let use_it n = spawn { double(n) }.join()";
        assert_eq!(run(src, "use_it", vec![Value::Int(5)]), Value::Int(10));
    }

    #[test]
    fn spawn_captures_local_bindings_deep_copied_into_the_new_thread() {
        // `n` is a plain Int, trivially portable — proves the captured-
        // environment crossing mechanism works for the simple case
        // before the heap-shaped case below.
        assert_eq!(eval("{ let n = 41; spawn { n + 1 }.join() }"), Value::Int(42));
    }

    #[test]
    fn spawn_captures_a_heap_shaped_local_via_deep_copy() {
        // `p` is a struct (heap-shaped) — this is the actual DECIDED
        // mechanism (deep-copy across the thread boundary, not a
        // shared heap) actually being exercised, not just primitives.
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; spawn { match p { Point(a, b) => a + b } }.join() }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    #[test]
    fn spawned_tasks_result_is_usable_back_in_the_joining_threads_own_heap() {
        // The result crosses BACK too — constructed in the spawned
        // thread's heap, `.join()` deep-copies it into the CALLING
        // thread's heap, and it's usable there afterward (matched,
        // not just discarded) exactly like any other value.
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let t = spawn { Point { x: 1, y: 2 } }; match t.join() { Point(a, b) => a + b } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(3));
    }

    #[test]
    fn nested_spawn_inside_a_spawned_task() {
        assert_eq!(eval("spawn { spawn { 5 }.join() + 1 }.join()"), Value::Int(6));
    }

    #[test]
    fn joining_the_same_task_twice_is_a_runtime_error() {
        let src = "let use_it dummy = { let t = spawn { 1 }; { t.join(); t.join() } }";
        let err = run_err(src, "use_it", vec![Value::Unit]);
        assert!(err.contains("already joined"), "expected an already-joined error, got: {err}");
    }

    #[test]
    fn a_spawned_task_that_errors_surfaces_the_error_at_join() {
        let src = "let use_it dummy = spawn { 1 / 0 }.join()";
        let err = run_err(src, "use_it", vec![Value::Unit]);
        assert!(err.contains("spawned task failed"), "expected a propagated task error, got: {err}");
    }

    #[test]
    fn joining_a_non_task_value_is_a_runtime_error() {
        eval_err("5.join()");
    }

    // --- `channel[T]()` / `.send()` / `.recv()` ---

    #[test]
    fn send_then_recv_round_trips_a_value_on_the_same_thread() {
        let src = "{ let (tx, rx) = channel[Int](); tx.send(42); rx.recv() }";
        assert_eq!(eval(src), Value::Int(42));
    }

    #[test]
    fn multiple_sends_are_received_in_order() {
        let src = "{ let (tx, rx) = channel[Int]();\
                     tx.send(1); tx.send(2); tx.send(3);\
                     rx.recv() + rx.recv() + rx.recv() }";
        assert_eq!(eval(src), Value::Int(6));
    }

    #[test]
    fn a_heap_shaped_value_crosses_a_channel_via_deep_copy() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let (tx, rx) = channel[Point]();\
                    tx.send(Point { x: 3, y: 4 });\
                    match rx.recv() { Point(a, b) => a + b } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    #[test]
    fn a_channel_sender_can_be_captured_by_a_spawned_task() {
        // The end-to-end concurrency shape: `tx` crosses INTO the
        // spawned thread (via the SAME captured-environment mechanism
        // `spawn` already uses), the spawned task sends on it, and the
        // ORIGINAL thread receives — real cross-thread communication,
        // not just same-thread round-tripping.
        let src = "let use_it dummy = { let (tx, rx) = channel[Int]();\
                    let t = spawn { tx.send(99) };\
                    let received = rx.recv();\
                    t.join();\
                    received }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(99));
    }

    #[test]
    fn a_heap_shaped_value_crosses_a_real_thread_boundary_via_a_channel() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let (tx, rx) = channel[Point]();\
                    let t = spawn { tx.send(Point { x: 5, y: 6 }) };\
                    let p = rx.recv();\
                    t.join();\
                    match p { Point(a, b) => a + b } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(11));
    }

    // A "sender dropped -> recv errors" test was attempted and removed:
    // the toy heap's `Ctor` cell created by `channel[T]()` holds its
    // OWN clone of the `Sender`, and this interpreter's simulated
    // refcounting only frees a cell via explicit FBIP-inserted `Dec`
    // ops — nothing here reliably drives that cell's refcount to zero
    // within a single test, so the real `mpsc::Sender` never actually
    // drops and `.recv()` hangs forever instead of erroring. Not a
    // regression to fix now: a genuinely disconnected channel (every
    // `Sender` clone this INTERPRETER can reach truly gone) still
    // correctly errors — see `ChannelRecv`'s eval arm — this is only
    // about reliably CONSTRUCTING that scenario in a test under the
    // current toy heap's leak-until-FBIP-frees-it model.

    #[test]
    fn recv_on_an_empty_channel_blocks_until_a_value_arrives() {
        // Proves `.recv()` genuinely BLOCKS (doesn't error/return
        // early on an empty channel) by having the send happen on a
        // DELAYED spawned task — if `.recv()` didn't block correctly,
        // this would either error immediately or hang forever; instead
        // it should complete once the task actually sends.
        let src = "let use_it dummy = { let (tx, rx) = channel[Int]();\
                    let t = spawn { tx.send(7) };\
                    let v = rx.recv();\
                    t.join();\
                    v }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    // --- `select` ---

    #[test]
    fn select_picks_the_one_arm_with_a_value_already_ready() {
        let src = "{ let (tx, rx) = channel[Int](); tx.send(5); select { v = rx.recv() => v } }";
        assert_eq!(eval(src), Value::Int(5));
    }

    #[test]
    fn select_picks_whichever_of_several_channels_is_ready() {
        let src = "{ let (tx1, rx1) = channel[Int]();\
                     let (tx2, rx2) = channel[Int]();\
                     tx2.send(99);\
                     select { v = rx1.recv() => v, w = rx2.recv() => w } }";
        assert_eq!(eval(src), Value::Int(99));
    }

    #[test]
    fn select_wildcard_arm_discards_the_received_value() {
        let src = "{ let (tx, rx) = channel[Int](); tx.send(5); select { _ = rx.recv() => 42 } }";
        assert_eq!(eval(src), Value::Int(42));
    }

    #[test]
    fn select_receives_a_heap_shaped_value() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let (tx, rx) = channel[Point]();\
                    tx.send(Point { x: 3, y: 4 });\
                    select { p = rx.recv() => match p { Point(a, b) => a + b } } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    #[test]
    fn select_waits_for_a_delayed_send_from_a_spawned_task() {
        // The genuine end-to-end concurrency shape: nothing is ready
        // when `select` starts polling — it correctly blocks (busy-
        // polls) until the SPAWNED task actually sends, rather than
        // erroring or returning early.
        let src = "let use_it dummy = { let (tx, rx) = channel[Int]();\
                    let t = spawn { tx.send(11) };\
                    let v = select { v = rx.recv() => v };\
                    t.join();\
                    v }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(11));
    }

    #[test]
    fn selecting_on_a_non_receiver_value_is_a_runtime_error() {
        eval_err("select { v = 5.recv() => v }");
    }

    // --- Arrays ---

    #[test]
    fn array_literal_and_index_round_trip() {
        assert_eq!(eval("[10, 20, 30][1]"), Value::Int(20));
    }

    #[test]
    fn array_index_zero_and_last() {
        assert_eq!(eval("[7, 8, 9][0]"), Value::Int(7));
        assert_eq!(eval("[7, 8, 9][2]"), Value::Int(9));
    }

    #[test]
    fn array_index_out_of_bounds_is_a_runtime_error() {
        eval_err("[1, 2, 3][5]");
    }

    #[test]
    fn array_index_negative_is_a_runtime_error() {
        eval_err("[1, 2, 3][0 - 1]");
    }

    #[test]
    fn array_len() {
        assert_eq!(eval("[1, 2, 3].len()"), Value::Int(3));
        assert_eq!(eval("[].len()"), Value::Int(0));
    }

    #[test]
    fn array_push_returns_a_new_longer_array() {
        assert_eq!(eval("[1, 2].push(3).len()"), Value::Int(3));
        assert_eq!(eval("[1, 2].push(3)[2]"), Value::Int(3));
    }

    #[test]
    fn array_push_does_not_mutate_the_original() {
        // Purely functional semantics: `a` is unaffected by `a.push(_)`
        // — this is what makes future in-place-growth optimization
        // (still deferred — see ir.rs's `ArrayPush` doc comment) purely
        // a PERFORMANCE change later, never a semantic one.
        let src = "{ let a = [1, 2]; let b = a.push(3); a.len() }";
        assert_eq!(eval(src), Value::Int(2));
    }

    #[test]
    fn array_pop_returns_a_new_shorter_array() {
        assert_eq!(eval("[1, 2, 3].pop().len()"), Value::Int(2));
        assert_eq!(eval("[1, 2, 3].pop()[1]"), Value::Int(2));
    }

    #[test]
    fn array_pop_does_not_mutate_the_original() {
        let src = "{ let a = [1, 2, 3]; let b = a.pop(); a.len() }";
        assert_eq!(eval(src), Value::Int(3));
    }

    #[test]
    fn popping_an_empty_array_is_a_runtime_error() {
        eval_err("[].pop()");
    }

    #[test]
    fn array_set_replaces_the_element_at_the_given_index() {
        assert_eq!(eval("[1, 2, 3].set(1, 99)[1]"), Value::Int(99));
        assert_eq!(eval("[1, 2, 3].set(1, 99).len()"), Value::Int(3));
    }

    #[test]
    fn array_set_does_not_mutate_the_original() {
        let src = "{ let a = [1, 2, 3]; let b = a.set(0, 99); a[0] }";
        assert_eq!(eval(src), Value::Int(1));
    }

    #[test]
    fn array_set_out_of_bounds_is_a_runtime_error() {
        eval_err("[1, 2, 3].set(5, 0)");
        eval_err("[1, 2, 3].set(-1, 0)");
    }

    #[test]
    fn array_remove_shifts_later_elements_down() {
        assert_eq!(eval("[1, 2, 3].remove(1).len()"), Value::Int(2));
        assert_eq!(eval("[1, 2, 3].remove(1)[1]"), Value::Int(3));
    }

    #[test]
    fn array_remove_does_not_mutate_the_original() {
        let src = "{ let a = [1, 2, 3]; let b = a.remove(0); a.len() }";
        assert_eq!(eval(src), Value::Int(3));
    }

    #[test]
    fn array_remove_out_of_bounds_is_a_runtime_error() {
        eval_err("[1, 2, 3].remove(5)");
    }

    #[test]
    fn array_map_applies_the_function_to_every_element() {
        assert_eq!(eval("[1, 2, 3].map(|x| x * 2).len()"), Value::Int(3));
        assert_eq!(eval("[1, 2, 3].map(|x| x * 2)[0]"), Value::Int(2));
        assert_eq!(eval("[1, 2, 3].map(|x| x * 2)[2]"), Value::Int(6));
    }

    #[test]
    fn array_map_on_an_empty_array_is_empty() {
        assert_eq!(eval("[].map(|x| x * 2).len()"), Value::Int(0));
    }

    #[test]
    fn array_filter_keeps_only_matching_elements() {
        assert_eq!(eval("[1, 2, 3, 4].filter(|x| x > 2).len()"), Value::Int(2));
        assert_eq!(eval("[1, 2, 3, 4].filter(|x| x > 2)[0]"), Value::Int(3));
    }

    #[test]
    fn array_filter_preserves_relative_order() {
        assert_eq!(eval("[3, 1, 4, 1, 5].filter(|x| x > 2)[1]"), Value::Int(4));
    }

    #[test]
    fn array_fold_accumulates_left_to_right() {
        assert_eq!(eval("[1, 2, 3, 4].fold(0, |acc, x| acc + x)"), Value::Int(10));
    }

    #[test]
    fn array_fold_on_an_empty_array_returns_the_initial_value() {
        assert_eq!(eval("[].fold(42, |acc, x| acc + x)"), Value::Int(42));
    }

    #[test]
    fn array_of_heap_shaped_values() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let arr = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];\
                    match arr[1] { Point(a, b) => a + b } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    #[test]
    fn indexing_a_non_array_is_a_runtime_error() {
        eval_err("5[0]");
    }

    // --- A bare top-level function name as a first-class value ---

    #[test]
    fn a_bare_function_name_stored_in_a_variable_can_be_called() {
        let src = "let square x = x * x\n\
                    let use_it n = { let f = square; f(n) }";
        assert_eq!(run(src, "use_it", vec![Value::Int(5)]), Value::Int(25));
    }

    #[test]
    fn a_bare_function_name_passed_as_a_higher_order_argument() {
        // `square` itself, not a closure wrapping it — proves a
        // top-level function is a real value usable anywhere a closure
        // would be, not just storable in a `let`.
        let src = "let square x = x * x\n\
                    let apply f x = f(x)\n\
                    let use_it n = apply(square, n)";
        assert_eq!(run(src, "use_it", vec![Value::Int(4)]), Value::Int(16));
    }

    #[test]
    fn a_global_aliasing_a_function_can_be_called_through() {
        let src = "let square x = x * x\n\
                    let f = square\n\
                    let use_it n = f(n)";
        assert_eq!(run(src, "use_it", vec![Value::Int(6)]), Value::Int(36));
    }

    #[test]
    fn calling_directly_by_name_still_works_alongside_first_class_values() {
        // A local binding of the SAME name as a real function shadows
        // it, matching ordinary lexical scoping — `lookup`'s function
        // fallback only kicks in when nothing else already bound the
        // name.
        let src = "let square x = x * x\n\
                    let use_it n = { let square = |x| x + 1; square(n) }";
        assert_eq!(run(src, "use_it", vec![Value::Int(5)]), Value::Int(6));
    }

    // --- Nested patterns ---

    #[test]
    fn struct_nested_inside_tuple_pattern() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, y: 4 }, 10) { (Point { x, y }, n) => x + y + n }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(17));
    }

    #[test]
    fn struct_nested_inside_struct_pattern() {
        let src = "struct Point { x: Int, y: Int }\n\
                    struct Line { start: Point, end: Point }\n\
                    let dx (Line { start: Point { x: x0, .. }, end: Point { x: x1, .. } }) = x1 - x0\n\
                    let use_it dummy = dx(Line { start: Point { x: 1, y: 0 }, end: Point { x: 9, y: 0 } })";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(8));
    }

    #[test]
    fn variant_pattern_syntax_nested_inside_tuple_pattern() {
        // `Point(x, y)` is `Pattern::Variant` syntax (distinct from the
        // `Point { x, y }` struct-pattern tests above) — matched here
        // against a struct value, proving a Variant-shaped nested
        // pattern works regardless of what it's matching against.
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, y: 4 }, 1) { (Point(x, y), n) => x + y + n }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(8));
    }

    #[test]
    fn real_enum_variant_nested_inside_tuple_pattern() {
        // Now that variant CONSTRUCTION expressions are lowered too
        // (previously only variant PATTERNS and struct literals were),
        // this proves the same nesting with a genuine enum end to end:
        // constructing `Circle(5.0)`, then destructuring it nested
        // inside a tuple pattern.
        let src = "enum Shape { Circle(Float) }\n\
                    let use_it dummy = match (Circle(5.0), 1.0) { (Shape.Circle(r), n) => r + n }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(6.0));
    }

    #[test]
    fn deeply_nested_pattern_three_levels() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match ((Point { x: 1, y: 2 }, 3), 4) { ((Point { x, y }, a), b) => x + y + a + b }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(10));
    }

    // --- Variant construction ---

    #[test]
    fn bare_variant_construction_and_immediate_match() {
        let src = "enum Shape { Circle(Float) }\n\
                    let use_it dummy = match Circle(3.0) { Circle(r) => r }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(3.0));
    }

    #[test]
    fn qualified_variant_construction_and_immediate_match() {
        let src = "enum Shape { Circle(Float) }\n\
                    let use_it dummy = match Shape.Circle(3.0) { Circle(r) => r }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(3.0));
    }

    #[test]
    fn zero_arity_variant_construction() {
        let src = "enum Shape { Empty, Circle(Float) }\n\
                    let use_it dummy = match Empty { Empty => 1, Circle(r) => 0 }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(1));
    }

    #[test]
    fn multi_field_variant_construction() {
        let src = "enum Shape { Rectangle(Float, Float) }\n\
                    let use_it dummy = match Rectangle(2.0, 3.0) { Rectangle(w, h) => w * h }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(6.0));
    }

    #[test]
    fn bare_variant_constructor_is_a_real_callable_value() {
        let src = "enum Shape { Circle(Float) }\n\
                    let use_it dummy = { let make = Circle; match make(3.0) { Circle(r) => r } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(3.0));
    }

    #[test]
    fn bare_variant_constructor_passed_as_a_higher_order_argument() {
        let src = "enum Shape { Circle(Float) }\n\
                    let apply f x = f(x)\n\
                    let use_it dummy = match apply(Circle, 5.0) { Circle(r) => r }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Float(5.0));
    }

    // --- Struct patterns (`Point { x, y }`) ---

    #[test]
    fn struct_destructuring_param() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let area (Point { x, y }) = x * y\n\
                    let use_it dummy = area(Point { x: 3, y: 4 })";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(12));
    }

    #[test]
    fn struct_destructuring_param_field_rename() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let area (Point { x: px, y: py }) = px * py\n\
                    let use_it dummy = area(Point { x: 3, y: 4 })";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(12));
    }

    #[test]
    fn struct_destructuring_param_with_rest() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let get_x (Point { x, .. }) = x\n\
                    let use_it dummy = get_x(Point { x: 3, y: 4 })";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(3));
    }

    #[test]
    fn match_arm_struct_pattern() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, y: 4 }) { Point { x, y } => x * y }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(12));
    }

    #[test]
    fn match_guard_falls_through_to_the_next_arm_sharing_the_same_tag() {
        let src = "enum Shape { Circle(Float) }\n\
                    let classify r = match (Circle(r)) { Circle(r) if r > 5.0 => 1, Circle(r) => 0 }";
        assert_eq!(run(src, "classify", vec![Value::Float(10.0)]), Value::Int(1));
        assert_eq!(run(src, "classify", vec![Value::Float(2.0)]), Value::Int(0));
    }

    #[test]
    fn match_guard_can_reference_the_arms_own_bindings() {
        let src = "enum Shape { Circle(Float), Rectangle(Float, Float) }\n\
                    let use_it dummy = match (Rectangle(3.0, 3.0)) { Rectangle(w, h) if w == h => 1, Rectangle(w, h) => 0, Circle(r) => 2 }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(1));
    }

    #[test]
    fn no_matching_guarded_arm_is_a_runtime_error() {
        let src = "enum Shape { Circle(Float) }\n\
                    let use_it dummy = match (Circle(1.0)) { Circle(r) if r > 5.0 => 1 }";
        let err = run_err(src, "use_it", vec![Value::Unit]);
        assert!(err.contains("no match arm"), "expected a no-match error, got: {err}");
    }

    #[test]
    fn block_let_struct_destructure() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let Point { x, y } = Point { x: 3, y: 4 }; x + y }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    #[test]
    fn struct_literal_spread_copies_omitted_fields() {
        // No field-access syntax exists yet, so the result is checked
        // by matching back out of it rather than `q.x + q.y`.
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = { let p = Point { x: 3, y: 4 }; let q = Point { x: 9, ..p }; match q { Point { x, y } => x + y } }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(13));
    }

    #[test]
    fn struct_literal_spread_with_no_fields_overridden_is_a_copy() {
        let src = "struct Point { x: Int, y: Int }\n\
                    let use_it dummy = match (Point { x: 3, ..(Point { x: 3, y: 4 }) }) { Point { x, y } => x + y }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(7));
    }

    // --- Tuples ---

    #[test]
    fn tuple_construct_and_match() {
        assert_eq!(eval("match (1, 2) { (a, b) => a + b }"), Value::Int(3));
    }

    #[test]
    fn three_element_tuple_construct_and_match() {
        assert_eq!(eval("match (1, 2, 3) { (a, b, c) => a + b + c }"), Value::Int(6));
    }

    #[test]
    fn block_let_tuple_destructure() {
        assert_eq!(eval("{ let (a, b) = (1, 2); a + b }"), Value::Int(3));
    }

    #[test]
    fn tuple_of_different_arity_does_not_match() {
        eval_err("match (1, 2) { (a, b, c) => a }");
    }

    #[test]
    fn the_swap_example_from_design_md() {
        // `let swap (a, b) = (b, a)` — the flagship destructuring-param
        // example, now real end to end: a synthetic param, a Match
        // destructure, a real tuple construction on the way back out.
        let src = "let swap p = match p { (a, b) => (b, a) }\n\
                    let use_it dummy = match swap((1, 2)) { (x, y) => x * 10 + y }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(21));
    }

    #[test]
    fn swap_with_real_tuple_destructuring_param_syntax() {
        let src = "let swap (a, b) = (b, a)\n\
                    let use_it dummy = match swap((1, 2)) { (x, y) => x * 10 + y }";
        assert_eq!(run(src, "use_it", vec![Value::Unit]), Value::Int(21));
    }

    #[test]
    fn tuple_reuse_in_place_fires_via_fbip() {
        // Same reuse mechanism as structs — a same-arity deconstruct-
        // then-reconstruct is exactly the shape `mark_reuse` looks for,
        // and tuples lower to the identical `Ctor`/`Match` nodes.
        let tokens = Lexer::new("match p { (a, b) => (b, a) }").tokenize();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_expr().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ir = lower_expr(&ast, &LoweringContext::new()).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let optimized = plum_ir::fbip::optimize(ir);
        let mut interp = Interpreter::new();
        // Construct the initial tuple directly on the heap, matching
        // the capstone struct-reuse test's approach.
        let addr = {
            let tuple_val = interp.eval(&Expr::Ctor {
                tag: "2Tuple".to_string(),
                fields: vec![Expr::Int(1), Expr::Int(2)],
            });
            let Ok(Value::HeapRef(addr)) = tuple_val else { panic!("expected a HeapRef") };
            addr
        };
        interp.env.push(("p".to_string(), Value::HeapRef(addr)));
        let result = interp.eval(&optimized).unwrap_or_else(|e| panic!("eval error: {e}"));
        assert_eq!(result, Value::HeapRef(addr), "reuse should recycle the original cell");
    }

    // --- Heap-shaped values: Ctor/Match/CtorReuse/RcAnnotated ---
    //
    // No surface syntax lowers to these yet (see plum-ir's scope
    // notes), so — same as fbip.rs — these tests construct small
    // ir::Expr trees by hand rather than going through the parser.

    use plum_ir::ir::{MatchArm, RcOp};

    fn var(name: &str) -> Expr {
        Expr::Var(name.to_string())
    }
    fn int(n: i64) -> Expr {
        Expr::Int(n)
    }
    fn ctor(tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::Ctor {
            tag: tag.to_string(),
            fields,
        }
    }
    fn ctor_reuse(reuse_of: &str, tag: &str, fields: Vec<Expr>) -> Expr {
        Expr::CtorReuse {
            reuse_of: reuse_of.to_string(),
            tag: tag.to_string(),
            fields,
        }
    }
    fn let_(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let {
            name: name.to_string(),
            value: Box::new(value),
            body: Box::new(body),
        }
    }
    fn match_(scrutinee: Expr, arms: Vec<MatchArm>) -> Expr {
        Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        }
    }
    fn arm(tag: &str, bindings: Vec<&str>, body: Expr) -> MatchArm {
        MatchArm {
            tag: tag.to_string(),
            bindings: bindings.into_iter().map(|s| s.to_string()).collect(),
            guard: None,
            body,
        }
    }
    fn inc(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Inc,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }
    fn dec(name: &str, rest: Expr) -> Expr {
        Expr::RcAnnotated {
            op: RcOp::Dec,
            target: name.to_string(),
            rest: Box::new(rest),
        }
    }

    fn eval_ir(ir: &Expr) -> Value {
        Interpreter::new()
            .eval(ir)
            .unwrap_or_else(|e| panic!("eval error: {e}"))
    }

    fn eval_ir_err(ir: &Expr) -> String {
        Interpreter::new()
            .eval(ir)
            .expect_err("expected evaluation to fail")
    }

    #[test]
    fn construct_and_match_extracts_fields() {
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            match_(var("p"), vec![arm("Point", vec!["x", "y"], Expr::Binary(BinOp::Add, Box::new(var("x")), Box::new(var("y"))))]),
        );
        assert_eq!(eval_ir(&program), Value::Int(3));
    }

    #[test]
    fn match_with_no_matching_arm_is_an_error() {
        let program = let_(
            "p",
            ctor("Foo", vec![]),
            match_(var("p"), vec![arm("Bar", vec![], int(0))]),
        );
        let err = eval_ir_err(&program);
        assert!(err.contains("Foo"), "error should mention the unmatched tag, got: {err}");
    }

    #[test]
    fn match_on_non_heap_scrutinee_is_an_error() {
        let program = match_(int(5), vec![arm("Whatever", vec![], int(0))]);
        eval_ir_err(&program);
    }

    #[test]
    fn rc_inc_dec_actually_change_the_refcount() {
        // Proves the interpreter's RcAnnotated evaluation is really
        // wired to the heap (not just heap.rs's own unit tests of the
        // Heap type in isolation): alloc (rc=1) -> inc (rc=2) -> dec
        // (rc=1, must still be alive — if Inc were a no-op, this dec
        // alone would already free it) -> dec again (rc=0, now freed).
        // A bare `Var(p)` never dereferences the heap, so "still
        // alive"/"freed" can only be proven by actually reading via
        // Match at each checkpoint.
        let read = || match_(var("p"), vec![arm("Point", vec![], int(0))]);

        let still_alive = let_("p", ctor("Point", vec![]), inc("p", dec("p", read())));
        assert_eq!(eval_ir(&still_alive), Value::Int(0), "should still be readable after inc+dec");

        let now_freed = let_("p", ctor("Point", vec![]), inc("p", dec("p", dec("p", read()))));
        let err = eval_ir_err(&now_freed);
        assert!(err.contains("free"), "expected a use-after-free error, got: {err}");
    }

    #[test]
    fn dec_to_zero_makes_the_value_unreadable_afterward() {
        // Construct, drop explicitly, then try to match on it — proves
        // Dec actually freed the cell, using the heap's own
        // use-after-free detection as the check.
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            dec("p", match_(var("p"), vec![arm("Point", vec!["x", "y"], var("x"))])),
        );
        let err = eval_ir_err(&program);
        assert!(err.contains("free"), "expected a use-after-free error, got: {err}");
    }

    #[test]
    fn reuse_when_uniquely_owned_recycles_the_same_allocation() {
        // p has exactly one reference (the match itself) — FBIP's
        // analysis would leave this use bare (no preceding Inc), so at
        // runtime the match's implicit ownership transfer means
        // refcount is 1 at the CtorReuse, and reuse should fire.
        let mut interp = Interpreter::new();
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            match_(
                var("p"),
                vec![arm(
                    "Point",
                    vec!["x", "y"],
                    ctor_reuse("p", "Point", vec![var("y"), var("x")]),
                )],
            ),
        );
        let result = interp.eval(&program).unwrap();
        let Value::HeapRef(addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_eq!(addr, 0, "should reuse address 0 — the original Point's cell");
        assert_eq!(interp.alloc_count(), 1, "reuse must not count as a second allocation");
        assert_eq!(interp.refcount(&result).unwrap(), 1);
        let (tag, fields) = interp.heap.read(addr).unwrap();
        assert_eq!(tag, "Point");
        assert_eq!(fields, &[Value::Int(2), Value::Int(1)]);
    }

    #[test]
    fn reuse_when_shared_allocates_fresh_and_preserves_the_alias() {
        // `saved` aliases `p`, so by the time the match runs, the
        // cell's refcount is 2 (bumped by the Inc a real FBIP pass
        // would insert for the non-last use in `saved`'s binding).
        // Reuse must refuse, allocate a NEW cell, and `saved` must
        // still see the ORIGINAL untouched data afterward — this is
        // the actual safety property the whole refcount-gated design
        // exists to guarantee.
        let mut interp = Interpreter::new();
        let program = let_(
            "p",
            ctor("Point", vec![int(1), int(2)]),
            let_(
                "saved",
                inc("p", var("p")), // the Inc a real FBIP pass would insert here
                match_(
                    var("p"),
                    vec![arm(
                        "Point",
                        vec!["x", "y"],
                        ctor_reuse("p", "Point", vec![var("y"), var("x")]),
                    )],
                ),
            ),
        );
        let result = interp.eval(&program).unwrap();
        let Value::HeapRef(new_addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_ne!(new_addr, 0, "must not reuse memory that `saved` still references");
        assert_eq!(interp.alloc_count(), 2, "the shared path must allocate a fresh cell");
        assert_eq!(interp.heap.read(0).unwrap(), ("Point", &[Value::Int(1), Value::Int(2)][..]));
        assert_eq!(interp.heap.read(new_addr).unwrap(), ("Point", &[Value::Int(2), Value::Int(1)][..]));
    }

    // --- The capstone: real Plum SOURCE TEXT, through the entire
    // pipeline for the first time — parse -> lower (resolving a real
    // struct declaration's field order) -> FBIP optimize (refcount
    // insertion + reuse analysis) -> evaluate on the simulated heap.
    // Every other test in this whole thread used hand-built IR
    // specifically to isolate what was being validated; this is where
    // all of it is proven to actually fit together.

    #[test]
    fn end_to_end_real_source_struct_swap_via_reuse() {
        let src = "struct Point { x: Float, y: Float }\n\
                   let result = { let p = Point { x: 1.0, y: 2.0 }; match p { Point(x, y) => Point { x: y, y: x } } }";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);

        let ast::ItemKind::Let(def) = &program.items[1].kind else {
            panic!("expected the second item to be a let definition");
        };
        let ir = lower_expr(&def.body, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let optimized = plum_ir::fbip::optimize(ir);

        let mut interp = Interpreter::new();
        let result = interp.eval(&optimized).unwrap_or_else(|e| panic!("eval error: {e}"));

        let Value::HeapRef(addr) = result else {
            panic!("expected a HeapRef result")
        };
        assert_eq!(addr, 0, "single owner, single use — reuse should recycle the original cell");
        assert_eq!(
            interp.alloc_count(),
            1,
            "the swap should cost zero extra allocations — that's the whole point of FBIP"
        );
        assert_eq!(interp.heap.read(addr).unwrap(), ("Point", &[Value::Float(2.0), Value::Float(1.0)][..]));
    }

    // --- Real function definitions and calls, including recursion —
    // the big remaining gap this whole thread has been flagging since
    // the very first interpreter tests.

    use plum_ir::lower::lower_program;

    fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program).unwrap_or_else(|e| panic!("load error: {e}"));
        interp
            .call(fn_name, args)
            .unwrap_or_else(|e| panic!("call error: {e}"))
    }

    fn run_err(src: &str, fn_name: &str, args: Vec<Value>) -> String {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let ctx = LoweringContext::from_items(&program.items);
        let ir_program = lower_program(&program, &ctx).unwrap_or_else(|e| panic!("lowering error: {e}"));
        let mut interp = Interpreter::new();
        interp.load_program(&ir_program).unwrap_or_else(|e| panic!("load error: {e}"));
        interp.call(fn_name, args).expect_err("expected call to fail")
    }

    #[test]
    fn call_single_param_function() {
        assert_eq!(run("let double n = n * 2", "double", vec![Value::Int(5)]), Value::Int(10));
    }

    #[test]
    fn call_multi_param_function() {
        assert_eq!(
            run("let add a b = a + b", "add", vec![Value::Int(2), Value::Int(3)]),
            Value::Int(5)
        );
    }

    #[test]
    fn recursion() {
        // The capstone this whole thread has been building toward:
        // first real recursive Plum function actually running.
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        assert_eq!(run(src, "sum", vec![Value::Int(5), Value::Int(0)]), Value::Int(15));
    }

    #[test]
    fn cross_function_calls() {
        let src = "let square x = x * x\nlet sum_of_squares a b = square(a) + square(b)";
        assert_eq!(
            run(src, "sum_of_squares", vec![Value::Int(3), Value::Int(4)]),
            Value::Int(25)
        );
    }

    #[test]
    fn wrong_argument_count_is_an_error() {
        run_err("let add a b = a + b", "add", vec![Value::Int(1)]);
    }

    #[test]
    fn unknown_function_is_an_error() {
        run_err("let double n = n * 2", "triple", vec![Value::Int(1)]);
    }

    #[test]
    fn function_body_cannot_see_the_caller_environment() {
        // A function's body must only ever see its own parameters —
        // never whatever locals happened to exist wherever it was
        // called from. Proven by pre-populating the interpreter's env
        // with a binding the function references but never receives
        // as a parameter, and confirming the call still fails to find
        // it — if isolation were broken (e.g. `call` accidentally
        // reused `self.env` instead of swapping in a fresh one), this
        // would incorrectly succeed.
        let src = "let leak_check unused_param = outer_var + 1";
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap();
        let ctx = LoweringContext::new();
        let ir_program = lower_program(&program, &ctx).unwrap();

        let mut interp = Interpreter::new();
        interp.load_program(&ir_program).unwrap();
        interp.env.push(("outer_var".to_string(), Value::Int(99)));

        let err = interp.call("leak_check", vec![Value::Int(0)]).expect_err("expected call to fail");
        assert!(err.contains("outer_var"), "expected an unbound-variable error, got: {err}");
    }

    #[test]
    fn calling_a_computed_non_closure_value_is_an_error() {
        // A non-name callee IS supported now (see the closure tests
        // above) — it's evaluated like any other expression. But
        // whatever it produces still has to actually BE a closure;
        // `if true { 0 } else { 0 }` computes to a plain Int, which
        // isn't callable.
        let call_expr = Expr::Call {
            callee: Box::new(Expr::If {
                cond: Box::new(Expr::Bool(true)),
                then_branch: Box::new(Expr::Int(0)),
                else_branch: Box::new(Expr::Int(0)),
            }),
            args: vec![],
        };
        assert!(Interpreter::new().eval(&call_expr).is_err());
    }
}
