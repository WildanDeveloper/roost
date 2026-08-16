mod store;

use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub use store::TokenStore;

/// Claims of the JWTs the panel signs with the node's daemon token (HS256).
/// Different purposes carry different claims:
/// - websocket: scope "websocket", server_uuid, user_uuid, permissions[]
/// - file download/upload, backup download: scope + server_uuid + unique_id
/// - transfer: scope "transfer", sub = server uuid
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Option<String>,
    pub scope: Option<String>,
    pub permissions: Option<Vec<String>>,
    pub server_uuid: Option<String>,
    pub user_uuid: Option<String>,
    pub unique_id: Option<String>,
    pub jti: Option<String>,
    pub backup_uuid: Option<String>,
    pub iss: Option<String>,
    pub iat: Option<i64>,
    pub nbf: Option<i64>,
    pub exp: i64,
}

impl Claims {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.as_deref() == Some(scope)
    }

    pub fn has_permission(&self, perm: &str) -> bool {
        self.permissions.as_deref().map_or(false, |perms| {
            perms.iter().any(|p| {
                p == perm || (!perm.starts_with("admin") && p == "*")
            })
        })
    }

    /// The server this JWT is bound to (ws console/download/upload).
    pub fn server_uuid(&self) -> Option<&str> {
        self.server_uuid.as_deref()
    }
}

#[allow(dead_code)]
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Relevant part of the jsonwebtoken error so we can map it to a message
/// the panel understands.
fn jwt_error_message(e: &jsonwebtoken::errors::Error) -> String {
    match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => "jwt expired".to_string(),
        jsonwebtoken::errors::ErrorKind::InvalidSignature => "jwt signature invalid".to_string(),
        jsonwebtoken::errors::ErrorKind::InvalidToken => "jwt malformed".to_string(),
        jsonwebtoken::errors::ErrorKind::InvalidIssuer => "jwt invalid issuer".to_string(),
        jsonwebtoken::errors::ErrorKind::InvalidAudience => "jwt invalid audience".to_string(),
        jsonwebtoken::errors::ErrorKind::ImmatureSignature => "jwt not valid yet".to_string(),
        _ => "jwt invalid".to_string(),
    }
}

/// Validate a JWT signed by the panel. `tokens` is used to reject
/// revoked (logout) tokens; `boot_time` allows tokens issued before the
/// daemon booted (panel returns them after daemon reboots, wings treats
/// those as invalid).
pub async fn parse_token(
    token: &str,
    secret: &[u8],
    tokens: &TokenStore,
    boot_time: i64,
) -> AppResult<Claims> {
    let key = DecodingKey::from_secret(secret);

    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_exp = true;
    validation.validate_aud = false;
    validation.required_spec_claims = std::collections::HashSet::new();

    let data = decode::<Claims>(token, &key, &validation).map_err(|e| {
        let msg = jwt_error_message(&e);
        tracing::debug!(error = %e, "jwt decode failed");
        AppError::Unauthorized(msg)
    })?;

    let claims = data.claims;

    // Revocation checks mirror wings isDenylisted: only tokens that carry
    // both server_uuid and user_uuid (websocket/file/backup) are checked;
    // transfer tokens (sub + scope only) are not.
    let iat = claims.iat;
    let su = claims.server_uuid();
    let uu = claims.user_uuid.as_deref();
    if let (Some(iat), Some(_su), Some(uu)) = (iat, su, uu) {
        if iat < boot_time {
            return Err(AppError::Unauthorized("jwt created too far in past (denylist)".to_string()));
        }
        if let Some(jti) = &claims.jti {
            if tokens.is_jti_denied(jti, iat) {
                return Err(AppError::Unauthorized("jwt token was revoked".to_string()));
            }
        }
        if tokens.is_user_denied(uu, iat) {
            return Err(AppError::Unauthorized("jwt token was revoked".to_string()));
        }
    }

    // Single-use tokens (download/upload/backup) carry a unique_id; the
    // first use claims it, everything after is rejected. Websocket tokens
    // also carry a unique_id but are re-validated on every message, so they
    // must NOT be single-use (mirrors wings).
    if claims.scope.as_deref() != Some("websocket") {
        if let Some(id) = &claims.unique_id {
            if !tokens.claim(id).await {
                return Err(AppError::Unauthorized("jwt token was revoked".to_string()));
            }
        }
    }

    Ok(claims)
}

/// Sign a token (used for transfer/legacy endpoints that need short-lived
/// server-scoped tokens).
#[allow(dead_code)]
pub fn sign_token(claims: impl Serialize, secret: &[u8]) -> AppResult<String> {
    let key = jsonwebtoken::EncodingKey::from_secret(secret);
    jsonwebtoken::encode(&jsonwebtoken::Header::default(), &claims, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("jwt sign failed: {e}")))
}