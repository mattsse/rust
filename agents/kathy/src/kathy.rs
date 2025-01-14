use std::{sync::Arc, time::Duration};

use color_eyre::Result;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use tokio::{sync::Mutex, task::JoinHandle, time::sleep};
use tracing::instrument::Instrumented;
use tracing::{info, Instrument};

use ethers::core::types::H256;

use nomad_base::{decl_agent, decl_channel, AgentCore, CachingHome, CachingReplica, NomadAgent};
use nomad_core::{Common, Home, Message, Replica};

use crate::settings::KathySettings as Settings;

decl_agent!(Kathy {
    interval: u64,
    generator: ChatGenerator,
    home_lock: Arc<Mutex<()>>,
    messages_dispatched: prometheus::IntCounterVec,
});

impl Kathy {
    pub fn new(interval: u64, generator: ChatGenerator, core: AgentCore) -> Self {
        let messages_dispatched = core
            .metrics
            .new_int_counter(
                "messages_dispatched_count",
                "Number of messages dispatched to a given home for a given replica.",
                &["home", "replica", "agent"],
            )
            .expect("failed to register messages_dispatched_count metric");

        Self {
            interval,
            generator,
            core,
            home_lock: Arc::new(Mutex::new(())),
            messages_dispatched,
        }
    }
}

decl_channel!(Kathy {
    home_lock: Arc<Mutex<()>>,
    generator: ChatGenerator,
    messages_dispatched: prometheus::IntCounter,
    interval: u64,
});

#[async_trait::async_trait]
impl NomadAgent for Kathy {
    const AGENT_NAME: &'static str = "kathy";

    type Settings = Settings;

    type Channel = KathyChannel;

    async fn from_settings(settings: Settings) -> Result<Self> {
        Ok(Self::new(
            settings.interval.parse().expect("invalid u64"),
            settings.chat.into(),
            settings.base.try_into_core(Self::AGENT_NAME).await?,
        ))
    }

    fn build_channel(&self, replica: &str) -> Self::Channel {
        Self::Channel {
            base: self.channel_base(replica),
            home_lock: self.home_lock.clone(),
            generator: self.generator.clone(),
            messages_dispatched: self.messages_dispatched.with_label_values(&[
                self.home().name(),
                replica,
                Self::AGENT_NAME,
            ]),
            interval: self.interval,
        }
    }

    #[tracing::instrument]
    fn run(channel: Self::Channel) -> Instrumented<JoinHandle<Result<()>>> {
        tokio::spawn(async move {
            let home = channel.home();
            let destination = channel.replica().local_domain();
            let mut generator = channel.generator;
            let home_lock = channel.home_lock;
            let messages_dispatched = channel.messages_dispatched;
            let interval = channel.interval;

            loop {
                let msg = generator.gen_chat();
                let recipient = generator.gen_recipient();

                match msg {
                    Some(body) => {
                        let message = Message {
                            destination,
                            recipient,
                            body,
                        };
                        info!(
                            target: "outgoing_messages",
                            "Enqueuing message of length {} to {}::{}",
                            length = message.body.len(),
                            destination = message.destination,
                            recipient = message.recipient
                        );

                        let guard = home_lock.lock().await;
                        home.dispatch(&message).await?;

                        messages_dispatched.inc();

                        drop(guard);
                    }
                    _ => {
                        info!("Reached the end of the static message queue. Shutting down.");
                        return Ok(());
                    }
                }

                sleep(Duration::from_secs(interval)).await;
            }
        })
        .in_current_span()
    }
}

/// Generators for messages
#[derive(Debug, Clone)]
pub enum ChatGenerator {
    Static {
        recipient: H256,
        message: String,
    },
    OrderedList {
        messages: Vec<String>,
        counter: usize,
    },
    Random {
        length: usize,
    },
    Default,
}

impl Default for ChatGenerator {
    fn default() -> Self {
        Self::Default
    }
}

impl ChatGenerator {
    fn rand_string(length: usize) -> String {
        thread_rng()
            .sample_iter(&Alphanumeric)
            .take(length)
            .map(char::from)
            .collect()
    }

    pub fn gen_recipient(&mut self) -> H256 {
        match self {
            ChatGenerator::Default => Default::default(),
            ChatGenerator::Static {
                recipient,
                message: _,
            } => *recipient,
            ChatGenerator::OrderedList {
                messages: _,
                counter: _,
            } => Default::default(),
            ChatGenerator::Random { length: _ } => H256::random(),
        }
    }

    pub fn gen_chat(&mut self) -> Option<Vec<u8>> {
        match self {
            ChatGenerator::Default => Some(Default::default()),
            ChatGenerator::Static {
                recipient: _,
                message,
            } => Some(message.as_bytes().to_vec()),
            ChatGenerator::OrderedList { messages, counter } => {
                if *counter >= messages.len() {
                    return None;
                }

                let msg = messages[*counter].clone().into();

                // Increment counter to next message in list
                *counter += 1;

                Some(msg)
            }
            ChatGenerator::Random { length } => Some(Self::rand_string(*length).into()),
        }
    }
}
