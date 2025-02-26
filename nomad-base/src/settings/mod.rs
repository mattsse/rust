//! Settings and configuration for Nomad agents
//!
//! ## Introduction
//!
//! Nomad Agents have a shared core, which contains connection info for rpc,
//! relevant contract addresses on each chain, etc. In addition, each agent has
//! agent-specific settings. Be convention, we represent these as a base config
//! per-Home contract, and a "partial" config per agent. On bootup, the agent
//! loads the configuration, establishes RPC connections, and monitors each
//! configured chain.
//!
//! All agents share the [`Settings`] struct in this crate, and then define any
//! additional `Settings` in their own crate. By convention this is done in
//! `settings.rs` using the [`decl_settings!`] macro.
//!
//! ### Configuration
//!
//! Agents read settings from the config files and/or env.
//!
//! Config files are loaded from `rust/config/default` unless specified
//! otherwise. Currently deployment config directories are labeled by the
//! timestamp at which they were deployed
//!
//! Configuration key/value pairs are loaded in the following order, with later
//! sources taking precedence:
//!
//! 1. The config file specified by the `RUN_ENV` and `BASE_CONFIG`
//!    env vars. `$RUN_ENV/$BASE_CONFIG`
//! 2. The config file specified by the `RUN_ENV` env var and the
//!    agent's name. `$RUN_ENV/{agent}-partial.json`.
//!    E.g. `$RUN_ENV/updater-partial.json`
//! 3. Configuration env vars with the prefix `OPT_BASE` intended
//!    to be shared by multiple agents in the same environment
//!    E.g. `export OPT_BASE_REPLICAS_KOVAN_DOMAIN=3000`
//! 4. Configuration env vars with the prefix `OPT_{agent name}`
//!    intended to be used by a specific agent.
//!    E.g. `export OPT_KATHY_CHAT_TYPE="static message"`

use crate::{
    agent::AgentCore, CachingHome, CachingReplica, CommonIndexerVariants, CommonIndexers,
    ContractSync, ContractSyncMetrics, HomeIndexerVariants, HomeIndexers, Homes, NomadDB, Replicas,
};
use color_eyre::{eyre::bail, Report};
use config::{Config, ConfigError, Environment, File};
use ethers::prelude::AwsSigner;
use nomad_core::{db::DB, utils::HexString, Common, ContractLocator, Signers};
use nomad_ethereum::{make_home_indexer, make_replica_indexer};
use rusoto_core::{credential::EnvironmentProvider, HttpClient};
use rusoto_kms::KmsClient;
use serde::Deserialize;
use std::{collections::HashMap, env, sync::Arc};
use tracing::instrument;

/// Chain configuartion
pub mod chains;

pub use chains::{ChainConf, ChainSetup};

/// Tracing subscriber management
pub mod trace;

use crate::settings::trace::TracingConfig;

use once_cell::sync::OnceCell;

static KMS_CLIENT: OnceCell<KmsClient> = OnceCell::new();

/// Agent types
pub enum AgentType {
    /// Kathy
    Kathy,
    /// Updater
    Updater,
    /// Relayer
    Relayer,
    /// Processor
    Processor,
    /// Watcher
    Watcher,
}

/// Index data types and timelag settings
#[derive(serde::Deserialize, Debug, PartialEq, Clone)]
pub enum IndexDataTypes {
    /// Updates
    Updates,
    /// Updates and messages
    UpdatesAndMessages,
}

impl Default for IndexDataTypes {
    fn default() -> Self {
        Self::Updates
    }
}

/// Ethereum signer types
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SignerConf {
    /// A local hex key
    HexKey {
        /// Hex string of private key, without 0x prefix
        key: HexString<64>,
    },
    /// An AWS signer. Note that AWS credentials must be inserted into the env
    /// separately.
    Aws {
        /// The UUID identifying the AWS KMS Key
        id: String, // change to no _ so we can set by env
        /// The AWS region
        region: String,
    },
    #[serde(other)]
    /// Assume node will sign on RPC calls
    Node,
}

