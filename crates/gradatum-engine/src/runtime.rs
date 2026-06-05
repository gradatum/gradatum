//! `ForwardProxy` — reverse-proxy transparent vers `llama-server` enfant (vague-2).
//!
//! ## Architecture vague-2
//!
//! `ForwardProxy` forwarde le corps de requête **brut** tel quel et retourne la réponse
//! `llama-server` **non modifiée** (statut + headers + stream). Cela préserve
//! automatiquement `slot_id`, le sampling, `tools`, la vision (images dans `messages`),
//! `seed`, `response_format`, et le streaming SSE.
//!
//! ## Non-goals (vague-2)
//!
//! - Pas de strip `<think>` (R-1) : le consommateur curator dispose d'un fallback regex,
//!   et le modèle curator (variante non-thinking) n'émet pas de `<think>` par défaut.
//! - Pas de normalisation L2 explicite (R-2) : `llama-server --embedding` normalise
//!   déjà côté serveur.
//!
//! ## Connection refused pendant warm-up (SP-P1)
//!
//! Si l'enfant n'est pas encore prêt, reqwest retourne une erreur de connexion.
//! Les handlers renvoient `EngineError::Inference` (→ 500) plutôt que de paniquer.
//! Le `HealthState::starting` indique au gateway que le service est en démarrage.

use axum::body::Bytes;

use crate::error::EngineError;

/// Reverse-proxy **transparent** vers `llama-server` enfant (vague-2).
///
/// Contrairement à `ProxyBackend` (payload reconstruite), `ForwardProxy` forwarde
/// le corps de requête **brut** tel quel et retourne la réponse `llama-server`
/// **non modifiée** (statut + headers + stream). Cela préserve automatiquement
/// `slot_id`, le sampling (`temperature`/`top_k`/`top_p`/…), `tools`, la vision
/// (images dans `messages`), `seed`, `response_format`, et le streaming SSE.
///
/// `Clone` : reqwest `Client` est un Arc interne — le clone est léger.
#[derive(Clone)]
pub struct ForwardProxy {
    /// Client HTTP partagé (pool de connexions reqwest).
    client: reqwest::Client,
    /// URL de base de l'enfant : `http://127.0.0.1:{child_port}`.
    child_base_url: String,
}

impl ForwardProxy {
    /// Construit un `ForwardProxy`.
    ///
    /// `child_base_url` : ex. `"http://127.0.0.1:11436"` (sans slash final).
    pub fn new(client: reqwest::Client, child_base_url: String) -> Self {
        Self {
            client,
            child_base_url,
        }
    }

    /// Forward le body brut vers `child_base_url + subpath` et retourne la réponse
    /// reqwest brute (statut + headers + corps non consommé).
    ///
    /// Le corps de réponse n'est PAS lu ici — le handler le streame via
    /// `Body::from_stream(resp.bytes_stream())` (SSE pass-through pour `stream:true`).
    ///
    /// # Errors
    /// `EngineError::Inference` si l'enfant est injoignable (warm-up / crash).
    pub(crate) async fn forward(
        &self,
        subpath: &str,
        content_type: &str,
        body: Bytes,
    ) -> Result<reqwest::Response, EngineError> {
        let url = format!("{}{subpath}", self.child_base_url);
        self.client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| EngineError::Inference(format!("proxy {subpath} : {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use std::sync::Arc;
    use tokio::{net::TcpListener, sync::Mutex};

    /// Démarre un stub qui CAPTURE le body brut reçu et renvoie une réponse fixe + content-type.
    async fn start_capture_stub(
        path: &'static str,
        status: u16,
        resp_content_type: &'static str,
        resp_body: &'static str,
    ) -> (u16, Arc<Mutex<Vec<u8>>>) {
        use axum::body::Bytes as AxBytes;
        use axum::http::StatusCode as AxStatus;
        use axum::response::Response as AxResponse;
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let cap2 = captured.clone();
        let app = Router::new().route(
            path,
            post(move |body: AxBytes| {
                let cap = cap2.clone();
                async move {
                    *cap.lock().await = body.to_vec();
                    AxResponse::builder()
                        .status(AxStatus::from_u16(status).unwrap())
                        .header("content-type", resp_content_type)
                        .body(axum::body::Body::from(resp_body))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, captured)
    }

    fn make_forward(port: u16) -> ForwardProxy {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        ForwardProxy::new(client, format!("http://127.0.0.1:{port}"))
    }

    #[tokio::test]
    async fn forward_preserves_body_byte_for_byte() {
        let (port, captured) = start_capture_stub(
            "/v1/chat/completions",
            200,
            "application/json",
            "{\"ok\":true}",
        )
        .await;
        let fwd = make_forward(port);
        // Body avec slot_id, tools, sampling, seed — DOIT arriver intact côté child.
        let raw = br#"{"messages":[{"role":"user","content":"hi"}],"slot_id":3,"temperature":0.7,"tools":[{"type":"function"}],"seed":42,"stream":false}"#;
        let resp = fwd
            .forward(
                "/v1/chat/completions",
                "application/json",
                axum::body::Bytes::from(raw.to_vec()),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let got = captured.lock().await.clone();
        assert_eq!(
            got.as_slice(),
            raw.as_slice(),
            "le body forwardé doit être identique byte-for-byte (slot_id/tools/sampling/seed préservés)"
        );
    }

    #[tokio::test]
    async fn forward_passes_status_and_content_type() {
        let (port, _) = start_capture_stub(
            "/v1/chat/completions",
            503,
            "text/event-stream",
            "data: x\n\n",
        )
        .await;
        let fwd = make_forward(port);
        let resp = fwd
            .forward(
                "/v1/chat/completions",
                "application/json",
                axum::body::Bytes::from_static(b"{}"),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503, "statut upstream propagé");
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "content-type upstream propagé : {ct}"
        );
    }

    #[tokio::test]
    async fn forward_connection_refused_returns_inference_error() {
        let fwd = make_forward(1); // port 1 → connexion refusée
        let result = fwd
            .forward(
                "/v1/chat/completions",
                "application/json",
                axum::body::Bytes::from_static(b"{}"),
            )
            .await;
        assert!(
            matches!(result, Err(EngineError::Inference(_))),
            "connexion refusée → Inference (pas de panic)"
        );
    }
}
