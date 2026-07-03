//! DTOs for the Anthropic Messages API — inbound request/response types.
//!
//! These types cover the `POST /v1/messages` API surface (Anthropic).
//!
//! # Supported features
//! - Plain text requests and responses
//! - Full tool use (tools[], tool_choice, tool_use blocks, tool_result, image)
//! - Anthropic SSE streaming
//! - Anthropic error envelope, configurable model mapping
//!
//! Reference: <https://docs.anthropic.com/en/api/messages>

use serde::{Deserialize, Serialize};

// ── Requête entrant ────────────────────────────────────────────────────────────

/// Requête `POST /v1/messages` au format Anthropic Messages API.
///
/// Champs inconnus du JSON sont ignorés (comportement serde par défaut).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesRequest {
    /// Identifiant modèle — sera résolu vers un alias interne.
    pub model: String,
    /// Messages de la conversation.
    pub messages: Vec<AnthropicMessage>,
    /// Système optionnel (texte brut ou blocs de contenu).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    /// Nombre maximal de tokens à générer (obligatoire côté Anthropic).
    pub max_tokens: u32,
    /// Température d'échantillonnage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Probabilité cumulative (nucleus sampling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Séquences d'arrêt supplémentaires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// `true` = Anthropic SSE streaming mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Tool definitions exposed to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Tool selection strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Blocs `thinking` extended (hors scope MVP — ignorés silencieusement).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Contrôle de cache prompt (ignoré silencieusement au MVP).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub betas: Option<Vec<String>>,
    /// Métadonnées utilisateur arbitraires (ignorées).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Définition d'un outil exposé au modèle.
///
/// Correspond à `tools[i]` dans la requête Anthropic.
/// Mappé vers `ToolDefinition` OpenAI lors de la traduction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    /// Nom de l'outil — identifiant unique dans la liste.
    pub name: String,
    /// Description de l'outil (optionnelle mais recommandée).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Schéma JSON des paramètres de l'outil.
    ///
    /// Anthropic appelle ce champ `input_schema` ; il est mappé vers `parameters`
    /// dans la `FunctionDefinition` OpenAI.
    pub input_schema: serde_json::Value,
}

/// Stratégie de sélection d'outil.
///
/// Format Anthropic :
/// - `{"type": "auto"}` — le modèle choisit
/// - `{"type": "any"}` — le modèle doit appeler au moins un outil
/// - `{"type": "tool", "name": "X"}` — force l'outil nommé
/// - `{"type": "none"}` — aucun outil (rare, non officiel)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Le modèle choisit librement d'utiliser un outil ou non.
    Auto,
    /// Le modèle doit appeler au moins un outil (equiv. OpenAI "required").
    Any,
    /// Force l'appel à l'outil nommé.
    Tool {
        /// Nom de l'outil à forcer.
        name: String,
    },
    /// Désactive les outils (le modèle ne doit pas en appeler).
    None,
}

/// Message dans la conversation Anthropic.
///
/// `role` est `"user"` ou `"assistant"`.
/// `content` accepte une chaîne de texte directe OU un tableau de blocs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    /// Rôle de l'auteur : `"user"` ou `"assistant"`.
    pub role: String,
    /// Contenu du message — texte brut ou liste de blocs.
    pub content: AnthropicContent,
}

/// Contenu d'un message Anthropic — texte brut ou tableau de blocs.
///
/// `#[serde(untagged)]` permet la désérialisation transparente des deux formes :
/// - `"texte"` → `Text(String)`
/// - `[{...}]` → `Blocks(Vec<ContentBlock>)`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicContent {
    /// Texte brut (forme courte Anthropic).
    Text(String),
    /// Tableau de blocs de contenu (forme étendue).
    Blocks(Vec<ContentBlock>),
}

impl AnthropicContent {
    /// Extrait le texte concaténé de tous les blocs texte.
    ///
    /// Pour un `Text` simple, retourne directement la chaîne.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
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
}

