//! Anthropic ↔ internal (OpenAI-compatible) translation functions.
//!
//! This module is purely functional — no I/O, no Axum dependency.
//! All functions are independently testable.
//!
//! # Supported content types
//! - Plain text (system, user, assistant messages)
//! - Full tool use (tools[], tool_choice, tool_use/tool_result/image blocks)
//!
//! # Errors
//! All functions return `Result<_, TranslateError>` to ensure errors are never
//! silently dropped.

use thiserror::Error;

use crate::commons::{
    anthropic::{
        AnthropicContent, AnthropicMessage, AnthropicUsage, ContentBlock, ImageSource,
        MessagesRequest, MessagesResponse, ResponseBlock, SystemContent, Tool, ToolChoice,
    },
    chat::{
        ChatCompletionRequest, ChatCompletionResponse, ContentPart, ForcedFunction,
        FunctionCallResult, FunctionDefinition, ImageUrlDetail, Message, MessageContent, Role,
        ToolCall, ToolChoice as OpenAIToolChoice, ToolDefinition,
    },
};

/// Alias modèle par défaut utilisé quand aucun mapping explicite n'est configuré.
///
/// TODO: make this configurable via `[messages] default_alias` in the TOML.
pub const DEFAULT_ALIAS: &str = "default";

/// Erreur de traduction Anthropic ↔ interne.
#[derive(Debug, Error, PartialEq)]
pub enum TranslateError {
    /// La réponse interne ne contient aucun choix.
    #[error("réponse backend vide : aucun choix dans choices[]")]
    EmptyChoices,
    /// URL d'image invalide : le schéma doit être `https://`.
    ///
    /// Seules les URL HTTPS sont acceptées pour éviter SSRF via protocoles non chiffrés.
    /// La valeur contient l'URL rejetée (masquée dans les logs pour éviter la fuite).
    #[error("URL image invalide (schéma non-https) : schéma attendu https://")]
    InvalidImageUrl(String),
}

/// Traduit une `MessagesRequest` Anthropic en `ChatCompletionRequest` interne.
///
/// # Comportement
/// - Le champ `system` (texte ou blocs texte) est inséré en tête de `messages[]`
///   comme message de rôle `system`.
/// - Messages `user`/`assistant` : texte brut, blocs text, tool_use, tool_result, image.
/// - `tools[]` Anthropic → `ToolDefinition[]` OpenAI (`input_schema` → `parameters`).
/// - `tool_choice` Anthropic → OpenAI : auto→"auto", any→"required", tool→fonction forcée,
///   none→"none".
/// - `max_tokens`, `temperature`, `top_p`, `stop_sequences` mappés directement.
/// - `resolved_model` est injecté dans le champ `model` (alias interne).
///
/// # Errors
/// `TranslateError::EmptyChoices` n'est pas produit ici.
/// This function is infallible — all content block types are supported.
pub fn anthropic_to_chat(
    req: &MessagesRequest,
    resolved_model: &str,
) -> Result<ChatCompletionRequest, TranslateError> {
    let mut messages: Vec<Message> = Vec::new();

    // Injection du system en tête si présent.
    if let Some(system) = &req.system {
        let text = extract_text_from_system(system);
        messages.push(Message::system(text));
    }

    // Traduction de chaque message.
    // V5 : translate_message propage TranslateError::InvalidImageUrl si URL non-https.
    // C1 : translate_message retourne Vec<Message> pour supporter N tool_result parallèles.
    for msg in &req.messages {
        let translated = translate_message(msg)?;
        // N messages role:tool pour N tool_result (tool-use parallèle).
        // 1 message assistant avec tool_calls pour les blocs tool_use.
        messages.extend(translated);
    }

    // Traduction tools[] Anthropic → ToolDefinition[] OpenAI.
    let tools = req.tools.as_deref().map(translate_tools);

    // Traduction tool_choice Anthropic → OpenAI.
    //
    // INCIDENT b9780 — régression GBNF llama.cpp (voir ref. incident F-75 local-claude KO) :
    // Le backend b9780 génère une grammaire GBNF dès que `tools` est présent et que
    // `tool_choice` est absent ou vaut "required". La vraie cause est la TAILLE de la
    // grammaire générée : les contraintes de schéma JSON telles que `maximum`, `minimum`,
    // `maxLength`, `pattern`, `maxItems`, etc. produisent des règles GBNF volumineuses
    // (notamment les règles `integer-range` pour `maximum=9007199254740991`). Ces contraintes
    // sont présentes dans les ~69 outils de Claude Code. Le parser GBNF de b9780 échoue
    // sur ces règles → HTTP 400 "failed to parse grammar".
    // Note : `tool_choice = "auto"` seul ne résout pas le problème si les schémas contiennent
    // encore des contraintes GBNF-bloat. La sanitization des `input_schema` (ci-dessous, via
    // `sanitize_schema`) supprime ces contraintes à la source. Les deux fixes sont
    // complémentaires : `tool_choice = "auto"` évite la génération forcée de grammaire,
    // `sanitize_schema` réduit la taille si elle est quand même générée.
    //
    // Fix : si des tools sont présents et que le client Anthropic n'a pas spécifié de
    // tool_choice (Claude Code n'en envoie pas), forcer `tool_choice = "auto"`.
    // Cela s'applique UNIQUEMENT au chemin /v1/messages (cette fonction) — le chemin
    // OpenAI-compat /v1/chat/completions est non-impacté.
    let tool_choice = match req.tool_choice.as_ref() {
        // tool_choice explicite du client → traduire normalement.
        Some(tc) => Some(translate_tool_choice(tc)),
        // Aucun tool_choice : forcer "auto" si des tools sont présents pour éviter la
        // génération de grammaire GBNF par b9780 (régression llama.cpp).
        None if tools.is_some() => Some(OpenAIToolChoice::auto()),
        // Sans tools, pas de tool_choice à émettre.
        None => None,
    };

    Ok(ChatCompletionRequest {
        model: resolved_model.to_string(),
        messages,
        max_tokens: Some(req.max_tokens),
        temperature: req.temperature,
        top_p: req.top_p,
        stop: req.stop_sequences.clone(),
        stream: req.stream,
        tools,
        tool_choice,
        chat_template_kwargs: None,
    })
}

/// Traduit un `AnthropicMessage` en `Vec<Message>` interne.
///
/// Retourne un vecteur car un message `role:"user"` contenant N blocs `tool_result`
/// (tool-use parallèle) produit N messages `role:tool` distincts (convention OpenAI).
///
/// Logique par rôle et contenu :
/// - `role:"user"` avec N blocs `tool_result` → N messages `role:tool` (C1)
/// - `role:"user"` avec texte/images → 1 message `role:user`
/// - `role:"assistant"` avec blocs → 1 message assistant avec `tool_calls` si tool_use présent
///
/// # Errors
/// - `TranslateError::InvalidImageUrl` si un bloc image contient une URL non-https.
fn translate_message(msg: &AnthropicMessage) -> Result<Vec<Message>, TranslateError> {
    match msg.role.as_str() {
        "user" => translate_user_message(&msg.content),
        "assistant" => Ok(vec![translate_assistant_message(&msg.content)]),
        other => {
            tracing::warn!(role = %other, "rôle Anthropic inconnu — traité comme user");
            translate_user_message(&msg.content)
        }
    }
}

