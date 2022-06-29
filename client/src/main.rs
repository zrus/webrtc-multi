use std::sync::Arc;

use serde_json::json;
use tokio::{
    spawn,
    sync::{mpsc, Mutex, Notify},
};
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
    media::io::h264_writer::H264Writer,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication,
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_receiver::RTCRtpReceiver,
    },
    track::track_remote::TrackRemote,
};

#[tokio::main]
async fn main() {
    let conn = nats::asynk::connect("demo.nats.io").await.unwrap();
    let conn = Arc::new(conn);
    for id in 1..=50 {
        let conn_cl = Arc::clone(&conn);
        spawn(async move {
            println!("Client {id} running..");

            let (sdp_tx, mut sdp_rx) = mpsc::channel::<Vec<u8>>(1);
            let (ice_tx, mut ice_rx) = mpsc::channel::<Vec<u8>>(1);

            let conn = Arc::clone(&conn_cl);
            spawn(async move {
                let sub = conn.subscribe(&format!("{id}.sdp")).await.unwrap();
                while let Some(msg) = sub.next().await {
                    sdp_tx
                        .send(msg.data)
                        .await
                        .expect("channel SDP cannot send message to receiver");
                }
            });

            let conn = Arc::clone(&conn_cl);
            spawn(async move {
                let sub = conn.subscribe(&format!("{id}.ice")).await.unwrap();
                while let Some(msg) = sub.next().await {
                    ice_tx
                        .send(msg.data)
                        .await
                        .expect("channel ICE cannot send message to receiver");
                }
            });

            spawn(async move {
                let pending_candidates: Arc<Mutex<Vec<RTCIceCandidate>>> =
                    Arc::new(Mutex::new(vec![]));

                let config = RTCConfiguration {
                    ice_servers: vec![RTCIceServer {
                        urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                        ..Default::default()
                    }],
                    ..Default::default()
                };

                let mut m = MediaEngine::default();
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
                )
                .expect("cannot register H264 profile");

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

                peer_connection
                    .add_transceiver_from_kind(
                        RTPCodecType::Video,
                        &[],
                    )
                    .await
                    .expect("cannot add video transceiver");

                let notify_tx = Arc::new(Notify::new());
                let notify_rx = notify_tx.clone();
                let h264_writer: Arc<Mutex<dyn webrtc::media::io::Writer + Send + Sync>> =
                    Arc::new(Mutex::new(H264Writer::new(
                        std::fs::File::create(format!("{id}.h264")).unwrap(),
                    )));

                let pc = Arc::downgrade(&peer_connection);
                peer_connection.on_track(Box::new(move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                    if let Some(track) = track {
                        // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
                        let media_ssrc = track.ssrc();
                        let pc2 = pc.clone();
                        tokio::spawn(async move {
                            let mut result = Result::<usize, ()>::Ok(0);
                            while result.is_ok() {
                                let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(3));
                                tokio::pin!(timeout);

                                tokio::select! {
                                    _ = timeout.as_mut() =>{
                                        if let Some(pc) = pc2.upgrade(){
                                            result = pc.write_rtcp(&[Box::new(PictureLossIndication{
                                                sender_ssrc: 0,
                                                media_ssrc,
                                            })]).await.map_err(|e| eprintln!("{e}"));
                                        } else {
                                            break;
                                        }
                                    }
                                };
                            }
                        });

                        let notify_rx2 = Arc::clone(&notify_rx);
                        let h264_writer = Arc::clone(&h264_writer);
                        Box::pin(async move {
                            let codec = track.codec().await;
                            let mime_type = codec.capability.mime_type.to_lowercase();
                            if mime_type == MIME_TYPE_H264.to_lowercase() {
                                tokio::spawn(async move {
                                    loop {
                                        tokio::select! {
                                            result = track.read_rtp() => {
                                                if let Ok((rtp_packet, _)) = result {
                                                    if rtp_packet.payload.len() > 2 {
                                                        let mut writer = h264_writer.lock().await;
                                                        writer.write_rtp(&rtp_packet).expect("cannot write rtp to file");
                                                    }
                                                } else {
                                                    println!("file closing begin after read_rtp error");
                                                    let mut w = h264_writer.lock().await;
                                                    if let Err(err) = w.close() {
                                                        println!("file close err: {}", err);
                                                    }
                                                    println!("file closing end after read_rtp error");
                                                }
                                            }
                                            _ = notify_rx2.notified() => {
                                                println!("file closing begin after notified");
                                                let mut w = h264_writer.lock().await;
                                                if let Err(err) = w.close() {
                                                    println!("file close err: {}", err);
                                                }
                                                println!("file closing end after notified");
                                            }
                                        }
                                    }
                                });
                            }
                        })
                    }else {
                        Box::pin(async {})
                    }
                })).await;

                let pc = Arc::downgrade(&peer_connection);
                let pending_candidates2 = Arc::clone(&pending_candidates);
                let conn = Arc::clone(&conn_cl);
                peer_connection
                    .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                        let pc2 = pc.clone();
                        let pending_candidates3 = Arc::clone(&pending_candidates2);
                        let conn = conn.clone();
                        Box::pin(async move {
                            if let Some(c) = c {
                                if let Some(pc) = pc2.upgrade() {
                                    let desc = pc.remote_description().await;
                                    if desc.is_none() {
                                        let mut cs = pending_candidates3.lock().await;
                                        cs.push(c);
                                    } else {
                                        let msg = json!({
                                            "ice": json!({
                                                "cam_id": 1,
                                                "device_id": id,
                                                "payload": c.to_json().await.unwrap().candidate,
                                            })
                                        })
                                        .to_string()
                                        .as_bytes()
                                        .to_vec();
                                        conn.publish("cam_manager", msg)
                                            .await
                                            .expect("cannot publish ICE to NATS");
                                    }
                                }
                            }
                        })
                    }))
                    .await;

                let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

                let done_tx1 = done_tx.clone();
                peer_connection
                    .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                        println!("Peer Connection {id} State has changed: {}", s);

                        if s == RTCPeerConnectionState::Failed {
                            notify_tx.notify_waiters();
                            println!("Peer Connection {id} has gone to failed exiting");
                            let _ = done_tx1.try_send(());
                        }

                        Box::pin(async {})
                    }))
                    .await;

                let offer = peer_connection
                    .create_offer(None)
                    .await
                    .expect("cannot create offer");
                let sdp = match serde_json::to_string(&offer) {
                    Ok(offer) => offer,
                    Err(e) => panic!("Err: {e}"),
                };

                peer_connection
                    .set_local_description(offer)
                    .await
                    .expect("cannot set local description");

                let msg = json!({
                    "sdp": json!({
                        "cam_id": 1,
                        "device_id": id,
                        "payload": sdp,
                    })
                })
                .to_string()
                .as_bytes()
                .to_vec();

                conn_cl
                    .publish("cam_manager", msg)
                    .await
                    .expect("cannot send offer SDP to NATS");

                let pc_clone = Arc::downgrade(&peer_connection);
                let pending_candidates_clone = Arc::downgrade(&pending_candidates);
                loop {
                    let pc = pc_clone.clone();
                    let pending_candidates = pending_candidates_clone.clone();
                    let gonna_break_loop = tokio::select! {
                        sdp = sdp_rx.recv() => {
                            if let Some(pc) = pc.upgrade() {
                                if let Ok(sdp) = serde_json::from_slice::<RTCSessionDescription>(&sdp.unwrap()) {
                                    if let Err(err) = pc.set_remote_description(sdp).await {
                                        panic!("{err}");
                                    }

                                    if let Some(cs) = pending_candidates.upgrade() {
                                        let cs = cs.lock().await;
                                        for c in &*cs {
                                            let msg = json!({
                                                "ice": json!({
                                                    "cam_id": 1,
                                                    "device_id": id,
                                                    "payload": c.to_json().await.unwrap().candidate,
                                                })
                                            }).to_string().as_bytes().to_vec();
                                            conn_cl.publish("cam_manager", msg).await.expect("cannot publish ICE to NATS");
                                        }
                                    }
                                } else {
                                    println!("failed parse sdp");
                                }
                            }
                            false
                        }
                        ice = ice_rx.recv() => {
                            if let Some(pc) = pc.upgrade() {
                                let ice = String::from_utf8(ice.unwrap()).unwrap();
                                let candidate = RTCIceCandidateInit {
                                    candidate: ice,
                                    ..Default::default()
                                };
                                if let Err(e) = pc.add_ice_candidate(candidate).await {
                                    panic!("Error: {e}");
                                }
                            }
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

                peer_connection
                    .close()
                    .await
                    .expect("cannot close connection");
            });
        });
    }
    loop {}
}
