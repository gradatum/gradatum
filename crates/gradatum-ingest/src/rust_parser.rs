//! Tree-sitter parser for Rust files (feature `code-rust`).
//!
//! ## Extracted entities
//!
//! - Top-level functions (`fn`) — public only in `Pub` mode, all in `All` mode
//! - Top-level structs, enums, traits — same visibility filter
//! - `impl` blocks (qualified_name = `"impl Type"` or `"impl Trait for Type"`)
//! - Top-level constants and associated types — same visibility filter
//! - Top-level modules (always, regardless of mode — they structure the namespace)
//! - Methods inside an `impl` block — same visibility filter
//!
//! ## Not extracted
//!
//! - Procedural macros (`#[derive]`, `proc_macro`) — invisible to tree-sitter
//! - Closures, lambdas, items inside functions
//!
//! ## Accuracy > coverage
//!
//! If a node is malformed or of an unrecognized kind, the symbol is omitted.
//! A file that cannot be parsed at all returns `Ok(vec![])`.

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Visibility mode for symbol extraction.
///
/// - `Pub`: only public items (`pub`, `pub(crate)`, etc.) are extracted.
///   Default behavior, preserving the historical default.
/// - `All`: all items are extracted regardless of visibility.
///   Useful for indexing a crate's internal surface (tests, refactoring, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// Index only public items (default behavior).
    Pub,
    /// Index all items, including private ones.
    All,
}

/// Extracts top-level items from a node (`source_file` or `mod_item`).
fn extract_top_level_items(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    visibility: Visibility,
    symbols: &mut Vec<DerivedSymbol>,
    seen: &mut std::collections::HashSet<(String, String)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(sym) = extract_function(child, source, source_path, None, visibility) {
                    push_symbol(sym, symbols, seen);
                }
            }
            "struct_item" => {
                if let Some(sym) =
                    extract_named_item(child, source, source_path, "struct", visibility)
                {
                    push_symbol(sym, symbols, seen);
                }
            }
            "enum_item" => {
                if let Some(sym) =
                    extract_named_item(child, source, source_path, "enum", visibility)
                {
                    push_symbol(sym, symbols, seen);
                }
            }
            "trait_item" => {
                if let Some(sym) =
                    extract_named_item(child, source, source_path, "trait", visibility)
                {
                    push_symbol(sym, symbols, seen);
                }
            }
            "impl_item" => {
                extract_impl(child, source, source_path, visibility, symbols, seen);
            }
            "const_item" => {
                if let Some(sym) = extract_const(child, source, source_path, visibility) {
                    push_symbol(sym, symbols, seen);
                }
            }
            "mod_item" => {
                if let Some(sym) = extract_mod(child, source, source_path) {
                    push_symbol(sym, symbols, seen);
                }
            }
            "type_alias" => {
                if let Some(sym) =
                    extract_named_item(child, source, source_path, "type", visibility)
                {
                    push_symbol(sym, symbols, seen);
                }
            }
            // Attributs, use, extern crate, etc. — ignorés.
            _ => {}
        }
    }
}

/// Inserts a symbol into the list, marking `ambiguous=true` if the (kind, name) pair already exists.
fn push_symbol(
    mut sym: DerivedSymbol,
    symbols: &mut Vec<DerivedSymbol>,
    seen: &mut std::collections::HashSet<(String, String)>,
) {
    let key = (sym.kind.clone(), sym.qualified_name.clone());
    if seen.contains(&key) {
        // Marquer l'existant comme ambigu.
        for s in symbols.iter_mut() {
            if s.kind == sym.kind && s.qualified_name == sym.qualified_name {
                s.ambiguous = true;
            }
        }
        sym.ambiguous = true;
    } else {
        seen.insert(key);
    }
    symbols.push(sym);
}

/// Returns `true` if the node has a `pub` or `pub(crate)` visibility attribute.
fn is_public(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = node_text(child, source);
            // "pub" seul ou "pub(crate)" → public.
            // "pub(super)" / "pub(in path)" → traités comme public (approche conservative).
            return text.starts_with("pub");
        }
    }
    false
}