/// Traduit un message de rôle `user` en `Vec<Message>`.
///
/// # Convention OpenAI — tool-use parallèle (C1)
///
/// Quand Claude Code (ou tout client Anthropic) émet un tour `role:user` contenant
/// N blocs `tool_result` (réponses à N `tool_use` parallèles), la convention OpenAI
/// exige **un message `role:tool` par résultat** (chacun avec son `tool_call_id`).
///
/// Avant C1, le code ne traitait que `blocks.first()` et perdait les résultats 2..N,
/// rendant la boucle agentique incohérente côté backend.
///
/// # Contenu mixte (tool_result + texte)
///
/// Si le message contient à la fois des blocs `tool_result` et du texte/images :
/// - Chaque `tool_result` → 1 message `role:tool` (dans l'ordre)
/// - Le texte/images résiduel → 1 message `role:user` supplémentaire (en fin)
///
/// Aucun bloc n'est perdu.
///
/// # Errors
/// - `TranslateError::InvalidImageUrl` si un bloc image contient une URL non-https (V5).
fn translate_user_message(content: &AnthropicContent) -> Result<Vec<Message>, TranslateError> {
    match content {
        AnthropicContent::Text(s) => Ok(vec![Message::user(s)]),
        AnthropicContent::Blocks(blocks) => {
            // Partitionner les blocs : tool_result d'un côté, reste de l'autre.
            let mut tool_messages: Vec<Message> = Vec::new();
            let mut non_tool_blocks: Vec<&ContentBlock> = Vec::new();

            for block in blocks {
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content: result_content,
                        ..
                    } => {
                        // C1 : chaque tool_result → 1 message role:tool distinct.
                        let text = extract_tool_result_text(result_content);
                        tool_messages.push(Message {
                            role: Role::Tool,
                            content: MessageContent::Text(text),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                            name: None,
                        });
                    }
                    other => {
                        non_tool_blocks.push(other);
                    }
                }
            }

            // Si seulement des tool_result, on retourne la liste directement.
            if non_tool_blocks.is_empty() {
                return Ok(tool_messages);
            }

            // Construire le message user résiduel depuis les blocs non-tool_result.
            // V5 : translate_content_blocks_to_parts propage TranslateError::InvalidImageUrl.
            // On ne peut pas passer &[&ContentBlock] directement — on les clone pour avoir &[ContentBlock].
            let owned: Vec<ContentBlock> = non_tool_blocks.into_iter().cloned().collect();
            let parts = translate_content_blocks_to_parts(&owned)?;
            let user_msg = if parts.len() == 1
                && let ContentPart::Text { text } = &parts[0]
            {
                Message::user(text)
            } else {
                Message {
                    role: Role::User,
                    content: MessageContent::Parts(parts),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                }
            };

            // Ordre : tool messages d'abord, user résiduel ensuite.
            tool_messages.push(user_msg);
            Ok(tool_messages)
        }
    }
}

/// Traduit un message de rôle `assistant`.
///
/// Gère :
/// - Texte pur → message assistant texte
/// - Blocs mixtes (text + tool_use) → message assistant avec `tool_calls` + contenu texte
fn translate_assistant_message(content: &AnthropicContent) -> Message {
    match content {
        AnthropicContent::Text(s) => Message::assistant(s),
        AnthropicContent::Blocks(blocks) => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            for block in blocks {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        // Sérialisation des arguments en String JSON (convention OpenAI).
                        let arguments = serde_json::to_string(input).unwrap_or_else(|e| {
                            tracing::warn!(
                                tool_id = %id,
                                error = %e,
                                "échec sérialisation input tool_use → arguments vide"
                            );
                            "{}".to_string()
                        });
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            tool_type: "function".to_string(),
                            function: FunctionCallResult {
                                name: name.clone(),
                                arguments,
                            },
                        });
                    }
                    // Image/ToolResult/Thinking dans un message assistant → ignorés
                    // (ne devraient pas apparaître mais on ne panic pas).
                    other => {
                        tracing::warn!(
                            block_type = ?other,
                            "bloc inattendu dans message assistant — ignoré"
                        );
                    }
                }
            }

            let content = if text_parts.is_empty() {
                MessageContent::Text(String::new())
            } else {
                MessageContent::Text(text_parts.join(""))
            };

            let tool_calls_opt = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            Message {
                role: Role::Assistant,
                content,
                tool_calls: tool_calls_opt,
                tool_call_id: None,
                name: None,
            }
        }
    }
}

/// Traduit une liste de blocs en `Vec<ContentPart>` OpenAI.
///
/// Blocs texte → `ContentPart::Text` ;
/// blocs image base64 → `ContentPart::ImageUrl` avec data-URI.
fn translate_content_blocks_to_parts(
    blocks: &[ContentBlock],
) -> Result<Vec<ContentPart>, TranslateError> {
    let mut parts = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                parts.push(ContentPart::Text { text: text.clone() });
            }
            ContentBlock::Image { source } => {
                // V5 (security-reviewer) : translate_image_source valide le schéma URL.
                if let Some(part) = translate_image_source(source)? {
                    parts.push(part);
                }
            }
            // ToolUse / ToolResult / Thinking dans un bloc user non tool_result
            // → ignorés silencieusement (ne devraient pas apparaître).
            _ => {}
        }
    }

    // Fallback si aucun part produit.
    if parts.is_empty() {
        parts.push(ContentPart::Text {
            text: String::new(),
        });
    }

    Ok(parts)
}

/// Traduit une `ImageSource` Anthropic en `ContentPart::ImageUrl` OpenAI.
///
/// Formats supportés :
/// - `type:"base64"` → `data:<media_type>;base64,<data>` (toujours accepté)
/// - `type:"url"` → URL distante (HTTPS uniquement — anti-SSRF)
///
/// # Errors
/// - `TranslateError::InvalidImageUrl` si le schéma de l'URL n'est pas `https://`.
///
/// # Sécurité
/// Seules les URL `https://` sont acceptées. Les URL `http://`, `ftp://`, vides,
/// ou à schéma personnalisé sont rejetées pour éviter SSRF et exfiltration de données
/// via protocoles non chiffrés.
fn translate_image_source(source: &ImageSource) -> Result<Option<ContentPart>, TranslateError> {
    match source.source_type.as_str() {
        "base64" => {
            let media_type = source.media_type.as_deref().unwrap_or("image/jpeg");
            let data = source.data.as_deref().unwrap_or("");
            let url = format!("data:{};base64,{}", media_type, data);
            Ok(Some(ContentPart::ImageUrl {
                image_url: ImageUrlDetail { url },
            }))
        }
        "url" => {
            let url = source.url.as_deref().unwrap_or("").to_string();
            // Anti-SSRF : rejeter tout schéma non-https.
            if !url.starts_with("https://") {
                tracing::warn!("URL image rejetée : schéma non-https (url tronquée pour logs)");
                return Err(TranslateError::InvalidImageUrl(url));
            }
            Ok(Some(ContentPart::ImageUrl {
                image_url: ImageUrlDetail { url },
            }))
        }
        other => {
            tracing::warn!(source_type = %other, "type de source image inconnu — ignoré");
            Ok(None)
        }
    }
}

/// Extrait le texte d'un `tool_result.content`.
///
/// Anthropic accepte :
/// - `String` → texte direct
/// - `Array` de blocs Text → concaténation
/// - `null` → chaîne vide
fn extract_tool_result_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                // Blocs Text : `{"type":"text","text":"..."}` ou `{"text":"..."}`
                b.get("text").and_then(|t| t.as_str()).map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join(""),
        serde_json::Value::Null => String::new(),
        other => {
            // Cas inattendu : on sérialise en JSON pour ne pas perdre l'info.
            tracing::warn!("format tool_result.content inattendu — sérialisé en JSON brut");
            other.to_string()
        }
    }
}

