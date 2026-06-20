//! Parser tree-sitter pour les fichiers Rust (feature `code-rust`).
//!
//! ## Entités extraites
//!
//! - Fonctions top-level (`fn`) — publiques en mode `Pub`, toutes en mode `All`
//! - Structs, enums, traits top-level — filtre de visibilité identique
//! - Blocs `impl` (qualified_name = `"impl Type"` ou `"impl Trait for Type"`)
//! - Constantes et types associés top-level — filtre de visibilité identique
//! - Modules top-level (tous, quel que soit le mode — structurent l'espace de noms)
//! - Méthodes dans un bloc `impl` — filtre de visibilité identique
//!
//! ## Non-extraits
//!
//! - Macros procédurales (`#[derive]`, `proc_macro`) — invisibles à tree-sitter
//! - Closures, lambda, items dans des fonctions
//!
//! ## Accuracy > coverage
//!
//! En cas de nœud mal formé ou de type non reconnu, le symbole est omis.
//! Un fichier entièrement non-parsable retourne `Ok(vec![])`.

use tree_sitter::Node;

use crate::DerivedSymbol;

/// Mode de visibilité pour l'extraction de symboles.
///
/// - `Pub` : seuls les items publics (`pub`, `pub(crate)`, etc.) sont extraits.
///   Comportement par défaut, préserve le comportement historique.
/// - `All` : tous les items sont extraits, indépendamment de leur visibilité.
///   Utile pour indexer la surface interne d'un crate (tests, refactoring, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// Indexer uniquement les items publics (comportement par défaut).
    Pub,
    /// Indexer tous les items, y compris les items privés.
    All,
}

/// Extrait les items top-level d'un nœud (source_file ou mod_item).
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

/// Insère un symbole dans la liste, en marquant `ambiguous=true` si le couple (kind, name) existe déjà.
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

/// Retourne `true` si le nœud a un attribut `pub` ou `pub(crate)` à sa visibilité.
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

/// Retourne le texte UTF-8 d'un nœud.
///
/// ## Invariant de sécurité (SecAudit #1)
///
/// `source` doit être le MÊME buffer `content.as_bytes()` que celui passé à
/// `parser.parse(content, None)` (voir `parse()` lignes 59 + 69). Les offsets
/// byte de l'AST tree-sitter sont garantis dans ce slice : `node.utf8_text(source)`
/// ne peut pas indexer hors-bornes tant que `source` est identique au buffer parsé.
/// `.unwrap_or("")` est défensif mais ne devrait jamais se déclencher.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Calcule le span 1-based inclusif `(start_line, end_line)` du nœud tree-sitter.
///
/// Règles (caveats council B2/B3) :
/// - Span = nœud de l'item seul (pas les attributs `#[...]` ni les doc-comments — siblings).
/// - Lines 1-based : `row + 1` (tree-sitter = 0-based).
/// - Si `end_position().row` pointe une ligne vide terminale (colonne 0, donc newline final)
///   → `end_line = end_position().row` (sans +1) pour exclure la ligne vide.
/// - Span dégénéré (`start > end`, `start = 0` impossible mais guard) → `None`.
/// - `None` si non extractible (accuracy > coverage).
///
/// Borne des lignes : le contenu brut n'est pas vérifié ici (les lignes du fichier ne sont
/// pas comptées) — la validation finale (start > nb_lignes, fichier raccourci) est faite
/// côté handler avant le slice (B3 complet).
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

/// Extrait le premier enfant de type `identifier` ou `type_identifier`.
fn first_identifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier") {
            return Some(node_text(child, source));
        }
    }
    None
}

/// Extrait le doc-comment précédant un nœud (ligne(s) `/// ...` ou `/** ... */`).
///
/// Cherche les siblings précédents de type `line_comment` ou `block_comment`.
/// Limite à 5 lignes.
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

/// Extrait une fonction ou méthode.
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
    let deps = extract_deps(node, source);
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

/// Extrait la signature textuelle d'une fonction (params + retour), ≤ 1 ligne.
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