/// Returns the UTF-8 text of a node.
///
/// ## Safety invariant
///
/// `source` must be the SAME buffer `content.as_bytes()` that was passed to
/// `parser.parse(content, None)`. The tree-sitter AST byte offsets are guaranteed
/// to lie within this slice: `node.utf8_text(source)` cannot index out of bounds
/// as long as `source` is identical to the parsed buffer.
/// `.unwrap_or("")` is defensive but should never trigger.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Computes the 1-based inclusive span `(start_line, end_line)` of a tree-sitter node.
///
/// Rules:
/// - Span covers the item node only (not preceding `#[...]` attributes or doc-comments — siblings).
/// - Lines are 1-based: `row + 1` (tree-sitter is 0-based).
/// - If `end_position().row` points at a trailing blank line (column 0 = final newline)
///   → `end_line = end_position().row` (no +1) to exclude the blank line.
/// - Degenerate span (`start > end`; `start = 0` is guarded) → `None`.
/// - `None` when not extractable (accuracy > coverage).
///
/// Line-count validation (start > file_lines, truncated file) is performed by the
/// handler before slicing, not here.
fn extract_node_span(node: Node<'_>, _source: &[u8]) -> Option<(u32, u32)> {
    let start_line = (node.start_position().row as u32).saturating_add(1);
    // Si la colonne de fin est 0, le nœud se termine sur un newline terminal →
    // exclure cette ligne vide (pointer la dernière ligne réelle du corps).
    let end_line = if node.end_position().column == 0 && node.end_position().row > 0 {
        node.end_position().row as u32 // sans +1 : ligne vide exclue
    } else {
        (node.end_position().row as u32).saturating_add(1)
    };

    if start_line == 0 || start_line > end_line {
        // Span dégénéré (B3) → None.
        return None;
    }
    Some((start_line, end_line))
}

/// Returns the first child node of kind `identifier` or `type_identifier`.
fn first_identifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier") {
            return Some(node_text(child, source));
        }
    }
    None
}

/// Extracts the doc-comment preceding a node (`/// ...` or `/** ... */` lines).
///
/// Searches preceding siblings of kind `line_comment` or `block_comment`.
/// Capped at 5 lines.
fn extract_doc_comment(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut prev = node.prev_sibling();

    // Remonter les siblings précédents en collectant les doc-comments.
    let mut doc_candidates: Vec<String> = Vec::new();
    while let Some(p) = prev {
        match p.kind() {
            "line_comment" => {
                let text = node_text(p, source);
                if text.starts_with("///") || text.starts_with("//!") {
                    // Nettoyer le préfixe `/// ` ou `/// `.
                    let clean = text
                        .trim_start_matches("///")
                        .trim_start_matches("//!")
                        .trim()
                        .to_string();
                    doc_candidates.push(clean);
                    prev = p.prev_sibling();
                } else {
                    break;
                }
            }
            // Attributs (#[...]) entre les doc-comments → continuer.
            "attribute_item" => {
                prev = p.prev_sibling();
            }
            _ => break,
        }
    }

    // Les candidats sont en ordre inverse → inverser.
    doc_candidates.reverse();
    for line in doc_candidates.iter().take(5) {
        lines.push(line.clone());
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Extracts a function or method.
fn extract_function(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    impl_type: Option<&str>,
    visibility: Visibility,
) -> Option<DerivedSymbol> {
    let pub_item = is_public(node, source);

    // En mode Pub : exiger pub. En mode All : extraire tous les items.
    if visibility == Visibility::Pub && !pub_item {
        return None;
    }

    let name = first_identifier(node, source)?;
    let qualified_name = match impl_type {
        Some(t) => format!("{t}::{name}"),
        None => name.to_string(),
    };

    let signature = extract_fn_signature(node, source);
    let doc_comment = extract_doc_comment(node, source);
    let deps = extract_deps(node, source, impl_type);
    let span = extract_node_span(node, source);

    Some(DerivedSymbol {
        qualified_name,
        kind: if impl_type.is_some() {
            "method".to_string()
        } else {
            "fn".to_string()
        },
        signature,
        doc_comment,
        deps,
        source_path: source_path.to_string(),
        visibility: if pub_item {
            "pub".to_string()
        } else {
            "priv".to_string()
        },
        span,
        ambiguous: false,
    })
}

/// Extracts the textual signature of a function (params + return type), ≤ 1 line.
fn extract_fn_signature(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let mut params: Option<String> = None;
    let mut ret: Option<String> = None;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "parameters" => {
                // Récupérer le texte brut des paramètres, tronqué à ≤120 bytes, arrondi
                // au char suivant (borne effective ≤123 pour un codepoint 4 bytes à la
                // frontière), en préservant les frontières UTF-8 (troncature char-safe).
                //
                // Un slice `&raw[..120]` naïf paniquerait si l'octet 120 tombe au
                // milieu d'un codepoint multi-byte (accents, emoji, etc.).
                //
                // `str::floor_char_boundary` (stable ≥ 1.91) serait idéal mais le
                // MSRV du projet est 1.88. On utilise `char_indices` à la place :
                // `find` retourne le premier char dont l'offset de DÉBUT atteint ou
                // dépasse 120 ; cet offset est la borne exclusive du slice — il est
                // toujours une frontière de codepoint valide.
                let raw = node_text(child, source);
                let truncated = if raw.len() > 120 {
                    let boundary = raw
                        .char_indices()
                        .find(|(i, _)| *i >= 120)
                        .map(|(i, _)| i)
                        .unwrap_or(raw.len());
                    format!("{}…", &raw[..boundary])
                } else {
                    raw.to_string()
                };
                params = Some(truncated);
            }
            "type_identifier"
            | "scoped_type_identifier"
            | "generic_type"
            | "reference_type"
            | "tuple_type"
            | "primitive_type"
            | "unit_type"
            | "never_type"
                if ret.is_none() =>
            {
                // Type de retour simple (après `->` dans l'AST).
                ret = Some(node_text(child, source).to_string());
            }
            "->" => {}
            _ => {}
        }
    }

    // Vérifier si le nœud `->` existe (indicateur d'un type de retour explicite).
    let has_arrow = {
        let mut c2 = node.walk();

        node.children(&mut c2).any(|ch| ch.kind() == "->")
    };

    match (params, ret, has_arrow) {
        (Some(p), Some(r), true) => Some(format!("{p} -> {r}")),
        (Some(p), None, true) => Some(format!("{p} -> ?")),
        (Some(p), _, false) => Some(p),
        _ => None,
    }
}

