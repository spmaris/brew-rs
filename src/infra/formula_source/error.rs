//! Typed failure of the installed-service metadata query.

use crate::domain::plan::{NameError, ServiceError};
use crate::infra::api::ApiError;

/// Why official metadata could not be proven to describe an installed receipt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstalledServiceError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Service(#[from] ServiceError),
}
