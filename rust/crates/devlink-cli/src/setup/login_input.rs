use std::future::Future;

/// Ephemeral credentials. Deliberately has no Debug or Serialize implementation.
pub struct LoginCredentials {
    email: String,
    password: String,
}

impl LoginCredentials {
    pub fn new(email: String, password: String) -> Result<Self, LoginInputError> {
        if email.trim().is_empty()
            || email.len() > 320
            || password.is_empty()
            || password.len() > 72
        {
            return Err(LoginInputError::Invalid);
        }
        Ok(Self { email, password })
    }

    pub(crate) fn email(&self) -> &str {
        &self.email
    }
    pub(crate) fn password(&self) -> &str {
        &self.password
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LoginInputError {
    Unavailable,
    Invalid,
    Cancelled,
    ReadFailed,
}

/// UI/stdio belongs to an injected adapter, never to authentication or orchestration.
pub trait LoginInput: Send {
    fn available(&self) -> bool;
    fn interactive(&self) -> bool {
        false
    }
    fn request(&mut self)
    -> impl Future<Output = Result<LoginCredentials, LoginInputError>> + Send;
}

#[derive(Default)]
pub struct NoLoginInput;

impl LoginInput for NoLoginInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
        Err(LoginInputError::Unavailable)
    }
}
