//! Fail-closed checks for text passed to UI message boundaries.
//! Diagnostic forwarding is explicitly documented below; application copy is resolved.
use super::tests::{rust_files, test_only};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use syn::{
    visit::{self, Visit},
    Expr, Lit, Pat, Stmt,
};

#[derive(Clone)]
struct Function {
    file: String,
    name: String,
    block: syn::Block,
    locals: BTreeMap<String, Vec<Expr>>,
    sinks: Vec<Expr>,
}

#[derive(Default)]
struct Index {
    file: String,
    functions: Vec<Function>,
    constants: BTreeMap<(String, String), Expr>,
    imports: BTreeMap<(String, String), String>,
}

fn names(pattern: &Pat, result: &mut Vec<String>) {
    match pattern {
        Pat::Ident(pattern) => result.push(pattern.ident.to_string()),
        Pat::Tuple(pattern) => pattern
            .elems
            .iter()
            .for_each(|pattern| names(pattern, result)),
        Pat::TupleStruct(pattern) => pattern
            .elems
            .iter()
            .for_each(|pattern| names(pattern, result)),
        Pat::Type(pattern) => names(&pattern.pat, result),
        _ => {}
    }
}

struct Body<'a>(&'a mut Function);
impl<'ast> Visit<'ast> for Body<'_> {
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|part| part.ident == "GpuDiagnostics")
        {
            for field in &node.fields {
                if matches!(&field.member, syn::Member::Named(name) if name == "recommendation") {
                    self.0.sinks.push(field.expr.clone());
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }
    fn visit_expr_let(&mut self, node: &'ast syn::ExprLet) {
        let mut bound = Vec::new();
        names(&node.pat, &mut bound);
        for name in bound {
            self.0
                .locals
                .entry(name)
                .or_default()
                .push(*node.expr.clone());
        }
        visit::visit_expr_let(self, node);
    }
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init {
            let mut bound = Vec::new();
            names(&node.pat, &mut bound);
            for name in bound {
                self.0
                    .locals
                    .entry(name)
                    .or_default()
                    .push(*init.expr.clone());
            }
        }
        visit::visit_local(self, node);
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let Expr::Path(path) = node.func.as_ref() {
            let name = path.path.segments.last().unwrap().ident.to_string();
            let index = match name.as_str() {
                "emit_state" | "emit_stage" => Some(3),
                "emit_status" => Some(2),
                "emit_ai_failure" => Some(4),
                "show_notice"
                    if path
                        .path
                        .segments
                        .iter()
                        .any(|part| part.ident == "answer_panel") =>
                {
                    Some(2)
                }
                _ => None,
            };
            if let Some(expr) = index.and_then(|index| node.args.iter().nth(index)) {
                self.0.sinks.push(expr.clone());
            }
        }
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if matches!(
            node.method.to_string().as_str(),
            "complete" | "transition" | "transition_stage" | "publish_notice" | "stage"
        ) {
            if let Some(expr) = node.args.iter().nth(1) {
                self.0.sinks.push(expr.clone());
            }
        }
        visit::visit_expr_method_call(self, node);
    }
}

impl Index {
    fn function(&mut self, name: String, block: syn::Block) {
        let mut function = Function {
            file: self.file.clone(),
            name,
            block: block.clone(),
            locals: BTreeMap::new(),
            sinks: Vec::new(),
        };
        Body(&mut function).visit_block(&block);
        self.functions.push(function);
    }
}
impl<'ast> Visit<'ast> for Index {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        fn collect(index: &mut Index, tree: &syn::UseTree, prefix: &[String]) {
            match tree {
                syn::UseTree::Path(path) => {
                    let mut prefix = prefix.to_vec();
                    prefix.push(path.ident.to_string());
                    collect(index, &path.tree, &prefix);
                }
                syn::UseTree::Group(group) => {
                    for tree in &group.items {
                        collect(index, tree, prefix);
                    }
                }
                syn::UseTree::Name(name) if prefix.first().is_some_and(|part| part == "crate") => {
                    let file = format!("{}.rs", prefix[1..].join("/"));
                    index
                        .imports
                        .insert((index.file.clone(), name.ident.to_string()), file);
                }
                _ => {}
            }
        }
        if !test_only(&node.attrs) {
            collect(self, &node.tree, &[]);
        }
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_only(&node.attrs) {
            visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !test_only(&node.attrs) {
            self.function(node.sig.ident.to_string(), *node.block.clone());
        }
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !test_only(&node.attrs) {
            self.function(node.sig.ident.to_string(), node.block.clone());
        }
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        let key = (self.file.clone(), node.ident.to_string());
        assert!(
            self.constants.insert(key, *node.expr.clone()).is_none(),
            "Ambiguous message constant"
        );
    }
}

