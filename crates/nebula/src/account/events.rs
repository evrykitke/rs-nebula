//! Facts the account context announces. Other contexts subscribe
//! instead of being called — the account module never learns who cares.

use crate::events::Event;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Self-service sign-up completed: the tenant's admin account exists
/// (in single-tenant mode `tenant_id` is `None` and this is a host user).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRegistered {
    pub tenant_id: Option<Uuid>,
    pub user_id: Uuid,
    pub email: String,
}

impl Event for UserRegistered {
    const NAME: &'static str = "account.user_registered";
}
/// An administrator onboarded a team member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserCreated {
    pub tenant_id: Option<Uuid>,
    pub user_id: Uuid,
    pub email: String,
}

impl Event for UserCreated {
    const NAME: &'static str = "account.user_created";
}