impl Default for SignerConf {
    fn default() -> Self {
        Self::Node
    }
}

impl SignerConf {
    /// Try to convert the ethereum signer to a local wallet
    #[instrument(err)]
    pub async fn try_into_signer(&self) -> Result<Signers, Report> {
        match self {
            SignerConf::HexKey { key } => Ok(Signers::Local(key.as_ref().parse()?)),
            SignerConf::Aws { id, region } => {
                let client = KMS_CLIENT.get_or_init(|| {
                    KmsClient::new_with_client(
                        rusoto_core::Client::new_with(
                            EnvironmentProvider::default(),
                            HttpClient::new().unwrap(),
                        ),
                        region.parse().expect("invalid region"),
                    )
                });

                let signer = AwsSigner::new(client, id, 0).await?;
                Ok(Signers::Aws(signer))
            }
            SignerConf::Node => bail!("Node signer"),
        }
    }
}

/// Home indexing settings
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IndexSettings {
    /// The height at which to start indexing the Home contract
    pub from: Option<String>,
    /// The number of blocks to query at once at which to start indexing the Home contract
    pub chunk: Option<String>,
    /// Data types to index
    #[serde(default)]
    pub data_types: IndexDataTypes,
    /// Whether or not to use timelag
    #[serde(default)]
    pub use_timelag: bool,
}

impl IndexSettings {
    /// Get the `from` setting
    pub fn from(&self) -> u32 {
        self.from
            .as_ref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or_default()
    }

    /// Get the `chunk_size` setting
    pub fn chunk_size(&self) -> u32 {
        self.chunk
            .as_ref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1999)
    }

    /// Get IndexDataTypes
    pub fn data_types(&self) -> IndexDataTypes {
        self.data_types.clone()
    }

    /// Get timelag on/off status
    pub fn timelag_on(&self) -> bool {
        self.use_timelag
    }
}

/// Settings. Usually this should be treated as a base config and used as
/// follows:
///
/// ```
/// use nomad_base::*;
/// use serde::Deserialize;
///
/// pub struct OtherSettings { /* anything */ };
///
/// #[derive(Debug, Deserialize)]
/// pub struct MySettings {
///     #[serde(flatten)]
///     base_settings: Settings,
///     #[serde(flatten)]
///     other_settings: (),
/// }
///
/// // Make sure to define MySettings::new()
/// impl MySettings {
///     fn new() -> Self {
///         unimplemented!()
///     }
/// }
/// ```
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// The path to use for the DB file
    pub db: String,
    /// Port to listen for prometheus scrape requests
    pub metrics: Option<String>,
    /// Settings for the home indexer
    #[serde(default)]
    pub index: IndexSettings,
    /// The home configuration
    pub home: ChainSetup,
    /// The replica configurations
    pub replicas: HashMap<String, ChainSetup>,
    /// The tracing configuration
    pub tracing: TracingConfig,
    /// Transaction signers
    pub signers: HashMap<String, SignerConf>,
}

impl Settings {
    /// Private to preserve linearity of AgentCore::from_settings -- creating an agent consumes the settings.
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            metrics: self.metrics.clone(),
            index: self.index.clone(),
            home: self.home.clone(),
            replicas: self.replicas.clone(),
            tracing: self.tracing.clone(),
            signers: self.signers.clone(),
        }
    }
}

impl Settings {
    /// Try to get a signer instance by name
    pub async fn get_signer(&self, name: &str) -> Option<Signers> {
        self.signers.get(name)?.try_into_signer().await.ok()
    }

    /// Set agent-specific index data types
    pub fn set_index_data_types(&mut self, data_types: IndexDataTypes) {
        self.index.data_types = data_types;
    }

    /// Set agent-specific timelag on/off
    pub fn set_use_timelag(&mut self, use_timelag: bool) {
        self.index.use_timelag = use_timelag;
    }

