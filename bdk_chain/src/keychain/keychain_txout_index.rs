use crate::{
    collections::*,
    miniscript::{Descriptor, DescriptorPublicKey},
    ForEachTxOut, SpkTxOutIndex,
};
use alloc::vec::Vec;
use bitcoin::{secp256k1::Secp256k1, OutPoint, Script, TxOut};
use core::{fmt::Debug, ops::Deref};

use super::DerivationAdditions;

const DERIVED_KEY_COUNT: u32 = 1 << 31;

/// A convenient wrapper around [`SpkTxOutIndex`] that relates script pubkeys to miniscript public
/// [`Descriptor`]s.
///
/// Descriptors are referenced by the provided keychain generic (`K`).
///
/// Script pubkeys for a descriptor are revealed chronologically from index 0. I.e. If the last
/// revealed index of a descriptor is 5, scripts of indices 0 to 4 are guaranteed to already be
/// revealed. In addition to revealed scripts, we have a `lookahead` parameter for each keychain
/// which defines the number of scripts to store ahead of last revealed.
///
/// Methods that may result in changes to the number of stored script pubkeys will return
/// [`DerivationAdditions`] to reflect the changes. This can be persisted for future recovery.
///
/// ## Synopsis
///
/// ```
/// use bdk_chain::keychain::KeychainTxOutIndex;
/// # use bdk_chain::{ miniscript::{Descriptor, DescriptorPublicKey} };
/// # use core::str::FromStr;
///
/// // imagine our service has internal and external addresses but also addresses for users
/// #[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
/// enum MyKeychain {
///     External,
///     Internal,
///     MyAppUser {
///         user_id: u32
///     }
/// }
///
/// let mut txout_index = KeychainTxOutIndex::<MyKeychain>::default();
///
/// # let secp = bdk_chain::bitcoin::secp256k1::Secp256k1::signing_only();
/// # let (external_descriptor,_) = Descriptor::<DescriptorPublicKey>::parse_descriptor(&secp, "tr([73c5da0a/86'/0'/0']xprv9xgqHN7yz9MwCkxsBPN5qetuNdQSUttZNKw1dcYTV4mkaAFiBVGQziHs3NRSWMkCzvgjEe3n9xV8oYywvM8at9yRqyaZVz6TYYhX98VjsUk/0/*)").unwrap();
/// # let (internal_descriptor,_) = Descriptor::<DescriptorPublicKey>::parse_descriptor(&secp, "tr([73c5da0a/86'/0'/0']xprv9xgqHN7yz9MwCkxsBPN5qetuNdQSUttZNKw1dcYTV4mkaAFiBVGQziHs3NRSWMkCzvgjEe3n9xV8oYywvM8at9yRqyaZVz6TYYhX98VjsUk/1/*)").unwrap();
/// # let descriptor_for_user_42 = external_descriptor.clone();
/// txout_index.add_keychain(MyKeychain::External, external_descriptor);
/// txout_index.add_keychain(MyKeychain::Internal, internal_descriptor);
/// txout_index.add_keychain(MyKeychain::MyAppUser { user_id: 42 }, descriptor_for_user_42);
///
/// let new_spk_for_user = txout_index.reveal_next_script_pubkey(&MyKeychain::MyAppUser{ user_id: 42 });
/// ```
///
/// [`Ord`]: core::cmp::Ord
/// [`SpkTxOutIndex`]: crate::spk_txout_index::SpkTxOutIndex
/// [`Descriptor`]: crate::miniscript::Descriptor
#[derive(Clone, Debug)]
pub struct KeychainTxOutIndex<K> {
    inner: SpkTxOutIndex<(K, u32)>,
    // descriptors of each keychain
    keychains: BTreeMap<K, Descriptor<DescriptorPublicKey>>,
    // last stored indexes
    last_revealed: BTreeMap<K, u32>,
    // lookahead settings for each keychain
    lookahead: BTreeMap<K, u32>,
}

impl<K> Default for KeychainTxOutIndex<K> {
    fn default() -> Self {
        Self {
            inner: SpkTxOutIndex::default(),
            keychains: BTreeMap::default(),
            last_revealed: BTreeMap::default(),
            lookahead: BTreeMap::default(),
        }
    }
}

