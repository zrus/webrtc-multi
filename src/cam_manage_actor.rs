use bastion::prelude::*;
use serde::Deserialize;

use crate::{
    camera::{Camera, IceMessage, SdpMessage},
    nats_actor::{SubscribeMessage, UnsubscribeMessage},
};

#[derive(Debug, Deserialize)]
enum NATSReceivedMessages {
    #[serde(rename = "sdp")]
    Sdp {
        cam_id: u8,
        device_id: u8,
        payload: String,
    },
    #[serde(rename = "ice")]
    Ice {
        cam_id: u8,
        device_id: u8,
        payload: String,
    },
}
impl<'a> TryFrom<&'a [u8]> for NATSReceivedMessages {
    type Error = ();

    fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
        let msg: Self = serde_json::from_slice(value).map_err(|e| eprintln!("{e}"))?;
        Ok(msg)
    }
}

pub struct CamManager;
impl CamManager {
    pub fn run() -> Result<SupervisorRef, ()> {
        Bastion::supervisor(|s| {
            s.children(|c| {
                c.with_callbacks(
                    Callbacks::new()
                        .with_after_start(|| println!("Camera manager started"))
                        .with_before_restart(|| {
                            Distributor::named("nats_actor")
                                .tell_one(UnsubscribeMessage {
                                    topic: "cam_manager",
                                })
                                .expect("cannot send unsubscribe message to NATS actor");
                        }),
                )
                .with_distributor(Distributor::named("cam_manager_actor"))
                .with_exec(executor)
            })
        })
    }
}

async fn executor(ctx: BastionContext) -> Result<(), ()> {
    Distributor::named("nats_actor")
        .tell_one(SubscribeMessage {
            topic: "cam_manager",
        })
        .expect("cannot send subscribe message to NATS actor");
    loop {
        MessageHandler::new(ctx.recv().await?).on_tell(|msg: Vec<u8>, _| {
            match NATSReceivedMessages::try_from(&msg[..]) {
                Ok(msg) => match msg {
                    NATSReceivedMessages::Sdp {
                        cam_id,
                        device_id,
                        payload,
                    } => {
                        let dist = Distributor::named(format!("cam_{cam_id}_actor"));
                        if dist.tell_one(()).is_err() {
                            Camera::run(cam_id).expect("cannot start camera actor");
                        }
                        dist.tell_one(SdpMessage {
                            device_id,
                            sdp: payload.to_owned(),
                        })
                        .expect("cannot send sdp message to camera actor");
                    }
                    NATSReceivedMessages::Ice {
                        cam_id,
                        device_id,
                        payload,
                    } => {
                        let dist = Distributor::named(format!("cam_{cam_id}_actor"));
                        dist.tell_one(IceMessage {
                            device_id,
                            ice: payload.to_owned(),
                        })
                        .expect("cannot send ice message to camera actor");
                    }
                },
                Err(_) => eprintln!("cannot parse NATS message"),
            }
        });
    }
}