/// Extracts a named item (struct/enum/trait/type).
fn extract_named_item(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    kind: &str,
    visibility: Visibility,
) -> Option<DerivedSymbol> {
    let pub_item = is_public(node, source);

    // En mode Pub : exiger pub. En mode All : extraire tous les items.
    if visibility == Visibility::Pub && !pub_item {
        return None;
    }

    let name = first_identifier(node, source)?;
    let doc_comment = extract_doc_comment(node, source);
    let span = extract_node_span(node, source);

    Some(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: kind.to_string(),
        signature: None,
        doc_comment,
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: if pub_item {
            "pub".to_string()
        } else {
            "priv".to_string()
        },
        span,
        ambiguous: false,
    })
}

/// Extracts an impl block and its methods.
fn extract_impl(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    visibility: Visibility,
    symbols: &mut Vec<DerivedSymbol>,
    seen: &mut std::collections::HashSet<(String, String)>,
) {
    // Construire le qualified_name du bloc impl.
    // Chercher type_identifier (type) et optionnellement un trait.
    let mut type_name: Option<String> = None;
    let mut trait_name: Option<String> = None;
    let mut has_for = false;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_identifier" | "scoped_type_identifier" | "generic_type" => {
                let text = node_text(child, source).to_string();
                if has_for {
                    type_name = Some(text);
                } else if type_name.is_none() {
                    // Premier type_identifier = le type ou le trait.
                    type_name = Some(text);
                }
            }
            "for" => {
                has_for = true;
                // Ce qui était type_name est en fait le trait.
                trait_name = type_name.take();
            }
            _ => {}
        }
    }

    let impl_name = match (trait_name, type_name) {
        (Some(tr), Some(ty)) => format!("impl {tr} for {ty}"),
        (None, Some(ty)) => format!("impl {ty}"),
        _ => return, // impl sans type_identifier → ignorer.
    };

    // Extraire le type de base (sans paramètres génériques) pour les méthodes.
    let base_type = impl_name
        .split_whitespace()
        .last()
        .unwrap_or("Unknown")
        .split('<')
        .next()
        .unwrap_or("Unknown")
        .to_string();

    // Émettre un symbole pour le bloc impl lui-même.
    // Les blocs impl n'ont pas de visibilité propre en Rust — toujours "pub".
    let impl_sym = DerivedSymbol {
        qualified_name: impl_name.clone(),
        kind: "impl".to_string(),
        signature: None,
        doc_comment: extract_doc_comment(node, source),
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: "pub".to_string(),
        span: extract_node_span(node, source),
        ambiguous: false,
    };
    push_symbol(impl_sym, symbols, seen);

    // Extraire les méthodes du bloc impl (filtrées selon visibility).
    let declaration_list = {
        let mut c2 = node.walk();

        node.children(&mut c2)
            .find(|ch| ch.kind() == "declaration_list")
    };

    if let Some(decl) = declaration_list {
        let mut dc = decl.walk();
        for item in decl.children(&mut dc) {
            if item.kind() == "function_item"
                && let Some(sym) =
                    extract_function(item, source, source_path, Some(&base_type), visibility)
            {
                push_symbol(sym, symbols, seen);
            }
        }
    }
}

/// Extracts a top-level constant.
fn extract_const(
    node: Node<'_>,
    source: &[u8],
    source_path: &str,
    visibility: Visibility,
) -> Option<DerivedSymbol> {
    let pub_item = is_public(node, source);

    // En mode Pub : exiger pub. En mode All : extraire toutes les constantes.
    if visibility == Visibility::Pub && !pub_item {
        return None;
    }

    let name = first_identifier(node, source)?;
    Some(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "const".to_string(),
        signature: None,
        doc_comment: extract_doc_comment(node, source),
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: if pub_item {
            "pub".to_string()
        } else {
            "priv".to_string()
        },
        span: extract_node_span(node, source),
        ambiguous: false,
    })
}

