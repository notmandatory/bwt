use serde::Serialize;
use std::collections::HashMap;
use std::result::Result as StdResult;

use bitcoin::util::bip32::ChildNumber;
use bitcoin::{Address, Network};
use bitcoincore_rpc::json::{ImportMultiRequest, ImportMultiRequestScriptPubkey};
use bitcoincore_rpc::{self as rpc, Client as RpcClient, RpcApi};

use crate::error::{Context, Result};
use crate::store::MemoryStore;
use crate::types::RescanSince;
use crate::util::descriptor::{Checksum, DescKeyInfo, ExtendedDescriptor};
use crate::util::xpub::{Bip32Origin, XyzPubKey};

const LABEL_PREFIX: &str = "bwt";

#[derive(Debug)]
pub struct WalletWatcher {
    wallets: HashMap<Checksum, Wallet>,
}

impl WalletWatcher {
    pub fn new(wallets: Vec<Wallet>) -> Result<Self> {
        let num_wallets = wallets.len();
        let wallets = wallets
            .into_iter()
            .map(|wallet| (wallet.checksum.clone(), wallet))
            .collect::<HashMap<_, _>>();
        ensure!(
            wallets.len() == num_wallets,
            "Descriptor checksum collision detected"
        );
        Ok(Self { wallets })
    }

    pub fn from_config(
        descs: &[(ExtendedDescriptor, RescanSince)],
        xpubs: &[(XyzPubKey, RescanSince)],
        bare_xpubs: &[(XyzPubKey, RescanSince)],
        network: Network,
        gap_limit: u32,
        initial_import_size: u32,
    ) -> Result<Self> {
        let mut wallets = vec![];
        for (desc, rescan) in descs {
            wallets.push(
                Wallet::from_descriptor(
                    desc.clone(),
                    network,
                    gap_limit,
                    initial_import_size,
                    *rescan,
                )
                .with_context(|| format!("invalid descriptor {}", desc))?,
            );
        }
        for (xpub, rescan) in xpubs {
            wallets.append(
                &mut Wallet::from_xpub(
                    xpub.clone(),
                    network,
                    gap_limit,
                    initial_import_size,
                    *rescan,
                )
                .with_context(|| format!("invalid xpub {}", xpub))?,
            );
        }
        for (xpub, rescan) in bare_xpubs {
            wallets.push(
                Wallet::from_bare_xpub(
                    xpub.clone(),
                    network,
                    gap_limit,
                    initial_import_size,
                    *rescan,
                )
                .with_context(|| format!("invalid xpub {}", xpub))?,
            );
        }
        if wallets.is_empty() {
            error!("Please provide at least one wallet to track (via --descriptor, --xpub or --bare-xpub).");
            bail!("no xpubs provided");
        }
        Self::new(wallets)
    }

    pub fn wallets(&self) -> &HashMap<Checksum, Wallet> {
        &self.wallets
    }

    pub fn get(&self, checksum: &Checksum) -> Option<&Wallet> {
        self.wallets.get(checksum)
    }

    // Mark an address as funded
    pub fn mark_funded(&mut self, origin: &KeyOrigin) {
        if let KeyOrigin::Descriptor(checksum, index) = origin {
            if let Some(wallet) = self.wallets.get_mut(checksum) {
                if wallet.max_imported_index.map_or(true, |max| *index > max) {
                    wallet.max_imported_index = Some(*index);
                }

                if wallet.max_funded_index.map_or(true, |max| *index > max) {
                    wallet.max_funded_index = Some(*index);
                }
            }
        }
    }

