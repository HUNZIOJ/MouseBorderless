use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct DropAuthorizationRegistry {
    inner: Arc<Mutex<HashMap<(Uuid, Uuid), AuthorizedDrop>>>,
}

struct AuthorizedDrop {
    transfer_id: Uuid,
    destination_dir: PathBuf,
    expires_at: Instant,
}

impl DropAuthorizationRegistry {
    pub fn register(
        &self,
        session_id: Uuid,
        transfer_id: Uuid,
        destination_dir: PathBuf,
        ttl: Duration,
    ) -> Uuid {
        let token = Uuid::new_v4();
        self.inner
            .lock()
            .expect("authorization registry poisoned")
            .insert(
                (session_id, token),
                AuthorizedDrop {
                    transfer_id,
                    destination_dir,
                    expires_at: Instant::now() + ttl,
                },
            );
        token
    }

    pub fn consume(
        &self,
        session_id: Uuid,
        transfer_id: Uuid,
        token: Uuid,
    ) -> anyhow::Result<PathBuf> {
        let mut entries = self.inner.lock().expect("authorization registry poisoned");
        let entry = entries
            .get(&(session_id, token))
            .context("drop destination authorization not found")?;
        if entry.transfer_id != transfer_id {
            bail!("drop destination transfer id mismatch");
        }
        if Instant::now() > entry.expires_at {
            entries.remove(&(session_id, token));
            bail!("drop destination authorization expired");
        }
        Ok(entries
            .remove(&(session_id, token))
            .expect("authorization disappeared")
            .destination_dir)
    }

    pub fn revoke_session(&self, session_id: Uuid) {
        self.inner
            .lock()
            .expect("authorization registry poisoned")
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("authorization registry poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use uuid::Uuid;

    use super::*;

    #[test]
    fn authorization_is_bound_to_session_transfer_and_consumed_once() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\Users\\demo\\Desktop"),
            Duration::from_secs(30),
        );

        assert!(registry
            .consume(session_id, transfer_id, token)
            .unwrap()
            .ends_with("Desktop"));
        assert!(registry.consume(session_id, transfer_id, token).is_err());
    }

    #[test]
    fn wrong_transfer_id_does_not_consume_authorization() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\drop"),
            Duration::from_secs(30),
        );

        assert!(registry
            .consume(session_id, Uuid::from_u128(99), token)
            .is_err());
        assert!(registry.consume(session_id, transfer_id, token).is_ok());
    }

    #[test]
    fn revoke_session_removes_all_authorizations() {
        let registry = DropAuthorizationRegistry::default();
        let session_id = Uuid::from_u128(1);
        let transfer_id = Uuid::from_u128(2);
        let token = registry.register(
            session_id,
            transfer_id,
            PathBuf::from("C:\\drop"),
            Duration::from_secs(30),
        );
        registry.revoke_session(session_id);
        assert!(registry.consume(session_id, transfer_id, token).is_err());
    }
}
