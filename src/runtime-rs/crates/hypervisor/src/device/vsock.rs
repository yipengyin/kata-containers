// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use anyhow::{Context, Result};
use rand::Rng;
use std::os::unix::prelude::AsRawFd;
use tokio::fs::{File, OpenOptions};

#[derive(Debug)]
pub struct HybridVsockConfig {
    /// Unique identifier of the device
    pub id: String,

    /// A 32-bit Context Identifier (CID) used to identify the guest.
    pub guest_cid: u32,

    /// unix domain socket path
    pub uds_path: String,
}

#[derive(Debug)]
pub struct VsockConfig {
    /// Unique identifier of the device
    pub id: String,

    /// A 32-bit Context Identifier (CID) used to identify the guest.
    pub guest_cid: u32,

    pub vhost_fd: File,
}

const VHOST_VIRTIO: u8 = 0xAF;
nix::ioctl_write_ptr!(vhost_vsock_set_guest_cid, VHOST_VIRTIO, 0x60, u64);

impl VsockConfig {
    pub async fn new(id: String) -> Result<Self> {
        let vhost_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vhost-vsock")
            .await
            .context("failed to open /dev/vhost-vsock")?;
        let mut rng = rand::thread_rng();

        // Try 50 times to find a context ID that is not in use.
        for _ in 0..50 {
            let rand_cid = rng.gen_range(3..=(u32::MAX));
            match unsafe { vhost_vsock_set_guest_cid(vhost_fd.as_raw_fd(), &(rand_cid as u64)) } {
                Ok(_) => {
                    return Ok(VsockConfig {
                        id,
                        guest_cid: rand_cid,
                        vhost_fd,
                    });
                }
                Err(nix::Error::EADDRINUSE) => {
                    // The CID is already in use. Try another one.
                }
                Err(err) => {
                    return Err(err).context("failed to set guest CID");
                }
            }
        }

        anyhow::bail!("failed to find a free vsock context ID after 50 attempts");
    }
}
