//! Types d'erreur et décision du warden.

/// Erreur de construction du warden.
#[derive(Debug, thiserror::Error)]
pub enum WardenError {
    /// Configuration de rate limit invalide : `per_minute` ou `burst` est nul.
    #[error("rate limit invalide: per_minute={0} burst={1} (les deux doivent être > 0)")]
    InvalidRateLimit(u32, u32),
}

/// Décision du warden pour une requête entrante.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WardenDecision {
    /// Requête autorisée — passe au handler suivant.
    Allow,
    /// Bypass loopback — transmise directement au handler sans rate limit ni filtres IP.
    Bypass,
    /// IP refusée par le filtre CIDR → 403 Forbidden.
    DenyIp,
    /// Quota dépassé → 429 Too Many Requests.
    DenyRateLimit,
}
