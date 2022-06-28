use std::sync::{Arc, Weak};

use bastion::prelude::*;
use nats::asynk::{Connection, Subscription};
use tokio::select;

use crate::upgrade_weak;

#[derive(Debug, Clone)]
struct NatsConn(Arc<ConnInner>);
#[derive(Debug, Clone)]
struct NatsWeakConn(Weak<ConnInner>);
#[derive(Debug)]
struct ConnInner {
    connection: Connection,
}

impl std::ops::Deref for NatsConn {
    type Target = ConnInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl NatsWeakConn {
    fn upgrade(&self) -> Option<NatsConn> {
        self.0.upgrade().map(NatsConn)
    }
}

impl NatsConn {
    fn downgrade(&self) -> NatsWeakConn {
        NatsWeakConn(Arc::downgrade(&self.0))
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription, ()> {
        let sub = self
            .connection
            .subscribe(topic)
            .await
            .map_err(|e| eprintln!("{e}"))?;
        Ok(sub)
    }

    async fn send(&self, topic: &str, payload: impl AsRef<[u8]>) -> Result<(), ()> {
        self.connection
            .publish(topic, payload)
            .await
            .map_err(|e| eprintln!("{e}"))
    }
}

#[derive(Debug, Clone)]
pub struct PublishMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SubscribeMessage<'a> {
    pub topic: &'a str,
}

#[derive(Debug, Clone)]
pub struct UnsubscribeMessage<'a> {
    pub topic: &'a str,
}

pub struct Nats;
impl Nats {
    pub fn run() -> Result<SupervisorRef, ()> {
        Bastion::supervisor(|s| {
            s.with_restart_strategy(
                RestartStrategy::default().with_restart_policy(RestartPolicy::Tries(10)),
            )
            .children(|c| {
                c.with_callbacks(Callbacks::new().with_after_start(|| println!("NATS started")))
                    .with_distributor(Distributor::named("nats_actor"))
                    .with_exec(executor)
            })
        })
    }
}

async fn executor(ctx: BastionContext) -> Result<(), ()> {
    let conn = connect_nats().await?;
    let connection = NatsConn(Arc::new(conn));

    let conn_weak = connection.downgrade();
    let (msg_tx, _) = tokio::sync::broadcast::channel::<UnsubscribeMessage>(16);

    loop {
        let conn_weak = conn_weak.clone();
        let msg_tx = msg_tx.clone();
        let mut msg_rx = msg_tx.subscribe();
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|msg: SubscribeMessage, _| {
                let connection = upgrade_weak!(conn_weak);
                spawn!(async move {
                    let sub = connection
                        .subscribe(&msg.topic)
                        .await
                        .map_err(|e| eprintln!("{e:?}"))
                        .expect(&format!("cannot subscribe topic {msg:?}"));

                    let actor = format!("{}_actor", msg.topic);
                    let dist = Distributor::named(&actor);

                    loop {
                        let gonna_break_loop = select! {
                            unsub_msg = msg_rx.recv() => {
                                if let Ok(unsub_msg) = unsub_msg {
                                    if msg.topic == unsub_msg.topic {
                                        sub.unsubscribe().await.expect("cannot unsubscribe");
                                    }
                                }
                                true
                            }
                            msg = sub.next() => {
                                if let Some(msg) = msg {
                                    dist.tell_one(msg.data).expect(&format!("cannot tell everyone in {actor}"));
                                }
                                false
                            }
                        };
                        if gonna_break_loop {
                            break;
                        }
                    }
                });
            })
            .on_tell(|msg: UnsubscribeMessage, _| {
                msg_tx.send(msg).expect("cannot tell subscription to unsubscribe");
            }).on_tell(|msg: PublishMessage, _| {
                let connection = upgrade_weak!(conn_weak);
                run!(async {
                    connection.send(&msg.topic, &msg.payload).await.expect("cannot publish message");
                });
            });
    }
}

async fn connect_nats() -> Result<ConnInner, ()> {
    let connection = nats::asynk::Options::new()
        .retry_on_failed_connect()
        .connect("demo.nats.io")
        .await
        .map_err(|e| eprintln!("Error: {e}"))?;

    Ok(ConnInner { connection })
}
