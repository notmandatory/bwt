use std::sync::{mpsc, Arc, RwLock};
use std::{net, thread, time};

use bitcoincore_rpc::{self as rpc, Client as RpcClient, RpcApi};

use crate::util::{banner, debounce_sender};
use crate::{Config, Indexer, Query, Result, WalletWatcher};

#[cfg(feature = "electrum")]
use crate::electrum::ElectrumServer;
#[cfg(feature = "http")]
use crate::http::HttpServer;
#[cfg(unix)]
use crate::listener;
#[cfg(feature = "webhooks")]
use crate::webhooks::WebHookNotifier;

const DEBOUNCE_SEC: u64 = 7;

pub struct App {
    config: Config,
    indexer: Arc<RwLock<Indexer>>,
    query: Arc<Query>,
    sync_chan: (mpsc::Sender<()>, mpsc::Receiver<()>),

    #[cfg(feature = "electrum")]
    electrum: ElectrumServer,
    #[cfg(feature = "http")]
    http: HttpServer,
    #[cfg(feature = "webhooks")]
    webhook: Option<WebHookNotifier>,
}

impl App {
    pub fn boot(config: Config) -> Result<Self> {
        debug!("{:?}", config);

        let watcher = WalletWatcher::from_config(
            &config.descriptors[..],
            &config.xpubs[..],
            &config.bare_xpubs[..],
            config.network,
            config.gap_limit,
            config.initial_import_size,
        )?;

        let rpc = Arc::new(RpcClient::new(
            config.bitcoind_url(),
            config.bitcoind_auth()?,
        )?);
        let indexer = Arc::new(RwLock::new(Indexer::new(rpc.clone(), watcher)));
        let query = Arc::new(Query::new((&config).into(), rpc.clone(), indexer.clone()));

        if let Some(bitcoind_wallet) = &config.bitcoind_wallet {
            load_wallet(&rpc, bitcoind_wallet)?;
        }

        wait_bitcoind(&rpc)?;

        if config.startup_banner {
            println!("{}", banner::get_welcome_banner(&query, false)?);
        }

        // do an initial sync without keeping track of updates
        indexer.write().unwrap().initial_sync()?;

        let (sync_tx, sync_rx) = mpsc::channel();
        // debounce sync message rate to avoid excessive indexing when bitcoind catches up
        let debounced_sync_tx = debounce_sender(sync_tx.clone(), DEBOUNCE_SEC);

        #[cfg(feature = "electrum")]
        let electrum = ElectrumServer::start(
            config.electrum_rpc_addr(),
            config.electrum_skip_merkle,
            query.clone(),
        );

        #[cfg(feature = "http")]
        let http = HttpServer::start(
            config.http_server_addr,
            config.http_cors.clone(),
            query.clone(),
            debounced_sync_tx.clone(),
        );

        #[cfg(unix)]
        {
            if let Some(listener_path) = &config.unix_listener_path {
                listener::start(listener_path.clone(), debounced_sync_tx);
            }
        }

        #[cfg(feature = "webhooks")]
        let webhook = config.webhook_urls.clone().map(WebHookNotifier::start);

        Ok(App {
            config,
            indexer,
            query,
            sync_chan: (sync_tx, sync_rx),
            #[cfg(feature = "electrum")]
            electrum,
            #[cfg(feature = "http")]
            http,
            #[cfg(feature = "webhooks")]
            webhook,
        })
    }

    /// Start a sync loop blocking the current thread
    pub fn sync(&self, shutdown_rx: Option<mpsc::Receiver<()>>) {
        let shutdown_rx = shutdown_rx
            .map(|rx| self.pipe_shutdown(rx))
            .or_else(|| self.default_shutdown_signal());

        loop {
            if let Some(shutdown_rx) = &shutdown_rx {
                match shutdown_rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => (),
                    Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            #[allow(clippy::option_map_unit_fn)]
            match self.indexer.write().unwrap().sync() {
                Ok(updates) if !updates.is_empty() => {
                    #[cfg(feature = "electrum")]
                    self.electrum.send_updates(&updates);

                    #[cfg(feature = "http")]
                    self.http.send_updates(&updates);

                    #[cfg(feature = "webhooks")]
                    self.webhook
                        .as_ref()
                        .map(|webhook| webhook.send_updates(&updates));
                }
                Ok(_) => (), // no updates
                Err(e) => warn!("error while updating index: {:#?}", e),
            }

            // wait for poll_interval seconds, or until we receive a sync notification message,
            // or until the shutdown signal is emitted
            self.sync_chan
                .1
                .recv_timeout(self.config.poll_interval)
                .ok();
        }
    }

    /// Get the `Query` instance
    pub fn query(&self) -> Arc<Query> {
        self.query.clone()
    }

    #[cfg(feature = "electrum")]
    pub fn electrum_addr(&self) -> net::SocketAddr {
        self.electrum.addr()
    }

    #[cfg(feature = "http")]
    pub fn http_addr(&self) -> net::SocketAddr {
        self.http.addr()
    }

    // Pipe the shutdown receiver `rx` to trigger `sync_tx`. This is needed to start the next
    // sync loop run immediately, which will then process the shutdown signal itself. Without
    // this, the shutdown signal will only be noticed after a delay.
    fn pipe_shutdown(&self, rx: mpsc::Receiver<()>) -> mpsc::Receiver<()> {
        let sync_tx = self.sync_chan.0.clone();
        let (c_tx, c_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            rx.recv().ok();
            c_tx.send(()).unwrap();
            sync_tx.send(()).unwrap();
        });
        c_rx
    }