impl<K> Deref for KeychainTxOutIndex<K> {
    type Target = SpkTxOutIndex<(K, u32)>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<K: Clone + Ord + Debug> KeychainTxOutIndex<K> {
    /// Scans an object for relevant outpoints, which are stored and indexed internally.
    ///
    /// If the matched script pubkey is part of the lookahead, the last stored index is updated for
    /// the script pubkey's keychain and the [`DerivationAdditions`] returned will reflect the
    /// change.
    ///
    /// Typically this method is used in two situations:
    ///
    /// 1. After loading transaction data from disk you may scan over all the txouts to restore all
    /// your txouts.
    /// 2. When getting new data from the chain you usually scan it before incorporating it into
    /// your chain state (i.e. `SparseChain`, `ChainGraph`).
    ///
    /// See [`ForEachTxout`] for the types that support this.
    ///
    /// [`ForEachTxout`]: crate::ForEachTxOut
    pub fn scan(&mut self, txouts: &impl ForEachTxOut) -> DerivationAdditions<K> {
        let mut additions = DerivationAdditions::<K>::default();
        txouts.for_each_txout(|(op, txout)| additions.append(self.scan_txout(op, txout)));
        additions
    }

    /// Scan a single outpoint for a matching script pubkey.
    ///
    /// If it matches the index will store and index it.
    pub fn scan_txout(&mut self, op: OutPoint, txout: &TxOut) -> DerivationAdditions<K> {
        match self.inner.scan_txout(op, txout).cloned() {
            Some((keychain, index)) => self.reveal_to(&keychain, index),
            None => DerivationAdditions::default(),
        }
    }

    /// Return a reference to the internal [`SpkTxOutIndex`].
    pub fn inner(&self) -> &SpkTxOutIndex<(K, u32)> {
        &self.inner
    }

    /// Return a reference to the internal map of keychain to descriptors.
    pub fn keychains(&self) -> &BTreeMap<K, Descriptor<DescriptorPublicKey>> {
        &self.keychains
    }

    /// Add a keychain to the tracker's `txout_index` with a descriptor to derive addresses for it.
    ///
    /// Adding a keychain means you will be able to derive new script pubkeys under that keychain
    /// and the txout index will discover transaction outputs with those script pubkeys.
    ///
    /// # Panics
    ///
    /// This will panic if a different `descriptor` is introduced to the same `keychain`.
    pub fn add_keychain(&mut self, keychain: K, descriptor: Descriptor<DescriptorPublicKey>) {
        let old_descriptor = &*self.keychains.entry(keychain).or_insert(descriptor.clone());
        assert_eq!(
            &descriptor, old_descriptor,
            "keychain already contains a different descriptor"
        );
    }

    /// Return the lookahead setting for each keychain.
    ///
    /// Refer to [`set_lookahead`] for a deeper explanation on `lookahead`.
    ///
    /// [`set_lookahead`]: Self::set_lookahead
    pub fn lookaheads(&self) -> &BTreeMap<K, u32> {
        &self.lookahead
    }

    /// Convenience method to call [`set_lookahead`] for all keychains.
    ///
    /// [`set_lookahead`]: Self::set_lookahead
    pub fn set_all_lookaheads(&mut self, lookahead: u32) {
        for keychain in &self.keychains.keys().cloned().collect::<Vec<_>>() {
            self.lookahead.insert(keychain.clone(), lookahead);
            self.replenish_lookahead(&keychain);
        }
    }

    /// Set the lookahead count for `keychain`.
    ///
    /// The lookahead is the number of scripts to cache ahead of the last stored script index. This
    /// is useful during a scan via [`scan`] or [`scan_txout`].
    ///
    /// # Panics
    ///
    /// This will panic if `keychain` does not exist.
    ///
    /// [`scan`]: Self::scan
    /// [`scan_txout`]: Self::scan_txout
    pub fn set_lookahead(&mut self, keychain: &K, lookahead: u32) {
        self.lookahead.insert(keychain.clone(), lookahead);
        self.replenish_lookahead(keychain);
    }

    fn replenish_lookahead(&mut self, keychain: &K) {
        let descriptor = self.keychains.get(keychain).expect("keychain must exist");
        let next_index = self.last_revealed.get(keychain).map_or(0, |v| *v + 1);
        let lookahead = self.lookahead.get(keychain).map_or(0, |v| *v);

        for (new_index, new_spk) in
            range_descriptor_spks(descriptor.clone(), next_index..next_index + lookahead)
        {
            let _inserted = self
                .inner
                .insert_script_pubkey((keychain.clone(), new_index), new_spk);
            debug_assert!(_inserted, "must not have existing spk");
        }
    }

    /// Generates script pubkey iterators for every `keychain`. The iterators iterate over all
    /// derivable script pubkeys.
    pub fn all_keychain_script_pubkeys(
        &self,
    ) -> BTreeMap<K, impl Iterator<Item = (u32, Script)> + Clone> {
        self.keychains
            .iter()
            .map(|(keychain, descriptor)| {
                (
                    keychain.clone(),
                    range_descriptor_spks(descriptor.clone(), 0..),
                )
            })
            .collect()
    }

    /// Generates a script pubkey iterator for the given `keychain`'s descriptor (if exists). The
    /// iterator iterates over all derivable scripts of the keychain's descriptor.
    ///
    /// # Panics
    ///
    /// This will panic if `keychain` does not exist.
    pub fn keychain_script_pubkeys(
        &self,
        keychain: &K,
    ) -> impl Iterator<Item = (u32, Script)> + Clone {
        let descriptor = self
            .keychains
            .get(keychain)
            .expect("keychain must exist")
            .clone();
        range_descriptor_spks(descriptor, 0..)
    }

    /// Convenience method to get [`revealed_script_pubkeys`] for all keychains.
    ///
    /// [`revealed_script_pubkeys`]: Self::revealed_script_pubkeys
    pub fn all_revealed_script_pubkeys(
        &self,
    ) -> BTreeMap<K, impl Iterator<Item = (u32, &Script)> + Clone> {
        self.keychains
            .keys()
            .map(|keychain| (keychain.clone(), self.revealed_script_pubkeys(keychain)))
            .collect()
    }

    /// Iterates over the script pubkeys revealed by this index under `keychain`.
    pub fn revealed_script_pubkeys(
        &self,
        keychain: &K,
    ) -> impl DoubleEndedIterator<Item = (u32, &Script)> + Clone {
        let next_index = self.last_revealed.get(keychain).map_or(0, |v| *v + 1);
        self.inner
            .script_pubkeys()
            .range((keychain.clone(), u32::MIN)..(keychain.clone(), next_index))
            .map(|((_, derivation_index), spk)| (*derivation_index, spk))
    }

    /// Get the next derivation index for `keychain`. This is the index after the last revealed
    /// derivation index.
    ///
    /// The second field in the returned tuple represents whether the next derivation index is new.
    /// There are two scenarios where the next derivation index is reused (not new):
    ///
    /// 1. The keychain's descriptor has no wildcard, and a script has already been revealed.
    /// 2. The number of revealed scripts has already reached 2^31 (refer to BIP-32).
    ///
    /// Not checking the second field of the tuple may result in address reuse.
    ///
    /// # Panics
    ///
    /// Panics if the `keychain` does not exist.
    pub fn next_index(&self, keychain: &K) -> (u32, bool) {
        let descriptor = self.keychains.get(keychain).expect("keychain must exist");
        let last_index = self.last_revealed.get(keychain).cloned();

        // we can only get the next index if wildcard exists
        let has_wildcard = descriptor.has_wildcard();

        match last_index {
            // if there is no index, next_index is always 0
            None => (0, true),
            // descriptors without wildcards can only have one index
            Some(_) if !has_wildcard => (0, false),
            // derivation index must be < 2^31 (BIP-32)
            Some(index) if index >= DERIVED_KEY_COUNT => unreachable!("index is out of bounds"),
            Some(index) if index == DERIVED_KEY_COUNT - 1 => (index, false),
            // get next derivation index
            Some(index) => (index + 1, true),
        }
    }

    /// Get the last derivation index that is revealed for each keychain.
    ///
    /// Keychains with no revealed indices will not be included in the returned [`BTreeMap`].
    pub fn last_revealed_indices(&self) -> &BTreeMap<K, u32> {
        &self.last_revealed
    }

    /// Get the last derivation index revealed for `keychain`.
    pub fn last_revealed_index(&self, keychain: &K) -> Option<u32> {
        self.last_revealed.get(keychain).cloned()
    }

    /// Convenience method to call [`Self::reveal_to`] on several keychains.
    pub fn reveal_all_to(&mut self, keychains: &BTreeMap<K, u32>) -> DerivationAdditions<K> {
        let mut additions = DerivationAdditions::default();
        for (keychain, &index) in keychains {
            additions.append(self.reveal_to(keychain, index));
        }
        additions
    }

    /// Reveals script pubkeys from the descriptor **up to and including** `index`, unless the
    /// script pubkey is already revealed.
    ///
    /// Returns [`DerivationAdditions`] for new script pubkeys that have been revealed. If no
    /// script pubkeys are revealed, [`DerivationAdditions`] will be empty.
    ///
    /// # Panics
    ///
    /// Panics if `keychain` does not exist.
    pub fn reveal_to(&mut self, keychain: &K, target_index: u32) -> DerivationAdditions<K> {
        let descriptor = self.keychains.get(keychain).expect("keychain must exist");
        let has_wildcard = descriptor.has_wildcard();

        let target_index = if has_wildcard { target_index } else { 0 };
        let next_index = self.last_revealed.get(keychain).map_or(0, |v| *v + 1);
        let lookahead = self.lookahead.get(keychain).map_or(0, |v| *v);

        // if we are able to reveal new indexes, the latest revealed index goes here
        let mut revealed_index = None;

        // if target is already surpassed, there is nothing to be done
        if target_index < next_index {
            return DerivationAdditions::default();
        }

        // if target is already stored (due to lookahead), this can be our new revealed index
        if target_index < next_index + lookahead {
            revealed_index = Some(target_index);
        }

        // we range over indexes that are not stored
        let range = next_index + lookahead..=target_index + lookahead;

        for (new_index, new_spk) in range_descriptor_spks(descriptor.clone(), range) {
            let _inserted = self
                .inner
                .insert_script_pubkey((keychain.clone(), new_index), new_spk);
            debug_assert!(_inserted, "must not have existing spk",);

            // everything after `target_index` is stored for lookahead only
            if new_index <= target_index {
                revealed_index = Some(new_index);
            }
        }

        match revealed_index {
            Some(index) => {
                let _old_index = self.last_revealed.insert(keychain.clone(), index);
                debug_assert!(_old_index < Some(index));
                DerivationAdditions([(keychain.clone(), index)].into())
            }
            None => DerivationAdditions::default(),
        }
    }

    /// Attempts to reveal the next script pubkey for `keychain`. This is the script pubkey
    ///
    /// Returns the derivation index of the revealed script pubkey, the revealed script pubkey and a
    /// [`DerivationAdditions`] which represents changes in the last revealed index (if any).
    ///
    /// When a new script cannot be revealed, we return the last revealed script and an empty
    /// [`DerivationAdditions`]. There are two scenarios when a new script pubkey cannot be derived:
    ///
    ///  1. The descriptor has no wildcard and already has one script revealed.
    ///  2. The descriptor has already revealed scripts up to the numeric bound.
    ///
    /// # Panics
    ///
    /// Panics if the `keychain` does not exist.
    pub fn reveal_next_script_pubkey(
        &mut self,
        keychain: &K,
    ) -> ((u32, &Script), DerivationAdditions<K>) {
        let (next_index, _) = self.next_index(keychain);
        let additions = self.reveal_to(keychain, next_index);
        let script = self
            .inner
            .spk_at_index(&(keychain.clone(), next_index))
            .expect("script must already be stored");
        ((next_index, script), additions)
    }

    /// Gets the next unused script pubkey in the keychain. I.e. the script pubkey with the lowest
    /// index that has not been used yet.
    ///
    /// This will derive and reveal a new script pubkey if no more unused script pubkeys exist.
    ///
    /// If the descriptor has no wildcard and already has a used script pubkey, or if a descriptor
    /// has used all scripts up to the derivation bounds, the last derived script pubkey will be
    /// returned.
    ///
    /// # Panics
    ///
    /// Panics if `keychain` has never been added to the index
    pub fn next_unused_script_pubkey(
        &mut self,
        keychain: &K,
    ) -> ((u32, &Script), DerivationAdditions<K>) {
        let need_new = self.unused_script_pubkeys(keychain).next().is_none();
        // this rather strange branch is needed because of some lifetime issues
        if need_new {
            self.reveal_next_script_pubkey(keychain)
        } else {
            (
                self.unused_script_pubkeys(keychain)
                    .next()
                    .expect("we already know next exists"),
                DerivationAdditions::default(),
            )
        }
    }

    /// Marks the script pubkey at `index` as used even though it hasn't seen an output with it.
    /// This only has an effect when the `index` had been added to `self` already and was unused.
    ///
    /// Returns whether the `index` was originally present as `unused`.
    ///
    /// This is useful when you want to reserve a script pubkey for something but don't want to add
    /// the transaction output using it to the index yet. Other callers will consider `index` on
    /// `keychain` used until you call [`unmark_used`].
    ///
    /// [`unmark_used`]: Self::unmark_used
    pub fn mark_used(&mut self, keychain: &K, index: u32) -> bool {
        self.inner.mark_used(&(keychain.clone(), index))
    }

    /// Undoes the effect of [`mark_used`]. Returns whether the `index` is inserted back into
    /// `unused`.
    ///
    /// Note that if `self` has scanned an output with this script pubkey then this will have no
    /// effect.
    ///
    /// [`mark_used`]: Self::mark_used
    pub fn unmark_used(&mut self, keychain: &K, index: u32) -> bool {
        self.inner.unmark_used(&(keychain.clone(), index))
    }

    /// Iterates over all unused script pubkeys for a `keychain` that have been stored in the index.
    pub fn unused_script_pubkeys(
        &self,
        keychain: &K,
    ) -> impl DoubleEndedIterator<Item = (u32, &Script)> {
        let next_index = self.last_revealed.get(keychain).map_or(0, |&v| v + 1);
        let range = (keychain.clone(), u32::MIN)..(keychain.clone(), next_index);
        self.inner
            .unused(range)
            .map(|((_, i), script)| (*i, script))
    }

    /// Iterates over all the [`OutPoint`] that have a `TxOut` with a script pubkey derived from
    /// `keychain`.
    pub fn keychain_txouts(
        &self,
        keychain: &K,
    ) -> impl DoubleEndedIterator<Item = (u32, OutPoint)> + '_ {
        self.inner
            .outputs_in_range((keychain.clone(), u32::MIN)..(keychain.clone(), u32::MAX))
            .map(|((_, i), op)| (*i, op))
    }

