//! Exercises source invalidation without a native VM or Keychain.

use super::*;
use std::{pin::pin, task::Poll};

#[tokio::test]
async fn source_changes_ignore_republication_and_observe_missing_ca_or_new_vm() {
    for change in 0..3 {
        let (authority_tx, authority) = watch::channel(Some(1));
        let (vm_tx, vm) = watch::channel(1);
        let current = || Ok(*authority.borrow() == Some(1) && *vm.borrow() == 1);
        let mut changed = pin!(source_changed(authority.clone(), vm.clone(), &current));
        assert!(matches!(futures::poll!(&mut changed), Poll::Pending));
        authority_tx.send_replace(Some(1));
        assert!(matches!(futures::poll!(&mut changed), Poll::Pending));
        match change {
            0 => {
                authority_tx.send_replace(None);
            }
            1 => {
                authority_tx.send_replace(Some(2));
            }
            _ => {
                vm_tx.send_replace(2);
            }
        }
        assert!(matches!(futures::poll!(&mut changed), Poll::Ready(Ok(()))));
    }
}
