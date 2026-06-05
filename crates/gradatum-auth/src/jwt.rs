//! JWT Ed25519 — caveats C1, R-A1 (design spec P2.0a).
//!
//! Propriétés garanties :
//! - Algorithme : EdDSA (Ed25519) uniquement — pas HS256/RS256.
//! - `kid` obligatoire dans l'header : le verify rejette si absent ou différent.
//! - Audience stricte (`aud` exact match) + `exp` validé, leeway = 0.
//! - Scope-based TTL (R-A1) : `TokenScope::Human` → `ttl_human_secs` (défaut 3600s),
//!   `TokenScope::Service` → `ttl_service_secs` (défaut 86400s).
//! - `jti` ULID unique par token (utile pour révocation via `RevocationStore`).
//!
//! ## API publique
//!
//! ```rust,no_run
//! use ed25519_dalek::SigningKey;
//! use gradatum_auth::jwt::{JwtService, TokenScope};
//!
//! let mut rng = rand::rngs::OsRng;
//! let signing = SigningKey::generate(&mut rng);
//! let svc = JwtService::new(signing, "kid-2026".into(), "gradatum".into(), 3600, 86400);
//! let token = svc.sign("user-1", &["read".into()], TokenScope::Human, "main").unwrap();
//! let claims = svc.verify(&token).unwrap();
//! assert_eq!(claims.sub, "user-1");
//! assert_eq!(claims.tenant_id, "main");
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{SigningKey, VerifyingKey};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use pkcs8::EncodePrivateKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Erreurs JWT.
#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    /// L'audience du token ne correspond pas à celle attendue par ce service.
    #[error("audience invalide")]
    InvalidAudience,

    /// Le `kid` de l'header ne correspond pas au `kid` configuré dans ce `JwtService`.
    #[error("kid invalide ou absent")]
    InvalidKid,

    /// Le token est expiré (`exp < now`, leeway = 0).
    #[error("token expiré")]
    Expired,

    /// Token malformé, signature invalide, ou algorithme incorrect.
    #[error("token malformé : {0}")]
    Malformed(#[from] jsonwebtoken::errors::Error),

    /// Erreur de lecture de l'horloge système.
    #[error("erreur horloge système : {0}")]
    Time(#[from] std::time::SystemTimeError),
}

/// Scope utilisé pour choisir le TTL du token (R-A1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    /// Utilisateur humain ou Studio — TTL court (défaut 3600s = 1h).
    Human,
    /// Service machine (mcp-stub static) — TTL long (défaut 86400s = 24h).
    Service,
}

/// Claims JWT inclus dans le payload.
///
/// Champs standards JWT : `sub`, `aud`, `iat`, `exp`, `jti`.
/// Champs custom : `scopes` (liste de permissions) + `tenant_id` (D10 multi-tenancy invariant).
///
/// # D3-complet (AUTH-T7, spec V2 2026-05-06)
///
/// `tenant_id` est obligatoire dans tous les tokens émis par ce service.
/// Valeur `"main"` pour le tenant racine (défaut alpha.5).
/// Multi-tenancy granulaire différé à Phase 2.1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// Subject — identifiant de l'entité authentifiée.
    pub sub: String,
    /// Audience — nom du service cible (doit correspondre exactement au `audience` du JwtService).
    pub aud: String,
    /// Issued-at (UNIX timestamp secondes).
    pub iat: u64,
    /// Expiry (UNIX timestamp secondes). `exp - iat` = TTL selon le scope.
    pub exp: u64,
    /// JWT ID unique (ULID) — utilisé pour la révocation via `RevocationStore`.
    pub jti: String,
    /// Permissions accordées à ce token.
    pub scopes: Vec<String>,
    /// Tenant cible — D10 multi-tenancy invariant (D3-complet, spec V2 2026-05-06).
    /// Valeur `"main"` pour le tenant racine (défaut alpha.5).
    pub tenant_id: String,
}

/// Service JWT Ed25519 : signe et vérifie des tokens pour une audience et un kid donnés.
///
/// Instancier une fois au démarrage, partager derrière un `Arc<JwtService>` dans AppState.
pub struct JwtService {
    signing: SigningKey,
    verifying: VerifyingKey,
    kid: String,
    audience: String,
    ttl_human_secs: u64,
    ttl_service_secs: u64,
}

