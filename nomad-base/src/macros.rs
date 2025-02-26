#[macro_export]
/// Shortcut for aborting a joinhandle and then awaiting and discarding its result
macro_rules! cancel_task {
    ($task:ident) => {
        #[allow(unused_must_use)]
        {
            let t = $task.into_inner();
            t.abort();
            t.await;
        }
    };
}

#[macro_export]
/// Shortcut for implementing agent traits
macro_rules! impl_as_ref_core {
    ($agent:ident) => {
        impl AsRef<nomad_base::AgentCore> for $agent {
            fn as_ref(&self) -> &nomad_base::AgentCore {
                &self.core
            }
        }
    };
}

#[macro_export]
/// Declare a new agent struct with the additional fields
macro_rules! decl_agent {
    (
        $(#[$outer:meta])*
        $name:ident{
            $($prop:ident: $type:ty,)*
        }) => {

        $(#[$outer])*
        #[derive(Debug)]
        pub struct $name {
            $($prop: $type,)*
            core: nomad_base::AgentCore,
        }

        $crate::impl_as_ref_core!($name);
    };
}

#[macro_export]
/// Declare a new channel block
/// ### Usage
///
/// ```ignore
/// decl_channel!(Relayer {
///     updates_relayed_counts: prometheus::IntCounterVec,
///     interval: u64,
/// });

/// ```
macro_rules! decl_channel {
    (
        $name:ident {
            $($(#[$tags:meta])* $prop:ident: $type:ty,)*
        }
    ) => {
        paste::paste! {
            #[derive(Debug, Clone)]
            #[doc = "Channel for `" $name]
            pub struct [<$name Channel>] {
                pub(crate) base: nomad_base::ChannelBase,
                $(
                    $(#[$tags])*
                    pub(crate) $prop: $type,
                )*
            }

            impl AsRef<nomad_base::ChannelBase> for [<$name Channel>] {
                fn as_ref(&self) -> &nomad_base::ChannelBase {
                    &self.base
                }
            }

            impl [<$name Channel>] {
                pub fn home(&self) -> Arc<CachingHome> {
                    self.as_ref().home.clone()
                }

                pub fn replica(&self) -> Arc<CachingReplica> {
                    self.as_ref().replica.clone()
                }

                pub fn db(&self) -> nomad_base::NomadDB {
                    self.as_ref().db.clone()
                }
            }
        }
    }
}

#[macro_export]
/// Declare a new settings block
///
/// This macro declares a settings struct for an agent. The new settings block
/// contains a [`crate::Settings`] and any other specified attributes.
///
/// Please note that integers must be specified as `String` in order to allow
/// them to be configured via env var. They must then be parsed in the
/// [`NomadAgent::from_settings`](crate::agent::NomadAgent::from_settings)
/// method.
///
/// ### Usage
///
/// ```ignore
/// decl_settings!(Updater {
///    updater: SignerConf,
///    polling_interval: String,
/// });
/// ```
macro_rules! decl_settings {
    (
        $name:ident {
            $($(#[$tags:meta])* $prop:ident: $type:ty,)*
        }
    ) => {
        paste::paste! {
            #[derive(Debug, serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            #[doc = "Settings for `" $name]
            pub struct [<$name Settings>] {
                #[serde(flatten)]
                pub(crate) base: nomad_base::Settings,
                $(
                    $(#[$tags])*
                    pub(crate) $prop: $type,
                )*
            }

            impl AsRef<nomad_base::Settings> for [<$name Settings>] {
                fn as_ref(&self) -> &nomad_base::Settings {
                    &self.base
                }
            }

            impl [<$name Settings>] {
                /// Read settings from the config files and/or env
                /// The config will be located at `config/default` unless specified
                /// otherwise
                ///
                /// Configs are loaded in the following precedence order:
                ///
                /// 1. The file specified by the `RUN_ENV` and `BASE_CONFIG`
                ///    env vars. `RUN_ENV/BASECONFIG`
                /// 2. The file specified by the `RUN_ENV` env var and the
                ///    agent's name. `RUN_ENV/AGENT-partial.json`
                /// 3. Configuration env vars with the prefix `OPT_BASE` intended
                ///    to be shared by multiple agents in the same environment
                /// 4. Configuration env vars with the prefix `OPT_AGENTNAME`
                ///    intended to be used by a specific agent.
                ///
                /// Specify a configuration directory with the `RUN_ENV` env
                /// variable. Specify a configuration file with the `BASE_CONFIG`
                /// env variable.
                pub fn new() -> Result<Self, config::ConfigError> {
                    let mut s = config::Config::new();

                    let env = std::env::var("RUN_ENV").unwrap_or_else(|_| "default".into());

                    let fname = std::env::var("BASE_CONFIG").unwrap_or_else(|_| "base".into());

                    s.merge(config::File::with_name(&format!("./config/{}/{}", env, fname)))?;
                    s.merge(config::File::with_name(&format!("./config/{}/{}-partial", env, stringify!($name).to_lowercase())).required(false))?;

                    // Use a base configuration env variable prefix
                    s.merge(config::Environment::with_prefix(&"OPT_BASE").separator("_"))?;

                    // Derive additional prefix from agent name
                    let prefix = format!("OPT_{}", stringify!($name).to_ascii_uppercase());
                    s.merge(config::Environment::with_prefix(&prefix).separator("_"))?;

                    let settings_res: Result<Self, config::ConfigError> = s.try_into();
                    let mut settings = settings_res?;

                    /// Kludge, use proc macro to match on enum later
                    match std::stringify!($name) {
                        "Kathy" => {
                            settings.base.set_index_data_types(nomad_base::settings::IndexDataTypes::Updates);
                            settings.base.set_use_timelag(false);
                        }
                        "Updater" => {
                            settings.base.set_index_data_types(nomad_base::settings::IndexDataTypes::Updates);
                            settings.base.set_use_timelag(true);
                        }
                        "Relayer" => {
                            settings.base.set_index_data_types(nomad_base::settings::IndexDataTypes::Updates);
                            settings.base.set_use_timelag(false);
                        }
                        "Processor" => {
                            settings.base.set_index_data_types(nomad_base::settings::IndexDataTypes::UpdatesAndMessages);
                            settings.base.set_use_timelag(true);
                        }
                        "Watcher" => {
                            settings.base.set_index_data_types(nomad_base::settings::IndexDataTypes::Updates);
                            settings.base.set_use_timelag(false);
                        }
                        _ => std::panic!("Invalid agent-specific settings name!"),
                    };

                    Ok(settings)
                }
            }
        }
    }
}
