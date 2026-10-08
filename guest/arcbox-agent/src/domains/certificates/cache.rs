//! Bounded certificate reuse with one active signing operation per DNS name.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use arcbox_local_ca::CertificateValidity;
use tokio_rustls::rustls::ServerConfig;

const MAX_CACHED: usize = 256;
type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

#[derive(Default)]
pub(super) struct Cache {
    slots: Mutex<HashMap<String, Slot>>,
}

pub(super) struct Cached {
    config: Arc<ServerConfig>,
    validity: CertificateValidity,
    renew_at: SystemTime,
}

impl Cached {
    pub(super) fn new(
        config: Arc<ServerConfig>,
        validity: CertificateValidity,
        now: SystemTime,
    ) -> Result<Self> {
        ensure!(
            validity.is_valid_at(now.into()),
            "signed leaf certificate is not currently valid"
        );
        let expires: SystemTime = validity.not_after.into();
        let remaining = expires
            .duration_since(now)
            .context("signed leaf certificate expired")?;
        // Renew after three quarters of the remaining lifetime, including root-clipped leaves.
        let renew_at = now + remaining / 4 * 3;
        Ok(Self {
            config,
            validity,
            renew_at,
        })
    }

    fn reusable_at(&self, now: SystemTime) -> bool {
        self.validity.is_valid_at(now.into()) && now < self.renew_at
    }
}

impl Cache {
    pub(super) async fn configuration(
        &self,
        name: &str,
        issue: impl Future<Output = Result<Cached>>,
    ) -> Result<Arc<ServerConfig>> {
        let slot = self.slot(name)?;
        let mut cached = slot.lock().await;
        if let Some(leaf) = cached
            .as_ref()
            .filter(|leaf| leaf.reusable_at(SystemTime::now()))
        {
            return Ok(Arc::clone(&leaf.config));
        }
        *cached = None;
        let leaf = issue.await?;
        let config = Arc::clone(&leaf.config);
        *cached = Some(leaf);
        Ok(config)
    }

    fn slot(&self, name: &str) -> Result<Slot> {
        let mut slots = self.slots.lock().expect("certificate cache poisoned");
        if let Some(slot) = slots.get(name) {
            return Ok(Arc::clone(slot));
        }
        if slots.len() >= MAX_CACHED {
            // Evict only idle slots so callers waiting on a name keep one signing operation.
            let idle = slots
                .iter()
                .find(|(_, slot)| Arc::strong_count(slot) == 1)
                .map(|(name, _)| name.clone())
                .context("too many concurrent certificate requests")?;
            slots.remove(&idle);
        }
        let slot = Slot::default();
        slots.insert(name.to_owned(), Arc::clone(&slot));
        Ok(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_rustls::rustls;

    fn config() -> Arc<ServerConfig> {
        Arc::new(
            ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new())),
        )
    }

    fn leaf(now: SystemTime) -> Cached {
        Cached::new(
            config(),
            CertificateValidity {
                not_before: (now - Duration::from_secs(60)).into(),
                not_after: (now + Duration::from_secs(3600)).into(),
            },
            now,
        )
        .unwrap()
    }

    #[test]
    fn renewal_uses_actual_leaf_bounds_and_rejects_inactive_certificates() {
        let validity =
            CertificateValidity::from_der(include_bytes!("../testdata/probe-cert.der")).unwrap();
        let expires: SystemTime = validity.not_after.into();
        let now = expires - Duration::from_secs(3600);
        let leaf = Cached::new(config(), validity, now).unwrap();
        assert!(leaf.reusable_at(now + Duration::from_secs(2699)));
        assert!(!leaf.reusable_at(now + Duration::from_secs(2700)));
        assert!(!leaf.reusable_at(expires));
        assert!(!leaf.reusable_at(SystemTime::from(validity.not_before) - Duration::from_secs(1)));
        assert!(Cached::new(config(), validity, expires + Duration::from_secs(1)).is_err());
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_name_share_the_signed_leaf() {
        let cache = Cache::default();
        let calls = AtomicUsize::new(0);
        let issue = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(leaf(SystemTime::now()))
        };
        let (first, second) = tokio::join!(
            cache.configuration("web.arcbox.local", issue()),
            cache.configuration("web.arcbox.local", issue()),
        );
        assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_names_sign_independently_and_failures_do_not_poison_retry() {
        let cache = Cache::default();
        let release = tokio::sync::Notify::new();
        let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                cache.configuration("first.arcbox.local", async {
                    release.notified().await;
                    anyhow::bail!("signer unavailable")
                }),
                cache.configuration("second.arcbox.local", async {
                    release.notify_one();
                    Ok(leaf(SystemTime::now()))
                }),
            )
        })
        .await
        .unwrap();
        assert!(first.is_err());
        assert!(second.is_ok());
        assert!(
            cache
                .configuration("first.arcbox.local", async { Ok(leaf(SystemTime::now())) })
                .await
                .is_ok()
        );
    }

    #[test]
    fn capacity_never_evicts_an_active_signing_slot() {
        let cache = Cache::default();
        let active: Vec<_> = (0..MAX_CACHED)
            .map(|index| cache.slot(&index.to_string()).unwrap())
            .collect();
        assert!(Arc::ptr_eq(&active[0], &cache.slot("0").unwrap()));
        assert!(cache.slot("overflow").is_err());
        drop(active);
        assert!(cache.slot("after-release").is_ok());
        assert_eq!(cache.slots.lock().unwrap().len(), MAX_CACHED);
    }
}