    #[cfg(all(unix, feature = "signal_hook"))]
    fn default_shutdown_signal(&self) -> Option<mpsc::Receiver<()>> {
        use signal_hook::iterator::Signals;

        let signals = Signals::new(&[signal_hook::SIGINT, signal_hook::SIGTERM]).unwrap();
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        let sync_tx = self.sync_chan.0.clone();

        thread::spawn(move || {
            let signal = signals.into_iter().next().unwrap();
            trace!("received shutdown signal {}", signal);
            shutdown_tx.send(()).unwrap();
            // Need to also trigger `sync_tx`, see rational above
            sync_tx.send(()).unwrap();
        });

        Some(shutdown_rx)
    }

    #[cfg(not(all(unix, feature = "signal_hook")))]
    fn default_shutdown_signal(&self) -> Option<mpsc::Receiver<()>> {
        None
    }
}

// Load the specified wallet, ignore "wallet is already loaded" errors
fn load_wallet(rpc: &RpcClient, name: &str) -> Result<()> {
    match rpc.load_wallet(name) {
        Ok(_) => Ok(()),
        Err(rpc::Error::JsonRpc(rpc::jsonrpc::Error::Rpc(ref e))) if e.code == -4 => Ok(()),
        Err(e) => bail!(e),
    }
}

// wait for bitcoind to sync and finish rescanning
fn wait_bitcoind(rpc: &RpcClient) -> Result<()> {
    let netinfo = rpc.get_network_info()?;
    let mut bcinfo = rpc.get_blockchain_info()?;
    info!(
        "bwt v{} connected to {} on {}, protocolversion={}, bestblock={}",
        crate::BWT_VERSION,
        netinfo.subversion,
        bcinfo.chain,
        netinfo.protocol_version,
        bcinfo.best_block_hash
    );

    trace!("{:?}", netinfo);
    trace!("{:?}", bcinfo);

    let dur = time::Duration::from_secs(15);
    while (bcinfo.chain != "regtest" && bcinfo.initial_block_download)
        || bcinfo.blocks < bcinfo.headers
    {
        info!(
            "waiting for bitcoind to sync [{}/{} blocks, progress={:.1}%, initialblockdownload={}]",
            bcinfo.blocks,
            bcinfo.headers,
            bcinfo.verification_progress * 100.0,
            bcinfo.initial_block_download
        );
        thread::sleep(dur);
        bcinfo = rpc.get_blockchain_info()?;
    }
    loop {
        match check_scanning(rpc)? {
            ScanningResult::NotScanning => break,
            ScanningResult::Unsupported => {
                warn!("Your bitcoin node does not report the `scanning` status in `getwalletinfo`. It is recommended to upgrade to Bitcoin Core v0.19+ to enable this.");
                warn!("This is needed for bwt to wait for scanning to finish before starting up. Starting bwt while the node is scanning may lead to unexpected results. Continuing anyway...");
                break;
            }
            ScanningResult::Scanning(scanning) => {
                info!(
                    "waiting for bitcoind to finish scanning [done {:.1}%, running for {:?}]",
                    scanning.progress * 100f64,
                    time::Duration::from_secs(scanning.duration)
                );
            }
        };
        thread::sleep(dur);
    }

    Ok(())
}

fn check_scanning(rpc: &RpcClient) -> Result<ScanningResult> {
    let mut wallet_info: serde_json::Value = rpc.call("getwalletinfo", &[])?;

    // the "rescanning" field is only supported as of Bitcoin Core v0.19
    let rescanning = some_or_ret!(
        wallet_info.get_mut("scanning"),
        Ok(ScanningResult::Unsupported)
    );

    Ok(if rescanning.as_bool() == Some(false) {
        ScanningResult::NotScanning
    } else {
        let details = serde_json::from_value(rescanning.take())?;
        ScanningResult::Scanning(details)
    })
}

enum ScanningResult {
    Scanning(ScanningDetails),
    NotScanning,
    Unsupported,
}
#[derive(Deserialize)]
struct ScanningDetails {
    duration: u64,
    progress: f64,
}