/// Content block in an Anthropic message.
///
/// Supported variants:
/// - `Text`
/// - `ToolUse`, `ToolResult`, `Image`
/// - `Thinking` (extended thinking — silently ignored)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Bloc de texte.
    Text {
        /// Contenu textuel du bloc.
        text: String,
    },
    /// Tool-call block generated by the assistant.
    ///
    /// Mapped to `tool_calls[i]` in the OpenAI `Message` with role `assistant`.
    ToolUse {
        /// Identifiant unique de cet appel d'outil dans le tour courant.
        id: String,
        /// Nom de l'outil appelé.
        name: String,
        /// Arguments fournis à l'outil (objet JSON).
        input: serde_json::Value,
    },
    /// Tool result provided by the user.
    ///
    /// Mapped to an OpenAI `Message` with role `tool` and a `tool_call_id`.
    ToolResult {
        /// Identifiant de l'appel d'outil auquel ce résultat répond.
        tool_use_id: String,
        /// Contenu du résultat — texte brut ou tableau de blocs.
        ///
        /// Anthropic accepte `String` ou `Vec<ContentBlock>` (blocs Text).
        /// On stocke en `serde_json::Value` pour absorber les deux formes ;
        /// la traduction extrait le texte.
        content: serde_json::Value,
        /// Indique si l'exécution a échoué (pour les tool results en erreur).
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Image block (base64 or URL).
    ///
    /// Mapped to an OpenAI `ContentPart::ImageUrl` with a data-URI `data:<media_type>;base64,<data>`.
    Image {
        /// Source de l'image.
        source: ImageSource,
    },
    /// Bloc thinking (extended thinking Anthropic) — hors scope MVP, ignoré.
    Thinking {
        /// Contenu du thinking (ignoré).
        thinking: String,
    },
    /// Bloc de type inconnu — ignoré silencieusement lors de la traduction.
    ///
    /// Absorbe les variantes futures de l'API Anthropic (ex: `"document"`,
    /// `"redacted_thinking"`) pour éviter qu'un type non reconnu ne provoque
    /// une erreur de désérialisation 400 sur l'ensemble de la requête.
    ///
    /// Note : `#[serde(other)]` sur une variante unit fonctionne avec les enums
    /// `internally_tagged` — les champs de l'objet inconnu sont ignorés (dropped).
    #[serde(other)]
    Unknown,
}

/// Source d'une image dans un bloc `ContentBlock::Image`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    /// Type de source : `"base64"` ou `"url"`.
    #[serde(rename = "type")]
    pub source_type: String,
    /// Type MIME de l'image (ex: `"image/jpeg"`, `"image/png"`).
    ///
    /// Présent pour `type = "base64"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Données base64 de l'image (pour `type = "base64"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// URL de l'image (pour `type = "url"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ── DTO count_tokens ──────────────────────────────────────────────────────────

/// Requête `POST /v1/messages/count_tokens` au format Anthropic Messages API.
///
/// Structurellement identique à `MessagesRequest` SAUF que `max_tokens` est **absent** :
/// l'API Anthropic count_tokens ne le requiert pas (contrairement à /v1/messages).
///
/// Utiliser ce DTO dédié évite de rendre `max_tokens` optionnel dans `MessagesRequest`
/// (qui doit rester strict pour la route principale).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountTokensRequest {
    /// Identifiant modèle — ignoré pour le comptage (pas de dispatch).
    pub model: String,
    /// Messages de la conversation.
    pub messages: Vec<AnthropicMessage>,
    /// Système optionnel (texte brut ou blocs de contenu).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    /// Définitions d'outils exposés au modèle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Blocs `thinking` extended (ignorés).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
}

/// Contenu d'un message système — texte brut ou blocs.
///
/// Identique à `AnthropicContent` mais sémantiquement distinct
/// (le système n'accepte que des blocs `Text` en pratique).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SystemContent {
    /// Texte brut.
    Text(String),
    /// Tableau de blocs (généralement `Text` uniquement pour le system).
    Blocks(Vec<ContentBlock>),
}

impl SystemContent {
    /// Extrait le texte concaténé des blocs texte.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
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
}

// ── Réponse sortant ────────────────────────────────────────────────────────────

/// Réponse `POST /v1/messages` au format Anthropic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessagesResponse {
    /// Identifiant unique du message — commence par `msg_`.
    pub id: String,
    /// Type de l'objet — toujours `"message"`.
    #[serde(rename = "type")]
    pub object_type: String,
    /// Rôle de l'auteur de la réponse — toujours `"assistant"`.
    pub role: String,
    /// Modèle utilisé (tel que fourni dans la requête).
    pub model: String,
    /// Blocs de contenu de la réponse.
    pub content: Vec<ResponseBlock>,
    /// Raison d'arrêt de la génération.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Séquence d'arrêt qui a déclenché la fin (si `stop_reason = "stop_sequence"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Usage tokens (entrée + sortie).
    pub usage: AnthropicUsage,
}

/// Content block in an Anthropic response.
///
/// Supported variants: `Text`, `ToolUse`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseBlock {
    /// Bloc de texte généré.
    Text {
        /// Texte généré par le modèle.
        text: String,
    },
    /// Tool-call block generated by the model.
    ToolUse {
        /// Identifiant unique de l'appel dans ce message.
        id: String,
        /// Nom de l'outil appelé.
        name: String,
        /// Arguments de l'appel (objet JSON parsé depuis la chaîne OpenAI).
        input: serde_json::Value,
    },
}

/// Usage tokens dans la réponse Anthropic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicUsage {
    /// Tokens d'entrée (prompt).
    pub input_tokens: u32,
    /// Tokens de sortie (completion).
    pub output_tokens: u32,
}
