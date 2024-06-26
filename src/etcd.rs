// Copyright Rivtower Technologies LLC.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;

use color_eyre::{
    eyre::{eyre, OptionExt},
    Result,
};
use etcd_client::{Client, ConnectOptions, DeleteOptions, GetOptions, KeyValue as KV, PutOptions};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::service_register::{ServiceRegister, ServiceRegisterConfig};

pub type KeyValue = KV;

#[derive(Clone)]
pub struct Etcd {
    pub client: Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EtcdConfig {
    pub endpoints: Vec<String>,
    pub timeout: u64,
    pub keep_alive: u64,
}

impl Default for EtcdConfig {
    fn default() -> Self {
        Self {
            endpoints: vec!["http://127.0.0.1:2379".to_owned()],
            timeout: 2000,
            keep_alive: 300,
        }
    }
}

impl Etcd {
    pub async fn new(config: &EtcdConfig) -> Result<Self> {
        let client = Client::connect(
            &config.endpoints,
            Some(
                ConnectOptions::new()
                    .with_connect_timeout(Duration::from_millis(config.timeout))
                    .with_keep_alive(
                        Duration::from_secs(config.keep_alive),
                        Duration::from_millis(config.timeout),
                    )
                    .with_keep_alive_while_idle(true)
                    .with_timeout(Duration::from_millis(config.timeout)),
            ),
        )
        .await
        .map_err(|e| eyre!("etcd connect failed: {e}"))?;
        Ok(Self { client })
    }

    pub async fn put(
        &self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
        ttl: i64,
    ) -> Result<Option<KeyValue>> {
        let mut client = self.client.clone();
        let option = if ttl == 0 {
            PutOptions::new().with_prev_key()
        } else {
            let lease = client
                .lease_grant(ttl, None)
                .await
                .map_err(|e| eyre!("etcd lease_grant failed: {e}"))?;
            PutOptions::new().with_lease(lease.id()).with_prev_key()
        };
        let put_rsp = client
            .put(key, value, Some(option))
            .await
            .map_err(|e| eyre!("etcd put failed: {e}"))?;
        Ok(put_rsp.prev_key().cloned())
    }

    pub async fn get(&self, key: impl Into<Vec<u8>>) -> Result<KeyValue> {
        self.client
            .to_owned()
            .get(key, Some(GetOptions::new().with_limit(1)))
            .await
            .map_err(|e| eyre!("etcd get failed: {e}"))?
            .kvs()
            .first()
            .cloned()
            .ok_or_eyre("data not found")
    }

    pub async fn get_with_prefix(&self, key: impl Into<Vec<u8>>) -> Result<Vec<KeyValue>> {
        Ok(self
            .client
            .to_owned()
            .get(key, Some(GetOptions::new().with_prefix()))
            .await
            .map_err(|e| eyre!("etcd get failed: {e}"))?
            .kvs()
            .to_vec())
    }

    pub async fn delete(&self, key: impl Into<Vec<u8>>) -> Result<i64> {
        Ok(self
            .client
            .to_owned()
            .delete(key, None)
            .await
            .map_err(|e| eyre!("etcd delete failed: {e}"))?
            .deleted())
    }

    pub async fn delete_with_prefix(&self, key: impl Into<Vec<u8>>) -> Result<i64> {
        Ok(self
            .client
            .to_owned()
            .delete(key, Some(DeleteOptions::new().with_prefix()))
            .await
            .map_err(|e| eyre!("etcd delete failed: {e}"))?
            .deleted())
    }

    pub async fn touch(&self, key: impl Into<Vec<u8>>) -> Result<()> {
        let mut client = self.client.clone();
        let lease = client
            .get(key, Some(GetOptions::new().with_limit(1)))
            .await
            .map_err(|e| eyre!("etcd get failed: {e}"))?
            .kvs()
            .first()
            .map(|kv| kv.lease())
            .unwrap_or(0);
        if lease != 0 {
            client
                .lease_keep_alive(lease)
                .await
                .map_err(|e| eyre!("etcd lease_keep_alive failed: {e}"))?;
        }
        Ok(())
    }

    pub async fn put_or_touch(&self, key: &str, value: impl Into<Vec<u8>>, ttl: i64) -> Result<()> {
        let mut client = self.client.clone();
        if let Some(prev) = client
            .get(key, Some(GetOptions::new().with_limit(1)))
            .await
            .map_err(|e| eyre!("etcd get failed: {e}"))?
            .kvs()
            .first()
        {
            client
                .lease_keep_alive(prev.lease())
                .await
                .map_err(|e| eyre!("etcd lease_keep_alive failed: {e}"))?;
        } else {
            self.put(key, value, ttl).await?;
        }
        Ok(())
    }

    pub async fn service_register(
        &self,
        service_name: &str,
        config: ServiceRegisterConfig,
    ) -> Result<()> {
        self.keep_service_register(service_name, config).await
    }
}

impl ServiceRegister for Etcd {
    async fn keep_service_register(
        &self,
        service_name: &str,
        config: ServiceRegisterConfig,
    ) -> Result<()> {
        info!("keep_service_register: {config:?}");
        let mut keep_alive_interval =
            tokio::time::interval(tokio::time::Duration::from_secs((config.ttl / 2) as u64));

        let etcd = self.clone();
        let service_name = service_name.to_owned();
        tokio::spawn(async move {
            loop {
                keep_alive_interval.tick().await;
                let tags = config.tags.clone();
                let service_name = service_name.clone();

                if let Err(e) = etcd
                    .put_or_touch(
                        &format!(
                            "traefik/http/services/{}/loadbalancer/servers/{}/url",
                            service_name, service_name
                        ),
                        config.url.clone(),
                        config.ttl,
                    )
                    .await
                {
                    error!("keep_service_register failed: {:?}", e);
                }
                if let Err(e) = etcd
                    .put_or_touch(
                        &format!("traefik/http/routers/{}/service", service_name),
                        service_name,
                        config.ttl,
                    )
                    .await
                {
                    error!("keep_service_register failed: {:?}", e);
                }
                for tag in tags {
                    let (key, value) = tag.split_once('=').unwrap_or_default();
                    if let Err(e) = etcd.put_or_touch(key, value, config.ttl).await {
                        error!("keep_service_register failed: {:?}", e);
                    }
                }
            }
        });
        Ok(())
    }
}
