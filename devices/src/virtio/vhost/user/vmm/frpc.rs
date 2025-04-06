/*Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
SPDX-License-Identifier: BSD-3-Clause-Clear

Copyright 2021 The Chromium OS Authors. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above
      copyright notice, this list of conditions and the following disclaimer
      in the documentation and/or other materials provided with the
      distribution.
    * Neither the name of Google Inc. nor the names of its
      contributors may be used to endorse or promote products derived from
      this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
*/
use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::u32;
use data_model::Le64;

use base::{error, Event, RawDescriptor};
use virtio_sys::virtio_ring::{VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC };
use vm_memory::GuestMemory;
use vmm_vhost::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};

use crate::virtio::vhost::user::vmm::{handler::VhostUserHandler, worker::Worker, Error, Result};
use crate::virtio::{Interrupt, Queue, VirtioDevice, TYPE_QCOM_FRPC};

const QUEUE_SIZE: u16 = 16;
const VIRTIO_FASTRPC_F_VERSION: u32 = 5;
/* indicates domain num is available in config space */
const VIRTIO_FASTRPC_F_DOMAIN_NUM: u32 = 6;
const VIRTIO_FASTRPC_F_VQUEUE_SETTING: u32  = 7;
/* indicates fastrpc_mmap/fastrpc_munmap is supported */
const VIRTIO_FASTRPC_F_HYBRID: u32 = 9;
const VIRTIO_FASTRPC_F_DEVICE_DISCOVERY: u32 = 11;

pub struct Frpc {
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<Worker>>,
    handler: RefCell<VhostUserHandler>,
    queue_sizes: Vec<u16>,
}

impl Frpc {
    pub fn new<P: AsRef<Path>>(base_features: u64, socket_path: P) -> Result<Frpc> {
        let socket = UnixStream::connect(&socket_path).map_err(Error::SocketConnect)?;

      //Enable transport specific flags in VirtIO feature set defined by vhost-user
        let init_features = base_features | 1 << VIRTIO_FASTRPC_F_HYBRID | 1 << VIRTIO_FASTRPC_F_VERSION
      | 1 << VIRTIO_FASTRPC_F_DOMAIN_NUM | 1 << VIRTIO_FASTRPC_F_VQUEUE_SETTING |1 << VIRTIO_FASTRPC_F_DEVICE_DISCOVERY
      |VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

        let allow_features = init_features
            | 1u64 << crate::virtio::VIRTIO_F_VERSION_1
            | 1 << VIRTIO_RING_F_EVENT_IDX;
      // Support Virtio device configuration.
        let allow_protocol_features = VhostUserProtocolFeatures::CONFIG;

        let mut handler = VhostUserHandler::new_from_stream(
            socket,
            2, /* queues_num */
            allow_features,
            init_features,
            allow_protocol_features,
        )?;
        let queue_sizes = handler.queue_sizes(QUEUE_SIZE, 2)?;

        Ok(Frpc {
            kill_evt: None,
            worker_thread: None,
            handler: RefCell::new(handler),
            queue_sizes,
        })
    }
}

impl Drop for Frpc {
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

impl VirtioDevice for Frpc {
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

    fn device_type(&self) -> u32 {
        TYPE_QCOM_FRPC
    }

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
        if let Err(e) = self
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
            .name("vhost_user_frpc".to_string())
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
                error!("failed to spawn vhost-user-Frpc worker: {}", e);
            }
            Ok(join_handle) => {
                self.worker_thread = Some(join_handle);
            }
        }
    }

    fn reset(&mut self) -> bool {
        if let Err(e) = self.handler.borrow_mut().reset(self.queue_sizes.len()) {
            error!("Failed to reset Frpc device: {}", e);
            false
        } else {
            true
        }
    }

}
