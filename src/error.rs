use std::error::Error;
use std::fmt::{self, Display, Formatter};

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    message: String,
}

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AppError {}

pub trait Context<T> {
    fn context(self, message: impl Display) -> Result<T>;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: Display,
{
    fn context(self, message: impl Display) -> Result<T> {
        self.map_err(|error| AppError::new(format!("{message}: {error}")))
    }
}

pub trait OptionContext<T> {
    fn context(self, message: impl Display) -> Result<T>;
}

impl<T> OptionContext<T> for Option<T> {
    fn context(self, message: impl Display) -> Result<T> {
        self.ok_or_else(|| AppError::new(message.to_string()))
    }
}