/// Extracts a top-level module.
///
/// Modules are extracted in both modes (Pub and All) because they structure the namespace —
/// a private module may contain important `pub(crate)` items.
/// The module's own visibility is still reflected in the `visibility` field.
fn extract_mod(node: Node<'_>, source: &[u8], source_path: &str) -> Option<DerivedSymbol> {
    let pub_item = is_public(node, source);
    let name = first_identifier(node, source)?;
    Some(DerivedSymbol {
        qualified_name: name.to_string(),
        kind: "mod".to_string(),
        signature: None,
        doc_comment: extract_doc_comment(node, source),
        deps: Vec::new(),
        source_path: source_path.to_string(),
        visibility: if pub_item {
            "pub".to_string()
        } else {
            "priv".to_string()
        },
        span: extract_node_span(node, source),
        ambiguous: false,
    })
}

/// Resolution context threaded through call-dep collection.
///
/// Groups the two pieces of syntactic type information available intra-function:
/// - `impl_type`: the base type of the enclosing `impl` block (for `self.method()`),
///   `None` for free functions.
/// - `locals`: intra-function symbol table `variable → explicit type` built from typed
///   parameters and `let x: T` bindings (for `x.method()`). Only syntactically explicit
///   types are present (inference / chaining are absent → terminal-only fallback).
///
/// Copy: both fields are shared references, so passing by value is free.
#[derive(Clone, Copy)]
struct ResolveCtx<'a> {
    impl_type: Option<&'a str>,
    locals: &'a std::collections::HashMap<String, String>,
}

/// Extracts the dependencies of a node (function/method): a combination of intra-body
/// `use` items and actual callees (call-graph edges).
///
/// ## Entry format
///
/// Free functions and scoped calls (`A::b()`) store the **terminal** (`b`) and, when a
/// qualified path is available, the **fully qualified** form (`A::b`).
///
/// Method calls whose receiver type is **syntactically resolvable** also store the
/// qualified form `Type::method` in addition to the terminal:
/// - `self.method()` → `<ImplType>::method` (the impl block type is known — [`ResolveCtx::impl_type`]).
/// - `x.method()` where `let x: T` / a typed parameter `x: T` is in scope → `T::method`
///   (intra-function symbol table — [`ResolveCtx::locals`]).
///
/// This lets `code_scope_reverse_deps` (filter `WHERE d.value = ?`) match callers of
/// `Type::method`, not only free functions.
///
/// ## Precision > recall (out of scope)
///
/// Only syntactically explicit receivers are resolved. The following remain
/// terminal-only (no qualified form → never a false positive in the reverse graph):
/// type inference (`let x = f()`), method chaining (`a.b().c()`), trait dispatch,
/// generic type parameters, macro-generated calls, and closures.
///
/// ## Deduplication and cap
///
/// Duplicate callees are removed. Global cap at 20.
///
/// ## stdlib filtering
///
/// Highly generic stdlib methods (`clone`, `to_string`, `len`, `iter`, etc.) are
/// excluded: they carry no structural information for the call graph and add noise
/// to reverse-deps results.
fn extract_deps(node: Node<'_>, source: &[u8], impl_type: Option<&str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deps = Vec::new();

    // Table de symboles intra-fonction (F-70 tâche 4.2) : params typés + `let x: T`.
    let locals = build_local_types(node, source);
    let ctx = ResolveCtx {
        impl_type,
        locals: &locals,
    };

    // 1. Use-deps (imports dans le corps).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "use_declaration"
            && let Some(path) = extract_use_path(child, source)
            && seen.insert(path.clone())
        {
            deps.push(path);
        }
    }

    // 2. Call-deps : arêtes d'appel réelles (DFS sur le corps de la fonction).
    if let Some(body) = find_block(node) {
        collect_call_deps(body, source, &mut seen, &mut deps, ctx);
    }

    // Cap global à 20 (accuracy > coverage).
    deps.truncate(20);
    deps
}

/// Extracts the binding name from a simple pattern (`identifier`, `mut x`, `ref x`).
///
/// Returns `None` for composite patterns (tuple, struct, slice) — those are not
/// resolvable to a single typed variable.
fn binding_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, source).to_string()),
        "mut_pattern" | "ref_pattern" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|ch| ch.kind() == "identifier")
                .map(|id| node_text(id, source).to_string())
        }
        _ => None,
    }
}

/// Resolves a type node to a simple type name, when syntactically explicit.
///
/// - `type_identifier` (`Foo`) / `scoped_type_identifier` (`a::Foo`) → the text as-is.
/// - `generic_type` (`Vec<T>`, `MyType<T>`) → the base type (generics stripped),
///   matching how `extract_impl` computes the base type of an impl block.
/// - `reference_type` (`&T`, `&mut T`) → the inner type.
/// - Anything else (inference, `impl Trait`, `dyn Trait`, tuples, arrays, fn pointers)
///   → `None` (not resolvable syntactically → terminal-only fallback).
fn resolve_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "scoped_type_identifier" => Some(node_text(node, source).to_string()),
        "generic_type" | "reference_type" => {
            // Unwrap to the base/inner type: the first child that itself resolves.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(t) = resolve_type_name(child, source) {
                    return Some(t);
                }
            }
            None
        }
        _ => None,
    }
}