impl JwtService {
    /// Crée un nouveau `JwtService`.
    ///
    /// # Arguments
    /// - `signing` : clé privée Ed25519 (générée par `ed25519_dalek::SigningKey::generate`)
    /// - `kid` : identifiant de la clé (obligatoire dans le header JWT)
    /// - `audience` : audience exacte des tokens émis (ex. `"gradatum"`)
    /// - `ttl_human_secs` : TTL tokens Human (R-A1, défaut recommandé : 3600)
    /// - `ttl_service_secs` : TTL tokens Service (R-A1, défaut recommandé : 86400)
    pub fn new(
        signing: SigningKey,
        kid: String,
        audience: String,
        ttl_human_secs: u64,
        ttl_service_secs: u64,
    ) -> Self {
        let verifying = signing.verifying_key();
        Self {
            signing,
            verifying,
            kid,
            audience,
            ttl_human_secs,
            ttl_service_secs,
        }
    }

    /// Construit un `JwtService` depuis les bytes bruts de la seed Ed25519 (32 bytes).
    ///
    /// Utilisé au boot de production pour charger une clé persistée par [`FileSecretsProvider`]
    /// (ou tout autre [`SecretsProvider`]). Les bytes doivent être la seed raw d'une `SigningKey`
    /// Ed25519 (format ed25519-dalek : 32 bytes, non PEM, non PKCS8).
    ///
    /// # Erreurs
    ///
    /// Retourne `Err(&'static str)` si `bytes.len() != 32`.
    ///
    /// # Sécurité
    ///
    /// Ne pas logguer les bytes passés à cette fonction. L'appelant doit les obtenir
    /// via [`secrecy::ExposeSecret::expose_secret`] et les transmettre directement.
    ///
    /// [`FileSecretsProvider`]: gradatum_core::secrets::FileSecretsProvider
    /// [`SecretsProvider`]: gradatum_core::secrets::SecretsProvider
    pub fn from_signing_bytes(
        bytes: &[u8],
        kid: String,
        audience: String,
        ttl_human_secs: u64,
        ttl_service_secs: u64,
    ) -> Result<Self, &'static str> {
        if bytes.len() != 32 {
            return Err("clé Ed25519 doit être 32 bytes (seed raw)");
        }
        // SAFETY : slice de 32 bytes garanti par le check ci-dessus.
        let seed: [u8; 32] = bytes
            .try_into()
            .expect("slice de 32 bytes → array[32] ne peut pas échouer");
        let signing = SigningKey::from_bytes(&seed);
        Ok(Self::new(
            signing,
            kid,
            audience,
            ttl_human_secs,
            ttl_service_secs,
        ))
    }

    /// Génère une nouvelle clé Ed25519 via `OsRng` et retourne la seed brute zeroize-on-drop.
    ///
    /// Utilisé au premier boot si aucune clé persistée n'existe.
    /// Les bytes retournés doivent être persistés atomiquement (chmod 600) via
    /// [`FileSecretsProvider::write_atomic`].
    ///
    /// La seed est encapsulée dans [`Zeroizing`] : elle est écrasée en mémoire
    /// dès que le `Zeroizing<[u8; 32]>` est dropped — aucune fenêtre de fuite.
    ///
    /// [`FileSecretsProvider::write_atomic`]: gradatum_core::secrets::FileSecretsProvider::write_atomic
    pub fn generate_signing_bytes() -> (Zeroizing<[u8; 32]>, SigningKey) {
        let mut rng = rand::rngs::OsRng;
        let signing = SigningKey::generate(&mut rng);
        // to_bytes() retourne la seed raw 32 bytes ; on la wrape dans Zeroizing
        // pour garantir l'effacement mémoire dès que l'appelant n'en a plus besoin.
        let seed = Zeroizing::new(signing.to_bytes());
        (seed, signing)
    }

    /// Crée un `JwtService` avec une clé Ed25519 éphémère (dev/test uniquement).
    ///
    /// La clé est générée via `OsRng` à chaque appel — elle n'est pas persistée.
    /// Tous les tokens émis par cette instance sont invalides après redémarrage.
    ///
    /// **WARN :** ne jamais utiliser en production — utiliser [`JwtService::new`]
    /// avec une clé chargée depuis `cfg.auth.jwt_private_key_path`.
    pub fn new_ephemeral() -> Self {
        let mut rng = rand::rngs::OsRng;
        let signing = SigningKey::generate(&mut rng);
        tracing::warn!(
            "JwtService initialisé avec une clé éphémère — \
            UNIQUEMENT acceptable en dev/test. \
            Configurer jwt_private_key_path en production."
        );
        Self::new(
            signing,
            "ephemeral-dev".to_string(),
            "gradatum".to_string(),
            3600,
            86400,
        )
    }

    /// Signe un token JWT Ed25519.
    ///
    /// Le TTL est déterminé par `scope` (R-A1) :
    /// - `TokenScope::Human` → `ttl_human_secs`
    /// - `TokenScope::Service` → `ttl_service_secs`
    ///
    /// Le header `kid` est toujours positionné.
    /// Le `jti` est un ULID unique généré à chaque appel.
    ///
    /// # Arguments
    /// - `sub` : subject (owner/agent ID)
    /// - `scopes` : permissions accordées
    /// - `scope` : scope de TTL (Human = 1h, Service = 24h)
    /// - `tenant_id` : tenant cible (D3-complet, AUTH-T7) — `"main"` pour le tenant racine
    ///
    /// # Erreurs
    /// - `JwtError::Time` si l'horloge système est avant UNIX_EPOCH (impossible en prod)
    /// - `JwtError::Malformed` si l'encodage jsonwebtoken échoue (ne devrait pas arriver)
    pub fn sign(
        &self,
        sub: &str,
        scopes: &[String],
        scope: TokenScope,
        tenant_id: &str,
    ) -> Result<String, JwtError> {
        let iat = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let ttl = match scope {
            TokenScope::Human => self.ttl_human_secs,
            TokenScope::Service => self.ttl_service_secs,
        };

        let claims = Claims {
            sub: sub.to_string(),
            aud: self.audience.clone(),
            iat,
            exp: iat + ttl,
            jti: ulid::Ulid::new().to_string(),
            scopes: scopes.to_vec(),
            tenant_id: tenant_id.to_string(),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());

        // to_pkcs8_der() sur une SigningKey Ed25519 valide ne peut pas échouer :
        // c'est une sérialisation déterministe de la seed privée en PKCS8 v1 DER.
        // ring::Ed25519KeyPair::from_pkcs8_maybe_unchecked attend exactement ce format (48 bytes).
        let priv_der = self
            .signing
            .to_pkcs8_der()
            .expect("ed25519 SigningKey→PKCS8 DER ne peut pas échouer sur une clé valide");
        let encoding_key = EncodingKey::from_ed_der(priv_der.as_bytes());

        Ok(encode(&header, &claims, &encoding_key)?)
    }

    /// Retourne le `kid` configuré dans ce service.
    ///
    /// Utilisé par le middleware serveur pour construire [`TrustContext::BearerToken`].
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Retourne le TTL (secondes) pour les tokens `TokenScope::Service`.
    ///
    /// Utilisé par `/auth/exchange` pour inclure `ttl_secs` dans la réponse (AUTH-T5, spec §2.4 E2 fix).
    pub fn ttl_service_secs(&self) -> u64 {
        self.ttl_service_secs
    }

    /// Vérifie et décode un token JWT.
    ///
    /// # Vérifications effectuées
    /// 1. `kid` header == `self.kid` (sinon `JwtError::InvalidKid`)
    /// 2. Algorithme == EdDSA
    /// 3. Signature Ed25519 valide
    /// 4. `aud` == `self.audience` (sinon `JwtError::InvalidAudience`)
    /// 5. `exp` non dépassé, leeway = 0 (sinon `JwtError::Expired`)
    ///
    /// # Erreurs
    /// - `JwtError::InvalidKid` si le `kid` de l'header est absent ou différent
    /// - `JwtError::InvalidAudience` si l'audience ne correspond pas
    /// - `JwtError::Expired` si le token est expiré
    /// - `JwtError::Malformed` pour toute autre erreur de validation
    pub fn verify(&self, token: &str) -> Result<Claims, JwtError> {
        // Vérification du kid AVANT décodage (fast-fail, pas de crypto inutile).
        let header = jsonwebtoken::decode_header(token)?;
        match header.kid.as_deref() {
            Some(k) if k == self.kid => {}
            _ => return Err(JwtError::InvalidKid),
        }

        // DecodingKey::from_ed_der attend les 32 bytes bruts de la clé publique Ed25519
        // (format ring::UnparsedPublicKey), PAS le SPKI DER (44 bytes).
        // VerifyingKey::as_bytes() retourne exactement les 32 bytes de la clé publique.
        let decoding_key = DecodingKey::from_ed_der(self.verifying.as_bytes());

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(std::slice::from_ref(&self.audience));
        validation.validate_exp = true;
        // Council 1bis Archi v81 security review P1 2026-05-26 — jsonwebtoken v9 défaut validate_nbf=false
        // accepterait silencieusement un token avec nbf futur (not-before non respecté).
        // Activation explicite : rejette tout token dont le claim `nbf` est dans le futur.
        validation.validate_nbf = true;
        validation.leeway = 0;

        let data = decode::<Claims>(token, &decoding_key, &validation).map_err(|e| {
            use jsonwebtoken::errors::ErrorKind;
            match e.kind() {
                ErrorKind::ExpiredSignature => JwtError::Expired,
                ErrorKind::InvalidAudience => JwtError::InvalidAudience,
                _ => JwtError::Malformed(e),
            }
        })?;

        Ok(data.claims)
    }
}
