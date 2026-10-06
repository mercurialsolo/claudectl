//! Module map: paths, doc comments and public signatures — never bodies
//! (#428, RFC §4.2).
//!
//! "Structure without bodies" is the whole contract. A function body can
//! contain a hardcoded credential, a customer name, or an embedded SQL string;
//! a signature and its doc comment cannot say much beyond what the module is
//! for. So this extractor tracks brace depth and emits nothing while inside a
//! body.
//!
//! Deliberately not a Rust parser. It understands four things: `//!` at the top
//! of a file, `///` immediately before an item, the signature of a `pub` item,
//! and where a body begins. Anything it cannot classify is dropped, which is
//! the safe direction.
//!
//! Classification works on an accumulated *logical statement*, not on a
//! physical line. Rust statements wrap: rustfmt breaks signatures at 100
//! columns and puts a generic `impl`'s `where` clause on its own line, and a
//! line-at-a-time reading of either one loses the whole block.

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
    // struct's fields, a match arm. Nothing is emitted. That is the rule that
    // keeps bodies out of the index.
    //
    // `container_depth` counts blocks we deliberately descend into: `impl` and
    // inline `mod`. Their contents are more items, which we do want.
    let mut skip_depth: i32 = 0;
    let mut container_depth: i32 = 0;
    let mut impl_depth: Option<i32> = None;
    let mut seen_code = false;

    // Block-comment state has to persist across lines. A stray `{` inside
    // `/* … */` would otherwise desync the brace counter and silently swallow
    // the rest of the file's public surface.
    let mut in_block_comment = false;

    // The statement being accumulated, if any.
    let mut stmt = String::new();

    for raw in source.lines() {
        // Doc comments have to be read from the raw line: `strip_block_comments`
        // truncates at `//`, so it would eat `///` before we ever saw it. They
        // only mean anything outside a block comment, outside a body, and
        // between statements.
        if !in_block_comment && skip_depth == 0 && stmt.is_empty() {
            let raw_trim = raw.trim();
            if let Some(rest) = raw_trim.strip_prefix("//!") {
                if !seen_code {
                    module_doc.push(rest.trim().to_string());
                }
                continue;
            }
            if let Some(rest) = raw_trim.strip_prefix("///") {
                pending_doc.push(rest.trim().to_string());
                continue;
            }
        }

        let code = strip_block_comments(raw, &mut in_block_comment);
        let line = code.trim();

        if skip_depth > 0 {
            skip_depth += brace_delta(line);
            if skip_depth < 0 {
                skip_depth = 0;
            }
            continue;
        }

        if stmt.is_empty() {
            // Attributes sit between the doc and the item; keep the doc pending.
            if line.starts_with("#[") || line.starts_with("#![") {
                continue;
            }
            if line.is_empty() {
                // A blank line divorces a doc comment from whatever follows.
                pending_doc.clear();
                continue;
            }
            if line.starts_with("//") {
                pending_doc.clear();
                continue;
            }
        } else if line.is_empty() {
            continue;
        }

        seen_code = true;

        if !stmt.is_empty() {
            stmt.push(' ');
        }
        stmt.push_str(line);

        if !statement_is_complete(&stmt) {
            continue;
        }
        let whole = std::mem::take(&mut stmt);
        let statement = whole.trim();

        if let Some(ctx) = impl_header(statement) {
            impl_context = ctx;
            impl_depth = Some(container_depth);
            container_depth += brace_delta(statement).max(0);
            pending_doc.clear();
            continue;
        }

        if let Some(sig) = public_item_signature(statement) {
            let is_inline_mod = sig.contains("mod ") && statement.contains('{');
            items.push(ItemSummary {
                context: impl_context.clone(),
                signature: sig,
                doc: pending_doc.join("\n"),
            });
            pending_doc.clear();

            let delta = brace_delta(statement);
            if is_inline_mod {
                // `pub mod x {` holds more items — descend.
                container_depth += delta.max(0);
            } else if delta > 0 {
                // Anything else that opens a brace opens a body — skip it.
                skip_depth = delta;
            }
            continue;
        }

        // Some other statement at container level.
        pending_doc.clear();
        let delta = brace_delta(statement);
        if delta > 0 {
            // An unrecognized block: a private item, or a macro invocation.
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

/// Whether an accumulated statement has reached a `{` or a `;`, ignoring both
/// inside string and char literals.
///
/// Until it has, further physical lines belong to it — which is what makes a
/// `where` clause and a wrapped signature work.
fn statement_is_complete(stmt: &str) -> bool {
    let mut in_str = false;
    let mut in_char = false;
    let mut escaped = false;
    let mut chars = stmt.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_str || in_char => escaped = true,
            '"' if !in_char => in_str = !in_str,
            '\'' if !in_str => {
                if in_char {
                    in_char = false;
                } else if chars.peek().is_some_and(|n| *n == '\\')
                    || matches!(chars.clone().nth(1), Some('\''))
                {
                    in_char = true;
                }
            }
            // `}` terminates too, or a bare closing brace — an impl block's
            // own, say — would accumulate forever and stall the extractor.
            '{' | ';' | '}' if !in_str && !in_char => return true,
            _ => {}
        }
    }
    false
}

