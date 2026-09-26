//! One client connection: public-key authentication against the daemon's
//! client key.

use std::sync::Arc;

use russh::keys::PublicKey;
use russh::server::{Auth, Handler};

use crate::target::Target;

pub struct Connection {
    client_key: Arc<PublicKey>,
}

impl Connection {
    pub const fn new(client_key: Arc<PublicKey>) -> Self {
        Self { client_key }
    }

    /// The target `user` selects, when `key` is the daemon's client key.
    fn authorize(&self, user: &str, key: &PublicKey) -> Option<Target> {
        if key.key_data() != self.client_key.key_data() {
            return None;
        }
        user.parse().ok()
    }
}

impl Handler for Connection {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(auth(self.authorize(user, key).is_some()))
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let target = self.authorize(user, key);
        if let Some(target) = &target {
            tracing::debug!(%target, "ssh login");
        }
        Ok(auth(target.is_some()))
    }
}

fn auth(accepted: bool) -> Auth {
    if accepted {
        Auth::Accept
    } else {
        Auth::reject()
    }
}
