// Copyright (c) 2023 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear
use std::{path::PathBuf, thread};

use data_model::{DataInit, Le64};

use base::{error, warn, AsRawDescriptor, Event, RawDescriptor};
use vhost::Scmi as VhostScmiHandle;
use vhost::Vhost;
use vm_memory::GuestMemory;

use super::worker::Worker;
use super::{Error, Result};
use crate::virtio::{copy_config, Interrupt, Queue, VirtioDevice, TYPE_SCMI};

const QUEUE_SIZE: u16 = 128;
const NUM_QUEUES: usize = 2;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE; NUM_QUEUES];
const VIRTIO_SCMI_F_P2A_CHANNELS: u32 = 0;

pub struct Scmi {
    worker_kill_evt: Option<Event>,
    kill_evt: Option<Event>,
    vhost_handle: Option<VhostScmiHandle>,
    interrupts: Option<Vec<Event>>,
    avail_features: u64,
    acked_features: u64,
}

impl Scmi {
    /// Create a new virtio-scmi device.
    pub fn new(vhost_scmi_device_path: &PathBuf, base_features: u64) -> Result<Scmi> {
        let kill_evt = Event::new().map_err(Error::CreateKillEvent)?;
        let handle = VhostScmiHandle::new(vhost_scmi_device_path).map_err(Error::VhostOpen)?;

        let avail_features = base_features | 1 << VIRTIO_SCMI_F_P2A_CHANNELS;

        let mut interrupts = Vec::new();
        for _ in 0..NUM_QUEUES {
            interrupts.push(Event::new().map_err(Error::VhostIrqCreate)?);
        }

        Ok(Scmi {
            worker_kill_evt: Some(kill_evt.try_clone().map_err(Error::CloneKillEvent)?),
            kill_evt: Some(kill_evt),
            vhost_handle: Some(handle),
            interrupts: Some(interrupts),
            avail_features,
            acked_features: 0,
        })
    }

    pub fn acked_features(&self) -> u64 {
        self.acked_features
    }
}

impl Drop for Scmi {
    fn drop(&mut self) {
        // Only kill the child if it claimed its event.
        if self.worker_kill_evt.is_none() {
            if let Some(kill_evt) = &self.kill_evt {
                // Ignore the result because there is nothing we can do about it.
                let _ = kill_evt.write(1);
            }
        }
    }
}

impl VirtioDevice for Scmi {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = Vec::new();

        if let Some(handle) = &self.vhost_handle {
            keep_rds.push(handle.as_raw_descriptor());
        }

        if let Some(interrupt) = &self.interrupts {
            for vhost_int in interrupt.iter() {
                keep_rds.push(vhost_int.as_raw_descriptor());
            }
        }

        if let Some(worker_kill_evt) = &self.worker_kill_evt {
            keep_rds.push(worker_kill_evt.as_raw_descriptor());
        }

        keep_rds
    }

    fn device_type(&self) -> u32 {
        TYPE_SCMI
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        self.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        let mut v = value;

        // Check if the guest is ACK'ing a feature that we didn't claim to have.
        let unrequested_features = v & !self.avail_features;
        if unrequested_features != 0 {
            warn!("scmi: virtio-scmi got unknown feature ack: {:x}", v);

            // Don't count these features as acked.
            v &= !unrequested_features;
        }
        self.acked_features |= v;
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<Event>,
    ) {
        if queues.len() != NUM_QUEUES || queue_evts.len() != NUM_QUEUES {
            return;
        }
        if let Some(vhost_handle) = self.vhost_handle.take() {
            if let Some(interrupts) = self.interrupts.take() {
                if let Some(kill_evt) = self.worker_kill_evt.take() {
                    let _ = vhost_handle.set_owner();
                    let acked_features = self.acked_features;

                    let vhost_queues = queues[..NUM_QUEUES].to_vec();
                    let mut worker = Worker::new(
                        vhost_queues,
                        vhost_handle,
                        interrupts,
                        interrupt,
                        acked_features,
                        kill_evt,
                        None,
                    );
                    let activate_vqs = |_handle: &VhostScmiHandle| -> Result<()> {
                        Ok(())
                    };

                    let result = worker.init(mem, queue_evts, QUEUE_SIZES, activate_vqs);
                    if let Err(e) = result {
                        error!("scmi worker thread exited with error: {:?}", e);
                    }

                    let worker_result = thread::Builder::new()
                        .name("vhost_scmi".to_string())
                        .spawn(move || {
                            let cleanup_vqs = |_handle: &VhostScmiHandle| -> Result<()> { Ok(()) };
                            let result = worker.run(cleanup_vqs);
                            if let Err(e) = result {
                                error!("scmi worker thread exited with error: {:?}", e);
                            }
                        });

                    if let Err(e) = worker_result {
                        println!("failed to spawn vhost_scmi worker: {}", e);
                        return;
                    }
                }
            }
        }
    }

    fn on_device_sandboxed(&mut self) {
        // ignore the error but to log the error. We don't need to do
        // anything here because when activate, the other vhost set up
        // will be failed to stop the activate thread.
        if let Some(vhost_handle) = &self.vhost_handle {
            match vhost_handle.set_owner() {
                Ok(_) => {}
                Err(e) => error!("{}: failed to set owner: {:?}", self.debug_label(), e),
            }
        }
    }
}