/// Supprime récursivement les mots-clés JSON Schema qui produisent des règles GBNF
/// volumineuses dans llama.cpp, à tous les niveaux de nesting.
///
/// # Contexte
///
/// Le parser GBNF de llama.cpp b9780 génère des règles `integer-range` et autres
/// constructions volumineuses pour les contraintes numériques et textuelles (`maximum`,
/// `minimum`, `maxLength`, `pattern`, etc.). Ces règles peuvent dépasser la capacité
/// du parser et provoquer `HTTP 400 "failed to parse grammar"`.
///
/// Ces contraintes ne sont pas critiques pour le fonctionnement du LLM — elles servent
/// uniquement à la validation stricte du JSON Schema côté client. Les supprimer permet
/// de réduire drastiquement la taille de la grammaire sans altérer la structure de la
/// réponse attendue.
///
/// # Mots-clés supprimés
///
/// `maximum`, `minimum`, `exclusiveMaximum`, `exclusiveMinimum`, `multipleOf`,
/// `maxLength`, `minLength`, `pattern`, `maxItems`, `minItems`, `uniqueItems`
///
/// # Mots-clés conservés
///
/// `type`, `properties`, `items`, `required`, `description`, `enum`, `format`,
/// `additionalProperties`, `$defs`, `definitions`, `anyOf`, `oneOf`, `allOf`,
/// `title`, `default`, `const`, `$ref`
///
/// # Stabilité
///
/// La sortie est déterministe : `serde_json::Map` sans la feature `preserve_order`
/// utilise un `BTreeMap` (tri lexicographique des clés). Deux appels consécutifs
/// avec la même entrée produisent exactement le même JSON octet-pour-octet.
/// C'est cette propriété — et non un ordre d'insertion — qui garantit la stabilité
/// du prompt-cache LCP (préfixe identique entre requêtes).
///
/// # Niveaux traités récursivement
///
/// - Objet courant (root)
/// - `properties.<key>` → chaque valeur (sous-schéma)
/// - `items` → sous-schéma
/// - `$defs` / `definitions` → chaque valeur (sous-schéma)
/// - `anyOf`, `oneOf`, `allOf` → tableaux de sous-schémas
/// - `additionalProperties` → sous-schéma (si objet ; booléen laissé tel quel)
/// - `prefixItems` (draft 2020-12 / schemars 1.0) → tableau de sous-schémas
pub(crate) fn sanitize_schema(value: serde_json::Value) -> serde_json::Value {
    // Mots-clés à supprimer : contraintes qui génèrent du GBNF-bloat.
    // La liste est statique et exhaustive — ne pas ajouter de mots-clés structurants.
    const GBNF_BLOAT_KEYWORDS: &[&str] = &[
        "maximum",
        "minimum",
        "exclusiveMaximum",
        "exclusiveMinimum",
        "multipleOf",
        "maxLength",
        "minLength",
        "pattern",
        "maxItems",
        "minItems",
        "uniqueItems",
    ];

    match value {
        serde_json::Value::Object(map) => {
            // Construire une nouvelle Map en filtrant les GBNF-bloat keywords et en
            // appliquant la récursion aux valeurs structurantes (properties, items,
            // $defs, definitions, anyOf, additionalProperties, prefixItems, etc.).
            // Note : serde_json sans `preserve_order` utilise un BTreeMap (tri
            // lexicographique) — le déterminisme de la sortie est garanti par ce tri,
            // pas par un ordre d'insertion.
            let mut new_map = serde_json::Map::with_capacity(map.len());

            for (key, val) in map {
                // Filtrage des contraintes GBNF-bloat.
                if GBNF_BLOAT_KEYWORDS.contains(&key.as_str()) {
                    continue;
                }

                // Récursion sur les sous-schémas imbriqués.
                let sanitized_val = match key.as_str() {
                    // `properties` : map de sous-schémas.
                    "properties" | "$defs" | "definitions" => {
                        if let serde_json::Value::Object(inner) = val {
                            let mut inner_map = serde_json::Map::with_capacity(inner.len());
                            for (prop_key, prop_val) in inner {
                                inner_map.insert(prop_key, sanitize_schema(prop_val));
                            }
                            serde_json::Value::Object(inner_map)
                        } else {
                            val
                        }
                    }
                    // `items` : un seul sous-schéma (ou tableau de schémas en draft-3,
                    // mais on traite les deux cas).
                    "items" => match val {
                        serde_json::Value::Object(_) => sanitize_schema(val),
                        serde_json::Value::Array(arr) => {
                            serde_json::Value::Array(arr.into_iter().map(sanitize_schema).collect())
                        }
                        other => other,
                    },
                    // `anyOf`, `oneOf`, `allOf` : tableaux de sous-schémas.
                    "anyOf" | "oneOf" | "allOf" => {
                        if let serde_json::Value::Array(arr) = val {
                            serde_json::Value::Array(arr.into_iter().map(sanitize_schema).collect())
                        } else {
                            val
                        }
                    }
                    // `additionalProperties` : booléen (laissé tel quel) OU sous-schéma.
                    "additionalProperties" => match val {
                        serde_json::Value::Object(_) => sanitize_schema(val),
                        other => other,
                    },
                    // `prefixItems` (draft 2020-12 / schemars 1.0) : tableau de sous-schémas.
                    "prefixItems" => {
                        if let serde_json::Value::Array(arr) = val {
                            serde_json::Value::Array(arr.into_iter().map(sanitize_schema).collect())
                        } else {
                            val
                        }
                    }
                    // NON récursé volontairement : patternProperties / not / if / then / else
                    // / contains / propertyNames — non émis par les tools Claude Code / MCP.
                    // À ajouter ici si ces mots-clés apparaissent dans des schémas futurs.
                    // Toutes les autres clés : valeur laissée telle quelle.
                    _ => val,
                };

                new_map.insert(key, sanitized_val);
            }

            serde_json::Value::Object(new_map)
        }
        // Les non-objets (string, number, bool, null, array) sont retournés tels quels.
        other => other,
    }
}

/// Traduit la liste de `Tool` Anthropic en `Vec<ToolDefinition>` OpenAI.
///
/// Mapping : `{name, description, input_schema}` → `{type:"function", function:{name, description, parameters:input_schema}}`.
///
/// Les `input_schema` sont sanitizés via [`sanitize_schema`] pour supprimer les
/// contraintes JSON Schema qui génèrent du GBNF-bloat dans llama.cpp (voir doc
/// de [`sanitize_schema`] pour la liste des mots-clés supprimés).
fn translate_tools(tools: &[Tool]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|t| {
            ToolDefinition::function(FunctionDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                // Sanitization GBNF-bloat : supprime les contraintes numériques/textuelles
                // qui font exploser le parser GBNF de llama.cpp b9780 (incident F-75).
                // Inoffensif pour les autres providers — ces contraintes ne sont pas
                // critiques pour la génération de réponse.
                parameters: sanitize_schema(t.input_schema.clone()),
                strict: None,
            })
        })
        .collect()
}

/// Traduit un `ToolChoice` Anthropic en `OpenAIToolChoice`.
///
/// | Anthropic | OpenAI |
/// |---|---|
/// | `auto` | `"auto"` |
/// | `any` | `"required"` |
/// | `tool {name}` | `{type:"function",function:{name}}` |
/// | `none` | `"none"` |
fn translate_tool_choice(tc: &ToolChoice) -> OpenAIToolChoice {
    match tc {
        ToolChoice::Auto => OpenAIToolChoice::auto(),
        ToolChoice::Any => OpenAIToolChoice::required(),
        ToolChoice::None => OpenAIToolChoice::none(),
        ToolChoice::Tool { name } => OpenAIToolChoice::Function {
            tool_type: "function".to_string(),
            function: ForcedFunction { name: name.clone() },
        },
    }
}

/// Extrait le texte depuis un `SystemContent`.
fn extract_text_from_system(system: &SystemContent) -> String {
    match system {
        SystemContent::Text(s) => s.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Traduit une `ChatCompletionResponse` interne en `MessagesResponse` Anthropic.
///
/// Handles plain text responses and mixed tool-call + text responses.
///
/// `model` est le nom de modèle à renvoyer dans la réponse (tel que reçu dans la requête).
///
/// # Errors
/// `TranslateError::EmptyChoices` si `choices` est vide.
pub fn chat_to_anthropic(
    resp: &ChatCompletionResponse,
    model: &str,
) -> Result<MessagesResponse, TranslateError> {
    let choice = resp.choices.first().ok_or(TranslateError::EmptyChoices)?;

    let mut content: Vec<ResponseBlock> = Vec::new();

    // Texte de la réponse (si présent) — toujours en premier.
    let text = choice.message.content.text_content();
    if !text.is_empty() {
        content.push(ResponseBlock::Text { text });
    }

    // Tool calls → blocs tool_use Anthropic.
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tc in tool_calls {
            let input = parse_tool_arguments(&tc.function.arguments, &tc.id);
            content.push(ResponseBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input,
            });
        }
    }

    // Si aucun bloc produit (réponse vide), on retourne un bloc texte vide.
    if content.is_empty() {
        content.push(ResponseBlock::Text {
            text: String::new(),
        });
    }

    // Contrat Anthropic : si la réponse contient des blocs ToolUse, stop_reason DOIT être
    // "tool_use" — indépendamment du finish_reason retourné par le provider.
    // Certains providers (llama.cpp, vLLM) retournent "stop" au lieu de "tool_calls"
    // quand ils génèrent des appels d'outils, ce qui provoque un mauvais routage côté client.
    let has_tool_use = content
        .iter()
        .any(|b| matches!(b, ResponseBlock::ToolUse { .. }));
    let stop_reason = if has_tool_use {
        Some("tool_use".to_string())
    } else {
        choice
            .finish_reason
            .as_deref()
            .map(map_stop_reason)
            .map(String::from)
    };

    let usage = build_usage(resp.usage.as_ref());
    let id = format!("msg_{}", resp.id);

    Ok(MessagesResponse {
        id,
        object_type: "message".to_string(),
        role: "assistant".to_string(),
        model: model.to_string(),
        content,
        stop_reason,
        stop_sequence: None,
        usage,
    })
}

