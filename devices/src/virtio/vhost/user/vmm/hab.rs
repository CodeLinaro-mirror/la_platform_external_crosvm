// Copyright (c) 2023 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear

// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::u32;

use base::{info, error, Event, RawDescriptor};
use virtio_sys::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::GuestMemory;
use vmm_vhost::message::{MasterReq, VhostUserProtocolFeatures, VhostUserVirtioFeatures};

use vmm_vhost::{
    connection::socket::Endpoint as SocketEndpoint, Master, VhostBackend, VhostUserMaster,
    VhostUserMemoryRegionInfo, VringConfigData, VHOST_USER_F_PROTOCOL_FEATURES,
};

type SocketMaster = Master<SocketEndpoint<MasterReq>>;
use crate::virtio::vhost::user::vmm::{handler::VhostUserHandler, worker::Worker, Error, Result};

use crate::virtio::{ Interrupt, Queue, VirtioDevice};

const QUEUE_SIZE: u16 = 1024;

pub struct Hab {
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<Worker>>,
    handler: RefCell<VhostUserHandler>,
    queue_sizes: Vec<u16>,
    dev_id : u32,
    vu : SocketMaster,
}

impl Hab {
    pub fn new<P: AsRef<Path>>(base_features: u64, socket_path: P , device_id : u32,  numOfQueue : u32) -> Result<Hab>  {
         let socket = UnixStream::connect(&socket_path).map_err(Error::SocketConnect)?;
        let sock_copy = socket.try_clone().expect("Couldn't clone socket");
         let allow_features = 1u64 << crate::virtio::VIRTIO_F_VERSION_1
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()  ;

        let init_features = base_features | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let allow_protocol_features = VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ;
        let mut handler = VhostUserHandler::new_from_stream(
            socket,
            // TODO(b/181753022): Support multiple queues.
            numOfQueue.into(), /* queues_num */
            allow_features,
            init_features,
            allow_protocol_features,
        )?;
        let queue_sizes = handler.queue_sizes(QUEUE_SIZE, numOfQueue.try_into().unwrap())?;
         Ok(Hab {
            kill_evt: None,
            worker_thread: None,
            handler: RefCell::new(handler),
            queue_sizes,
             dev_id: device_id,
             vu: SocketMaster::from_stream(sock_copy, numOfQueue.into()),
        })
    }
}

impl Drop for Hab {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            let _ = worker_thread.join();
        }
    }
}

impl VirtioDevice for Hab {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        Vec::new()
    }

    fn features(&self) -> u64 {
        self.handler.borrow().avail_features

    }

    fn ack_features(&mut self, features: u64) {
        if let Err(e) = self.handler.borrow_mut().ack_features(features) {
            error!("failed to enable features 0x{:x}: {}", features, e);
        }
    }

    fn device_type(&self,) -> u32 {
        self.dev_id }

    fn queue_max_sizes(&self) -> &[u16] {
        self.queue_sizes.as_slice()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if let Err(e) = self.handler.borrow_mut().read_config(offset, data) {
            error!("failed to read config: {}", e);
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<Event>,
    ) {
        if let Err(e) =  self
            .handler
            .borrow_mut()
            .activate(&mem, &interrupt, &queues, &queue_evts)
        {
            error!("failed to activate queues: {}", e);
            return;
        }

        let (self_kill_evt, kill_evt) = match Event::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed creating kill Event pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);

        let worker_result = thread::Builder::new()
            .name("vhost_user_hab".to_string())
            .spawn(move || {
                let mut worker = Worker {
                    queues,
                    mem,
                    kill_evt,
                };

                if let Err(e) = worker.run(interrupt) {
                    error!("failed to start a worker: {}", e);
                }
                worker
            });

        match worker_result {
            Err(e) => {
                error!("failed to spawn vhost-user-scmi worker: {}", e);
            }
            Ok(join_handle) => {
                self.worker_thread = Some(join_handle);
            }
        }
    }

    fn reset(&mut self) -> bool {

        if let Err(e) = self.vu.reset_owner().map_err(Error::ResetOwner) {
            error!("Failed to reset HAB device: {}", e);
            false
        } else {
            true
        }
    }
}
