//! Which captured bindings a closure may capture BY VALUE.
//!
//! A captured binding is normally boxed into a heap cell so the closure and
//! the defining scope share one mutable slot. A binding nobody ever writes
//! once it is initialized does not need the sharing: a closure created after
//! the initialization can hold the value itself (`UpvalSource::ParentValue`),
//! and the binding stays a plain register in its own function. Every cell is
//! a heap slot, and a framework's per-object handler closes over dozens of
//! such `const`s.
//!
//! The analysis is a walk of the function in source order, and conservative
//! by NAME: anything it is unsure about keeps the cell. A binding qualifies
//! when it is
//!
//! * a `let`/`const` declared exactly once in the function (not also a `var`,
//!   function, class, catch parameter or parameter, and not directly in a
//!   `switch` case, where a jump can skip its initialization), or a parameter
//!   of a simple parameter list in a function that never names `arguments`
//!   (a mapped `arguments` object aliases the parameters);
//! * never the target of an assignment, update, destructuring or `for` head
//!   anywhere in the function, nested functions included;
//! * initialized before every closure over it is created: no closure that
//!   names it is written before its declaration ends, and no hoisted function
//!   declaration names it;
//!
//! and the function contains no `eval` reference and no `with` statement
//! (both can reach a binding the walk cannot see).

use std::collections::HashSet;

use super::{arrow_free, class_free, fn_node_free, Names};
use crate::parse::ast::{
    Arg, ArrayElem, Arrow, ArrowBody, Class, ClassMember, Expr, ForInit, ForTarget, Function,
    MemberProp, ObjectMember, Params, Pattern, PropKey, Stmt, Target, VarDecl, VarKind,
};

/// The bindings of `captured` a closure may capture by value, for a function
/// with parameters `params` (`simple`: a simple parameter list) and `body`.
/// `arrow`: an arrow, which has no `arguments` of its own.
pub fn value_captures(
    params: &[String],
    params_ast: Option<&Params>,
    body: &[Stmt],
    arrow: bool,
    captured: &HashSet<String>,
) -> HashSet<String> {
    let mut w = Walk::default();
    w.params(params, params_ast);
    for s in body {
        w.stmt(s);
    }
    w.result(params, params_ast, arrow, captured)
}

/// [`value_captures`] for an expression-bodied arrow.
pub fn value_captures_expr(
    params: &[String],
    params_ast: &Params,
    body: &Expr,
    captured: &HashSet<String>,
) -> HashSet<String> {
    let mut w = Walk::default();
    w.params(params, Some(params_ast));
    w.expr(body);
    w.result(params, Some(params_ast), true, captured)
}

/// [`value_captures`] for either arrow body.
pub fn value_captures_arrow(
    params: &[String],
    a: &Arrow,
    captured: &HashSet<String>,
) -> HashSet<String> {
    match &a.body {
        ArrowBody::Block(b) => value_captures(params, Some(&a.params), &b.stmts, true, captured),
        ArrowBody::Expr(e) => value_captures_expr(params, &a.params, e, captured),
    }
}

#[derive(Default)]
struct Walk<'a> {
    /// Nesting depth in functions below the one analysed (0 = its own code).
    depth: u32,
    /// `let`/`const` declarations of the function's own scopes, by name.
    lexical: rustc_hash::FxHashMap<&'a str, u32>,
    /// Names the function's own scopes bind any other way.
    other: Names<'a>,
    /// Names written anywhere (function and nested functions).
    assigned: Names<'a>,
    /// Names whose declaration has completed, in walk order.
    init: Names<'a>,
    /// Names some closure may capture before they are initialized.
    early: Names<'a>,
    /// A `with` or an `eval` reference: nothing qualifies.
    opaque: bool,
    /// `arguments` is named somewhere.
    arguments: bool,
}