    /// Returns the highest derivation index of the `keychain` where [`KeychainTxOutIndex`] has
    /// found a [`TxOut`] with it's script pubkey.
    pub fn last_used_index(&self, keychain: &K) -> Option<u32> {
        self.keychain_txouts(keychain).last().map(|(i, _)| i)
    }

    /// Returns the highest derivation index of each keychain that [`KeychainTxOutIndex`] has found
    /// a [`TxOut`] with it's script pubkey.
    pub fn last_used_indices(&self) -> BTreeMap<K, u32> {
        self.keychains
            .iter()
            .filter_map(|(keychain, _)| {
                self.last_used_index(keychain)
                    .map(|index| (keychain.clone(), index))
            })
            .collect()
    }

    /// Applies the derivation additions to the [`KeychainTxOutIndex`], extending the number of
    /// derived scripts per keychain, as specified in the `additions`.
    pub fn apply_additions(&mut self, additions: DerivationAdditions<K>) {
        let _ = self.reveal_all_to(&additions.0);
    }
}

fn range_descriptor_spks<'a, R>(
    descriptor: Descriptor<DescriptorPublicKey>,
    range: R,
) -> impl Iterator<Item = (u32, Script)> + Clone + Send
where
    R: Iterator<Item = u32> + Clone + Send,
{
    let secp = Secp256k1::verification_only();
    let has_wildcard = descriptor.has_wildcard();
    range
        .into_iter()
        // non-wildcard descriptors can only have one derivation index (0)
        .take_while(move |&index| has_wildcard || index == 0)
        // we can only iterate over non-hardened indices
        .take_while(|&index| index < DERIVED_KEY_COUNT)
        // take until failure
        .map_while(move |index| {
            descriptor
                .derived_descriptor(&secp, index)
                .map(|desc| (index, desc.script_pubkey()))
                .ok()
        })
}
