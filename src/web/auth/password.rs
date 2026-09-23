use anyhow::{Context, Result};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

/// 使用 Argon2 默认安全参数（Argon2id v19）生成 PHC 格式哈希。
pub fn hash_password(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .context("生成密码哈希失败")
}

/// 哈希格式错误会返回错误；只有合法哈希但密码不匹配时返回 false。
pub fn verify_password(password: &str, encoded: &str) -> Result<bool> {
    let hash = PasswordHash::new(encoded).context("解析密码哈希失败")?;
    match Argon2::default().verify_password(password.as_bytes(), &hash) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
        Err(error) => Err(error).context("校验密码哈希失败"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2id_roundtrip_and_rejects_wrong_password() -> Result<()> {
        let encoded = hash_password("correct horse battery staple")?;
        assert!(encoded.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery staple", &encoded)?);
        assert!(!verify_password("wrong", &encoded)?);
        Ok(())
    }
}
