//Copyright (c) 2023,2025 Qualcomm Innovation Center, Inc. All rights reserved.
//SPDX-License-Identifier: BSD-3-Clause-Clear

//Copyright 2021 The Chromium OS Authors. All rights reserved.

// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//    * Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//    * Redistributions in binary form must reproduce the above
// copyright notice, this list of conditions and the following disclaimer
// in the documentation and/or other materials provided with the
// distribution.
//    * Neither the name of Google Inc. nor the names of its
// contributors may be used to endorse or promote products derived from
// this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

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
use crate::virtio::{Interrupt, Queue, VirtioDevice, TYPE_QCOM_EAVB};
/* set queues_num to 2 for eavb*/
const QUEUE_SIZE: u16 = 16;
const VIRTIO_EAVB_F_VERSION: u32 = 5;
/* indicates domain num is available in config space */
const VIRTIO_EAVB_F_DOMAIN_NUM: u32 = 6;
const VIRTIO_EAVB_F_VQUEUE_SETTING: u32  = 7;
/* indicates fastrpc_mmap/fastrpc_munmap is supported */
const VIRTIO_EAVB_F_HYBRID: u32 = 9;

/*version should be 0x00010000*/
const VUEAVB_VERSION : u32 = 0x0001_0000;
/*domain_num should be 0x00000004*/
const VUEAVB_DOMAIN_NUM : u32 = 0x0000_0004;
/*max_buf_size should be 0x00001000(4K) for test as bounce buffer only have 16M*/
const VUEAVB_MAX_BUF_SZ : u32 = 0x0002_0000;

pub struct Eavb {
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<Worker>>,
    handler: RefCell<VhostUserHandler>,
    queue_sizes: Vec<u16>,
}

#[repr(C)]
struct vueavb_config_data {
   version: u32,
   domain_num: u32,
   max_buff_size: u32
}

impl vueavb_config_data {
    fn copy_cfg_space_at_offset(&self, target: &mut [u8], offset: u64) {
        let size_cdata = std::mem::size_of::<vueavb_config_data>() as u64;
      // Ensure that the target slice has enough capacity
        assert!(offset + size_cdata <= target.len() as u64);

      let mut loffset: usize = offset.try_into().expect(&format!("{}:{}", file!(), line!()));

      // Serialize the struct fields into the target slice
        let version_bytes = self.version.to_le_bytes();
      let len = version_bytes.len();
        target[loffset as usize..loffset as usize + len].copy_from_slice(&version_bytes);
      loffset = loffset + len;

        let domain_bytes = self.domain_num.to_le_bytes();
      let len2 = domain_bytes.len();
        target[loffset as usize..loffset as usize + len2].copy_from_slice(&domain_bytes);
      loffset = loffset + len2;

      let max_buff_size = self.max_buff_size.to_le_bytes();
      let len3 = max_buff_size.len();
        target[loffset as usize..loffset as usize + len3].copy_from_slice(&max_buff_size);
      loffset = loffset + len3;

        // Add serialization for other fields as needed
    }
}

impl Eavb {
    pub fn new<P: AsRef<Path>>(base_features: u64, socket_path: P) -> Result<Eavb> {
        let socket = UnixStream::connect(&socket_path).map_err(Error::SocketConnect)?;

         let init_features = base_features | 1 << VIRTIO_EAVB_F_HYBRID | 1 << VIRTIO_EAVB_F_VERSION
      | 1 << VIRTIO_EAVB_F_DOMAIN_NUM | 1 << VIRTIO_EAVB_F_VQUEUE_SETTING | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let allow_features = init_features
            | 1u64 << crate::virtio::VIRTIO_F_VERSION_1
            | 1 << VIRTIO_RING_F_EVENT_IDX;
        let allow_protocol_features = VhostUserProtocolFeatures::CONFIG;

        let mut handler = VhostUserHandler::new_from_stream(
            socket,
            2, /* queues_num */
            allow_features,
            init_features,
            allow_protocol_features,
        )?;
        let queue_sizes = handler.queue_sizes(QUEUE_SIZE, 2)?;

        Ok(Eavb {
            kill_evt: None,
            worker_thread: None,
            handler: RefCell::new(handler),
            queue_sizes,
        })
    }
}

impl Drop for Eavb {
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

impl VirtioDevice for Eavb {
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
        TYPE_QCOM_EAVB
    }

    fn queue_max_sizes(&self) -> &[u16] {
        self.queue_sizes.as_slice()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut vueavb_cspace = vueavb_config_data {
            version: VUEAVB_VERSION,
            domain_num: VUEAVB_DOMAIN_NUM,
            max_buff_size: VUEAVB_MAX_BUF_SZ,
        };

        // Copy the struct data to the byte array at a specific offset (e.g., at 256 offset)
        vueavb_cspace.copy_cfg_space_at_offset(data, offset);
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
            .activate(&mem, &interrupt, &queues, &queue_evts) {
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
            .name("vhost_user_eavb".to_string())
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
                error!("failed to spawn vhost-user-eavb worker: {}", e);
            }
            Ok(join_handle) => {
                self.worker_thread = Some(join_handle);
            }
        }
    }

    fn reset(&mut self) -> bool {
        if let Err(e) = self.handler.borrow_mut().reset(self.queue_sizes.len()) {
            error!("Failed to reset eavb device: {}", e);
            false
        } else {
            true
        }
    }
}
