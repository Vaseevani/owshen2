use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use bindings::owshen::{SentFilter, SpendFilter};
use ethers::{contract::ContractInstance, prelude::*, types::ValueOrArray};
use tokio::time::timeout;

use crate::{
    apis::{GetEventsResponse, GetHandShakeResponse, GetPeersResponse},
    config::{Network, NetworkManager, NodeManager, Peer, TokenInfo},
};

impl NodeManager {
    pub fn add_peer(&mut self, peer: Peer) {
        if let Some(ext_addr) = self.external_addr.clone() {
            if peer.addr == ext_addr {
                return;
            }
        }

        if !self.peers.contains(&peer) {
            self.peers.push(peer);
        }
    }

    pub fn get_peers(&self) -> Vec<Peer> {
        self.peers.clone()
    }

    pub fn remove_peer(&mut self, peer: Peer) {
        self.peers.retain(|p| p.addr != peer.addr);
    }

    fn update_peer(&mut self, peer: Peer) {
        self.remove_peer(peer.clone());
        self.add_peer(peer);
    }

    pub async fn sync_with_peers(&mut self) -> Result<(), eyre::Report> {
        let mut elected_peer: Option<Peer> = None;
        let mut max_length: u64 = 0;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()?;

        for mut peer in self.get_peers() {
            let mut url = format!(
                "http://{}/handshake?is_client={}",
                peer.addr, self.is_client
            );
            if !self.is_client {
                url = format!(
                    "{}&addr={}",
                    url,
                    self.external_addr
                        .clone()
                        .ok_or(eyre::eyre!("Caller not a node!"))?
                );
            }
            let resp = client.get(&url).send().await;

            if let Ok(resp) = resp {
                if resp.status().is_success() {
                    let body = resp.text().await;
                    if let Ok(body) = body {
                        let handshake: GetHandShakeResponse = serde_json::from_str(&body)?;
                        log::info!(
                            "Synced with peer: {} - {}",
                            url,
                            handshake.current_block_number
                        );
                        peer.current_block = handshake.current_block_number;
                        self.update_peer(peer.clone());

                        if handshake.current_block_number >= max_length {
                            elected_peer = Some(peer.clone());
                            max_length = handshake.current_block_number;
                        }

                        self._add_batch_peer_peers(peer.clone()).await?;
                    } else {
                        log::error!("Failed to parse response from peer: {}", url);
                        self.remove_peer(peer.clone());
                    }
                } else {
                    log::error!("Failed to sync with peer: {}", url);
                    self.remove_peer(peer.clone());
                }
            } else {
                log::error!("Failed to handshake with peer: {}", url);
                self.remove_peer(peer.clone());
            }
        }
        if let Some(elected_peer) = elected_peer {
            log::info!("Elected peer: {}", elected_peer.addr);
            self.elected_peer = Some(elected_peer);
        }

        log::info!("Synced with peers: {}", self.get_peers().len());

        Ok(())
    }

    async fn _add_batch_peer_peers(&mut self, peer: Peer) -> Result<(), eyre::Report> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()?;

        let url = format!("http://{}/get-peers", peer.addr);
        let resp = client.get(&url).send().await;