/// Parse les arguments d'un tool_call OpenAI (String JSON) en `serde_json::Value`.
///
/// Si le parse échoue, retourne un objet vide `{}` + tracing warn.
/// Ne panic jamais (ADN 1).
fn parse_tool_arguments(arguments: &str, tool_id: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or_else(|e| {
        tracing::warn!(
            tool_id = %tool_id,
            arguments = %arguments,
            error = %e,
            "parse arguments tool_call échoué — input remplacé par {{}}"
        );
        serde_json::Value::Object(serde_json::Map::new())
    })
}

/// Mappe un `finish_reason` OpenAI vers un `stop_reason` Anthropic.
///
/// | OpenAI `finish_reason` | Anthropic `stop_reason` |
/// |---|---|
/// | `"stop"` | `"end_turn"` |
/// | `"length"` | `"max_tokens"` |
/// | `"tool_calls"` | `"tool_use"` |
/// | `"stop_sequence"` | `"stop_sequence"` |
/// | tout autre | `"end_turn"` (fallback sûr) |
#[must_use]
pub fn map_stop_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "stop_sequence" => "stop_sequence",
        _ => "end_turn",
    }
}

/// Construit l'`AnthropicUsage` depuis l'`Usage` interne optionnel.
///
/// Si `usage` est absent, retourne des zéros.
fn build_usage(usage: Option<&crate::commons::chat::Usage>) -> AnthropicUsage {
    match usage {
        Some(u) => AnthropicUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        },
        None => AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
        },
    }
}

