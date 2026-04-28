//! Input validation for credentials at the auth boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Username is required")]
    UsernameRequired,

    #[error("Username is too long (maximum {max} characters)")]
    UsernameTooLong { max: usize },

    #[error("Username contains invalid characters")]
    UsernameInvalidChars,

    #[error("Password is required")]
    PasswordRequired,

    #[error("Password is too long (maximum {max} characters)")]
    PasswordTooLong { max: usize },
}

pub const USERNAME_MAX_LENGTH: usize = 128;
pub const PASSWORD_MAX_LENGTH: usize = 1024;

/// Allow alphanumerics plus `_-.@` — matches what Jellyfin itself accepts.
pub fn validate_username(username: &str) -> Result<(), ValidationError> {
    let trimmed = username.trim();

    if trimmed.is_empty() {
        return Err(ValidationError::UsernameRequired);
    }

    if trimmed.len() > USERNAME_MAX_LENGTH {
        return Err(ValidationError::UsernameTooLong {
            max: USERNAME_MAX_LENGTH,
        });
    }

    if !trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '@')
    {
        return Err(ValidationError::UsernameInvalidChars);
    }

    Ok(())
}

/// Cap password length to prevent DoS via expensive hash work; the backend
/// Jellyfin server enforces strength.
pub fn validate_password(password: &str) -> Result<(), ValidationError> {
    if password.is_empty() {
        return Err(ValidationError::PasswordRequired);
    }

    if password.len() > PASSWORD_MAX_LENGTH {
        return Err(ValidationError::PasswordTooLong {
            max: PASSWORD_MAX_LENGTH,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rejects_empty() {
        assert!(matches!(
            validate_username(""),
            Err(ValidationError::UsernameRequired)
        ));
        assert!(matches!(
            validate_username("   "),
            Err(ValidationError::UsernameRequired)
        ));
    }

    #[test]
    fn username_accepts_normal() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("alice.bob_42-x@host").is_ok());
    }

    #[test]
    fn username_rejects_invalid_chars() {
        assert!(matches!(
            validate_username("a b"),
            Err(ValidationError::UsernameInvalidChars)
        ));
        assert!(matches!(
            validate_username("a/b"),
            Err(ValidationError::UsernameInvalidChars)
        ));
    }

    #[test]
    fn username_rejects_too_long() {
        let s = "a".repeat(USERNAME_MAX_LENGTH + 1);
        assert!(matches!(
            validate_username(&s),
            Err(ValidationError::UsernameTooLong { .. })
        ));
    }

    #[test]
    fn password_rejects_empty() {
        assert!(matches!(
            validate_password(""),
            Err(ValidationError::PasswordRequired)
        ));
    }

    #[test]
    fn password_rejects_too_long() {
        let s = "a".repeat(PASSWORD_MAX_LENGTH + 1);
        assert!(matches!(
            validate_password(&s),
            Err(ValidationError::PasswordTooLong { .. })
        ));
    }
}