    /// Get optional indexing timelag enum for home
    pub fn home_timelag(&self) -> Option<u8> {
        if self.index.timelag_on() {
            Some(self.home.finality)
        } else {
            None
        }
    }

    /// Get optional indexing timelag for a replica
    pub fn replica_timelag(&self, replica_name: &str) -> Option<u8> {
        if self.index.timelag_on() {
            let replica_finality = self.replicas.get(replica_name).expect("!replica").finality;
            Some(replica_finality)
        } else {
            None
        }
    }

    /// Try to get a Homes object
    pub async fn try_home(&self) -> Result<Homes, Report> {
        let signer = self.get_signer(&self.home.name).await;
        let opt_home_timelag = self.home_timelag();
        self.home.try_into_home(signer, opt_home_timelag).await
    }

    /// Try to get a home ContractSync
    pub async fn try_home_contract_sync(
        &self,
        agent_name: &str,
        db: DB,
        metrics: ContractSyncMetrics,
    ) -> Result<ContractSync<HomeIndexers>, Report> {
        let finality = self.home.finality;
        let index_settings = self.index.clone();

        let indexer = Arc::new(self.try_home_indexer().await?);
        let home_name = &self.home.name;

        let nomad_db = NomadDB::new(&home_name, db);

        Ok(ContractSync::new(
            agent_name.to_owned(),
            home_name.to_owned(),
            nomad_db,
            indexer,
            index_settings,
            finality,
            metrics,
        ))
    }

    /// Try to get a CachingHome object
    pub async fn try_caching_home(
        &self,
        agent_name: &str,
        db: DB,
        metrics: ContractSyncMetrics,
    ) -> Result<CachingHome, Report> {
        let home = self.try_home().await?;
        let contract_sync = self
            .try_home_contract_sync(agent_name, db.clone(), metrics)
            .await?;
        let nomad_db = NomadDB::new(home.name(), db);

        Ok(CachingHome::new(home, contract_sync, nomad_db))
    }

    /// Try to get a Replicas object
    pub async fn try_replica(&self, replica_name: &str) -> Result<Replicas, Report> {
        let replica_setup = self.replicas.get(replica_name).expect("!replica");
        let signer = self.get_signer(replica_name).await;
        let opt_replica_timelag = self.replica_timelag(replica_name);

        replica_setup
            .try_into_replica(signer, opt_replica_timelag)
            .await
    }

    /// Try to get a replica ContractSync
    pub async fn try_replica_contract_sync(
        &self,
        replica_name: &str,
        agent_name: &str,
        db: DB,
        metrics: ContractSyncMetrics,
    ) -> Result<ContractSync<CommonIndexers>, Report> {
        let replica_setup = self.replicas.get(replica_name).expect("!replica");

        let finality = self.replicas.get(replica_name).expect("!replica").finality;
        let index_settings = self.index.clone();

        let indexer = Arc::new(self.try_replica_indexer(replica_setup).await?);
        let replica_name = &replica_setup.name;

        let nomad_db = NomadDB::new(&replica_name, db);

        Ok(ContractSync::new(
            agent_name.to_owned(),
            replica_name.to_owned(),
            nomad_db,
            indexer,
            index_settings,
            finality,
            metrics,
        ))
    }

    /// Try to get a CachingReplica object
    pub async fn try_caching_replica(
        &self,
        replica_name: &str,
        agent_name: &str,
        db: DB,
        metrics: ContractSyncMetrics,
    ) -> Result<CachingReplica, Report> {
        let replica = self.try_replica(replica_name).await?;
        let contract_sync = self
            .try_replica_contract_sync(replica_name, agent_name, db.clone(), metrics)
            .await?;
        let nomad_db = NomadDB::new(replica.name(), db);

        Ok(CachingReplica::new(replica, contract_sync, nomad_db))
    }