/// Remove `/* … */` spans, carrying the open state across lines.
///
/// Also truncates at a `//` line comment, so a brace inside one cannot reach
/// the brace counter either.
fn strip_block_comments(line: &str, in_block: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    let mut escaped = false;

    while i < bytes.len() {
        if *in_block {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                *in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        let c = bytes[i] as char;
        if escaped {
            escaped = false;
            out.push(c);
            i += 1;
            continue;
        }
        if in_str {
            match c {
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if bytes.get(i + 1) == Some(&b'*') => {
                *in_block = true;
                i += 2;
            }
            // A doc comment is handled by the caller, which sees the raw line.
            '/' if bytes.get(i + 1) == Some(&b'/') => break,
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `impl Foo {` / `impl<T> Trait for Foo where T: Clone {` → the header
/// without the brace or the bounds.
fn impl_header(stmt: &str) -> Option<String> {
    if !(stmt.starts_with("impl ") || stmt.starts_with("impl<")) {
        return None;
    }
    let head = stmt.split('{').next().unwrap_or(stmt).trim();
    // Drop a `where` clause from the rendered context: `impl Store` reads
    // better as a method's qualifier than `impl Store where T: Clone,`.
    let head = match head.find(" where ") {
        Some(i) => head[..i].trim(),
        None => head,
    };
    Some(head.to_string())
}

/// The signature of a `pub` item, or `None` for anything private.
///
/// Private items are dropped entirely — not just their bodies. A private
/// helper's name is an implementation detail, and the index exists to answer
/// "what can I use", which only public API can.
fn public_item_signature(stmt: &str) -> Option<String> {
    let rest = stmt.strip_prefix("pub")?;
    let rest = rest.trim_start();
    if rest.starts_with('(') {
        // `pub(crate)` and friends are not public API outside the crate.
        return None;
    }

    let after_modifiers = rest
        .strip_prefix("async ")
        .or_else(|| rest.strip_prefix("unsafe "))
        .or_else(|| rest.strip_prefix("extern "))
        .unwrap_or(rest);

    let keyword = ITEM_KEYWORDS
        .iter()
        .find(|kw| after_modifiers.starts_with(**kw))?;

    let mut cut = stmt.split('{').next().unwrap_or(stmt);

    // A `const` or `static` initializer is a body by another name. A one-line
    // `pub const TOKEN: &str = "sk-live-…"` leaks exactly what the no-bodies
    // rule exists to prevent: the name and type are the structure, the value
    // is not.
    if matches!(*keyword, "const " | "static ")
        && let Some(eq) = cut.find('=')
    {
        cut = &cut[..eq];
    }

    let cut = cut.trim_end_matches(';').trim();
    if cut.is_empty() {
        return None;
    }
    Some(cut.to_string())
}

/// Net brace change for a statement, ignoring braces inside string and char
/// literals so `"{"` does not unbalance the counter.
///
/// Block comments are already stripped by `strip_block_comments`.
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
            '/' if !in_str && !in_char && chars.peek() == Some(&'/') => break,
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

    fn blob(entry: &ModuleEntry) -> String {
        serde_json::to_string(entry).unwrap()
    }

    fn sigs(entry: &ModuleEntry) -> Vec<&str> {
        entry.items.iter().map(|i| i.signature.as_str()).collect()
    }

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
        let out = blob(&entry);
        assert!(
            !out.contains("BODY_SENTINEL"),
            "a function body leaked into the index: {out}"
        );
        assert!(out.contains("pub fn add"));
        assert!(out.contains("Adds things"));
    }

    #[test]
    fn const_and_static_initializers_never_appear() {
        // A one-line const RHS is a body by another name, and the threat model
        // ("a body can contain a hardcoded credential") applies to it exactly.
        let src = concat!(
            "pub const TOKEN: &str = \"sk-live-SECRET_SENTINEL\";\n",
            "pub static LICENSE: &str = \"LIC-SECRET_SENTINEL\";\n",
            "pub const MAX: usize = 64;\n",
        );
        let entry = module_entry("a.rs", src);
        let out = blob(&entry);
        assert!(!out.contains("SECRET_SENTINEL"), "{out}");
        // The name and type survive, because those are the structure.
        assert!(
            sigs(&entry).contains(&"pub const TOKEN: &str"),
            "{:?}",
            sigs(&entry)
        );
        assert!(sigs(&entry).contains(&"pub static LICENSE: &str"));
        assert!(sigs(&entry).contains(&"pub const MAX: usize"));
    }

    #[test]
    fn a_block_comment_containing_a_brace_does_not_swallow_the_file() {
        // An unbalanced `{` inside /* */ used to desync the counter and drop
        // every public item after it, silently.
        let src = concat!(
            "/* a comment with { a brace */\n",
            "pub fn first() {}\n",
            "pub fn second() {}\n",
        );
        let entry = module_entry("a.rs", src);
        assert_eq!(sigs(&entry), vec!["pub fn first()", "pub fn second()"]);
    }

    #[test]
    fn a_multiline_block_comment_is_skipped_whole() {
        let src = concat!("/*\n", " * braces: { { {\n", " */\n", "pub fn after() {}\n",);
        let entry = module_entry("a.rs", src);
        assert_eq!(sigs(&entry), vec!["pub fn after()"]);
    }

    #[test]
    fn a_brace_in_a_line_comment_is_ignored() {
        let src = "// opening { brace\npub fn after() {}\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(sigs(&entry), vec!["pub fn after()"]);
    }

    #[test]
    fn an_impl_with_a_where_clause_keeps_its_methods() {
        // rustfmt emits this layout for every generic impl with bounds, so
        // losing it would lose most methods in the codebase.
        let src = concat!(
            "pub struct S;\n",
            "\n",
            "impl Trait for S\n",
            "where\n",
            "    S: Clone,\n",
            "{\n",
            "    /// Does it.\n",
            "    pub fn method(&self) -> u32 {\n",
            "        let x = \"BODY_SENTINEL\";\n",
            "        drop(x);\n",
            "        1\n",
            "    }\n",
            "}\n",
            "\n",
            "pub fn tail() {}\n",
        );
        let entry = module_entry("a.rs", src);
        let out = blob(&entry);
        assert!(!out.contains("BODY_SENTINEL"), "{out}");

        let method = entry
            .items
            .iter()
            .find(|i| i.signature.contains("method"))
            .expect("pub fn method should survive a where clause");
        assert_eq!(method.context, "impl Trait for S");
        assert_eq!(method.doc, "Does it.");
        assert!(sigs(&entry).contains(&"pub fn tail()"));
    }

    #[test]
    fn a_wrapped_signature_is_emitted_whole() {
        // rustfmt wraps at 100 columns, so this is the common case, not an edge.
        let src = concat!(
            "pub fn configure(\n",
            "    name: &str,\n",
            "    value: u32,\n",
            ") -> Result<(), String> {\n",
            "    Ok(())\n",
            "}\n",
        );
        let entry = module_entry("a.rs", src);
        assert_eq!(
            sigs(&entry),
            vec!["pub fn configure( name: &str, value: u32, ) -> Result<(), String>"]
        );
    }

    #[test]
    fn private_items_are_dropped_entirely() {
        let src = "/// Doc for a private thing.\nfn hidden_helper() {}\n";
        let entry = module_entry("a.rs", src);
        assert!(entry.items.is_empty(), "{:?}", entry.items);
        let out = blob(&entry);
        assert!(!out.contains("hidden_helper"));
        assert!(!out.contains("Doc for a private thing"));
    }

    #[test]
    fn pub_crate_is_not_public_api() {
        let src = "pub(crate) fn internal() {}\npub fn exported() {}\n";
        let entry = module_entry("a.rs", src);
        assert_eq!(sigs(&entry), vec!["pub fn exported()"]);
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
        let out = blob(&entry);
        assert!(!out.contains("BODY_SENTINEL"), "{out}");

        let open = entry
            .items
            .iter()
            .find(|i| i.signature.contains("open"))
            .expect("pub fn open should be emitted");
        assert_eq!(open.context, "impl Store");

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
        let src = "pub struct A;\npub type B = u32;\npub enum E { X }\n";
        let entry = module_entry("a.rs", src);
        let got = sigs(&entry);
        assert!(got.contains(&"pub struct A"), "{got:?}");
        // A type alias's RHS is structure, not a value, so it survives.
        assert!(got.contains(&"pub type B = u32"), "{got:?}");
    }

    #[test]
    fn a_brace_in_a_string_does_not_unbalance_the_counter() {
        let src = r#"
pub fn a() {
    let s = "{ not a real brace";
    drop(s);
}

pub fn b() {}
"#;
        let entry = module_entry("a.rs", src);
        assert!(sigs(&entry).contains(&"pub fn b()"), "{:?}", sigs(&entry));
    }

    #[test]
    fn a_lifetime_is_not_a_char_literal() {
        let src = "pub fn a<'a>(x: &'a str) -> &'a str { x }\npub fn b() {}\n";
        let entry = module_entry("a.rs", src);
        assert!(sigs(&entry).contains(&"pub fn b()"), "{:?}", sigs(&entry));
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
        let out = blob(&entry);
        assert!(!out.contains("BODY_SENTINEL"), "{out}");
        assert!(out.contains("pub fn after"));
    }

    #[test]
    fn a_const_inside_a_body_does_not_escape() {
        // The skip path has to win over item recognition, or a nested public
        // const would be emitted with its value.
        let src = concat!(
            "pub fn outer() {\n",
            "    pub const INNER: &str = \"BODY_SENTINEL\";\n",
            "    drop(INNER);\n",
            "}\n",
        );
        let entry = module_entry("a.rs", src);
        let out = blob(&entry);
        assert!(!out.contains("BODY_SENTINEL"), "{out}");
        assert!(!out.contains("INNER"), "{out}");
    }

    #[test]
    fn an_empty_file_yields_an_empty_entry() {
        let entry = module_entry("a.rs", "");
        assert_eq!(entry.module_doc, "");
        assert!(entry.items.is_empty());
    }
}
