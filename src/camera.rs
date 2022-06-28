use std::{
    collections::HashSet,
    sync::{Arc, Weak},
};

use bastion::prelude::*;
use gst::prelude::{Cast, ElementExt, ElementExtManual};

use crate::webrtc::{IceMessage as WIceMsg, SdpMessage as WSdpMsg, WebRTC};

#[derive(Debug)]
pub struct StopMessage;

#[derive(Debug, Clone)]
pub struct SdpMessage {
    pub device_id: u8,
    pub sdp: String,
}

#[derive(Debug, Clone)]
pub struct IceMessage {
    pub device_id: u8,
    pub ice: String,
}

#[derive(Debug, Clone)]
pub struct Pipeline(Arc<PipelineInner>);

#[derive(Debug, Clone)]
pub struct PipelineWeak(Weak<PipelineInner>);

#[derive(Debug)]
pub struct PipelineInner {
    pipeline: gst::Pipeline,
}

impl std::ops::Deref for Pipeline {
    type Target = PipelineInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for PipelineInner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Pipeline {
    pub fn run(id: u8) -> Result<Self, ()> {
        gst::init().expect("cannot initialize Gstreamer");
        let pipeline = gst::parse_launch(
            &format!("videotestsrc pattern=ball is-live=true ! videoconvert ! x264enc ! 
            rtph264pay config-interval=-1 ! application/x-rtp,media=video,encoding-name=H264,payload=100,clock-rate=90000 ! udpsink host=127.0.0.1 port=5{id:0>3}")
        )
        .expect("cannot parse pipeline from string");

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("cannot downcast pipeline");

        pipeline.call_async(|pipeline| {
            if pipeline.set_state(gst::State::Playing).is_err() {
                gst::element_error!(
                    pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to set pipeline to Playing")
                );
            }
        });

        Ok(Self(Arc::new(PipelineInner { pipeline })))
    }
}

pub struct Camera;
impl Camera {
    pub fn run(id: u8) -> Result<SupervisorRef, ()> {
        let reff = Bastion::supervisor(move |s| {
            s.children(move |c| {
                c.with_callbacks(Callbacks::new().with_after_start(move || {
                    println!("Camera {id} started");
                }))
                .with_distributor(Distributor::named(format!("cam_{id}_actor")))
                .with_exec(move |ctx| executor(ctx, id))
            })
        });
        run! { async {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }};
        reff
    }
}

async fn executor(ctx: BastionContext, id: u8) -> Result<(), ()> {
    let _ = Pipeline::run(id)?;
    let mut webrtc_list = HashSet::new();
    loop {
        MessageHandler::new(ctx.recv().await?)
            .on_tell(|msg: SdpMessage, _| {
                let SdpMessage { device_id, sdp } = msg;
                if webrtc_list.insert(device_id) {
                    WebRTC::run(ctx.supervisor().unwrap(), device_id)
                        .expect("cannot create webrtc actor");
                    Distributor::named(format!("webrtc_{device_id}_actor"))
                        .tell_one(WSdpMsg(sdp))
                        .expect("cannot send sdp to webrtc actor");
                }
            })
            .on_tell(|msg: IceMessage, _| {
                let IceMessage { device_id, ice } = msg;
                if webrtc_list.contains(&device_id) {
                    Distributor::named(format!("webrtc_{device_id}_actor"))
                        .tell_one(WIceMsg(ice))
                        .expect("cannot send ice to webrtc actor");
                }
            })
            .on_tell(|_: StopMessage, _| {
                ctx.supervisor()
                    .unwrap()
                    .kill()
                    .expect("cannot kill camera actor and its children");
            });
    }
}