    // check previous imports and update max_imported_index
    pub fn check_imports(&mut self, rpc: &RpcClient) -> Result<()> {
        debug!("checking previous imports");
        let labels: Vec<String> = rpc.call("listlabels", &[]).map_err(labels_error)?;
        let mut imported_indexes: HashMap<Checksum, u32> = HashMap::new();
        for label in labels {
            if let Some(KeyOrigin::Descriptor(checksum, index)) = KeyOrigin::from_label(&label) {
                if self.wallets.contains_key(&checksum) {
                    imported_indexes
                        .entry(checksum)
                        .and_modify(|current| *current = (*current).max(index))
                        .or_insert(index);
                }
            }
        }

        for (checksum, max_imported_index) in imported_indexes {
            trace!(
                "wallet {} was imported up to index {}",
                checksum,
                max_imported_index
            );
            let wallet = self.wallets.get_mut(&checksum).unwrap();
            wallet.max_imported_index = Some(max_imported_index);

            // if anything was imported at all, assume we've finished the initial sync. this might
            // not hold true if bwt shuts down while syncing, but this only means that we'll use
            // the smaller gap_limit instead of the initial_import_size, which is acceptable.
            wallet.done_initial_import = true;
        }
        Ok(())
    }

    pub fn do_imports(&mut self, rpc: &RpcClient, rescan: bool) -> Result<bool> {
        let mut import_reqs = vec![];
        let mut pending_updates = vec![];

        for (checksum, wallet) in self.wallets.iter_mut() {
            let watch_index = wallet.watch_index();
            if wallet.max_imported_index.map_or(true, |i| watch_index > i) {
                let start_index = wallet
                    .max_imported_index
                    .map_or(0, |max_imported| max_imported + 1);

                debug!(
                    "importing {} range {}-{} with rescan={}",
                    checksum, start_index, watch_index, rescan,
                );

                import_reqs.append(&mut wallet.make_imports(start_index, watch_index, rescan));

                pending_updates.push((wallet, watch_index));
            } else if !wallet.done_initial_import {
                debug!(
                    "done initial import for {} up to index {}",
                    checksum,
                    wallet.max_imported_index.unwrap()
                );
                wallet.done_initial_import = true;
            } else {
                trace!("no imports needed for {}", checksum);
            }
        }

        let has_imports = !import_reqs.is_empty();

        if has_imports {
            // TODO report syncing progress
            info!(
                "importing batch of {} addresses... (this may take awhile)",
                import_reqs.len()
            );
            batch_import(rpc, import_reqs)?;
            info!("done importing batch");
        }

        for (wallet, imported_index) in pending_updates {
            debug!(
                "imported {} up to index {}",
                wallet.checksum, imported_index
            );
            wallet.max_imported_index = Some(imported_index);
        }

        Ok(has_imports)
    }
}

#[derive(Debug, Clone)]
pub struct Wallet {
    desc: ExtendedDescriptor,
    is_ranged: bool,
    checksum: Checksum,
    keys_info: Vec<DescKeyInfo>,
    network: Network,
    rescan_policy: RescanSince,

    gap_limit: u32,
    initial_import_size: u32,
    max_funded_index: Option<u32>,
    max_imported_index: Option<u32>,
    done_initial_import: bool,

    // Used for optimized derivation for simple p2*pkh descriptors.
    // Not available for more complex descriptor types.
    optimized_xpub: Option<XyzPubKey>,
}

impl Wallet {
    pub fn from_descriptor(
        desc: ExtendedDescriptor,
        network: Network,
        gap_limit: u32,
        initial_import_size: u32,
        rescan_policy: RescanSince,
    ) -> Result<Self> {
        ensure!(
            desc.address(network).is_some(),
            "Descriptor does not have address representation: `{}`",
            desc
        );

        let checksum = Checksum::from(&desc);
        let keys_info = DescKeyInfo::extract(&desc, network)?;
        let is_ranged = keys_info.iter().any(|x| x.is_ranged);
        let optimized_xpub = XyzPubKey::try_from_desc(&desc);

        Ok(Self {
            desc,
            checksum,
            keys_info,
            is_ranged,
            network,
            gap_limit,
            // setting initial_import_size < gap_limit makes no sense, the user probably meant to increase both
            initial_import_size: initial_import_size.max(gap_limit),
            rescan_policy,
            done_initial_import: false,
            max_funded_index: None,
            max_imported_index: None,
            optimized_xpub,
        })
    }

