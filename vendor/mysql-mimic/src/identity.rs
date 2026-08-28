//! Pluggable authentication and identity provider.
//!
//! Mirrors the Python library's `IdentityProvider` and `AuthPlugin` system.

use crate::protocol::auth;

/// Represents a known user with optional password hash.
#[derive(Debug, Clone)]
pub struct User {
    /// Username.
    pub name: String,
    /// Password (plaintext, for `mysql_native_password` hashing).
    pub password: Option<String>,
    /// Auth plugin name (default: `mysql_native_password`).
    pub auth_plugin: String,
}

impl User {
    /// Create a new user with no password.
    pub fn new(name: impl Into<String>) -> Self {
        User {
            name: name.into(),
            password: None,
            auth_plugin: auth::AUTH_PLUGIN_NAME.into(),
        }
    }

    /// Create a new user with a password.
    pub fn with_password(name: impl Into<String>, password: impl Into<String>) -> Self {
        User {
            name: name.into(),
            password: Some(password.into()),
            auth_plugin: auth::AUTH_PLUGIN_NAME.into(),
        }
    }
}

/// Result of an authentication attempt.
#[derive(Debug, Clone)]
pub enum AuthResult {
    /// Authentication succeeded.
    Success,
    /// Authentication failed with a message.
    Denied(String),
}

/// Provides identity and authentication services.
///
/// Implement this trait to control which users can connect and how
/// authentication is performed.
///
/// The default implementation ([`SimpleIdentityProvider`]) accepts all
/// users without a password.
pub trait IdentityProvider: Send + Sync + 'static {
    /// Look up a user by name.
    ///
    /// Return `None` to deny access.
    fn get_user(&self, username: &str) -> impl std::future::Future<Output = Option<User>> + Send {
        async move {
            let _ = username;
            None
        }
    }

    /// Authenticate a user given the scramble and client auth response.
    ///
    /// Default implementation uses `mysql_native_password`.
    fn authenticate(
        &self,
        user: &User,
        scramble: &[u8],
        client_response: &[u8],
    ) -> impl std::future::Future<Output = AuthResult> + Send {
        async move {
            match &user.password {
                None => {
                    // No password set → accept empty auth response
                    if client_response.is_empty() {
                        AuthResult::Success
                    } else {
                        AuthResult::Denied("unexpected auth data for passwordless user".into())
                    }
                }
                Some(pass) => {
                    if auth::verify_auth_response(pass.as_bytes(), scramble, client_response) {
                        AuthResult::Success
                    } else {
                        AuthResult::Denied(format!("Access denied for user '{}'", user.name))
                    }
                }
            }
        }
    }

    /// Get the default auth plugin name.
    fn default_auth_plugin(&self) -> &str {
        auth::AUTH_PLUGIN_NAME
    }
}

/// A simple identity provider that accepts all users without a password.
///
/// This is the default behavior matching the Python library.
#[derive(Debug, Clone, Default)]
pub struct SimpleIdentityProvider;

impl IdentityProvider for SimpleIdentityProvider {
    fn get_user(&self, username: &str) -> impl std::future::Future<Output = Option<User>> + Send {
        let user = User::new(username.to_string());
        async move { Some(user) }
    }

    async fn authenticate(
        &self,
        _user: &User,
        _scramble: &[u8],
        _client_response: &[u8],
    ) -> AuthResult {
        AuthResult::Success
    }
}

/// An identity provider backed by a static list of users.
#[derive(Debug, Clone)]
pub struct StaticIdentityProvider {
    users: Vec<User>,
}

impl StaticIdentityProvider {
    /// Create a new provider with the given users.
    pub fn new(users: Vec<User>) -> Self {
        StaticIdentityProvider { users }
    }
}

impl IdentityProvider for StaticIdentityProvider {
    fn get_user(&self, username: &str) -> impl std::future::Future<Output = Option<User>> + Send {
        let user = self.users.iter().find(|u| u.name == username).cloned();
        async move { user }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_identity_provider() {
        let provider = SimpleIdentityProvider;
        let user = provider.get_user("anyone").await;
        assert!(user.is_some());

        let user = user.unwrap();
        let result = provider.authenticate(&user, &[0; 20], &[]).await;
        assert!(matches!(result, AuthResult::Success));
    }

    #[tokio::test]
    async fn test_static_identity_provider() {
        let provider = StaticIdentityProvider::new(vec![User::with_password("admin", "secret")]);

        // Known user
        assert!(provider.get_user("admin").await.is_some());

        // Unknown user
        assert!(provider.get_user("unknown").await.is_none());
    }
}