/// Builds the intra-function symbol table `variable → explicit type`.
///
/// Records **every** binding site of a name — typed parameters (`x: T`), typed `let x: T`
/// AND type-inferred `let x = …` bindings anywhere in the body. A name resolves to a type
/// only when it has **exactly one** binding site and that site is syntactically typed.
///
/// Any name bound ≥2 times (rebind / shadowing, whatever the inferred-vs-typed mix) is
/// dropped: precision over recall. This is what makes a later explicit→inferred rebind
/// (`fn f(x: Foo){ let x = make_bar(); x.method() }`) correctly UNresolvable — the real
/// type at the call site is the inferred one, not `Foo` (audit C1).
///
/// A **composite** pattern (tuple / struct / tuple-struct / slice destructuring) binds each
/// of its identifiers to a *component* of the value — a per-name type cannot be attributed
/// syntactically. Every such identifier is recorded as an untyped ("poison") site, so a name
/// shadowed by destructuring (`let (x, _) = pair()`) also reaches ≥2 sites and is dropped
/// (audit C1-bis — destructuring shadow was a proven false positive, not a recall-only gap).
fn build_local_types(node: Node<'_>, source: &[u8]) -> std::collections::HashMap<String, String> {
    // name → liste des sites de binding ; chaque site = Some(type) si typé, None sinon.
    let mut sites: std::collections::HashMap<String, Vec<Option<String>>> =
        std::collections::HashMap::new();

    // Paramètres (`fn f(x: T)`).
    if let Some(params) = child_of_kind(node, "parameters") {
        let mut cursor = params.walk();
        for p in params.children(&mut cursor) {
            if p.kind() == "parameter"
                && let Some(pat) = p.child_by_field_name("pattern")
            {
                register_pattern_sites(pat, p.child_by_field_name("type"), source, &mut sites);
            }
        }
    }

    // Bindings `let` (typés OU inférés) — DFS sur le corps.
    if let Some(body) = find_block(node) {
        collect_let_sites(body, source, &mut sites);
    }

    // Résolu ⟺ un unique site de binding ET ce site est typé.
    sites
        .into_iter()
        .filter_map(|(name, mut binds)| {
            if binds.len() == 1 {
                binds.pop().flatten().map(|ty| (name, ty))
            } else {
                None // rebind (≥2 sites) → drop (précision > rappel).
            }
        })
        .collect()
}

/// Finds the first direct child of `node` with the given kind.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|ch| ch.kind() == kind)
}

/// Records the binding site(s) of a `let`/parameter pattern.
///
/// - Simple single-identifier pattern (`x`, `mut x`, `ref x`) → one site, typed with the
///   resolved annotation (`Some(type)`) or `None` if inferred/unresolvable.
/// - Composite pattern (tuple / struct / tuple-struct / slice) → one **untyped** site
///   (`None`) per bound identifier: the per-component type is not attributable, and any
///   name shadowed this way must reach ≥2 sites to be dropped (audit C1-bis).
fn register_pattern_sites(
    pattern: Node<'_>,
    type_node: Option<Node<'_>>,
    source: &[u8],
    sites: &mut std::collections::HashMap<String, Vec<Option<String>>>,
) {
    if let Some(name) = binding_name(pattern, source) {
        let ty = type_node.and_then(|t| resolve_type_name(t, source));
        sites.entry(name).or_default().push(ty);
    } else {
        let mut names = Vec::new();
        collect_pattern_idents(pattern, source, &mut names);
        for name in names {
            sites.entry(name).or_default().push(None);
        }
    }
}

/// Collects every identifier bound by a composite pattern (DFS).
///
/// Over-collection is precision-safe: an extra poisoned name only drops resolution, never
/// creates a qualified edge. Matches `identifier` (tuple / tuple-struct binds, and the
/// tuple-struct constructor path, harmlessly) and `shorthand_field_identifier` (struct binds).
fn collect_pattern_idents(node: Node<'_>, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "shorthand_field_identifier" => {
            out.push(node_text(node, source).to_string());
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_pattern_idents(child, source, out);
            }
        }
    }
}

/// Recursively records every `let` binding site into the symbol-table accumulator.
fn collect_let_sites(
    node: Node<'_>,
    source: &[u8],
    sites: &mut std::collections::HashMap<String, Vec<Option<String>>>,
) {
    if node.kind() == "let_declaration"
        && let Some(pat) = node.child_by_field_name("pattern")
    {
        register_pattern_sites(pat, node.child_by_field_name("type"), source, sites);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_let_sites(child, source, sites);
    }
}

