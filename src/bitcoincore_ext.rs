use bitcoin::Amount;
use bitcoincore_rpc::{Client, Error, RpcApi};

// Extensions for rust-bitcoincore-rpc

pub trait RpcApiExt: RpcApi {
    // pending https://github.com/rust-bitcoin/rust-bitcoincore-rpc/pull/114
    fn get_mempool_entry(&self, txid: &bitcoin::Txid) -> Result<GetMempoolEntryResult, Error> {
        self.call("getmempoolentry", &[json!(txid)])
    }
}

impl RpcApiExt for Client {}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct GetMempoolEntryResult {
    /// Virtual transaction size as defined in BIP 141. This is different from actual serialized
    /// size for witness transactions as witness data is discounted.
    pub vsize: u64,
    /// Transaction weight as defined in BIP 141. Added in Core v0.19.0.
    pub weight: Option<u64>,
    /// Local time transaction entered pool in seconds since 1 Jan 1970 GMT
    pub time: u64,
    /// Block height when transaction entered pool
    pub height: u64,
    /// Number of in-mempool descendant transactions (including this one)
    #[serde(rename = "descendantcount")]
    pub descendant_count: u64,
    /// Virtual transaction size of in-mempool descendants (including this one)
    #[serde(rename = "descendantsize")]
    pub descendant_size: u64,
    /// Number of in-mempool ancestor transactions (including this one)
    #[serde(rename = "ancestorcount")]
    pub ancestor_count: u64,
    /// Virtual transaction size of in-mempool ancestors (including this one)
    #[serde(rename = "ancestorsize")]
    pub ancestor_size: u64,
    /// Hash of serialized transaction, including witness data
    pub wtxid: bitcoin::Txid,
    /// Fee information
    pub fees: GetMempoolEntryResultFees,
    /// Unconfirmed transactions used as inputs for this transaction
    pub depends: Vec<bitcoin::Txid>,
    /// Unconfirmed transactions spending outputs from this transaction
    #[serde(rename = "spentby")]
    pub spent_by: Vec<bitcoin::Txid>,
    /// Whether this transaction could be replaced due to BIP125 (replace-by-fee)
    #[serde(rename = "bip125-replaceable")]
    pub bip125_replaceable: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct GetMempoolEntryResultFees {
    /// Transaction fee in BTC
    #[serde(with = "bitcoin::util::amount::serde::as_btc")]
    pub base: Amount,
    /// Transaction fee with fee deltas used for mining priority in BTC
    #[serde(with = "bitcoin::util::amount::serde::as_btc")]
    pub modified: Amount,
    /// Modified fees (see above) of in-mempool ancestors (including this one) in BTC
    #[serde(with = "bitcoin::util::amount::serde::as_btc")]
    pub ancestor: Amount,
    /// Modified fees (see above) of in-mempool descendants (including this one) in BTC
    #[serde(with = "bitcoin::util::amount::serde::as_btc")]
    pub descendant: Amount,
}