/// Extrait un item nommé (struct/enum/trait/type).
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

/// Extrait un bloc impl et ses méthodes.
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

/// Extrait une constante top-level.
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

/// Extrait un module top-level.
///
/// Les modules sont extraits dans les deux modes (Pub et All) car ils structurent
/// l'espace de noms — un module privé peut contenir des items `pub(crate)` importants.
/// La visibilité du module lui-même est quand même reflétée dans le champ `visibility`.
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

/// Extrait les dépendances d'un nœud (fonction/méthode) : combinaison des `use` items
/// intra-corps ET des callees réels (arêtes d'appel du callgraph).
///
/// ## Format retenu
///
/// Chaque entrée est le **segment terminal du path d'appel** (nom simple du callee).
/// Exemples : `helper` pour `helper()`, `new` pour `Token::new()`, `parse` pour `self.parse()`.
///
/// Ce format est cohérent avec `qualified_name` des **fonctions libres** (dont le
/// qualified_name est déjà le nom simple). En revanche, pour les **méthodes**, le
/// qualified_name stocké dans le vault est `Type::methode` — le segment terminal `methode`
/// ne matche donc qu'une partie du qualified_name. Conséquence : `code_scope_reverse_deps`
/// (filtre `WHERE d.value = ?`) fonctionne correctement pour les fonctions libres, mais
/// produit des résultats **partiels** pour les méthodes (plusieurs types peuvent avoir une
/// méthode du même nom). Le fix complet requiert une résolution de type à l'ingest
/// (dette connue, reportée).
///
/// ## Déduplication et cap
///
/// Les doublons (même callee appelé plusieurs fois) sont éliminés. Cap global à 20.
///
/// ## Filtrage stdlib
///
/// Les méthodes ultra-génériques de la stdlib (`clone`, `to_string`, `len`, `iter`, etc.)
/// sont exclues : elles n'apportent pas d'information structurelle utile au callgraph
/// et génèrent du bruit dans reverse-deps.
fn extract_deps(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deps = Vec::new();

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
        collect_call_deps(body, source, &mut seen, &mut deps);
    }

    // Cap global à 20 (accuracy > coverage).
    deps.truncate(20);
    deps
}

/// Noms de méthodes stdlib ultra-génériques exclus du callgraph (filtre anti-bruit).
///
/// Ces méthodes apparaissent dans quasiment tout le code Rust et n'apportent
/// aucune information structurelle utile pour le reverse-deps du callgraph.
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

/// Préfixes de crates stdlib/core/alloc à exclure des call_expression paths.
///
/// Un appel `std::mem::take(x)` → le segment final est `take`, mais le path commence
/// par `std` → filtrer à la source pour éviter d'indexer des fonctions stdlib.
const STDLIB_PREFIXES: &[&str] = &["std", "core", "alloc", "tokio", "futures", "serde"];

/// Trouve le nœud `block` (corps) d'une fonction.
fn find_block(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|&child| child.kind() == "block")
}