/// Highly generic stdlib method names excluded from the call graph (noise filter).
///
/// These methods appear in almost all Rust code and carry no structural information
/// useful for call-graph reverse-deps.
const STDLIB_NOISE_METHODS: &[&str] = &[
    "clone",
    "to_string",
    "to_owned",
    "len",
    "is_empty",
    "iter",
    "iter_mut",
    "into_iter",
    "push",
    "pop",
    "insert",
    "remove",
    "get",
    "contains",
    "map",
    "filter",
    "collect",
    "unwrap",
    "expect",
    "ok",
    "err",
    "and_then",
    "or_else",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "map_err",
    "flatten",
    "into",
    "from",
    "as_ref",
    "as_mut",
    "as_str",
    "as_bytes",
    "to_vec",
    "split",
    "split_whitespace",
    "split_once",
    "trim",
    "trim_start",
    "trim_end",
    "starts_with",
    "ends_with",
    "contains_key",
    "entry",
    "or_insert",
    "or_insert_with",
    "or_default",
    "take",
    "replace",
    "chars",
    "bytes",
    "lines",
    "next",
    "peekable",
    "enumerate",
    "zip",
    "chain",
    "fold",
    "any",
    "all",
    "find",
    "position",
    "count",
    "sum",
    "product",
    "min",
    "max",
    "clamp",
    "abs",
    "sqrt",
    "pow",
    "checked_add",
    "checked_sub",
    "checked_mul",
    "saturating_add",
    "saturating_sub",
    "wrapping_add",
    "try_into",
    "try_from",
    "lock",
    "read",
    "write",
    "await",
    "flush",
    "close",
    "send",
    "recv",
    "default",
    "fmt",
    "hash",
    "eq",
    "cmp",
    "partial_cmp",
    "drop",
    "deref",
    "index",
];

/// stdlib/core/alloc crate prefixes to exclude from `call_expression` paths.
///
/// For `std::mem::take(x)`, the final segment is `take` but the path starts with
/// `std` → filter at the source to avoid indexing stdlib functions.
const STDLIB_PREFIXES: &[&str] = &["std", "core", "alloc", "tokio", "futures", "serde"];

/// stdlib container / smart-pointer / wrapper type names excluded from method
/// qualification.
///
/// A receiver typed with one of these produces only noise (`Vec::sort_by`,
/// `Option::is_some`) or, worse, a **mis-qualification through `Deref`**: a method
/// called on `Arc<Foo>` / `Box<Foo>` belongs to `Foo`, not to the wrapper. Qualifying
/// to `Arc::method` would be a false edge in the reverse graph. Matched on the base
/// type name (generics already stripped by [`resolve_type_name`]).
const STDLIB_TYPE_DENYLIST: &[&str] = &[
    "Vec", "VecDeque", "Option", "Result", "HashMap", "HashSet", "BTreeMap", "BTreeSet", "String",
    "Box", "Rc", "Arc", "Cow", "Cell", "RefCell", "Mutex", "RwLock",
];

/// Finds the `block` node (function body).
fn find_block(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|&child| child.kind() == "block")
}

/// Recursively collects callees within an AST node.
///
/// Visits all descendants via DFS and extracts:
/// - `call_expression` with a direct `identifier` → stores the terminal.
/// - `call_expression` with `scoped_identifier` (`A::b()`) → stores the **terminal**
///   (`b`) AND the **fully qualified path** (`A::b`) if non-stdlib and different from
///   the terminal. This lets `code_scope_reverse_deps_batch` (which passes the exact
///   `qualified_name`) match callers of `A::b()` by searching for `"A::b"`.
/// - `call_expression` with `field_expression` (`self.x()`, `s.parse()`, `store.method()`) →
///   stores the **terminal** (`x`, `parse`, `method`) from the `field_identifier`, plus
///   the **qualified** form `Type::x` when the receiver type is syntactically resolvable
///   via [`ResolveCtx`] (`self` → impl type; typed local or parameter → its type).
///   tree-sitter-rust 0.24+ represents all method calls this way (there is NO distinct
///   `method_call_expression` node in this version of the grammar).
///   Non-resolvable receivers (inference, chaining, trait dispatch) keep the terminal
///   only — never a false positive in the reverse graph.
///
/// Callees matching `STDLIB_NOISE_METHODS` or whose path starts with a known
/// `STDLIB_PREFIXES` entry are ignored.
fn collect_call_deps(
    node: Node<'_>,
    source: &[u8],
    seen: &mut std::collections::HashSet<String>,
    deps: &mut Vec<String>,
    ctx: ResolveCtx<'_>,
) {
    match node.kind() {
        "call_expression" => {
            // Extraire le callee depuis le champ `function` (terminal + path qualifié optionnel).
            // Axe C hybride : pour les scoped_identifiers (`Token::new`), on stocke BOTH :
            // - le terminal (`new`) — pour la compatibilité ascendante,
            // - le path qualifié (`Token::new`) — pour le matching exact dans reverse_deps.
            if let Some((terminal, maybe_qualified)) = extract_call_callee_both(node, source, ctx)
                && !STDLIB_NOISE_METHODS.contains(&terminal.as_str())
            {
                if seen.insert(terminal.clone()) {
                    deps.push(terminal);
                }
                // Stocker aussi le path qualifié si disponible et non-dupliqué.
                if let Some(qualified) = maybe_qualified
                    && !STDLIB_NOISE_METHODS.contains(&qualified.as_str())
                    && seen.insert(qualified.clone())
                {
                    deps.push(qualified);
                }
            }
            // Continuer le DFS dans les arguments (peuvent contenir d'autres appels).
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "arguments" {
                    collect_call_deps(child, source, seen, deps, ctx);
                }
            }
        }
        "method_call_expression" => {
            // NOTE : dans tree-sitter-rust 0.24.2, ce nœud n'existe PAS —
            // les appels de méthode `self.parse()` sont des `call_expression`
            // avec un callee `field_expression`. Cet arm est conservé pour
            // compatibilité future uniquement (dead code en 0.24.2).
            // Le fix P2-1 est implémenté dans `extract_call_callee_both`.
            if let Some(method_name) = extract_method_name(node, source)
                && !STDLIB_NOISE_METHODS.contains(&method_name.as_str())
                && seen.insert(method_name.clone())
            {
                deps.push(method_name);
            }
            // DFS dans receiver et arguments.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_call_deps(child, source, seen, deps, ctx);
            }
        }
        _ => {
            // DFS récursif dans tous les enfants.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_call_deps(child, source, seen, deps, ctx);
            }
        }
    }
}