        if let Ok(resp) = resp {
            if resp.status().is_success() {
                let body = resp.text().await;
                if let Ok(body) = body {
                    let peers: GetPeersResponse = serde_json::from_str(&body)?;
                    for p in peers.peers {
                        self.add_peer(p);
                    }
                } else {
                    log::error!("Failed to parse response from peer: {}", url);
                    self.remove_peer(peer);
                }
            } else {
                log::error!("Failed to get peers with peer: {}", url);
                self.remove_peer(peer);
            }
        } else {
            log::error!("Failed to get peers with peer: {}", url);
            self.remove_peer(peer);
        }
        Ok(())
    }

    pub fn set_provider_network(&mut self, provider_network: Network) {
        self.network = Some(provider_network);
    }

    pub fn get_provider_network(&self) -> Option<Network> {
        self.network.clone()
    }

    pub async fn get_events_from_elected_peer(
        &self,
        mut from_spend: usize,
        mut from_sent: usize,
    ) -> Result<(Vec<SpendFilter>, Vec<SentFilter>, u64), eyre::Report> {
        if let Some(elected_peer) = self.elected_peer.clone() {
            let step: usize = 256;
            let mut spend_events = Vec::new();
            let mut sent_events = Vec::new();

            loop {
                let url = format!(
                    "http://{}/events?from_spend={}&from_sent={}&length={}",
                    elected_peer.addr, from_spend, from_sent, step
                );

                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(1))
                    .build()?;
                let resp = client.get(&url).send().await;

                if let Ok(resp) = resp {
                    if resp.status().is_success() {
                        let body = resp.text().await;
                        if let Ok(body) = body {
                            let json_resp: GetEventsResponse = serde_json::from_str(&body)?;
                            if json_resp.spend_events.is_empty() && json_resp.sent_events.is_empty()
                            {
                                break;
                            }

                            spend_events.extend(json_resp.spend_events);
                            sent_events.extend(json_resp.sent_events);

                            from_spend += step;
                            from_sent += step;
                        } else {
                            log::error!("Failed to parse response from peer: {}", url);
                        }
                    } else {
                        log::error!("Failed to get spend events with peer: {}", url);
                    }
                } else {
                    log::error!("Failed to get spend events with peer: {}", url);
                    break;
                }
            }
            Ok((spend_events, sent_events, elected_peer.current_block))
        } else {
            log::error!("Elected peer is not set");
            Ok((vec![], vec![], 0))
        }
    }

    pub async fn get_spend_events(&self, mut from: u64, to: u64) -> Vec<SpendFilter> {
        let network = self.get_provider_network();
        if let Some(network) = network {
            let contract: ContractInstance<Arc<Provider<Http>>, _> = Contract::new(
                network.config.owshen_contract_address,
                network.config.owshen_contract_abi,
                network.provider.clone(),
            );

            let mut step = 1024;
            let mut events = Vec::new();

            while from < to {
                log::info!("{} {}", from, to);
                if let Some(new_spent_events) = timeout(std::time::Duration::from_secs(10), async {
                    contract
                        .event::<SpendFilter>()
                        .from_block(from)
                        .to_block(from + step)
                        .address(ValueOrArray::Value(contract.address()))
                        .query()
                        .await
                })
                .await
                .map(|r| r.ok())
                .ok()
                .unwrap_or_default()
                {
                    events.extend(new_spent_events);
                    from += step;
                    if step < 1024 {
                        step = step * 2;
                    }
                } else {
                    step = step / 2;
                }
            }
            events
        } else {
            log::error!("Provider is not set");
            vec![]
        }
    }

    pub async fn get_sent_events(&self, mut from: u64, to: u64) -> Vec<SentFilter> {
        let network = self.get_provider_network();
        if let Some(network) = network {
            let contract: ContractInstance<Arc<Provider<Http>>, _> = Contract::new(
                network.config.owshen_contract_address,
                network.config.owshen_contract_abi,
                network.provider.clone(),
            );

            let mut step = 1024;
            let mut events = Vec::new();

            while from < to {
                if let Some(new_sent_events) = timeout(std::time::Duration::from_secs(10), async {
                    contract
                        .event::<SentFilter>()
                        .from_block(from)
                        .to_block(from + step)
                        .address(ValueOrArray::Value(contract.address()))
                        .query()
                        .await
                })
                .await
                .map(|r| r.ok())
                .ok()
                .unwrap_or_default()
                {
                    events.extend(new_sent_events);
                    from += step;
                    if step < 1024 {
                        step = step * 2;
                    }
                } else {
                    step = step / 2;
                }
            }
            events
        } else {
            log::error!("Provider is not set");
            vec![]
        }
    }
}

impl NetworkManager {
    pub fn new() -> NetworkManager {
        let mut networks: HashMap<String, Vec<TokenInfo>> = HashMap::new();

        networks.insert(
            "Goerli".to_string(),
            vec![TokenInfo {
                token_address: H160::from_str("0xdD69DB25F6D620A7baD3023c5d32761D353D3De9")
                    .unwrap(),
                symbol: "WETH".to_string(),
            }],
        );

        NetworkManager { networks }
    }

    // pub fn set(&mut self, data: HashMap<String, Vec<TokenInfo>>, expand: bool) {
    //     if expand {
    //         self.networks.extend(data);
    //     } else {
    //         self.networks = data;
    //     }
    // }

    pub fn add_network(&mut self, network: String, token_info: Vec<TokenInfo>) {
        self.networks.insert(network, token_info);
    }

    // pub fn get(&self, network: &str) -> Option<&Vec<TokenInfo>> {
    //     self.networks.get(network)
    // }

    // pub fn has(&self, network: &str, symbol: &str) -> bool {
    //     self.get(network).map_or(false, |tokens| {
    //         tokens.iter().any(|token_info| token_info.symbol == symbol)
    //     })
    // }
}