    pub fn from_bare_xpub(
        xpub: XyzPubKey,
        network: Network,
        gap_limit: u32,
        initial_import_size: u32,
        rescan_policy: RescanSince,
    ) -> Result<Self> {
        Self::from_descriptor(
            xpub.as_descriptor([][..].into()),
            network,
            gap_limit,
            initial_import_size,
            rescan_policy,
        )
    }

    pub fn from_xpub(
        xpub: XyzPubKey,
        network: Network,
        gap_limit: u32,
        initial_import_size: u32,
        rescan_policy: RescanSince,
    ) -> Result<Vec<Self>> {
        Ok(vec![
            // external chain (receive)
            Self::from_descriptor(
                xpub.as_descriptor([0.into()][..].into()),
                network,
                gap_limit,
                initial_import_size,
                rescan_policy,
            )?,
            // internal chain (change)
            Self::from_descriptor(
                xpub.as_descriptor([1.into()][..].into()),
                network,
                gap_limit,
                initial_import_size,
                rescan_policy,
            )?,
        ])
    }

    /// Derives the specified child key
    ///
    /// Panics if given a hardened child number
    pub fn derive(&self, index: u32) -> ExtendedDescriptor {
        self.desc
            .derive(ChildNumber::from_normal_idx(index).unwrap())
    }

    /// Returns the maximum index that needs to be watched
    fn watch_index(&self) -> u32 {
        if !self.is_ranged {
            return 0;
        }

        let chunk_size = if self.done_initial_import {
            self.gap_limit
        } else {
            self.initial_import_size
        };

        self.max_funded_index
            .map_or(chunk_size - 1, |max| max + chunk_size)
    }

    fn make_imports(
        &self,
        start_index: u32,
        end_index: u32,
        rescan: bool,
    ) -> Vec<(Address, RescanSince, String)> {
        let rescan_since = if rescan {
            self.rescan_policy
        } else {
            RescanSince::Now
        };

        (start_index..=end_index)
            .map(|index| {
                let address = self.derive_address(index);
                let origin = KeyOrigin::Descriptor(self.checksum.clone(), index);
                (address, rescan_since, origin.to_label())
            })
            .collect()
    }

    pub fn derive_address(&self, index: u32) -> Address {
        if let Some(optimized_xpub) = &self.optimized_xpub {
            // Derive simple p2*pkh descriptors using the extended pubkey directly, which
            // is *significantly* faster compared to invoking the full descriptor mechanism.
            optimized_xpub.derive_address(index, self.network)
        } else {
            self.derive(index)
                .address(self.network)
                .expect("constructed Wallet must have address representation")
        }
    }

    pub fn get_next_index(&self) -> u32 {
        if self.is_ranged {
            self.max_funded_index
                .map_or(0, |max_funded_index| max_funded_index + 1)
        } else {
            0
        }
    }

    pub fn is_valid_index(&self, index: u32) -> bool {
        if self.is_ranged {
            // non-hardended derivation only
            index & (1 << 31) == 0
        } else {
            index == 0
        }
    }

    pub fn find_gap(&self, store: &MemoryStore) -> Option<usize> {
        // return None if this wallet has no history at all
        let max_funded_index = self.max_funded_index?;

        Some(if self.is_ranged {
            (0..=max_funded_index)
                .map(|derivation_index| self.derive_address(derivation_index))
                .fold((0, 0), |(curr_gap, max_gap), address| {
                    if store.has_history(&address.into()) {
                        (0, curr_gap.max(max_gap))
                    } else {
                        (curr_gap + 1, max_gap)
                    }
                })
                .1
        } else {
            0
        })
    }

    /// Get the bip32 origins of the public keys used at the provided index
    pub fn bip32_origins(&self, index: u32) -> Vec<Bip32Origin> {
        self.keys_info
            .iter()
            .map(|i| {
                if i.is_ranged {
                    i.bip32_origin.child(index.into())
                } else {
                    i.bip32_origin.clone()
                }
            })
            .collect()
    }
}