/// Extracts the terminal and the qualified path of the callee in a `call_expression`.
///
/// Returns `Some((terminal, maybe_qualified))`:
/// - Direct `identifier` → `(name, None)` (no qualified path, no `::`)
/// - `scoped_identifier` (`A::b::c`) → `(c, Some("A::b::c"))` if non-stdlib
///   and the terminal differs from the full path (avoids a redundant duplicate for `b`).
/// - `field_expression` (`self.parse`, `store.method`) → `(field_identifier, maybe_qualified)`.
///   tree-sitter-rust 0.24.2 represents `self.parse()` as a `call_expression` whose
///   callee is a `field_expression`, NOT a `method_call_expression`. The
///   qualified form `Type::method` is produced when the receiver type is syntactically
///   resolvable via [`ResolveCtx`] (receiver `self` → impl type; receiver identifier
///   bound to a typed `let`/parameter → its type). Otherwise `maybe_qualified = None`.
/// - Any other node kind (`generic_function`, etc.) → `None`.
///
/// # Stdlib filter
///
/// If the first path segment is a known stdlib prefix (`std`, `core`, `alloc`, …),
/// returns `None` — neither the terminal nor the path is indexed. Applied to scoped
/// calls, and to resolved method receivers (a receiver typed `std::…` is not qualified).
///
/// # Precision > recall (out of scope)
///
/// Method receivers that are not syntactically resolvable — inference, chaining
/// (`a.b().c()`), trait dispatch, generics, macros, closures — keep the terminal only.
/// `reverse_deps("Type::method")` will not find those callers, but the reverse graph
/// never gains a false edge.
fn extract_call_callee_both(
    node: Node<'_>,
    source: &[u8],
    ctx: ResolveCtx<'_>,
) -> Option<(String, Option<String>)> {
    // Le champ `function` est le premier enfant significatif (identifier, scoped_identifier
    // ou field_expression pour les appels de méthode obj.method()).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = node_text(child, source);
                // Garde défensive : exclure si le nom est lui-même un préfixe stdlib.
                if STDLIB_PREFIXES.contains(&name) {
                    return None;
                }
                return Some((name.to_string(), None));
            }
            "scoped_identifier" => {
                let full_path = node_text(child, source);
                // Filtrer si le premier segment est un préfixe stdlib.
                let first_segment = full_path.split("::").next().unwrap_or("");
                if STDLIB_PREFIXES.contains(&first_segment) {
                    return None;
                }
                // Terminal = dernier segment du path.
                let terminal = full_path
                    .rsplit("::")
                    .next()
                    .unwrap_or(full_path)
                    .to_string();
                // Path qualifié = le full_path complet, s'il est différent du terminal.
                // (Si full_path == terminal, pas de `::` → pas de qualifié utile.)
                let maybe_qualified = if full_path != terminal {
                    Some(full_path.to_string())
                } else {
                    None
                };
                return Some((terminal, maybe_qualified));
            }
            "field_expression" => {
                // `receiver.method` — tree-sitter-rust 0.24.2 utilise `field_expression`
                // (pas `method_call_expression`) pour `obj.method()` et `self.method()`.
                // Terminal = `field_identifier` (nom de méthode). Qualifié (F-70) = résolu
                // depuis le type du receiver quand il est syntaxiquement explicite.
                let Some(method_node) = child.child_by_field_name("field") else {
                    // Pas de champ `field` (cas imprévu) → ignorer, robustesse.
                    return None;
                };
                if method_node.kind() != "field_identifier" {
                    // Accès de tuple (`t.0`) ou autre → pas un appel de méthode nommée.
                    return None;
                }
                let method_name = node_text(method_node, source).to_string();
                if STDLIB_NOISE_METHODS.contains(&method_name.as_str()) {
                    return None;
                }
                let maybe_qualified = child
                    .child_by_field_name("value")
                    .and_then(|recv| qualify_receiver(recv, &method_name, ctx, source));
                return Some((method_name, maybe_qualified));
            }
            // generic_function, etc. → ignorer.
            _ => {}
        }
    }
    None
}

