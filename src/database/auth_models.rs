//! Authentication records are deliberately not Debug or Serialize.

#[derive(sqlx::FromRow)]
pub struct UserCredential {
    pub username: String,
    pub password_hash: String,
}

#[derive(sqlx::FromRow)]
pub struct SessionCredential {
    pub username: String,
    pub password_hash: String,
    pub credential_fingerprint: String,
    pub expires_at: Option<i64>,
}
