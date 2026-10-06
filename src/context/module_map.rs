//! Module map: paths, doc comments and public signatures — never bodies
//! (#428, RFC §4.2).
//!
//! "Structure without bodies" is the whole contract. A function body can
//! contain a hardcoded credential, a customer name, or an embedded SQL string;
//! a signature and its doc comment cannot say much beyond what the module is
//! for. So this extractor is a line state machine that tracks brace depth and
//! emits nothing while inside one.
//!
//! Deliberately not a Rust parser. It understands four things: `//!` at the top
//! of a file, `///` immediately before an item, the signature line(s) of a
//! `pub` item, and where a body begins. Anything it cannot classify is dropped,
//! which is the safe direction.

use serde::{Deserialize, Serialize};

/// One public item's structural summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemSummary {
    /// `impl Foo` when the item is nested in an impl block, else empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// The signature, normalized to one line with the trailing `{` or `;` cut.
    pub signature: String,
    /// The `///` block that preceded it, joined with newlines.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub doc: String,
}

/// One source file's structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleEntry {
    /// Work-tree-relative path.
    pub path: String,
    /// The file's `//!` header.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module_doc: String,
    pub items: Vec<ItemSummary>,
}

/// Keywords that start a public item whose signature is publishable.
const ITEM_KEYWORDS: &[&str] = &[
    "fn ", "struct ", "enum ", "trait ", "mod ", "type ", "const ", "static ", "union ",
];