// ── Tests unitaires ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::anthropic::{
        AnthropicContent, AnthropicMessage, ContentBlock, ImageSource, SystemContent, Tool,
        ToolChoice,
    };
    use crate::commons::chat::{Choice, Message, MessageContent, Role, ToolCall, Usage};

    // ─── Helpers ─────────────────────────────────────────────────────────────

    /// Crée une `MessagesRequest` minimale (1 user message texte, sans tools).
    fn minimal_request(content: &str) -> MessagesRequest {
        MessagesRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text(content.to_string()),
            }],
            system: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        }
    }

    fn minimal_chat_response(text: &str, finish_reason: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(text),
                finish_reason: Some(finish_reason.to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
            }),
        }
    }

    // ─── Tests Slice A conservés ──────────────────────────────────────────────

    /// Traduction basique : 1 message user texte, pas de system.
    #[test]
    fn anthropic_to_chat_single_user_message_text() {
        let mut req = minimal_request("Bonjour");
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        req.max_tokens = 1024;

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        assert_eq!(result.model, "default");
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, Role::User);
        assert_eq!(
            result.messages[0].content,
            MessageContent::Text("Bonjour".to_string())
        );
        assert_eq!(result.max_tokens, Some(1024));
        assert_eq!(result.temperature, Some(0.7));
        assert_eq!(result.top_p, Some(0.9));
        assert!(result.stop.is_none());
    }

    /// System + 1 message user → 2 messages en sortie (system en tête).
    #[test]
    fn anthropic_to_chat_system_prepended_before_user() {
        let mut req = minimal_request("Question");
        req.system = Some(SystemContent::Text("Tu es un assistant utile.".to_string()));
        req.stop_sequences = Some(vec!["STOP".to_string()]);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        assert_eq!(result.messages.len(), 2, "system + user = 2 messages");
        assert_eq!(result.messages[0].role, Role::System);
        assert_eq!(
            result.messages[0].content,
            MessageContent::Text("Tu es un assistant utile.".to_string())
        );
        assert_eq!(result.messages[1].role, Role::User);
        assert_eq!(result.stop, Some(vec!["STOP".to_string()]));
    }

    /// max_tokens mappé correctement depuis le champ obligatoire Anthropic.
    #[test]
    fn anthropic_to_chat_max_tokens_mapped() {
        let mut req = minimal_request("x");
        req.max_tokens = 4096;

        let result = anthropic_to_chat(&req, "alias").expect("traduction doit réussir");
        assert_eq!(result.max_tokens, Some(4096));
        assert!(result.temperature.is_none());
        assert!(result.top_p.is_none());
    }

    /// System avec blocs Text → concaténé correctement.
    #[test]
    fn anthropic_to_chat_system_blocks_concatenated() {
        let mut req = minimal_request("bonjour");
        req.system = Some(SystemContent::Blocks(vec![
            ContentBlock::Text {
                text: "Partie 1. ".to_string(),
            },
            ContentBlock::Text {
                text: "Partie 2.".to_string(),
            },
        ]));

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(
            result.messages[0].content,
            MessageContent::Text("Partie 1. Partie 2.".to_string())
        );
    }

    /// Conversation multi-tour user/assistant.
    #[test]
    fn anthropic_to_chat_multi_turn_preserved() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Text(
                        "Quelle est la capitale de la France ?".to_string(),
                    ),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicContent::Text("Paris.".to_string()),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Text("Et de l'Espagne ?".to_string()),
                },
            ],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[0].role, Role::User);
        assert_eq!(result.messages[1].role, Role::Assistant);
        assert_eq!(result.messages[2].role, Role::User);
    }

    // ─── Tests Slice A — chat_to_anthropic conservés ─────────────────────────

    /// Réponse texte basique → MessagesResponse conforme.
    #[test]
    fn chat_to_anthropic_basic_text_response() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-abc123".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: "qwen3-30b".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("Voici la réponse."),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 42,
                completion_tokens: 15,
                total_tokens: 57,
                prompt_tokens_details: None,
            }),
        };

        let result = chat_to_anthropic(&resp, "claude-3-5-sonnet-20241022")
            .expect("traduction doit réussir");

        assert_eq!(result.object_type, "message");
        assert_eq!(result.role, "assistant");
        assert_eq!(result.model, "claude-3-5-sonnet-20241022");
        assert_eq!(result.id, "msg_chatcmpl-abc123");
        assert_eq!(result.content.len(), 1);
        assert_eq!(
            result.content[0],
            ResponseBlock::Text {
                text: "Voici la réponse.".to_string()
            }
        );
        assert_eq!(result.stop_reason, Some("end_turn".to_string()));
        assert!(result.stop_sequence.is_none());
    }

    /// Usage mappé correctement.
    #[test]
    fn chat_to_anthropic_usage_mapped() {
        let resp = minimal_chat_response("ok", "stop");
        let mut resp = resp;
        resp.usage = Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_tokens_details: None,
        });

        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir");
        assert_eq!(result.usage.input_tokens, 100);
        assert_eq!(result.usage.output_tokens, 50);
    }

    /// finish_reason "length" → stop_reason "max_tokens".
    #[test]
    fn chat_to_anthropic_stop_reason_length_becomes_max_tokens() {
        let resp = minimal_chat_response("tronqué", "length");
        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir");
        assert_eq!(result.stop_reason, Some("max_tokens".to_string()));
    }

    /// choices[] vide → TranslateError::EmptyChoices.
    #[test]
    fn chat_to_anthropic_empty_choices_returns_error() {
        let resp = ChatCompletionResponse {
            id: "id".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "m".to_string(),
            choices: vec![],
            usage: None,
        };

        let err = chat_to_anthropic(&resp, "m").expect_err("doit échouer sur choices vide");
        assert_eq!(err, TranslateError::EmptyChoices);
    }

    /// usage absent → input/output_tokens = 0.
    #[test]
    fn chat_to_anthropic_missing_usage_defaults_to_zero() {
        let resp = ChatCompletionResponse {
            id: "id".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "m".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("ok"),
                finish_reason: None,
            }],
            usage: None,
        };

        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir");
        assert_eq!(result.usage.input_tokens, 0);
        assert_eq!(result.usage.output_tokens, 0);
    }

    // ─── map_stop_reason table ────────────────────────────────────────────────

    /// Table complète des mappings stop_reason.
    #[test]
    fn map_stop_reason_table() {
        assert_eq!(map_stop_reason("stop"), "end_turn");
        assert_eq!(map_stop_reason("length"), "max_tokens");
        assert_eq!(map_stop_reason("tool_calls"), "tool_use");
        assert_eq!(map_stop_reason("stop_sequence"), "stop_sequence");
        assert_eq!(map_stop_reason("content_filter"), "end_turn");
        assert_eq!(map_stop_reason(""), "end_turn");
        assert_eq!(map_stop_reason("unknown_xyz"), "end_turn");
    }

    // ─── Tests Slice B — tools request ───────────────────────────────────────

    /// Tool definition round-trip : Tool Anthropic → ToolDefinition OpenAI.
    ///
    /// `input_schema` doit devenir `parameters` ; `name` et `description` préservés.
    #[test]
    fn translate_tools_definition_round_trip() {
        let mut req = minimal_request("utilise l'outil météo");
        req.tools = Some(vec![Tool {
            name: "get_weather".to_string(),
            description: Some("Retourne la météo pour une ville.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }),
        }]);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        let tools = result.tools.expect("tools doit être présent");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(
            tools[0].function.description.as_deref(),
            Some("Retourne la météo pour une ville.")
        );
        assert_eq!(
            tools[0].function.parameters["properties"]["location"]["type"],
            "string"
        );
    }

    /// Tool sans description → description None en sortie.
    #[test]
    fn translate_tools_without_description() {
        let mut req = minimal_request("test");
        req.tools = Some(vec![Tool {
            name: "search".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }]);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        let tools = result.tools.expect("tools doit être présent");
        assert!(tools[0].function.description.is_none());
    }

    /// Plusieurs outils → tous traduits dans l'ordre.
    #[test]
    fn translate_tools_multiple_preserved_order() {
        let mut req = minimal_request("test");
        req.tools = Some(vec![
            Tool {
                name: "tool_a".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            },
            Tool {
                name: "tool_b".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            },
        ]);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        let tools = result.tools.expect("tools doit être présent");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.name, "tool_a");
        assert_eq!(tools[1].function.name, "tool_b");
    }

    // ─── Tests Slice B — tool_choice ─────────────────────────────────────────

    /// tool_choice auto → OpenAI "auto".
    #[test]
    fn translate_tool_choice_auto() {
        let mut req = minimal_request("test");
        req.tool_choice = Some(ToolChoice::Auto);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode("auto".to_string()))
        );
    }

    /// tool_choice any → OpenAI "required".
    #[test]
    fn translate_tool_choice_any_becomes_required() {
        let mut req = minimal_request("test");
        req.tool_choice = Some(ToolChoice::Any);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode(
                "required".to_string()
            ))
        );
    }

    /// tool_choice tool {name} → OpenAI function forced.
    #[test]
    fn translate_tool_choice_tool_forced_function() {
        let mut req = minimal_request("test");
        req.tool_choice = Some(ToolChoice::Tool {
            name: "get_weather".to_string(),
        });

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Function {
                tool_type: "function".to_string(),
                function: ForcedFunction {
                    name: "get_weather".to_string()
                }
            })
        );
    }

    /// tool_choice none → OpenAI "none".
    #[test]
    fn translate_tool_choice_none() {
        let mut req = minimal_request("test");
        req.tool_choice = Some(ToolChoice::None);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode("none".to_string()))
        );
    }

    // ─── Fix incident b9780 — tool_choice auto forcé quand tools présents ───────

    /// Incident b9780 (régression GBNF llama.cpp) :
    /// une requête `/v1/messages` avec tools mais sans `tool_choice` doit produire
    /// un `ChatCompletionRequest` avec `tool_choice = "auto"`.
    ///
    /// Raison : llama.cpp b9780 génère une grammaire GBNF quand `tool_choice` est
    /// absent ou vaut "required". Ce parser échoue sur les schémas integer avec
    /// `maximum=9007199254740991` (les 69 tools de Claude Code) → HTTP 400.
    /// Avec `tool_choice = "auto"`, b9780 ne génère pas de grammaire → HTTP 200.
    #[test]
    fn tools_without_tool_choice_gets_auto_tool_choice() {
        let mut req = minimal_request("utilise l'outil météo");
        req.tools = Some(vec![Tool {
            name: "get_weather".to_string(),
            description: Some("Retourne la météo.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string"
                    },
                    "max_results": {
                        "type": "integer",
                        "maximum": 9007199254740991_i64
                    }
                },
                "required": ["location"]
            }),
        }]);
        // Pas de tool_choice explicite (cas Claude Code standard).
        assert!(req.tool_choice.is_none());

        let result = anthropic_to_chat(&req, "default-vl").expect("traduction doit réussir");

        // Le fix doit avoir injecté tool_choice = "auto".
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode("auto".to_string())),
            "tool_choice doit être 'auto' pour éviter la régression GBNF b9780"
        );
        // Les tools doivent toujours être présents.
        assert!(result.tools.is_some(), "tools doit rester présent");
    }

    /// Sans tools, tool_choice doit rester None (pas d'injection parasite).
    #[test]
    fn no_tools_no_tool_choice_injected() {
        let req = minimal_request("question simple sans outil");
        // Aucun tools, aucun tool_choice.
        assert!(req.tools.is_none());
        assert!(req.tool_choice.is_none());

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        assert!(
            result.tool_choice.is_none(),
            "sans tools, tool_choice doit rester None"
        );
    }

    /// tool_choice explicite du client (ex: `any`) est préservé même avec tools.
    ///
    /// Le fix ne doit pas écraser un tool_choice explicitement envoyé par le client.
    #[test]
    fn explicit_tool_choice_not_overridden_by_auto_fix() {
        let mut req = minimal_request("force tool use");
        req.tools = Some(vec![Tool {
            name: "search".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
        }]);
        // Client envoie explicitement `any` (= "required" OpenAI).
        req.tool_choice = Some(ToolChoice::Any);

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        // "any" → "required", pas "auto".
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode(
                "required".to_string()
            )),
            "tool_choice explicite 'any' doit rester 'required', pas 'auto'"
        );
    }

    // ─── Tests Slice B — message assistant avec tool_use ─────────────────────

    /// Message assistant avec bloc tool_use → role:assistant avec tool_calls.
    #[test]
    fn translate_assistant_tool_use_block_becomes_tool_calls() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "assistant".to_string(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "toolu_01A".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::json!({"location": "Paris"}),
                }]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(result.messages.len(), 1);

        let msg = &result.messages[0];
        assert_eq!(msg.role, Role::Assistant);

        let tool_calls = msg
            .tool_calls
            .as_ref()
            .expect("tool_calls doit être présent");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_01A");
        assert_eq!(tool_calls[0].tool_type, "function");
        assert_eq!(tool_calls[0].function.name, "get_weather");

        // Arguments = JSON sérialisé depuis `input`.
        let args: serde_json::Value =
            serde_json::from_str(&tool_calls[0].function.arguments).expect("arguments JSON valide");
        assert_eq!(args["location"], "Paris");
    }

    /// Message assistant avec texte + tool_use → contenu texte + tool_calls.
    #[test]
    fn translate_assistant_text_and_tool_use_mixed() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "assistant".to_string(),
                content: AnthropicContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Je vais chercher la météo.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_02".to_string(),
                        name: "get_weather".to_string(),
                        input: serde_json::json!({"location": "Lyon"}),
                    },
                ]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        let msg = &result.messages[0];
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(
            msg.content,
            MessageContent::Text("Je vais chercher la météo.".to_string())
        );
        let tool_calls = msg
            .tool_calls
            .as_ref()
            .expect("tool_calls doit être présent");
        assert_eq!(tool_calls[0].function.name, "get_weather");
    }

    // ─── Tests Slice B — message user avec tool_result ────────────────────────

    /// Message user avec bloc tool_result → role:tool avec tool_call_id.
    #[test]
    fn translate_user_tool_result_becomes_role_tool() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_01A".to_string(),
                    content: serde_json::json!("Température : 22°C, ensoleillé"),
                    is_error: None,
                }]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        assert_eq!(result.messages.len(), 1);

        let msg = &result.messages[0];
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(
            msg.tool_call_id.as_deref(),
            Some("toolu_01A"),
            "tool_call_id doit correspondre au tool_use_id"
        );
        assert_eq!(
            msg.content,
            MessageContent::Text("Température : 22°C, ensoleillé".to_string())
        );
    }

    /// tool_result avec contenu en tableau de blocs → texte concaténé.
    #[test]
    fn translate_tool_result_blocks_content_concatenated() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_03".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "Résultat A. "},
                        {"type": "text", "text": "Résultat B."}
                    ]),
                    is_error: None,
                }]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        let msg = &result.messages[0];
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(
            msg.content,
            MessageContent::Text("Résultat A. Résultat B.".to_string())
        );
    }

    // ─── Tests Slice B — image base64 ────────────────────────────────────────

    /// Bloc image base64 → ContentPart::ImageUrl avec data-URI.
    #[test]
    fn translate_image_base64_becomes_data_uri() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Décris cette image.".to_string(),
                    },
                    ContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".to_string(),
                            media_type: Some("image/png".to_string()),
                            data: Some("iVBORw0KGgo=".to_string()),
                            url: None,
                        },
                    },
                ]),
            }],
            system: None,
            max_tokens: 256,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");
        let msg = &result.messages[0];
        assert_eq!(msg.role, Role::User);

        let parts = match &msg.content {
            MessageContent::Parts(p) => p,
            other => panic!("attendu Parts, obtenu {:?}", other),
        };

        assert_eq!(parts.len(), 2);
        assert!(matches!(parts[0], ContentPart::Text { .. }));
        match &parts[1] {
            ContentPart::ImageUrl { image_url } => {
                assert_eq!(
                    image_url.url, "data:image/png;base64,iVBORw0KGgo=",
                    "data-URI doit être formé correctement"
                );
            }
            other => panic!("attendu ImageUrl, obtenu {:?}", other),
        }
    }

    // ─── Tests Slice B — réponse avec tool_calls ──────────────────────────────

    /// Réponse backend avec tool_calls → blocs ResponseBlock::ToolUse Anthropic.
    #[test]
    fn chat_to_anthropic_tool_calls_become_tool_use_blocks() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-tool".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(String::new()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc".to_string(),
                        tool_type: "function".to_string(),
                        function: FunctionCallResult {
                            name: "get_weather".to_string(),
                            arguments: r#"{"location":"Paris","unit":"celsius"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let result = chat_to_anthropic(&resp, "claude-3-5-sonnet-20241022")
            .expect("traduction doit réussir");

        // stop_reason doit être "tool_use"
        assert_eq!(result.stop_reason, Some("tool_use".to_string()));

        // Un seul bloc tool_use (pas de texte car content vide)
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ResponseBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Paris");
                assert_eq!(input["unit"], "celsius");
            }
            other => panic!("attendu ResponseBlock::ToolUse, obtenu {:?}", other),
        }
    }

    /// Réponse backend avec texte + tool_calls → blocs Text puis ToolUse dans cet ordre.
    #[test]
    fn chat_to_anthropic_text_and_tool_calls_mixed_order() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-mixed".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "qwen3".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("Je vais vérifier la météo.".to_string()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_xyz".to_string(),
                        tool_type: "function".to_string(),
                        function: FunctionCallResult {
                            name: "get_weather".to_string(),
                            arguments: r#"{"location":"Lyon"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir");

        // 2 blocs : Text d'abord, ToolUse ensuite.
        assert_eq!(result.content.len(), 2);
        assert!(
            matches!(&result.content[0], ResponseBlock::Text { text } if text == "Je vais vérifier la météo."),
            "premier bloc doit être Text"
        );
        assert!(
            matches!(&result.content[1], ResponseBlock::ToolUse { name, .. } if name == "get_weather"),
            "deuxième bloc doit être ToolUse"
        );
    }

    /// Arguments tool_call invalides (non-JSON) → input = objet vide, pas de panic.
    #[test]
    fn chat_to_anthropic_invalid_tool_arguments_fallback_to_empty_object() {
        let resp = ChatCompletionResponse {
            id: "id".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "m".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(String::new()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_bad".to_string(),
                        tool_type: "function".to_string(),
                        function: FunctionCallResult {
                            name: "broken_tool".to_string(),
                            arguments: "INVALID JSON {{{{".to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };

        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir — jamais panic");
        match &result.content[0] {
            ResponseBlock::ToolUse { input, .. } => {
                assert_eq!(
                    *input,
                    serde_json::Value::Object(serde_json::Map::new()),
                    "input doit être objet vide sur parse failure"
                );
            }
            other => panic!("attendu ToolUse, obtenu {:?}", other),
        }
    }

    /// finish_reason "tool_calls" → stop_reason "tool_use".
    #[test]
    fn chat_to_anthropic_finish_tool_calls_becomes_tool_use() {
        let resp = minimal_chat_response("", "tool_calls");

        let result = chat_to_anthropic(&resp, "m").expect("traduction doit réussir");
        assert_eq!(result.stop_reason, Some("tool_use".to_string()));
    }

    // ─── Tests C1 (reviewer P1) — tool_result parallèle ─────────────────────

    /// C1 — message user avec 2 blocs `tool_result` (tool-use parallèle) →
    /// 2 messages `role:tool` distincts avec les bons `tool_call_id`, dans l'ordre.
    ///
    /// Avant la correction C1, seul le premier `tool_result` était traité.
    #[test]
    fn user_message_with_multiple_tool_results_produces_n_tool_messages() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_01A".to_string(),
                        content: serde_json::json!("Résultat outil A"),
                        is_error: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_02B".to_string(),
                        content: serde_json::json!("Résultat outil B"),
                        is_error: None,
                    },
                ]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        // C1 : 2 blocs tool_result → 2 messages role:tool (pas 1).
        assert_eq!(
            result.messages.len(),
            2,
            "2 tool_result → 2 messages role:tool"
        );

        // Premier message : toolu_01A.
        assert_eq!(result.messages[0].role, Role::Tool);
        assert_eq!(
            result.messages[0].tool_call_id.as_deref(),
            Some("toolu_01A"),
            "premier message doit avoir tool_call_id=toolu_01A"
        );
        assert_eq!(
            result.messages[0].content,
            MessageContent::Text("Résultat outil A".to_string())
        );

        // Second message : toolu_02B.
        assert_eq!(result.messages[1].role, Role::Tool);
        assert_eq!(
            result.messages[1].tool_call_id.as_deref(),
            Some("toolu_02B"),
            "second message doit avoir tool_call_id=toolu_02B"
        );
        assert_eq!(
            result.messages[1].content,
            MessageContent::Text("Résultat outil B".to_string())
        );
    }

    /// C1 — message user avec 2 blocs `tool_result` + 1 bloc texte →
    /// aucun contenu perdu : 2 messages `role:tool` + 1 message `role:user`.
    #[test]
    fn user_message_tool_results_then_text_preserves_all() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_X1".to_string(),
                        content: serde_json::json!("Sortie X1"),
                        is_error: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_X2".to_string(),
                        content: serde_json::json!("Sortie X2"),
                        is_error: None,
                    },
                    ContentBlock::Text {
                        text: "Que penses-tu de ces résultats ?".to_string(),
                    },
                ]),
            }],
            system: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        // 2 tool_result + 1 texte résiduel → 3 messages au total.
        assert_eq!(
            result.messages.len(),
            3,
            "2 tool_result + 1 texte → 3 messages (rien perdu)"
        );

        // Deux premiers : role:tool.
        assert_eq!(result.messages[0].role, Role::Tool);
        assert_eq!(result.messages[0].tool_call_id.as_deref(), Some("toolu_X1"));
        assert_eq!(
            result.messages[0].content,
            MessageContent::Text("Sortie X1".to_string())
        );

        assert_eq!(result.messages[1].role, Role::Tool);
        assert_eq!(result.messages[1].tool_call_id.as_deref(), Some("toolu_X2"));
        assert_eq!(
            result.messages[1].content,
            MessageContent::Text("Sortie X2".to_string())
        );

        // Dernier : role:user avec le texte résiduel.
        assert_eq!(result.messages[2].role, Role::User);
        assert_eq!(
            result.messages[2].content,
            MessageContent::Text("Que penses-tu de ces résultats ?".to_string())
        );
        assert!(result.messages[2].tool_call_id.is_none());
    }

    // ─── Tests V5 (security-reviewer) — validation URL image ────────────────────

    fn req_with_image_url(url: &str) -> MessagesRequest {
        MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![ContentBlock::Image {
                    source: ImageSource {
                        source_type: "url".to_string(),
                        media_type: None,
                        data: None,
                        url: Some(url.to_string()),
                    },
                }]),
            }],
            system: None,
            max_tokens: 256,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        }
    }

    /// V5 (security-reviewer P2) — URL HTTP non-sécurisée → erreur de traduction.
    ///
    /// Un bloc image de type `url` avec `http://` (non-https) doit être rejeté
    /// pour éviter SSRF et fuites de données sur protocoles non chiffrés.
    #[test]
    fn translate_image_url_http_scheme_rejected() {
        let req = req_with_image_url("http://evil.example.com/image.png");
        let err = anthropic_to_chat(&req, "default")
            .expect_err("URL http non-https doit retourner une erreur");
        // Vérifie que c'est bien une erreur de validation URL image.
        assert!(
            matches!(err, TranslateError::InvalidImageUrl(_)),
            "attendu TranslateError::InvalidImageUrl, obtenu: {:?}",
            err
        );
    }

    /// V5 (security-reviewer P2) — URL HTTPS valide → acceptée.
    #[test]
    fn translate_image_url_https_scheme_accepted() {
        let req = req_with_image_url("https://cdn.example.com/image.png");
        let result =
            anthropic_to_chat(&req, "default").expect("URL https valide doit être acceptée");
        // Le message doit contenir une Part::ImageUrl avec l'URL correcte.
        let parts = match &result.messages[0].content {
            MessageContent::Parts(p) => p,
            other => panic!("attendu Parts, obtenu {:?}", other),
        };
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. }))
        );
    }

    /// V5 (security-reviewer P2) — image base64 → toujours acceptée (non-URL).
    #[test]
    fn translate_image_base64_still_accepted_after_v5() {
        let req = MessagesRequest {
            model: "m".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Blocks(vec![ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: Some("image/png".to_string()),
                        data: Some("abc123".to_string()),
                        url: None,
                    },
                }]),
            }],
            system: None,
            max_tokens: 256,
            temperature: None,
            top_p: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            betas: None,
            metadata: None,
        };
        let result = anthropic_to_chat(&req, "default").expect("base64 doit toujours être accepté");
        let parts = match &result.messages[0].content {
            MessageContent::Parts(p) => p,
            other => panic!("attendu Parts, obtenu {:?}", other),
        };
        assert!(parts.iter().any(|p| match p {
            ContentPart::ImageUrl { image_url } => image_url.url.starts_with("data:"),
            _ => false,
        }));
    }

    /// V5 (security-reviewer P2) — URL vide → erreur (pas de schéma).
    #[test]
    fn translate_image_url_empty_rejected() {
        let req = req_with_image_url("");
        let err =
            anthropic_to_chat(&req, "default").expect_err("URL vide doit retourner une erreur");
        assert!(matches!(err, TranslateError::InvalidImageUrl(_)));
    }

    // ─── Tests sanitize_schema (incident b9780 GBNF-bloat) ───────────────────

    /// Test 1 — sanitization complète avec nesting profond.
    ///
    /// Vérifie que TOUS les mots-clés GBNF-bloat sont supprimés à tous les niveaux
    /// (root, properties imbriquées, items, anyOf) et que les mots-clés structurants
    /// sont conservés.
    #[test]
    fn sanitize_schema_removes_gbnf_bloat_keywords_at_all_levels() {
        let schema = serde_json::json!({
            "type": "object",
            "description": "Outil de test avec contraintes",
            "required": ["name", "count"],
            "maximum": 9007199254740991_i64,
            "minLength": 1,
            "properties": {
                "name": {
                    "type": "string",
                    "maxLength": 524288,
                    "minLength": 1,
                    "pattern": "^[a-z]+$",
                    "description": "Nom de l'élément"
                },
                "nested": {
                    "type": "object",
                    "properties": {
                        "field": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 100,
                            "description": "Champ imbriqué"
                        }
                    }
                },
                "count": {
                    "type": "integer",
                    "minimum": 0,
                    "exclusiveMaximum": 1000,
                    "multipleOf": 1
                }
            },
            "items": {
                "type": "string",
                "maxLength": 100,
                "minItems": 1
            },
            "anyOf": [
                {
                    "type": "string",
                    "maxLength": 50
                },
                {
                    "type": "integer",
                    "minimum": 0
                }
            ]
        });

        let result = sanitize_schema(schema);

        // ── Mots-clés GBNF-bloat supprimés au niveau root ──
        assert!(result.get("maximum").is_none(), "maximum root supprimé");
        assert!(result.get("minLength").is_none(), "minLength root supprimé");

        // ── Mots-clés structurants conservés ──
        assert_eq!(result["type"], "object");
        assert_eq!(result["description"], "Outil de test avec contraintes");
        assert!(result.get("required").is_some(), "required conservé");
        assert!(result.get("properties").is_some(), "properties conservé");

        // ── Sanitization dans properties.name ──
        let name_prop = &result["properties"]["name"];
        assert!(
            name_prop.get("maxLength").is_none(),
            "maxLength dans name supprimé"
        );
        assert!(
            name_prop.get("minLength").is_none(),
            "minLength dans name supprimé"
        );
        assert!(
            name_prop.get("pattern").is_none(),
            "pattern dans name supprimé"
        );
        assert_eq!(name_prop["type"], "string", "type dans name conservé");
        assert_eq!(
            name_prop["description"], "Nom de l'élément",
            "description dans name conservé"
        );

        // ── Sanitization récursive dans properties.nested.properties.field ──
        let field = &result["properties"]["nested"]["properties"]["field"];
        assert!(
            field.get("minimum").is_none(),
            "minimum dans field supprimé"
        );
        assert!(
            field.get("maximum").is_none(),
            "maximum dans field supprimé"
        );
        assert_eq!(field["type"], "integer", "type dans field conservé");
        assert_eq!(
            field["description"], "Champ imbriqué",
            "description dans field conservé"
        );

        // ── Sanitization dans properties.count ──
        let count_prop = &result["properties"]["count"];
        assert!(
            count_prop.get("minimum").is_none(),
            "minimum dans count supprimé"
        );
        assert!(
            count_prop.get("exclusiveMaximum").is_none(),
            "exclusiveMaximum dans count supprimé"
        );
        assert!(
            count_prop.get("multipleOf").is_none(),
            "multipleOf dans count supprimé"
        );

        // ── Sanitization dans items ──
        let items = &result["items"];
        assert!(
            items.get("maxLength").is_none(),
            "maxLength dans items supprimé"
        );
        assert!(
            items.get("minItems").is_none(),
            "minItems dans items supprimé"
        );
        assert_eq!(items["type"], "string", "type dans items conservé");

        // ── Sanitization dans anyOf ──
        let any_of = result["anyOf"].as_array().expect("anyOf doit être tableau");
        assert!(
            any_of[0].get("maxLength").is_none(),
            "maxLength dans anyOf[0] supprimé"
        );
        assert!(
            any_of[1].get("minimum").is_none(),
            "minimum dans anyOf[1] supprimé"
        );
        assert_eq!(any_of[0]["type"], "string");
        assert_eq!(any_of[1]["type"], "integer");
    }

    /// Test 2 — tools toujours présents + tool_choice="auto" préservé après sanitization.
    ///
    /// Vérifie que la chaîne complète (tools présents, tool_choice absent → "auto" forcé)
    /// fonctionne après l'introduction de sanitize_schema.
    #[test]
    fn tools_present_and_tool_choice_auto_preserved_after_sanitization() {
        let mut req = minimal_request("utilise l'outil");
        req.tools = Some(vec![Tool {
            name: "search".to_string(),
            description: Some("Recherche dans le vault.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "maxLength": 524288
                    }
                },
                "required": ["query"]
            }),
        }]);
        // Pas de tool_choice explicite.
        assert!(req.tool_choice.is_none());

        let result = anthropic_to_chat(&req, "default").expect("traduction doit réussir");

        // tools doivent être présents après sanitization.
        let tools = result.tools.expect("tools doit être présent");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "search");

        // maxLength doit avoir été supprimé.
        assert!(
            tools[0].function.parameters["properties"]["query"]
                .get("maxLength")
                .is_none(),
            "maxLength doit être supprimé par sanitize_schema"
        );

        // tool_choice forcé à "auto".
        assert_eq!(
            result.tool_choice,
            Some(crate::commons::chat::ToolChoice::Mode("auto".to_string())),
            "tool_choice doit être 'auto' (fix b9780 conservé)"
        );
    }

    /// Test 3 — idempotence : schéma sans mots-clés GBNF-bloat reste inchangé.
    ///
    /// Un schéma ne contenant que des mots-clés structurants (type, properties, required,
    /// description, enum) doit sortir identique à l'entrée.
    #[test]
    fn sanitize_schema_is_noop_when_no_gbnf_bloat_keywords() {
        let schema = serde_json::json!({
            "type": "object",
            "description": "Schéma propre sans contraintes",
            "required": ["name", "tags"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Nom"
                },
                "status": {
                    "type": "string",
                    "enum": ["active", "inactive", "pending"],
                    "description": "Statut"
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    }
                }
            }
        });

        let result = sanitize_schema(schema.clone());

        // La sortie doit être structurellement identique à l'entrée.
        assert_eq!(result, schema, "schéma sans GBNF-bloat doit être inchangé");
    }

    /// Test 4 — stabilité déterministe de la sanitization (préservation de l'ordre).
    ///
    /// La sanitization doit être déterministe : deux appels consécutifs sur le même
    /// schéma doivent produire exactement le même résultat (même ordre des clés).
    /// Critique pour la stabilité du prompt-cache LCP (préfixe identique entre requêtes).
    ///
    /// Note : sans la feature `preserve_order` de serde_json, `serde_json::Map` utilise
    /// un `BTreeMap` (ordre lexicographique) plutôt qu'un `IndexMap` (ordre d'insertion).
    /// La stabilité est donc garantie par le déterminisme du BTreeMap — même entrée =
    /// même ordre de sortie, quelles que soient les clés supprimées.
    #[test]
    fn sanitize_schema_is_stable_deterministic() {
        let schema = serde_json::json!({
            "type": "object",
            "description": "Ordre des clés",
            "properties": {
                "alpha": {
                    "type": "string",
                    "maxLength": 100,
                    "description": "Première propriété"
                },
                "beta": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Deuxième propriété"
                },
                "gamma": {
                    "type": "boolean",
                    "description": "Troisième propriété"
                }
            },
            "required": ["alpha", "beta"],
            "maximum": 999
        });

        // Deux appels consécutifs → même résultat (déterminisme).
        let result1 = sanitize_schema(schema.clone());
        let result2 = sanitize_schema(schema);
        assert_eq!(result1, result2, "sanitize_schema doit être déterministe");

        // Le résultat ne doit plus contenir 'maximum' (supprimé).
        assert!(result1.get("maximum").is_none(), "maximum supprimé");

        // Les clés structurantes doivent toujours être présentes.
        assert!(result1.get("type").is_some(), "type conservé");
        assert!(result1.get("description").is_some(), "description conservé");
        assert!(result1.get("properties").is_some(), "properties conservé");
        assert!(result1.get("required").is_some(), "required conservé");

        // Les clés GBNF-bloat dans les propriétés doivent être supprimées.
        assert!(
            result1["properties"]["alpha"].get("maxLength").is_none(),
            "maxLength dans alpha supprimé"
        );
        assert!(
            result1["properties"]["beta"].get("minimum").is_none(),
            "minimum dans beta supprimé"
        );

        // Les clés non-bloat dans les propriétés doivent être conservées.
        assert_eq!(result1["properties"]["alpha"]["type"], "string");
        assert_eq!(
            result1["properties"]["alpha"]["description"],
            "Première propriété"
        );
        assert_eq!(result1["properties"]["gamma"]["type"], "boolean");

        // Vérifier la stabilité de la sérialisation JSON (même ordre de clés dans le JSON final).
        // C'est la propriété qui importe pour le prompt-cache : serde_json::to_string
        // produit le même bytes en sortie à chaque appel.
        let json1 = serde_json::to_string(&result1).expect("sérialisation doit réussir");
        let json2 = serde_json::to_string(&result2).expect("sérialisation doit réussir");
        assert_eq!(
            json1, json2,
            "sérialisation JSON stable (prompt-cache déterministe)"
        );
    }

    /// C1 (reviewer P1) — sanitization dans additionalProperties-objet et prefixItems.
    ///
    /// Ces deux cas n'étaient pas couverts dans la version initiale (trou de récursion).
    /// Un `maximum` ou `maxLength` imbriqué dans l'un ou l'autre doit être retiré.
    #[test]
    fn sanitize_schema_recurses_into_additional_properties_and_prefix_items() {
        let schema = serde_json::json!({
            "type": "object",
            "description": "Schéma avec additionalProperties et prefixItems",
            // additionalProperties en tant qu'objet-schéma (pas un booléen).
            "additionalProperties": {
                "type": "string",
                "maxLength": 1024,
                "description": "Valeur additionnelle"
            },
            // prefixItems : tableau de sous-schémas (draft 2020-12 / schemars 1.0).
            "prefixItems": [
                {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "Premier élément"
                },
                {
                    "type": "string",
                    "minLength": 1,
                    "description": "Deuxième élément"
                }
            ],
            // additionalProperties booléen : doit rester inchangé (not a schema).
            "required": ["x"]
        });

        let result = sanitize_schema(schema);

        // ── additionalProperties objet : maxLength supprimé, type+description conservés ──
        let add_props = &result["additionalProperties"];
        assert!(
            add_props.get("maxLength").is_none(),
            "maxLength dans additionalProperties supprimé"
        );
        assert_eq!(
            add_props["type"], "string",
            "type dans additionalProperties conservé"
        );
        assert_eq!(
            add_props["description"], "Valeur additionnelle",
            "description dans additionalProperties conservé"
        );

        // ── prefixItems : contraintes supprimées dans chaque sous-schéma ──
        let prefix = result["prefixItems"]
            .as_array()
            .expect("prefixItems doit rester un tableau");
        assert_eq!(prefix.len(), 2, "les deux éléments prefixItems conservés");

        // Premier élément : minimum et maximum supprimés, type+description conservés.
        assert!(
            prefix[0].get("minimum").is_none(),
            "minimum dans prefixItems[0] supprimé"
        );
        assert!(
            prefix[0].get("maximum").is_none(),
            "maximum dans prefixItems[0] supprimé"
        );
        assert_eq!(prefix[0]["type"], "integer");
        assert_eq!(prefix[0]["description"], "Premier élément");

        // Deuxième élément : minLength supprimé, type+description conservés.
        assert!(
            prefix[1].get("minLength").is_none(),
            "minLength dans prefixItems[1] supprimé"
        );
        assert_eq!(prefix[1]["type"], "string");
        assert_eq!(prefix[1]["description"], "Deuxième élément");

        // ── Les clés structurantes du root restent intactes ──
        assert!(result.get("required").is_some(), "required conservé");
        assert_eq!(
            result["description"],
            "Schéma avec additionalProperties et prefixItems"
        );
    }
}
