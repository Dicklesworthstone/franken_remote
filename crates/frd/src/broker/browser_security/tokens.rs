//! Bounded credential storage: retain HMAC tags, never replayable bearer bytes.
//! The public managers enforce their count/peer limits before entropy or insertion.
use super::RedactedSecret;
use ring::hmac;
use std::fmt;
use subtle::{ConditionallySelectable, ConstantTimeEq};

const MAX_CANDIDATES: usize = 8;

pub(super) struct Tokens<R> {
    key: Option<hmac::Key>,
    entries: Vec<([u8; 32], R)>,
}
impl<R> Default for Tokens<R> {
    fn default() -> Self {
        Self {
            key: None,
            entries: Vec::new(),
        }
    }
}
impl<R> fmt::Debug for Tokens<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Even binding metadata can identify peers. Report counts only.
        f.debug_struct("Tokens")
            .field("pending", &self.entries.len())
            .finish_non_exhaustive()
    }
}
impl<R> Tokens<R> {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn records(&self) -> impl Iterator<Item = &R> {
        self.entries.iter().map(|(_, r)| r)
    }
    pub(super) fn get(&self, index: usize) -> &R {
        &self.entries[index].1
    }
    pub(super) fn remove(&mut self, index: usize) {
        self.entries.swap_remove(index);
    }
    pub(super) fn retain(&mut self, keep: impl Fn(&R) -> bool) {
        self.entries.retain(|(_, r)| keep(r));
    }

    pub(super) fn issue(&mut self, domain: &[u8], record: R) -> Result<RedactedSecret, ()> {
        // No deterministic fallback, caller seed, clock, IP or session ID enters
        // production entropy. Native OS initialization/failure is handled by getrandom.
        self.issue_with(domain, record, |bytes| {
            getrandom::fill(bytes).map_err(|_| ())
        })
    }
    // Private injection point for entropy-failure/collision tests, not a public
    // alternative to the OS CSPRNG. Every candidate requires a new successful fill.
    fn issue_with(
        &mut self,
        domain: &[u8],
        record: R,
        mut entropy: impl FnMut(&mut [u8; 32]) -> Result<(), ()>,
    ) -> Result<RedactedSecret, ()> {
        if self.key.is_none() {
            let mut key = [0; 32];
            entropy(&mut key)?;
            self.key = Some(hmac::Key::new(hmac::HMAC_SHA256, &key));
        }
        for _ in 0..MAX_CANDIDATES {
            let mut raw = [0; 32];
            entropy(&mut raw)?;
            let tag = self.tag(domain, &raw).ok_or(())?;
            if self.find_tag(&tag).is_none() {
                self.entries.push((tag, record));
                return Ok(RedactedSecret::new(raw));
            }
        }
        // Never overwrite an existing credential or exceed a retry bound.
        Err(())
    }
    fn tag(&self, domain: &[u8], raw: &[u8; 32]) -> Option<[u8; 32]> {
        let mut mac = hmac::Context::with_key(self.key.as_ref()?);
        mac.update(domain);
        mac.update(raw);
        mac.sign().as_ref().try_into().ok()
    }
    pub(super) fn find(&self, domain: &[u8], raw: &[u8; 32]) -> Option<usize> {
        self.find_tag(&self.tag(domain, raw)?)
    }
    fn find_tag(&self, tag: &[u8; 32]) -> Option<usize> {
        let mut found = 0_u64;
        // Scan the entire bounded store without a prefix comparison, hash-table
        // probe or early match exit. Only occupancy and the final outcome vary.
        for (index, (stored, _)) in self.entries.iter().enumerate() {
            found.conditional_assign(&(index as u64 + 1), stored.ct_eq(tag));
        }
        found
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
    }
}

#[cfg(test)]
mod tests;