fn batch_import(rpc: &RpcClient, import_reqs: Vec<(Address, RescanSince, String)>) -> Result<()> {
    // XXX use importmulti with ranged descriptors? the key derivation info won't be
    //     directly available on `listtransactions` and would require an additional rpc all.

    let results = rpc.import_multi(
        &import_reqs
            .iter()
            .map(|(address, rescan, label)| {
                trace!("importing {} as {}", address, label,);

                ImportMultiRequest {
                    label: Some(&label),
                    watchonly: Some(true),
                    timestamp: rescan.into(),
                    script_pubkey: Some(ImportMultiRequestScriptPubkey::Address(&address)),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>(),
        None,
    )?;

    for (i, result) in results.iter().enumerate() {
        if !result.success {
            let req = import_reqs.get(i).unwrap(); // should not fail unless bitcoind is messing with us
            bail!("import for {:?} failed: {:?}", req, result);
        } else if !result.warnings.is_empty() {
            debug!("import succeed with warnings: {:?}", result);
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyOrigin {
    Descriptor(Checksum, u32),
    Standalone,
}

impl_string_serializer!(
    KeyOrigin,
    origin,
    match origin {
        KeyOrigin::Standalone => "standalone".into(),
        KeyOrigin::Descriptor(checksum, index) => {
            format!("{}/{}", checksum, index)
        }
    }
);

impl KeyOrigin {
    pub fn to_label(&self) -> String {
        match self {
            KeyOrigin::Descriptor(checksum, index) => {
                format!("{}/{}/{}", LABEL_PREFIX, checksum, index)
            }
            KeyOrigin::Standalone => LABEL_PREFIX.into(),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(3, '/').collect();
        match (parts.get(0), parts.get(1), parts.get(2)) {
            (Some(&LABEL_PREFIX), Some(parent), Some(index)) => Some(KeyOrigin::Descriptor(
                parent.parse().ok()?,
                index.parse().ok()?,
            )),
            (Some(&LABEL_PREFIX), None, None) => Some(KeyOrigin::Standalone),
            _ => None,
        }
    }

    pub fn is_standalone(origin: &KeyOrigin) -> bool {
        match origin {
            KeyOrigin::Standalone => true,
            KeyOrigin::Descriptor(..) => false,
        }
    }
}

// show a specialzied error message for unsupported `listlabels` (added in Bitcoin Core 0.17.0)
fn labels_error(error: rpc::Error) -> bitcoincore_rpc::Error {
    if let rpc::Error::JsonRpc(rpc::jsonrpc::Error::Rpc(ref e)) = error {
        // Method not found
        if e.code == -32601 {
            warn!("Your bitcoind node appears to be too old to support the labels API, which bwt relies on. \
                  Please upgrade your node. v0.19.0 is highly recommended, v0.17.0 is sufficient.");
        }
    }
    error
}

use serde::ser::SerializeStruct;

impl Serialize for Wallet {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let desc_str = format!("{}#{}", self.desc, self.checksum);
        let bip32_origins: Vec<_> = self.keys_info.iter().map(|i| &i.bip32_origin).collect();

        let mut rgb = serializer.serialize_struct("Wallet", 3)?;

        rgb.serialize_field("desc", &desc_str)?;
        rgb.serialize_field("network", &self.network)?;
        rgb.serialize_field("is_ranged", &self.is_ranged)?;
        rgb.serialize_field("bip32_origins", &bip32_origins)?;
        rgb.serialize_field("rescan_policy", &self.rescan_policy)?;
        rgb.serialize_field("done_initial_import", &self.done_initial_import)?;
        rgb.serialize_field("max_funded_index", &self.max_funded_index)?;
        rgb.serialize_field("max_imported_index", &self.max_imported_index)?;
        rgb.serialize_field("satisfaction_weight", &self.desc.max_satisfaction_weight())?;

        if self.is_ranged {
            rgb.serialize_field("gap_limit", &self.gap_limit)?;
            rgb.serialize_field("initial_import_size", &self.initial_import_size)?;
        }

        rgb.end()
    }
}