    /// Try to get all replicas from this settings object
    pub async fn try_caching_replicas(
        &self,
        agent_name: &str,
        db: DB,
        metrics: ContractSyncMetrics,
    ) -> Result<HashMap<String, Arc<CachingReplica>>, Report> {
        let mut result = HashMap::default();
        for (k, v) in self.replicas.iter().filter(|(_, v)| v.disabled.is_none()) {
            if k != &v.name {
                bail!(
                    "Replica key does not match replica name:\n key: {}  name: {}",
                    k,
                    v.name
                );
            }

            let caching_replica = self
                .try_caching_replica(k, agent_name, db.clone(), metrics.clone())
                .await?;
            result.insert(v.name.clone(), Arc::new(caching_replica));
        }
        Ok(result)
    }

    /// Try to get an indexer object for a home. Note that indexers are NOT
    /// instantiated with a built in timelag. The timelag is handled by the
    /// ContractSync.
    pub async fn try_home_indexer(&self) -> Result<HomeIndexers, Report> {
        let signer = self.get_signer(&self.home.name).await;
        let timelag = self.home_timelag();

        match &self.home.chain {
            ChainConf::Ethereum(conn) => Ok(HomeIndexerVariants::Ethereum(
                make_home_indexer(
                    conn.clone(),
                    &ContractLocator {
                        name: self.home.name.clone(),
                        domain: self.home.domain.parse().expect("invalid uint"),
                        address: self.home.address.parse::<ethers::types::Address>()?.into(),
                    },
                    signer,
                    timelag,
                    self.index.from(),
                    self.index.chunk_size(),
                )
                .await?,
            )
            .into()),
        }
    }

    /// Try to get an indexer object for a replica. Note that indexers are NOT
    /// instantiated with a built in timelag. The timelag is handled by the
    /// ContractSync.
    pub async fn try_replica_indexer(&self, setup: &ChainSetup) -> Result<CommonIndexers, Report> {
        let signer = self.get_signer(&setup.name).await;
        let timelag = self.replica_timelag(&setup.name);

        match &setup.chain {
            ChainConf::Ethereum(conn) => Ok(CommonIndexerVariants::Ethereum(
                make_replica_indexer(
                    conn.clone(),
                    &ContractLocator {
                        name: setup.name.clone(),
                        domain: setup.domain.parse().expect("invalid uint"),
                        address: setup.address.parse::<ethers::types::Address>()?.into(),
                    },
                    signer,
                    timelag,
                    self.index.from(),
                    self.index.chunk_size(),
                )
                .await?,
            )
            .into()),
        }
    }

    /// Try to generate an agent core for a named agent
    pub async fn try_into_core(&self, name: &str) -> Result<AgentCore, Report> {
        let metrics = Arc::new(crate::metrics::CoreMetrics::new(
            name,
            self.metrics
                .as_ref()
                .map(|v| v.parse::<u16>().expect("metrics port must be u16")),
            Arc::new(prometheus::Registry::new()),
        )?);
        let sync_metrics = ContractSyncMetrics::new(metrics.clone());

        let db = DB::from_path(&self.db)?;
        let home = Arc::new(
            self.try_caching_home(name, db.clone(), sync_metrics.clone())
                .await?,
        );
        let replicas = self
            .try_caching_replicas(name, db.clone(), sync_metrics.clone())
            .await?;

        Ok(AgentCore {
            home,
            replicas,
            db,
            settings: self.clone(),
            metrics,
            indexer: self.index.clone(),
        })
    }

    /// Read settings from the config file
    pub fn new() -> Result<Self, ConfigError> {
        let mut s = Config::new();

        s.merge(File::with_name("config/default"))?;

        let env = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());
        s.merge(File::with_name(&format!("config/{}", env)).required(false))?;

        // Add in settings from the environment (with a prefix of NOMAD)
        // Eg.. `NOMAD_DEBUG=1 would set the `debug` key
        s.merge(Environment::with_prefix("NOMAD"))?;

        s.try_into()
    }
}