struct Resolver<'a> {
    index: &'a Index,
    active: BTreeSet<String>,
    unresolved: BTreeSet<String>,
    values: BTreeSet<String>,
}
impl Resolver<'_> {
    fn forwarding(context: &Function, value: &str) -> bool {
        // These forward typed/OS diagnostics or messages already checked at
        // their public producer boundary. No application-copy helper is opaque.
        matches!(
            (context.file.as_str(), context.name.as_str(), value),
            ("lib.rs", "emit_state" | "emit_stage", "message")
                | ("pipeline/stop.rs", "run", "message")
                | ("pipeline/stop.rs", "stage", "message")
                | ("pipeline/orchestration.rs", "run", "message")
                // TerminalPhaseGuard forwards its checked terminal-state messages.
                | ("pipeline/history_text.rs", "run", "message")
                | ("pipeline/setup_tests.rs", "run_insertion", "message")
                | ("pipeline/ask.rs", "finish", "error")
                | ("commands.rs", "processing_message", "default")
                | ("commands.rs", "start_failure_message", "error")
                | (
                    "commands.rs",
                    "stop_recording_for" | "translate_selection",
                    "error"
                )
                | (
                    "lib.rs",
                    "toggle_voice_mode"
                        | "held_voice_mode"
                        | "cancel_voice_session"
                        | "initialize_model_runtime",
                    "error"
                )
        )
    }
    fn unknown(&mut self, context: &Function, value: &str) {
        if Self::forwarding(context, value) {
            return;
        }
        self.unresolved
            .insert(format!("{}::{}: {value}", context.file, context.name));
    }
    fn block(&mut self, context: &Function, block: &syn::Block) {
        // Tail values and explicit returns are both possible result paths.
        struct Returns(Vec<Expr>);
        impl<'ast> Visit<'ast> for Returns {
            fn visit_expr_return(&mut self, node: &'ast syn::ExprReturn) {
                if let Some(expr) = &node.expr {
                    self.0.push(*expr.clone());
                }
            }
            fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        }
        let mut returns = Returns(Vec::new());
        returns.visit_block(block);
        if let Some(Stmt::Expr(expr, None)) = block.stmts.last() {
            returns.0.push(expr.clone());
        }
        for expr in returns.0 {
            self.resolve(context, &expr);
        }
    }
    fn helper(&mut self, context: &Function, name: &str) {
        let mut candidates = self
            .index
            .functions
            .iter()
            .filter(|function| function.name == name)
            .collect::<Vec<_>>();
        if candidates
            .iter()
            .any(|function| function.file == context.file)
        {
            candidates.retain(|function| function.file == context.file);
        }
        if candidates.len() != 1 {
            self.unknown(
                context,
                &format!("helper {name} ({} definitions)", candidates.len()),
            );
            return;
        }
        let helper = candidates[0];
        let key = format!("{}::{}", helper.file, helper.name);
        if self.active.insert(key.clone()) {
            self.block(helper, &helper.block);
            self.active.remove(&key);
        } else {
            self.unknown(context, &format!("recursive helper {name}"));
        }
    }
    fn resolve(&mut self, context: &Function, expr: &Expr) {
        match expr {
            Expr::Lit(syn::ExprLit {
                lit: Lit::Str(value),
                ..
            }) => {
                self.values.insert(value.value());
            }
            Expr::Lit(_) => {}
            Expr::Reference(expr) => self.resolve(context, &expr.expr),
            Expr::Paren(expr) => self.resolve(context, &expr.expr),
            Expr::Group(expr) => self.resolve(context, &expr.expr),
            Expr::Path(path) => {
                let name = path.path.segments.last().unwrap().ident.to_string();
                if Self::forwarding(context, &name) {
                    return;
                }
                if name == "None"
                    || path.path.segments.iter().any(|part| {
                        matches!(
                            part.ident.to_string().as_str(),
                            "AppPhase" | "PipelinePhase"
                        )
                    })
                {
                    return;
                }
                let key = format!("{}::{}::{name}", context.file, context.name);
                if !self.active.insert(key.clone()) {
                    self.unknown(context, &format!("recursive binding {name}"));
                    return;
                }
                let module_file = path
                    .path
                    .segments
                    .iter()
                    .rev()
                    .nth(1)
                    .map(|part| format!("{}.rs", part.ident));
                let constant = self
                    .index
                    .constants
                    .get(&(context.file.clone(), name.clone()))
                    .or_else(|| {
                        self.index
                            .imports
                            .get(&(context.file.clone(), name.clone()))
                            .and_then(|file| {
                                self.index.constants.get(&(file.clone(), name.clone()))
                            })
                    })
                    .or_else(|| {
                        module_file.as_ref().and_then(|file| {
                            self.index.constants.get(&(file.clone(), name.clone()))
                        })
                    });
                if let Some(expr) = constant {
                    self.resolve(context, expr);
                } else if let Some(expressions) = context.locals.get(&name) {
                    for expr in expressions {
                        self.resolve(context, expr);
                    }
                } else {
                    self.unknown(context, &name);
                }
                self.active.remove(&key);
            }
            Expr::If(expr) => {
                self.block(context, &expr.then_branch);
                if let Some((_, expr)) = &expr.else_branch {
                    self.resolve(context, expr);
                }
            }
            Expr::Match(expr) => {
                for arm in &expr.arms {
                    self.resolve(context, &arm.body);
                }
            }
            Expr::Block(expr) => self.block(context, &expr.block),
            Expr::Return(expr) => {
                if let Some(expr) = &expr.expr {
                    self.resolve(context, expr);
                }
            }
            Expr::Closure(expr) => self.resolve(context, &expr.body),
            Expr::Tuple(expr) => {
                for expr in &expr.elems {
                    self.resolve(context, expr);
                }
            }
            Expr::Field(expr) => {
                if context.file == "lib.rs"
                    && context.name == "initialize_model_runtime"
                    && matches!(&expr.member, syn::Member::Named(name) if name == "recommendation")
                {
                    // GPU diagnostic copy is checked as fixed initializers separately.
                    return;
                }
                self.resolve(context, &expr.base);
            }
            Expr::Call(expr) => {
                if let Expr::Path(path) = expr.func.as_ref() {
                    let name = path.path.segments.last().unwrap().ident.to_string();
                    if matches!(name.as_str(), "Some" | "Ok" | "from") {
                        for expr in &expr.args {
                            self.resolve(context, expr);
                        }
                    } else {
                        if name == "processing_message" {
                            if let Some(default) = expr.args.iter().nth(2) {
                                self.resolve(context, default);
                            }
                        }
                        self.helper(context, &name);
                    }
                } else {
                    self.unknown(context, "indirect call");
                }
            }
            Expr::MethodCall(expr) => {
                let name = expr.method.to_string();
                if matches!(
                    name.as_str(),
                    "into" | "to_owned" | "to_string" | "clone" | "as_str" | "as_deref"
                ) {
                    self.resolve(context, &expr.receiver);
                } else if name == "map_or_else" {
                    self.resolve(context, &expr.receiver);
                    for arg in &expr.args {
                        if let Expr::Closure(closure) = arg {
                            self.resolve(context, &closure.body);
                        }
                    }
                } else if name == "then" || name == "then_some" {
                    for arg in &expr.args {
                        self.resolve(context, arg);
                    }
                } else {
                    self.helper(context, &name);
                }
            }
            Expr::Macro(expr) if expr.mac.path.is_ident("format") => {
                // Literal-format membership is checked by the companion test.
                // Values interpolated into a registered template are diagnostics.
                use syn::parse::Parser;
                let args = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                    .parse2(expr.mac.tokens.clone())
                    .unwrap();
                if let Some(Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(template),
                    ..
                })) = args.first()
                {
                    if !super::catalogue().contains_key(&template.value())
                        && !(context.file == "correction.rs"
                            && context.name == "failure_message"
                            && template.value() == "{} {}")
                    {
                        self.unknown(context, "unregistered format template");
                    }
                    self.values.insert(template.value());
                } else {
                    self.unknown(context, "nonliteral format template");
                }
            }
            _ => self.unknown(context, "unsupported message expression"),
        }
    }
}

