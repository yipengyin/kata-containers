// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::resource_persist::ResourceState;
use crate::{
    cgroups::{CgroupArgs, CgroupsResource},
    manager::ManagerArgs,
    network::{self, Network},
    rootfs::{RootFsResource, Rootfs},
    share_fs::{self, ShareFs},
    volume::{Volume, VolumeResource},
    ResourceConfig,
};
use agent::types::Device;
use agent::{Agent, Storage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use hypervisor::device_manager::{new_device_info, VIRTIO_MMIO};
use hypervisor::{device_manager::DeviceManager, Hypervisor};
use kata_types::config::TomlConfig;
use kata_types::mount::Mount;
use oci::{Linux, LinuxResources};
use persist::sandbox_persist::Persist;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) struct ResourceManagerInner {
    sid: String,
    toml_config: Arc<TomlConfig>,
    agent: Arc<dyn Agent>,
    hypervisor: Arc<dyn Hypervisor>,
    network: Option<Arc<dyn Network>>,
    share_fs: Option<Arc<dyn ShareFs>>,
    device_manager: Arc<RwLock<DeviceManager>>,
    pub rootfs_resource: RootFsResource,
    pub volume_resource: VolumeResource,
    pub cgroups_resource: CgroupsResource,
}

impl ResourceManagerInner {
    pub(crate) fn new(
        sid: &str,
        agent: Arc<dyn Agent>,
        hypervisor: Arc<dyn Hypervisor>,
        toml_config: Arc<TomlConfig>,
    ) -> Result<Self> {
        let cgroups_resource = CgroupsResource::new(sid, &toml_config)?;
        let hypervisor_name = &toml_config.runtime.hypervisor_name;
        let block_device_driver = &toml_config
            .hypervisor
            .get(hypervisor_name)
            .context("failed to get hypervisor config")?
            .blockdev_info
            .block_device_driver;
        Ok(Self {
            sid: sid.to_string(),
            toml_config: toml_config.clone(),
            agent,
            hypervisor,
            network: None,
            share_fs: None,
            rootfs_resource: RootFsResource::new(),
            volume_resource: VolumeResource::new(),
            device_manager: Arc::new(RwLock::new(DeviceManager::new(block_device_driver)?)),
            cgroups_resource,
        })
    }

    pub fn config(&self) -> Arc<TomlConfig> {
        self.toml_config.clone()
    }

    pub async fn prepare_before_start_vm(
        &mut self,
        device_configs: Vec<ResourceConfig>,
    ) -> Result<()> {
        for dc in device_configs {
            match dc {
                ResourceConfig::ShareFs(c) => {
                    self.share_fs = if self
                        .hypervisor
                        .capabilities()
                        .await?
                        .is_fs_sharing_supported()
                    {
                        let share_fs = share_fs::new(&self.sid, &c).context("new share fs")?;
                        share_fs
                            .setup_device_before_start_vm(self.hypervisor.as_ref())
                            .await
                            .context("setup share fs device before start vm")?;
                        Some(share_fs)
                    } else {
                        None
                    };
                }
                ResourceConfig::Network(c) => {
                    let d = network::new(&c).await.context("new network")?;
                    d.setup(self.hypervisor.as_ref())
                        .await
                        .context("setup network")?;
                    self.network = Some(d)
                }
            };
        }

        Ok(())
    }

    async fn handle_interfaces(&self, network: &dyn Network) -> Result<()> {
        for i in network.interfaces().await.context("get interfaces")? {
            // update interface
            info!(sl!(), "update interface {:?}", i);
            self.agent
                .update_interface(agent::UpdateInterfaceRequest { interface: Some(i) })
                .await
                .context("update interface")?;
        }

        Ok(())
    }

    async fn handle_neighbours(&self, network: &dyn Network) -> Result<()> {
        let neighbors = network.neighs().await.context("neighs")?;
        if !neighbors.is_empty() {
            info!(sl!(), "update neighbors {:?}", neighbors);
            self.agent
                .add_arp_neighbors(agent::AddArpNeighborRequest {
                    neighbors: Some(agent::ARPNeighbors { neighbors }),
                })
                .await
                .context("update neighbors")?;
        }
        Ok(())
    }

    async fn handle_routes(&self, network: &dyn Network) -> Result<()> {
        let routes = network.routes().await.context("routes")?;
        if !routes.is_empty() {
            info!(sl!(), "update routes {:?}", routes);
            self.agent
                .update_routes(agent::UpdateRoutesRequest {
                    route: Some(agent::Routes { routes }),
                })
                .await
                .context("update routes")?;
        }
        Ok(())
    }