impl<'a> Walk<'a> {
    fn result(
        &self,
        params: &[String],
        params_ast: Option<&Params>,
        arrow: bool,
        captured: &HashSet<String>,
    ) -> HashSet<String> {
        if self.opaque {
            return HashSet::new();
        }
        let simple = params_ast.is_none_or(|p| p.simple);
        let params_ok = simple && (arrow || !self.arguments);
        let fine = |n: &str| {
            !self.assigned.contains(n)
                && !self.early.contains(n)
                && !self.other.contains(n)
                && n != "arguments"
                && n != "eval"
        };
        let mut out = HashSet::new();
        for (i, p) in params.iter().enumerate() {
            let once = !params[..i].contains(p) && !params[i + 1..].contains(p);
            if params_ok
                && once
                && captured.contains(p)
                && !self.lexical.contains_key(p.as_str())
                && fine(p)
            {
                out.insert(p.clone());
            }
        }
        for (&n, &count) in &self.lexical {
            if count == 1 && captured.contains(n) && !params.iter().any(|p| p == n) && fine(n) {
                out.insert(n.to_string());
            }
        }
        out
    }

    /// A closure (function, arrow, class, method) whose free names are
    /// `free` is created here.
    fn closure(&mut self, free: Names<'a>, hoisted: bool) {
        if self.depth == 0 {
            for n in free {
                if hoisted || !self.init.contains(n) {
                    self.early.insert(n);
                }
            }
        }
    }

    fn declare(&mut self, name: &'a str, lexical: bool) {
        if self.depth == 0 {
            if lexical {
                *self.lexical.entry(name).or_insert(0) += 1;
            } else {
                self.other.insert(name);
            }
        }
    }

    fn initialized(&mut self, pat: &'a Pattern) {
        if self.depth == 0 {
            let mut names = Names::default();
            super::pattern_names(pat, &mut names);
            self.init.extend(names);
        }
    }

    fn params(&mut self, params: &'a [String], params_ast: Option<&'a Params>) {
        match params_ast {
            Some(p) if !p.simple => {
                // Defaults run in order; a closure in one sees only the
                // parameters before it. Parameter names never qualify here
                // (`result`), so only the closures matter.
                for item in &p.items {
                    self.pattern(item, None);
                }
            }
            _ => {}
        }
        if self.depth == 0 {
            self.init.extend(params.iter().map(String::as_str));
        }
    }

    /// A binding pattern: its defaults and computed keys are evaluated; its
    /// leaves are declared with `kind` (`None`: a parameter).
    fn pattern(&mut self, pat: &'a Pattern, kind: Option<VarKind>) {
        match pat {
            Pattern::Ident(n) => {
                if let Some(k) = kind {
                    self.declare(n, matches!(k, VarKind::Let | VarKind::Const));
                }
            }
            Pattern::Array(elems) => {
                for el in elems.iter().flatten() {
                    self.pattern(&el.pat, kind);
                }
            }
            Pattern::Object { props, rest } => {
                for p in props {
                    self.prop_key(&p.key);
                    self.pattern(&p.value, kind);
                }
                if let Some(r) = rest {
                    self.pattern(r, kind);
                }
            }
            Pattern::Assign { left, right } => {
                self.expr(right);
                self.pattern(left, kind);
            }
            Pattern::Rest(p) => self.pattern(p, kind),
        }
    }

    fn var_decl(&mut self, d: &'a VarDecl) {
        for decl in &d.decls {
            if let Some(init) = &decl.init {
                self.expr(init);
            }
            self.pattern(&decl.id, Some(d.kind));
            self.initialized(&decl.id);
        }
    }