/// Extract structure from one Rust source file.
pub fn module_entry(path: &str, source: &str) -> ModuleEntry {
    let mut module_doc: Vec<String> = Vec::new();
    let mut items: Vec<ItemSummary> = Vec::new();
    let mut pending_doc: Vec<String> = Vec::new();
    let mut impl_context = String::new();

    // Two different depths, and conflating them is the easy bug here.
    //
    // `skip_depth > 0` means we are inside an item *body* — a function, a
    // struct's fields, a match arm. Nothing is emitted and nothing is read.
    // That is the rule that keeps bodies out of the index.
    //
    // `container_depth` counts blocks we deliberately descend into: `impl` and
    // inline `mod`. Their contents are more items, which we do want.
    let mut skip_depth: i32 = 0;
    let mut container_depth: i32 = 0;
    // Container depth at which the current `impl` opened, so its context can
    // be cleared when it closes.
    let mut impl_depth: Option<i32> = None;
    let mut seen_code = false;

    for raw in source.lines() {
        let line = raw.trim();

        if skip_depth > 0 {
            skip_depth += brace_delta(line);
            if skip_depth < 0 {
                skip_depth = 0;
            }
            continue;
        }

        // Module doc, only before any code.
        if let Some(rest) = line.strip_prefix("//!") {
            if !seen_code {
                module_doc.push(rest.trim().to_string());
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("///") {
            pending_doc.push(rest.trim().to_string());
            continue;
        }

        // Attributes sit between the doc and the item; keep the doc pending.
        if line.starts_with("#[") || line.starts_with("#![") {
            continue;
        }

        if line.is_empty() {
            // A blank line divorces a doc comment from whatever follows.
            pending_doc.clear();
            continue;
        }

        // Ordinary comments do not carry docs forward either.
        if line.starts_with("//") {
            pending_doc.clear();
            continue;
        }

        seen_code = true;

        // An `impl` block is a container: descend so its methods are seen.
        if let Some(ctx) = impl_header(line) {
            impl_context = ctx;
            impl_depth = Some(container_depth);
            container_depth += brace_delta(line).max(0);
            pending_doc.clear();
            continue;
        }

        if let Some(sig) = public_item_signature(line) {
            let is_inline_mod = sig.contains("mod ") && line.contains('{');
            items.push(ItemSummary {
                context: impl_context.clone(),
                signature: sig,
                doc: pending_doc.join("\n"),
            });
            pending_doc.clear();

            let delta = brace_delta(line);
            if is_inline_mod {
                // `pub mod x {` holds more items — descend.
                container_depth += delta.max(0);
            } else if delta > 0 {
                // Anything else that opens a brace opens a body — skip it.
                skip_depth = delta;
            }
            continue;
        }

        // Some other code line at container level. Track depth so we notice a
        // container closing, and drop any pending doc so it cannot attach to
        // the wrong item.
        pending_doc.clear();
        let delta = brace_delta(line);
        if delta > 0 {
            // An unrecognized block (a private item, a macro invocation).
            skip_depth = delta;
            continue;
        }
        container_depth += delta;
        if container_depth < 0 {
            container_depth = 0;
        }
        if let Some(d) = impl_depth
            && container_depth <= d
        {
            impl_context.clear();
            impl_depth = None;
        }
    }

    ModuleEntry {
        path: path.to_string(),
        module_doc: module_doc.join("\n").trim().to_string(),
        items,
    }
}

/// `impl Foo {` / `impl<T> Trait for Foo {` → the header without the brace.
fn impl_header(line: &str) -> Option<String> {
    if !(line.starts_with("impl ") || line.starts_with("impl<")) {
        return None;
    }
    Some(line.trim_end_matches('{').trim().to_string())
}

/// The signature of a `pub` item, or `None` for anything private.
///
/// Private items are dropped entirely — not just their bodies. A private
/// helper's name is an implementation detail, and the index exists to answer
/// "what can I use", which only public API can.
fn public_item_signature(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub")?;
    // `pub fn`, `pub(crate) fn`, `pub (crate) fn` …
    let rest = rest.trim_start();
    let rest = if let Some(after) = rest.strip_prefix('(') {
        // `pub(crate)` is not public API outside the crate, so skip it.
        let _ = after;
        return None;
    } else {
        rest
    };

    let after_modifiers = rest
        .strip_prefix("async ")
        .or_else(|| rest.strip_prefix("unsafe "))
        .or_else(|| rest.strip_prefix("extern "))
        .unwrap_or(rest);

    if !ITEM_KEYWORDS
        .iter()
        .any(|kw| after_modifiers.starts_with(kw))
    {
        return None;
    }

    // Cut at the body or statement end. `->` return types and generics survive
    // because they precede the brace.
    let cut = line
        .find('{')
        .map(|i| &line[..i])
        .unwrap_or(line)
        .trim_end_matches(';')
        .trim();

    Some(cut.to_string())
}

/// Net brace change for a line, ignoring braces inside string literals and
/// char literals so `"{"` does not unbalance the counter.
fn brace_delta(line: &str) -> i32 {
    let mut depth = 0;
    let mut in_str = false;
    let mut in_char = false;
    let mut escaped = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_str || in_char => escaped = true,
            '"' if !in_char => in_str = !in_str,
            '\'' if !in_str => {
                // Lifetimes (`'a`) are not char literals. A char literal is
                // quote-content-quote; a lifetime is quote-ident.
                if in_char {
                    in_char = false;
                } else if chars.peek().is_some_and(|n| *n == '\\')
                    || matches!(chars.clone().nth(1), Some('\''))
                {
                    in_char = true;
                }
            }
            '/' if !in_str && !in_char && chars.peek() == Some(&'/') => break, // line comment
            '{' if !in_str && !in_char => depth += 1,
            '}' if !in_str && !in_char => depth -= 1,
            _ => {}
        }
    }
    depth
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance property: a body never reaches the index.
    #[test]
    fn function_bodies_never_appear() {
        let src = r#"
//! Module header.

/// Adds things.
pub fn add(a: u32, b: u32) -> u32 {
    let secret = "BODY_SENTINEL_AAA";
    println!("{secret}");
    a + b
}

fn private_helper() {
    let other = "BODY_SENTINEL_BBB";
    drop(other);
}
"#;
        let entry = module_entry("src/lib.rs", src);
        let blob = serde_json::to_string(&entry).unwrap();
        assert!(
            !blob.contains("BODY_SENTINEL"),
            "a function body leaked into the index: {blob}"
        );
        assert!(blob.contains("pub fn add"));
        assert!(blob.contains("Adds things"));
    }

    #[test]
    fn private_items_are_dropped_entirely() {
        let src = "/// Doc for a private thing.\nfn hidden_helper() {}\n";
        let entry = module_entry("a.rs", src);
        assert!(entry.items.is_empty(), "{:?}", entry.items);
        let blob = serde_json::to_string(&entry).unwrap();
        assert!(!blob.contains("hidden_helper"));
        assert!(!blob.contains("Doc for a private thing"));
    }

    #[test]
    fn pub_crate_is_not_public_api() {
        let src = "pub(crate) fn internal() {}\npub fn exported() {}\n";
        let entry = module_entry("a.rs", src);
        let sigs: Vec<&str> = entry.items.iter().map(|i| i.signature.as_str()).collect();
        assert_eq!(sigs, vec!["pub fn exported()"]);
    }

    #[test]
    fn module_doc_is_captured_and_stops_at_the_first_code() {
        let src = "//! Line one.\n//! Line two.\n\npub fn a() {}\n\n//! Not a header.\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(entry.module_doc, "Line one.\nLine two.");
        assert!(!entry.module_doc.contains("Not a header"));
    }

    #[test]
    fn doc_comments_attach_to_the_following_item() {
        let src = "/// First.\npub fn a() {}\n/// Second.\npub struct B;\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(entry.items[0].doc, "First.");
        assert_eq!(entry.items[1].doc, "Second.");
    }

    #[test]
    fn attributes_between_doc_and_item_do_not_break_the_link() {
        let src = "/// Documented.\n#[derive(Debug)]\n#[allow(dead_code)]\npub struct S;\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(entry.items.len(), 1);
        assert_eq!(entry.items[0].doc, "Documented.");
    }

    #[test]
    fn a_blank_line_divorces_a_doc_from_the_next_item() {
        let src = "/// Orphan.\n\npub fn a() {}\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(entry.items.len(), 1);
        assert_eq!(entry.items[0].doc, "", "a detached doc must not attach");
    }

    #[test]
    fn impl_methods_carry_their_impl_context() {
        let src = r#"
pub struct Store;

impl Store {
    /// Opens it.
    pub fn open(path: &str) -> Self {
        let p = "BODY_SENTINEL";
        drop(p);
        Store
    }
}

pub fn free_fn() {}
"#;
        let entry = module_entry("a.rs", src);
        let blob = serde_json::to_string(&entry).unwrap();
        assert!(!blob.contains("BODY_SENTINEL"), "{blob}");

        let open = entry
            .items
            .iter()
            .find(|i| i.signature.contains("open"))
            .expect("pub fn open should be emitted");
        assert_eq!(open.context, "impl Store");

        // The impl closed, so a later free function has no context.
        let free = entry
            .items
            .iter()
            .find(|i| i.signature.contains("free_fn"))
            .expect("free fn should be emitted");
        assert_eq!(free.context, "");
    }

    #[test]
    fn signatures_keep_generics_and_return_types() {
        let src = "pub fn parse<T: Clone>(s: &str) -> Result<T, String> {\n  todo!()\n}\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(
            entry.items[0].signature,
            "pub fn parse<T: Clone>(s: &str) -> Result<T, String>"
        );
    }

    #[test]
    fn bodyless_items_are_handled() {
        let src = "pub struct A;\npub type B = u32;\npub const C: u8 = 1;\npub enum E { X }\n";
        let entry = module_entry("a.rs", src);
        let sigs: Vec<&str> = entry.items.iter().map(|i| i.signature.as_str()).collect();
        assert!(sigs.contains(&"pub struct A"), "{sigs:?}");
        assert!(sigs.contains(&"pub type B = u32"), "{sigs:?}");
        assert!(sigs.contains(&"pub const C: u8 = 1"), "{sigs:?}");
    }

    #[test]
    fn a_brace_in_a_string_does_not_unbalance_the_counter() {
        // If `"{"` counted, the extractor would think it was still inside a
        // body and drop everything after.
        let src = r#"
pub fn a() {
    let s = "{ not a real brace";
    drop(s);
}

pub fn b() {}
"#;
        let entry = module_entry("a.rs", src);
        let sigs: Vec<&str> = entry.items.iter().map(|i| i.signature.as_str()).collect();
        assert!(
            sigs.contains(&"pub fn b()"),
            "lost track of depth: {sigs:?}"
        );
    }

    #[test]
    fn a_lifetime_is_not_a_char_literal() {
        let src = "pub fn a<'a>(x: &'a str) -> &'a str { x }\npub fn b() {}\n";
        let entry = module_entry("a.rs", src);
        let sigs: Vec<&str> = entry.items.iter().map(|i| i.signature.as_str()).collect();
        assert!(sigs.contains(&"pub fn b()"), "{sigs:?}");
    }

    #[test]
    fn nested_blocks_in_a_body_are_all_skipped() {
        let src = r#"
pub fn a() {
    if true {
        for _ in 0..3 {
            let x = "BODY_SENTINEL";
            drop(x);
        }
    }
}

pub fn after() {}
"#;
        let entry = module_entry("a.rs", src);
        let blob = serde_json::to_string(&entry).unwrap();
        assert!(!blob.contains("BODY_SENTINEL"), "{blob}");
        assert!(blob.contains("pub fn after"));
    }

    #[test]
    fn an_empty_file_yields_an_empty_entry() {
        let entry = module_entry("a.rs", "");
        assert_eq!(entry.module_doc, "");
        assert!(entry.items.is_empty());
    }
}