/// Resolves a method-call receiver to a qualified callee `Type::method`.
///
/// Returns `Some("Type::method")` only when the receiver type is syntactically explicit:
/// - receiver `self` → the enclosing impl type ([`ResolveCtx::impl_type`]),
/// - receiver a bare `identifier` bound to a typed `let`/parameter ([`ResolveCtx::locals`]).
///
/// Returns `None` for any non-resolvable receiver (chained call, field access, literal,
/// unknown variable), when the resolved type is a stdlib crate prefix, or when it is a
/// stdlib container/wrapper ([`STDLIB_TYPE_DENYLIST`] — noise + `Deref` mis-qualification).
/// Never a false positive.
fn qualify_receiver(
    receiver: Node<'_>,
    method: &str,
    ctx: ResolveCtx<'_>,
    source: &[u8],
) -> Option<String> {
    let ty: String = match receiver.kind() {
        "self" => ctx.impl_type?.to_string(),
        "identifier" => {
            let name = node_text(receiver, source);
            if name == "self" {
                // Défensif : certains dialectes exposent `self` en `identifier`.
                ctx.impl_type?.to_string()
            } else {
                ctx.locals.get(name)?.clone()
            }
        }
        _ => return None,
    };

    // Garde stdlib (préfixe de crate) : un receiver typé `std::…` n'est pas qualifié.
    if STDLIB_PREFIXES.contains(&ty.split("::").next().unwrap_or("")) {
        return None;
    }
    // Garde conteneurs/wrappers (R1) : Vec/Option/Arc/… → bruit ou mis-qualification Deref.
    let base = ty.rsplit("::").next().unwrap_or(&ty);
    if STDLIB_TYPE_DENYLIST.contains(&base) {
        return None;
    }
    Some(format!("{ty}::{method}"))
}

/// Extracts the method name from a `method_call_expression`.
///
/// ## tree-sitter-rust 0.24.2 note
///
/// In tree-sitter-rust 0.24.2, method calls like `self.parse()` / `obj.method()`
/// are represented as `call_expression` nodes with a `field_expression` callee, NOT
/// as `method_call_expression`. That node kind does NOT exist in this grammar version.
///
/// This function is therefore **dead code** for tree-sitter-rust 0.24.2:
/// the `"method_call_expression"` arm in `collect_call_deps` never executes.
/// The `field_expression` arm in `extract_call_callee_both` handles these calls instead.
///
/// The function is kept for forward compatibility if a future grammar version
/// reintroduces `method_call_expression`, and as AST exploration documentation.
#[allow(dead_code)]
fn extract_method_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Structure théorique si method_call_expression existait :
    // receiver `.` name:field_identifier type_arguments? arguments.
    let mut cursor = node.walk();
    let mut after_dot = false;
    for child in node.children(&mut cursor) {
        match child.kind() {
            "." => {
                after_dot = true;
            }
            // tree-sitter-rust : le nom de méthode est un `field_identifier`.
            // Fallback sur `identifier` pour robustesse.
            "field_identifier" | "identifier" if after_dot => {
                return Some(node_text(child, source).to_string());
            }
            _ => {
                if after_dot && child.kind() != "type_arguments" {
                    after_dot = false;
                }
            }
        }
    }
    None
}

/// Extracts the path from a `use` statement.
fn extract_use_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "scoped_identifier" | "identifier" | "scoped_use_list"
        ) {
            let text = node_text(child, source).to_string();
            // Filtrer les deps externes évidentes (std, core, alloc, gradatum_core, etc.).
            // Best-effort : on garde tout (l'appelant filtre si besoin).
            return Some(text);
        }
    }
    None
}

/// [`crate::language_parser::LanguageParser`] implementation for Rust (tree-sitter-rust).
///
/// Encapsulates knowledge of the Rust grammar: node kinds, symbol extraction,
/// and visibility filtering.
pub(crate) struct RustParser {
    /// Visibility mode applied during symbol extraction.
    pub(crate) visibility: Visibility,
}

impl crate::language_parser::LanguageParser for RustParser {
    fn ts_language(&self) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol> {
        let root = tree.root_node();
        let mut symbols = Vec::new();
        let mut seen_keys: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        extract_top_level_items(
            root,
            source,
            source_path,
            self.visibility,
            &mut symbols,
            &mut seen_keys,
        );

        symbols
    }
}
