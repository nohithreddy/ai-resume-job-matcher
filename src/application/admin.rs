use std::sync::Arc;

use crate::domain::{DomainError, User, UserRepository};

#[derive(Clone)]
pub struct AdminService {
    users: Arc<dyn UserRepository>,
}

impl AdminService {
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self { users }
    }

    pub async fn list_users(&self, offset: usize, limit: usize) -> Result<Vec<User>, DomainError> {
        self.users.list(offset, limit).await
    }
}