    pub async fn setup_after_start_vm(&mut self) -> Result<()> {
        if let Some(share_fs) = self.share_fs.as_ref() {
            share_fs
                .setup_device_after_start_vm(self.hypervisor.as_ref())
                .await
                .context("setup share fs device after start vm")?;
        }

        if let Some(network) = self.network.as_ref() {
            let network = network.as_ref();
            self.handle_interfaces(network)
                .await
                .context("handle interfaces")?;
            self.handle_neighbours(network)
                .await
                .context("handle neighbors")?;
            self.handle_routes(network).await.context("handle routes")?;
        }
        Ok(())
    }

    pub async fn get_storage_for_sandbox(&self) -> Result<Vec<Storage>> {
        let mut storages = vec![];
        if let Some(d) = self.share_fs.as_ref() {
            let mut s = d.get_storages().await.context("get storage")?;
            storages.append(&mut s);
        }
        Ok(storages)
    }

    pub async fn handler_rootfs(
        &self,
        cid: &str,
        bundle_path: &str,
        rootfs_mounts: &[Mount],
    ) -> Result<Arc<dyn Rootfs>> {
        self.rootfs_resource
            .handler_rootfs(
                &self.share_fs,
                self.device_manager.clone(),
                self.hypervisor.as_ref(),
                &self.sid,
                cid,
                bundle_path,
                rootfs_mounts,
            )
            .await
    }

    pub async fn handler_volumes(
        &self,
        cid: &str,
        oci_mounts: &[oci::Mount],
    ) -> Result<Vec<Arc<dyn Volume>>> {
        self.volume_resource
            .handler_volumes(
                &self.share_fs,
                cid,
                oci_mounts,
                self.device_manager.clone(),
                self.hypervisor.as_ref(),
                &self.sid,
            )
            .await
    }

    pub async fn handler_devices(
        &self,
        _cid: &str,
        linux: &Linux,
        devices_agent: &mut Vec<Device>,
    ) -> Result<()> {
        for d in linux.devices.iter() {
            let mut device_info = new_device_info(d, None, None)?;
            let device_id = self
                .device_manager
                .write()
                .await
                .try_add_device(&mut device_info, self.hypervisor.as_ref())
                .await?;
            let device = self
                .device_manager
                .read()
                .await
                .generate_agent_device(device_id)
                .await?;
            devices_agent.push(device);
        }
        return Ok(());
    }

    pub async fn update_cgroups(
        &self,
        cid: &str,
        linux_resources: Option<&LinuxResources>,
    ) -> Result<()> {
        self.cgroups_resource
            .update_cgroups(cid, linux_resources, self.hypervisor.as_ref())
            .await
    }

    pub async fn delete_cgroups(&self) -> Result<()> {
        self.cgroups_resource.delete().await
    }

    pub async fn dump(&self) {
        self.rootfs_resource.dump().await;
        self.volume_resource.dump().await;
    }
}

#[async_trait]
impl Persist for ResourceManagerInner {
    type State = ResourceState;
    type ConstructorArgs = ManagerArgs;

    /// Save a state of ResourceManagerInner
    async fn save(&self) -> Result<Self::State> {
        let mut endpoint_state = vec![];
        if let Some(network) = &self.network {
            if let Some(ens) = network.save().await {
                endpoint_state = ens;
            }
        }
        let cgroup_state = self.cgroups_resource.save().await?;
        Ok(ResourceState {
            endpoint: endpoint_state,
            cgroup_state: Some(cgroup_state),
        })
    }

    /// Restore ResourceManagerInner
    async fn restore(
        resource_args: Self::ConstructorArgs,
        resource_state: Self::State,
    ) -> Result<Self> {
        let args = CgroupArgs {
            sid: resource_args.sid.clone(),
            config: resource_args.config,
        };
        Ok(Self {
            sid: resource_args.sid,
            agent: resource_args.agent,
            hypervisor: resource_args.hypervisor,
            network: None,
            share_fs: None,
            rootfs_resource: RootFsResource::new(),
            volume_resource: VolumeResource::new(),
            cgroups_resource: CgroupsResource::restore(
                args,
                resource_state.cgroup_state.unwrap_or_default(),
            )
            .await?,
            toml_config: Arc::new(TomlConfig::default()),
            device_manager: Arc::new(RwLock::new(DeviceManager::new(VIRTIO_MMIO)?)),
        })
    }
}