/// Collecte récursivement les callees dans un nœud AST.
///
/// Visite tous les descendants en DFS et extrait :
/// - `call_expression` avec `identifier` direct → stocke le terminal.
/// - `call_expression` avec `scoped_identifier` (`A::b()`) → stocke le **terminal**
///   (`b`) ET le **path qualifié complet** (`A::b`) si non-stdlib et différent du terminal.
///   Cela permet à `code_scope_reverse_deps_batch` (qui passe le `qualified_name` exact)
///   de matcher les callers de `A::b()` en cherchant `"A::b"`.
/// - `method_call_expression` (`self.x()`) → stocke uniquement le terminal (`x`).
///   Le type du receiver n'est pas résolvable syntaxiquement sans analyse sémantique ;
///   `reverse_deps("Type::x")` ne trouvera donc PAS ces callers (compromis documenté).
///
/// Les callees correspondant à `STDLIB_NOISE_METHODS` ou dont le path commence par
/// `STDLIB_PREFIXES` sont ignorés.
fn collect_call_deps(
    node: Node<'_>,
    source: &[u8],
    seen: &mut std::collections::HashSet<String>,
    deps: &mut Vec<String>,
) {
    match node.kind() {
        "call_expression" => {
            // Extraire le callee depuis le champ `function` (terminal + path qualifié optionnel).
            // Axe C hybride : pour les scoped_identifiers (`Token::new`), on stocke BOTH :
            // - le terminal (`new`) — pour la compatibilité ascendante,
            // - le path qualifié (`Token::new`) — pour le matching exact dans reverse_deps.
            if let Some((terminal, maybe_qualified)) = extract_call_callee_both(node, source)
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
                    collect_call_deps(child, source, seen, deps);
                }
            }
        }
        "method_call_expression" => {
            // Extraire le nom de la méthode (champ `name` = identifier).
            if let Some(method_name) = extract_method_name(node, source)
                && !STDLIB_NOISE_METHODS.contains(&method_name.as_str())
                && seen.insert(method_name.clone())
            {
                deps.push(method_name);
            }
            // DFS dans receiver et arguments.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_call_deps(child, source, seen, deps);
            }
        }
        _ => {
            // DFS récursif dans tous les enfants.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_call_deps(child, source, seen, deps);
            }
        }
    }
}

/// Extrait le terminal ET le path qualifié du callee d'un `call_expression`.
///
/// Retourne `Some((terminal, maybe_qualified))` :
/// - `identifier` direct → `(name, None)` (pas de path qualifié, pas de `::`)
/// - `scoped_identifier` (`A::b::c`) → `(c, Some("A::b::c"))` si non-stdlib
///   et si le terminal diffère du path complet (évite un doublon inutile sur `b`).
/// - Tout autre type de nœud (field_expression, etc.) → `None`.
///
/// # Filtre stdlib
///
/// Si le premier segment du path est un préfixe stdlib connu (`std`, `core`, `alloc`, …),
/// retourne `None` — ni le terminal ni le path ne sont indexés.
///
/// # Compromis faux-positifs
///
/// Pour les appels `method_call_expression` (`self.set_pending()`), le type du receiver
/// n'est pas connu syntaxiquement. Seul le terminal est stocké via `extract_method_name` ;
/// `reverse_deps("SlowJobStore::set_pending")` ne trouvera **pas** ces callers.
/// Coverage partiel inévitable sans analyse sémantique complète.
fn extract_call_callee_both(node: Node<'_>, source: &[u8]) -> Option<(String, Option<String>)> {
    // Le champ `function` est le premier enfant significatif (identifier ou scoped_identifier).
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
            // field_expression, generic_function, etc. → ignorer.
            _ => {}
        }
    }
    None
}

/// Extrait le nom de méthode d'un `method_call_expression`.
///
/// Le champ `name` est l'identifier directement après le `.` (avant `(`).
/// tree-sitter-rust le représente comme un enfant `identifier` positionné entre
/// le receiver et les arguments.
fn extract_method_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    // Structure : receiver `.` name `(` arguments `)`.
    // On cherche l'identifier qui suit le `.`.
    let mut cursor = node.walk();
    let mut after_dot = false;
    for child in node.children(&mut cursor) {
        match child.kind() {
            "." => {
                after_dot = true;
            }
            "identifier" if after_dot => {
                return Some(node_text(child, source).to_string());
            }
            _ => {
                if after_dot && child.kind() != "type_arguments" {
                    // Reset si on a dépassé sans trouver l'identifier (ne devrait pas arriver).
                    after_dot = false;
                }
            }
        }
    }
    None
}

/// Extrait le chemin d'un `use` statement.
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

/// Implémentation [`crate::language_parser::LanguageParser`] pour Rust (tree-sitter-rust).
///
/// Encapsule la connaissance de la grammaire Rust : node-kinds, extraction de symboles,
/// filtrage de visibilité.
pub(crate) struct RustParser {
    /// Mode de visibilité appliqué lors de l'extraction.
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
