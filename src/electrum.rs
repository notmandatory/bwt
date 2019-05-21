use std::cmp;
use std::sync::Arc;

use bitcoin_hashes::{hex::FromHex, sha256};
use jsonrpc_tcp_server::jsonrpc_core::{
    Error as RpcServerError, IoHandler, Params, Result as RpcResult,
};
use jsonrpc_tcp_server::ServerBuilder;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::query::Query;

pub struct ElectrumServer {
    query: Arc<Query>,
}

impl ElectrumServer {
    const MAX_HEADERS: u32 = 2016;

    pub fn new(query: Arc<Query>) -> Self {
        Self { query }
    }

    pub fn start(self) -> Result<()> {
        let server = Arc::new(self);
        let mut io = IoHandler::default();

        io.add_method("blockchain.block.header", {
            let server = Arc::clone(&server);
            move |params| wrap(server.blockchain_block_header(params))
        });

        io.add_method("blockchain.block.headers", {
            let server = Arc::clone(&server);
            move |params| wrap(server.blockchain_block_headers(params))
        });

        io.add_method("blockchain.estimatefee", {
            let server = Arc::clone(&server);
            move |params| wrap(server.blockchain_estimatefee(params))
        });

        io.add_method("blockchain.relayfee", {
            let server = Arc::clone(&server);
            move |params| wrap(server.blockchain_relayfee(params))
        });

        io.add_method("blockchain.scripthash.get_history", {
            let server = Arc::clone(&server);
            move |params| wrap(server.blockchain_scripthash_get_history(params))
        });

        let server = ServerBuilder::new(io)
            .start(&"127.0.0.1:9009".parse().unwrap())
            .expect("failed starting server");

        server.wait();

        Ok(())
    }

    fn blockchain_block_header(&self, p: Params) -> Result<String> {
        let (height, cp_height): (u32, Option<u32>) = pad_params(p, 2).parse()?;
        // FIXME support cp_height

        Ok(self.query.get_header(height)?)
    }

    fn blockchain_block_headers(&self, p: Params) -> Result<Value> {
        let (start_height, count, cp_height): (u32, u32, Option<u32>) = pad_params(p, 3).parse()?;
        // FIXME support cp_height

        let count = cmp::min(count, Self::MAX_HEADERS);
        let heights: Vec<u32> = (start_height..(start_height + count)).collect();
        let headers = self.query.get_headers(&heights)?;

        Ok(json!({
            "count": headers.len(),
            "hex": headers.join(""),
            "max": Self::MAX_HEADERS,
        }))
    }

    fn blockchain_estimatefee(&self, p: Params) -> Result<f32> {
        let (target,): (u16,) = p.parse()?;
        // convert from sat/b to BTC/kB
        Ok(self
            .query
            .estimate_fee(target)?
            .map_or(-1.0, |rate| rate / 100_000f32))
    }

    fn blockchain_relayfee(&self, _params: Params) -> Result<f32> {
        Ok(1.0) // TODO read from bitcoind
    }

    fn blockchain_scripthash_get_history(&self, p: Params) -> Result<Vec<Value>> {
        let (scripthash,): (sha256::Hash,) = p.parse()?;

        Ok(self
            .query
            .get_history(&scripthash)?
            .iter()
            .map(|tx| json!({ "height": tx.height, "tx_hash": tx.txid }))
            .collect())
    }
}

fn wrap<T: Serialize>(res: Result<T>) -> RpcResult<Value> {
    res.and_then(|val| serde_json::to_value(val).map_err(Error::from))
        .map_err(|e| {
            warn!("request failed: {:?}", e);
            RpcServerError::invalid_params(e.to_string())
        })
}

/*
fn parse<T: DeserializeOwned>(p: Params) -> RpcResult<T> {
    p.parse().map_err(|e| {
        warn!("parse failed: {:?}", e);
        RpcServerError::invalid_params(e.to_string())
    })
}
*/

fn pad_params(mut params: Params, n: usize) -> Params {
    if let Params::Array(ref mut values) = params {
        while values.len() < n {
            values.push(Value::Null);
        }
    } // passing a non-array is a noop
    params
}