#[test]
fn resolver_follows_arbitrary_helpers_and_rejects_unknown_producers() {
    let source = syn::parse_file(
        r#"
        fn fresh_helper() -> &'static str { "A newly introduced sentence." }
        fn publish() { let text = fresh_helper(); emit_status(app, "warning", text); }
        fn unchecked() { emit_status(app, "warning", missing_helper()); }
        fn interpolated() { emit_status(app, "warning", &format!("{}", "Hidden new sentence.")); }
    "#,
    )
    .unwrap();
    let mut index = Index {
        file: "fixture.rs".into(),
        ..Index::default()
    };
    index.visit_file(&source);
    let mut resolver = Resolver {
        index: &index,
        active: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        values: BTreeSet::new(),
    };
    for function in &index.functions {
        for expr in &function.sinks {
            resolver.resolve(function, expr);
        }
    }
    assert!(resolver.values.contains("A newly introduced sentence."));
    assert!(resolver
        .unresolved
        .iter()
        .any(|value| value.contains("missing_helper")));
    assert!(resolver
        .unresolved
        .iter()
        .any(|value| value.contains("unregistered format template")));
}

#[test]
fn ui_message_sources_do_not_silently_skip_unresolved_expressions() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.sort();
    let mut index = Index::default();
    for path in files {
        index.file = path
            .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let ast = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
        if test_only(&ast.attrs) {
            continue;
        }
        index.visit_file(&ast);
    }
    let mut resolver = Resolver {
        index: &index,
        active: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        values: BTreeSet::new(),
    };
    for function in &index.functions {
        for expr in &function.sinks {
            resolver.resolve(function, expr);
        }
    }
    assert!(
        resolver.unresolved.is_empty(),
        "Unresolved UI sources require a resolver or a documented diagnostic boundary: {:#?}",
        resolver.unresolved
    );
    let missing = resolver
        .values
        .iter()
        .filter(|value| {
            value.contains(' ')
                && !matches!(
                    value.as_str(),
                    "{} {}" | "{} {message}"
        // The second element of cancellation_copy is an internal command sentinel.
        | "ask was cancelled" | "dictation was cancelled" | "editing was cancelled"
        | "selected-text translation was cancelled" | "translation was cancelled"
                )
        })
        .filter(|value| !super::catalogue().contains_key(*value))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Unregistered resolved UI text: {missing:#?}"
    );
}

#[test]
fn imported_message_constants_are_resolved_only_from_the_named_module() {
    let mut index = Index::default();
    for (file, source) in [
        (
            "commands.rs",
            r#"const COPY: &str = "The intended message.";"#,
        ),
        (
            "unrelated.rs",
            r#"const COPY: &str = "An unrelated message.";"#,
        ),
        (
            "pipeline/fixture.rs",
            r#"use crate::commands::COPY; fn publish() { emit_status(app, "notice", COPY); }"#,
        ),
        (
            "pipeline/unknown.rs",
            r#"fn publish_unknown() { emit_status(app, "notice", COPY); }"#,
        ),
    ] {
        index.file = file.into();
        index.visit_file(&syn::parse_file(source).unwrap());
    }
    let mut resolver = Resolver {
        index: &index,
        active: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        values: BTreeSet::new(),
    };
    for function in &index.functions {
        for sink in &function.sinks {
            resolver.resolve(function, sink);
        }
    }
    assert!(resolver.values.contains("The intended message."));
    assert!(!resolver.values.contains("An unrelated message."));
    assert_eq!(
        resolver.unresolved,
        BTreeSet::from(["pipeline/unknown.rs::publish_unknown: COPY".into()])
    );
}
