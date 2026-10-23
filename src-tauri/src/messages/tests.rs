use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    parse::Parser,
    visit::{self, Visit},
    Expr, Lit,
};

#[test]
fn dynamic_messages_keep_stable_codes_and_both_language_templates() {
    let codes = catalogue()
        .values()
        .map(|message| message.code.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(codes.len(), catalogue().len());
    assert_eq!(
        english("unregistered-message-code"),
        english("ai_provider_error")
    );
    for outcome in [
        crate::correction::FailureOutcome::UsingTranscript,
        crate::correction::FailureOutcome::UnchangedSelection,
        crate::correction::FailureOutcome::NoAction,
    ] {
        assert!(codes.contains(outcome.code()));
    }
    assert_eq!(
        code("Autostart update failed: unavailable"),
        Some("autostart_update_failed")
    );
    assert_eq!(
        code("Stale temporary audio cleanup failed."),
        Some("stale_temporary_audio_cleanup_failed")
    );
}

pub(super) fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| attribute.path().segments.last().is_some_and(|part| part.ident == "test")
        || (attribute.path().is_ident("cfg") && matches!(&attribute.meta, syn::Meta::List(list) if list.tokens.to_string() == "test")))
}

#[derive(Default)]
struct SourceMessages {
    constants: BTreeMap<String, String>,
    expressions: Vec<Expr>,
}

fn message_producer(name: &str) -> bool {
    name.ends_with("_message")
        || name.ends_with("_status")
        || name.ends_with("_terminal")
        || matches!(
            name,
            "cancellation_copy" | "message" | "status" | "failure_code" | "code"
        )
}

impl<'ast> Visit<'ast> for SourceMessages {
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|part| part.ident == "GpuDiagnostics")
        {
            for field in &node.fields {
                if matches!(&field.member, syn::Member::Named(name) if name == "recommendation") {
                    self.expressions.push(field.expr.clone());
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !test_only(&node.attrs) {
            visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !test_only(&node.attrs) {
            visit::visit_item_fn(self, node);
            let name = node.sig.ident.to_string();
            if message_producer(&name) {
                self.expressions.push(Expr::Block(syn::ExprBlock {
                    attrs: Vec::new(),
                    label: None,
                    block: *node.block.clone(),
                }));
            }
        }
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !test_only(&node.attrs) {
            visit::visit_impl_item_fn(self, node);
            if message_producer(&node.sig.ident.to_string()) {
                self.expressions.push(Expr::Block(syn::ExprBlock {
                    attrs: Vec::new(),
                    label: None,
                    block: node.block.clone(),
                }));
            }
        }
    }
    fn visit_local(&mut self, node: &'ast syn::Local) {
        // Local fixed copy can reach a sink through a variable (e.g. Ask clarification).
        // Inspect literal initializers too; do not treat a local name as resolved text.
        if let Some(init) = &node.init {
            if matches!(
                init.expr.as_ref(),
                Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(_),
                    ..
                })
            ) {
                self.expressions.push(*init.expr.clone());
            }
        }
        visit::visit_local(self, node);
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        if let Expr::Lit(value) = node.expr.as_ref() {
            if let Lit::Str(value) = &value.lit {
                if let Some(previous) = self.constants.insert(node.ident.to_string(), value.value())
                {
                    assert_eq!(
                        previous,
                        value.value(),
                        "Ambiguous constant in message validation: {}",
                        node.ident
                    );
                }
            }
        }
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
            if let Some(expression) = index.and_then(|index| node.args.iter().nth(index)) {
                self.expressions.push(expression.clone());
            }
        }
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "complete"
            || node.method == "transition"
            || node.method == "transition_stage"
            || node.method == "publish_notice"
            || node.method == "stage"
        {
            if let Some(message) = node.args.iter().nth(1) {
                self.expressions.push(message.clone());
            }
        }
        visit::visit_expr_method_call(self, node);
    }
}

struct Literals<'a> {
    constants: &'a BTreeMap<String, String>,
    values: BTreeSet<String>,
}
impl<'ast> Visit<'ast> for Literals<'_> {
    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.visit_block(&node.then_branch);
        if let Some((_, expression)) = &node.else_branch {
            self.visit_expr(expression);
        }
    }
    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        for arm in &node.arms {
            self.visit_expr(&arm.body);
        }
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if ![
            "starts_with",
            "contains",
            "strip_prefix",
            "trim_start_matches",
            "trim_end_matches",
        ]
        .iter()
        .any(|name| node.method == *name)
        {
            visit::visit_expr_method_call(self, node);
        }
    }
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.values.insert(node.value());
    }
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if let Some(value) = node
            .path
            .segments
            .last()
            .and_then(|name| self.constants.get(&name.ident.to_string()))
        {
            self.values.insert(value.clone());
        }
    }
    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        if node.mac.path.is_ident("format") {
            let arguments = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(node.mac.tokens.clone())
                .unwrap();
            if let Some(Expr::Lit(value)) = arguments.first() {
                if let Lit::Str(value) = &value.lit {
                    self.values.insert(value.value());
                }
            }
        }
    }
}

pub(super) fn rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn production_status_and_state_literals_have_both_languages() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.sort();
    let mut values = BTreeSet::new();
    for path in files {
        let ast = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
        if test_only(&ast.attrs) {
            continue;
        }
        // Constants are file-scoped: identical identifiers in separate modules
        // must never overwrite one another in the validator.
        let mut messages = SourceMessages::default();
        messages.visit_file(&ast);
        let mut literals = Literals {
            constants: &messages.constants,
            values: BTreeSet::new(),
        };
        for expression in &messages.expressions {
            literals.visit_expr(expression);
        }
        values.extend(literals.values);
    }
    let codes = catalogue()
        .values()
        .map(|message| message.code.as_str())
        .collect::<BTreeSet<_>>();
    for value in &values {
        if value.starts_with("ai_") || value.starts_with("outcome_") {
            assert!(
                codes.contains(value.as_str()),
                "Unregistered producer code: {value}"
            );
        }
    }
    let missing = values
        .iter()
        .filter(|message| message.contains(' ') && !message.contains('\n'))
        // Internal cancellation sentinels and transport diagnostics are consumed
        // by typed error handling, never displayed as application copy.
        .filter(|message| {
            !matches!(
                message.as_str(),
                "ask was cancelled"
                    | "dictation was cancelled"
                    | "editing was cancelled"
                    | "selected-text translation was cancelled"
                    | "translation was cancelled"
                    | "the streaming API reported an error"
                    | "the streaming response was incomplete"
                    | "the streaming response was incomplete: {reason}"
                    | "{} {message}"
                    | "{} {}"
            )
        })
        .filter(|message| !catalogue().contains_key(*message))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Unregistered backend UI messages: {missing:#?}"
    );
}
