mod cam_manage_actor;
mod camera;
mod nats_actor;
mod utils;
mod webrtc;

use bastion::prelude::*;
use cam_manage_actor::CamManager;
use nats_actor::Nats;

#[tokio::main]
async fn main() -> Result<(), ()> {
    Bastion::init();
    Bastion::start();

    // CODE GOES HERE
    Nats::run()?;
    CamManager::run()?;

    Bastion::block_until_stopped();
    Ok(())
}
