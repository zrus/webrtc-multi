use std::sync::Arc;

use bastion::prelude::*;
use serde_json::json;
use tokio::sync::Mutex;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_H264},
        APIBuilder,
    },
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::{
        track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter,
    },
    Error,
};

use crate::{nats_actor::PublishMessage, camera::ShutdownMessage};

#[derive(Debug)]
pub struct SdpMessage(pub String);
#[derive(Debug)]
pub struct IceMessage(pub String);

pub struct WebRTC;
impl WebRTC {
    pub fn run(parent: &SupervisorRef, device_id: u8, id: u8, rtp_rx: tokio::sync::broadcast::Receiver<Vec<u8>>) -> Result<ChildrenRef, ()> {
        let reff = parent.children(|c| {
            c.with_callbacks(
                Callbacks::new()
                    .with_after_start(move || println!("WebRTC actor {device_id} started")),
            )
            .with_distributor(Distributor::named(format!("webrtc_{device_id}_actor")))
            .with_exec(move |ctx| executor(ctx, device_id, id, rtp_rx.resubscribe()))
        });
        run! { async {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }};
        reff
    }
}

async fn executor(ctx: BastionContext, device_id: u8, id: u8, mut rtp_rx: tokio::sync::broadcast::Receiver<Vec<u8>>) -> Result<(), ()> {
    let pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>> = Arc::new(Mutex::new(vec![]));

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut m = MediaEngine::default();
    m.register_default_codecs()
        .expect("cannot register default codecs");
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 100,
            ..Default::default()
        },
        RTPCodecType::Video,
    ).expect("cannot register on H264 profile");

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)
        .expect("cannot register default interceptors");

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .expect("cannot create peer connection"),
    );

    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .expect("cannot add track");

    spawn!(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<(), ()>::Ok(())
    });

    let pc = Arc::downgrade(&peer_connection);
    let pending_candidates2 = Arc::clone(&pending_candidates);
    peer_connection
        .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let pc2 = pc.clone();
            let pending_candidates3 = Arc::clone(&pending_candidates2);
            Box::pin(async move {
                if let Some(c) = c {
                    if let Some(pc) = pc2.upgrade() {
                        let desc = pc.remote_description().await;
                        if desc.is_none() {
                            let mut cs = pending_candidates3.lock().await;
                            cs.push(c);
                        } else if let Err(err) = signal_candidate(device_id, &c).await {
                            assert!(false, "{}", err);
                        }
                    }
                }
            })
        }))
        .await;

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    let done_tx1 = done_tx.clone();
    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("Peer Connection {device_id} State has changed: {}", s);

            if s == RTCPeerConnectionState::Failed {
                println!("Peer Connection {device_id} has gone to failed exiting");
                let _ = done_tx1.try_send(());
            }

            Box::pin(async {})
        }))
        .await;

    let done_tx2 = done_tx.clone();
    
    let handler = spawn!(async move {
        while let Ok(value) = rtp_rx.recv().await {
            if let Err(err) = video_track.write(&value).await {
                if Error::ErrClosedPipe == err {
                    println!("the peerConnection has been closed");
                } else {
                    println!("video_track write err: {}", err);
                }
                let _ = done_tx2.try_send(());
                return;
            }
        }
        println!("{device_id} stopped!");
    });

    let pc_clone = Arc::downgrade(&peer_connection);
    let pending_candidates_clone = Arc::downgrade(&pending_candidates);
    loop {
        let pc = pc_clone.clone();
        let pending_candidates = pending_candidates_clone.clone();
        let gonna_break_loop = tokio::select! {
            msg = ctx.recv() => {
                MessageHandler::new(msg?).on_tell(|msg: SdpMessage, _| {
                    let sdp = match serde_json::from_str::<RTCSessionDescription>(&msg.0) {
                        Ok(s) => s,
                        Err(err) => panic!("{err}"),
                    };
                    run! { async {
                        if let Some(pc) = pc.upgrade() {
                            if let Err(err) = pc.set_remote_description(sdp).await {
                                panic!("{err}");
                            }

                            let answer = match pc.create_answer(None).await {
                                Ok(a) => {
                                    let sdp = json!({
                                        "type": "answer",
                                        "sdp": a.sdp.clone(),
                                    })
                                    .to_string()
                                    .as_bytes()
                                    .to_vec();
                                    let pub_msg = PublishMessage { topic: format!("{device_id}.sdp"), payload: sdp };
                                    Distributor::named("nats_actor").tell_one(pub_msg).expect("cannot send to NATS");
                                    a
                                },
                                Err(err) => panic!("{err}"),
                            };

                            if let Err(err) = pc.set_local_description(answer).await {
                                panic!("{}", err);
                            }

                            if let Some(cs) = pending_candidates.upgrade() {
                                let cs = cs.lock().await;
                                for c in &*cs {
                                    if let Err(e) = signal_candidate(device_id, c).await {
                                        panic!("{e}");
                                    }
                                }
                            }
                        }
                    }}
                }).on_tell(|msg: IceMessage, _| {
                    run! { async {
                        if let Some(pc) = pc.upgrade() {
                            let candidate = RTCIceCandidateInit {
                                candidate: msg.0,
                                ..Default::default()
                            };
                            if let Err(e) = pc.add_ice_candidate(candidate).await {
                                panic!("Error: {e}");
                            }
                        }
                    }}
                });
                false
            }
            _ = done_rx.recv() => {
                true
            }
        };
        if gonna_break_loop {
            break;
        }
    }

    handler.cancel();
    Distributor::named(format!("cam_{id}_actor")).tell_one(ShutdownMessage(device_id)).expect("cannot send message to its owner");

    Ok(())
}

async fn signal_candidate(device_id: u8, c: &RTCIceCandidate) -> anyhow::Result<()> {
    let candidate = c.to_json().await?.candidate;

    let pub_msg = PublishMessage {
        topic: format!("{device_id}.ice"),
        payload: candidate.as_bytes().to_vec(),
    };

    Distributor::named("nats_actor")
        .tell_one(pub_msg)
        .expect("cannot send to NATS actor");

    Ok(())
}
