//! Shared memory admission for peer-controlled wire frames.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::wire::MAX_WIRE_MESSAGE_LEN;

/// Node-wide and per-address limits for complete peer messages held in memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLimits {
    pub global_bytes: usize,
    pub per_address_bytes: usize,
}

impl FrameLimits {
    #[must_use]
    pub const fn new(global_bytes: usize, per_address_bytes: usize) -> Self {
        assert!(global_bytes > 0, "the global frame budget must be positive");
        assert!(
            per_address_bytes > 0,
            "the peer frame budget must be positive"
        );
        assert!(
            per_address_bytes <= global_bytes,
            "one peer cannot have more than the global budget"
        );
        Self {
            global_bytes,
            per_address_bytes,
        }
    }
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self::new(8 * MAX_WIRE_MESSAGE_LEN, 2 * MAX_WIRE_MESSAGE_LEN)
    }
}

/// Current frame memory and cumulative refusals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameBudgetStatus {
    pub bytes: usize,
    pub addresses: usize,
    pub global_byte_limit: usize,
    pub per_address_byte_limit: usize,
    pub saturations: u64,
}

/// One shared frame-memory budget.
#[derive(Clone, Debug)]
pub struct FrameBudget {
    inner: Arc<Mutex<Accounting>>,
}

impl FrameBudget {
    #[must_use]
    pub fn new(limits: FrameLimits) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Accounting {
                limits,
                ..Accounting::default()
            })),
        }
    }

    pub(crate) fn try_reserve(
        &self,
        address: IpAddr,
        bytes: usize,
    ) -> Result<FramePermit, FrameBudgetFull> {
        let mut accounting = lock(&self.inner);
        let address_bytes = accounting.addresses.get(&address).copied().unwrap_or(0);
        if bytes
            > accounting
                .limits
                .global_bytes
                .saturating_sub(accounting.bytes)
            || bytes
                > accounting
                    .limits
                    .per_address_bytes
                    .saturating_sub(address_bytes)
        {
            accounting.saturations = accounting.saturations.saturating_add(1);
            return Err(FrameBudgetFull);
        }
        accounting.bytes += bytes;
        *accounting.addresses.entry(address).or_default() += bytes;
        drop(accounting);
        Ok(FramePermit {
            budget: self.clone(),
            address,
            bytes,
        })
    }

    #[must_use]
    pub fn status(&self) -> FrameBudgetStatus {
        let accounting = lock(&self.inner);
        FrameBudgetStatus {
            bytes: accounting.bytes,
            addresses: accounting.addresses.len(),
            global_byte_limit: accounting.limits.global_bytes,
            per_address_byte_limit: accounting.limits.per_address_bytes,
            saturations: accounting.saturations,
        }
    }

    #[must_use]
    pub fn bytes_for(&self, address: IpAddr) -> usize {
        lock(&self.inner)
            .addresses
            .get(&address)
            .copied()
            .unwrap_or(0)
    }
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new(FrameLimits::default())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameBudgetFull;

#[derive(Debug)]
pub struct FramePermit {
    budget: FrameBudget,
    address: IpAddr,
    bytes: usize,
}

impl Drop for FramePermit {
    fn drop(&mut self) {
        let mut accounting = lock(&self.budget.inner);
        accounting.bytes -= self.bytes;
        let remaining = accounting
            .addresses
            .get_mut(&self.address)
            .expect("a frame permit has an address reservation");
        *remaining -= self.bytes;
        if *remaining == 0 {
            accounting.addresses.remove(&self.address);
        }
    }
}

#[derive(Debug, Default)]
struct Accounting {
    limits: FrameLimits,
    bytes: usize,
    addresses: HashMap<IpAddr, usize>,
    saturations: u64,
}

fn lock(accounting: &Mutex<Accounting>) -> MutexGuard<'_, Accounting> {
    accounting
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{FrameBudget, FrameLimits};
    use crate::wire::MAX_WIRE_MESSAGE_LEN;

    #[test]
    fn frame_bytes_are_bounded_per_address_and_globally() {
        let budget = FrameBudget::new(FrameLimits::new(10, 6));
        let first = "127.0.0.1".parse().expect("an address");
        let second = "127.0.0.2".parse().expect("an address");
        let third = "127.0.0.3".parse().expect("an address");

        let first_permit = budget.try_reserve(first, 6).expect("first peer fits");
        assert!(budget.try_reserve(first, 1).is_err(), "per-address bound");
        let second_permit = budget.try_reserve(second, 4).expect("global remainder");
        assert!(budget.try_reserve(third, 1).is_err(), "global bound");
        let status = budget.status();
        assert_eq!(status.bytes, 10);
        assert_eq!(status.global_byte_limit, 10);
        assert_eq!(status.per_address_byte_limit, 6);
        assert_eq!(budget.status().addresses, 2);
        assert_eq!(budget.status().saturations, 2);

        drop(first_permit);
        let third_permit = budget.try_reserve(third, 6).expect("released capacity");
        assert_eq!(budget.bytes_for(first), 0);
        assert_eq!(budget.bytes_for(second), 4);
        assert_eq!(budget.bytes_for(third), 6);
        drop((second_permit, third_permit));
        assert_eq!(budget.status().bytes, 0);
        assert_eq!(budget.status().addresses, 0);
    }

    #[test]
    fn maximum_frames_plateau_before_the_session_limit() {
        let budget = FrameBudget::default();
        let mut permits = Vec::new();
        for suffix in 1..=8 {
            let address = format!("192.0.2.{suffix}").parse().expect("an address");
            permits.push(
                budget
                    .try_reserve(address, MAX_WIRE_MESSAGE_LEN)
                    .expect("the declared global frame capacity"),
            );
        }
        assert_eq!(budget.status().bytes, 8 * MAX_WIRE_MESSAGE_LEN);
        let ninth = "192.0.2.9".parse().expect("an address");
        assert!(budget.try_reserve(ninth, MAX_WIRE_MESSAGE_LEN).is_err());

        drop(permits);
        assert_eq!(budget.status().bytes, 0);
        assert!(budget.try_reserve(ninth, MAX_WIRE_MESSAGE_LEN).is_ok());
    }
}