    fn stmts(&mut self, stmts: &'a [Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &'a Stmt) {
        match s {
            Stmt::Block(b) => self.stmts(b),
            Stmt::Empty | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Debugger => {}
            Stmt::Expr(e, _) | Stmt::Throw(e, _) => self.expr(e),
            Stmt::Return(e, _) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test);
                self.stmt(cons);
                if let Some(a) = alt {
                    self.stmt(a);
                }
            }
            Stmt::While { test, body } => {
                self.expr(test);
                self.stmt(body);
            }
            Stmt::DoWhile { body, test } => {
                self.stmt(body);
                self.expr(test);
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                match init {
                    Some(ForInit::Var(d)) => self.var_decl(d),
                    Some(ForInit::Expr(e)) => self.expr(e),
                    None => {}
                }
                if let Some(t) = test {
                    self.expr(t);
                }
                // The update runs after the body, but every iteration but the
                // first runs it before the body: walk it first.
                if let Some(u) = update {
                    self.expr(u);
                }
                self.stmt(body);
            }
            Stmt::ForIn { left, right, body }
            | Stmt::ForOf {
                left, right, body, ..
            } => {
                // The iterated expression runs before the head binds.
                self.expr(right);
                match left {
                    ForTarget::Var(d) => self.var_decl(d),
                    ForTarget::Target(t) => self.target(t),
                }
                self.stmt(body);
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc);
                for case in cases {
                    if let Some(t) = &case.test {
                        self.expr(t);
                    }
                    for st in &case.body {
                        // A jump to a later clause skips this one's
                        // initializations: its lexicals never qualify.
                        if let Stmt::VarDecl(d) = st {
                            if d.kind.is_lexical() {
                                for decl in &d.decls {
                                    let mut names = Names::default();
                                    super::pattern_names(&decl.id, &mut names);
                                    for n in names {
                                        self.declare(n, false);
                                    }
                                }
                            }
                        }
                        self.stmt(st);
                    }
                }
            }
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.stmts(block);
                if let Some(h) = handler {
                    if let Some(p) = &h.param {
                        self.pattern(p, Some(VarKind::Var));
                    }
                    self.stmts(&h.body);
                }
                if let Some(f) = finalizer {
                    self.stmts(f);
                }
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::With { object, body } => {
                self.opaque = true;
                self.expr(object);
                self.stmt(body);
            }
            Stmt::VarDecl(d) => self.var_decl(d),
            Stmt::FnDecl(f) => {
                if let Some(n) = &f.name {
                    self.declare(n, false);
                }
                let mut free = Names::default();
                fn_node_free(f, &mut free);
                self.closure(free, true);
                self.function(f);
            }
            Stmt::ClassDecl(c) => {
                if let Some(n) = &c.name {
                    self.declare(n, false);
                }
                self.class(c);
            }
            // Not in a function body; nothing qualifies if it ever is.
            Stmt::Import(_) | Stmt::Export(_) => self.opaque = true,
        }
    }

    /// Walk inside a nested function, for writes and `eval`/`with` only.
    fn function(&mut self, f: &'a Function) {
        self.depth += 1;
        for item in &f.params.items {
            self.pattern(item, None);
        }
        self.stmts(&f.body.stmts);
        self.depth -= 1;
    }

    fn arrow(&mut self, a: &'a Arrow) {
        let mut free = Names::default();
        arrow_free(a, &mut free);
        self.closure(free, false);
        self.depth += 1;
        for item in &a.params.items {
            self.pattern(item, None);
        }
        match &a.body {
            ArrowBody::Expr(e) => self.expr(e),
            ArrowBody::Block(b) => self.stmts(&b.stmts),
        }
        self.depth -= 1;
    }

    fn function_expr(&mut self, f: &'a Function) {
        let mut free = Names::default();
        fn_node_free(f, &mut free);
        self.closure(free, false);
        self.function(f);
    }

    /// A class is created where it is written; its methods, field
    /// initializers and static blocks are closures made then.
    fn class(&mut self, c: &'a Class) {
        let mut free = Names::default();
        class_free(c, &mut free);
        self.closure(free, false);
        self.depth += 1;
        for d in &c.decorators {
            self.expr(d);
        }
        if let Some(sc) = &c.superclass {
            self.expr(sc);
        }
        for el in &c.body {
            match el {
                ClassMember::Method(m) => {
                    for d in &m.decorators {
                        self.expr(d);
                    }
                    self.prop_key(&m.key);
                    self.function(&m.func);
                }
                ClassMember::Field(p) => {
                    for d in &p.decorators {
                        self.expr(d);
                    }
                    self.prop_key(&p.key);
                    if let Some(v) = &p.value {
                        self.expr(v);
                    }
                    if let Some(acc) = &p.accessor {
                        self.function(&acc.getter);
                        self.function(&acc.setter);
                    }
                }
                ClassMember::StaticBlock(b) => self.stmts(b),
            }
        }
        self.depth -= 1;
    }

    fn prop_key(&mut self, k: &'a PropKey) {
        if let PropKey::Computed(e) = k {
            self.expr(e);
        }
    }

    fn args(&mut self, args: &'a [Arg]) {
        for a in args {
            match a {
                Arg::Expr(e) | Arg::Spread(e) => self.expr(e),
            }
        }
    }

    fn member(&mut self, m: &'a crate::parse::ast::Member) {
        self.expr(&m.object);
        if let MemberProp::Computed(k) = &m.prop {
            self.expr(k);
        }
    }

    /// An assignment target: every identifier leaf is written.
    fn target(&mut self, t: &'a Target) {
        match t {
            Target::Ident { name, .. } => {
                self.assigned.insert(name);
                self.ident(name);
            }
            Target::Member(m) => self.member(m),
            Target::Call(c) => {
                self.expr(&c.callee);
                self.args(&c.args);
            }
            Target::Array(elems) => {
                for el in elems.iter().flatten() {
                    self.target(&el.target);
                    if let Some(d) = &el.default {
                        self.expr(d);
                    }
                }
            }
            Target::Object { props, rest } => {
                for p in props {
                    self.prop_key(&p.key);
                    self.target(&p.target);
                    if let Some(d) = &p.default {
                        self.expr(d);
                    }
                }
                if let Some(r) = rest {
                    self.target(r);
                }
            }
        }
    }

    fn ident(&mut self, n: &str) {
        match n {
            "eval" => self.opaque = true,
            "arguments" => self.arguments = true,
            _ => {}
        }
    }

    fn expr(&mut self, e: &'a Expr) {
        match e {
            Expr::Ident(n) => self.ident(n),
            Expr::This
            | Expr::Super
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Regex { .. }
            | Expr::NewTarget
            | Expr::ImportMeta => {}
            Expr::Array(els, _) => {
                for el in els.iter().flatten() {
                    match el {
                        ArrayElem::Expr(e) | ArrayElem::Spread(e) => self.expr(e),
                    }
                }
            }
            Expr::Object(members, _) => {
                for m in members {
                    match m {
                        ObjectMember::Prop {
                            key, value, init, ..
                        } => {
                            self.prop_key(key);
                            self.expr(value);
                            if let Some(i) = init {
                                self.expr(i);
                            }
                        }
                        ObjectMember::Method { key, func }
                        | ObjectMember::Get { key, func }
                        | ObjectMember::Set { key, func } => {
                            self.prop_key(key);
                            self.function_expr(func);
                        }
                        ObjectMember::Spread(e) => self.expr(e),
                    }
                }
            }
            Expr::Template(t) => {
                for e in &t.exprs {
                    self.expr(e);
                }
            }
            Expr::TaggedTemplate { tag, quasi } => {
                self.expr(tag);
                for e in &quasi.exprs {
                    self.expr(e);
                }
            }
            Expr::Unary { arg, .. } => self.expr(arg),
            Expr::Update { target, .. } => self.target(target),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::Assign { target, value, .. } => {
                self.target(target);
                self.expr(value);
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test);
                self.expr(cons);
                self.expr(alt);
            }
            Expr::Call(c) => {
                self.expr(&c.callee);
                self.args(&c.args);
            }
            Expr::New { callee, args } => {
                self.expr(callee);
                self.args(args);
            }
            Expr::Member(m) => self.member(m),
            Expr::Chain(inner) | Expr::Await(inner) => self.expr(inner),
            Expr::Seq(exprs) => {
                for e in exprs {
                    self.expr(e);
                }
            }
            Expr::Arrow(a) => self.arrow(a),
            Expr::Function(f) => self.function_expr(f),
            Expr::Class(c) => self.class(c),
            Expr::Yield { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a);
                }
            }
            Expr::PrivateIn { object, .. } => self.expr(object),
            Expr::ImportCall { spec, options, .. } => {
                self.expr(spec);
                if let Some(o) = options {
                    self.expr(o);
                }
            }
        }
    }
}
