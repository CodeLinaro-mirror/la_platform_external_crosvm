//Copyright (c) 2023-2024 Qualcomm Innovation Center, Inc. All rights reserved.
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
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;

use base::{error, Event, RawDescriptor};
use virtio_sys::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::GuestMemory;
use vmm_vhost::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};

use crate::virtio::vhost::user::vmm::{handler::VhostUserHandler, worker::Worker, Error, Result};
use crate::virtio::{Interrupt, Queue, VirtioDevice};

const PROTOCOL_FEATURES: u64 = 1 << 30;
const DISCOVERY_FEATURE: u64 = 1 << 40;
const VRING_PACKED: u64 = 1 << 34;

const GET_FEATURES: u32 = 1;
const SET_OWNER: u32 = 3;
const GET_PROTOCOL_FEATURES: u32 = 15;
const SET_PROTOCOL_FEATURES: u32 = 16;
const GET_CONFIG: u32 = 24;
const SET_CONFIG: u32 = 25;
const SET_STATUS: u32 = 39;

/* custom discovery protocol */
const DEVICE_ID: u32 = 5057; /* f: none; s: u64 */
const CONFIG_SIZE: u32 = 5058; /* f: none; s: u64 */
const QUEUE_NUM: u32 = 5059; /* f: none, s: u64 */
const QUEUE_SIZE: u32 = 5060; /* f: vring_state; s: vring_state */

pub struct GenericDevice {
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<Worker>>,
    handler: RefCell<VhostUserHandler>,
    queue_sizes: Vec<u16>,
    device_id: u32,
    config_len: u64,
    socket: RefCell<UnixStream>,
}

fn send_request(socket: &mut UnixStream, request: u32, data: &[u8]) -> Result<()> {
    send_request_io(socket, request, data).map_err(Error::SocketConnect)
}

fn send_request_io(socket: &mut UnixStream, request: u32, data: &[u8]) -> std::io::Result<()> {
    send_flags_io(socket, request, 1, data)
}

fn send_flags_io(
    socket: &mut UnixStream,
    request: u32,
    flags: u32,
    data: &[u8],
) -> std::io::Result<()> {
    let mut message = Vec::with_capacity(256);
    message.write_all(&request.to_ne_bytes())?;
    message.write_all(&flags.to_ne_bytes())?;
    let size = data.len() as u32;
    message.write_all(&size.to_ne_bytes())?;
    message.write_all(data)?;
    socket.write_all(&message)
}

fn send_and_recv(socket: &mut UnixStream, request: u32, data: &[u8]) -> Result<Vec<u8>> {
    send_and_recv_io(socket, request, data).map_err(Error::SocketConnect)
}

fn send_and_recv_io(
    socket: &mut UnixStream,
    request: u32,
    data: &[u8],
) -> std::io::Result<Vec<u8>> {
    send_request_io(socket, request, data)?;
    let mut hdr = [0u8; 12];
    socket.read_exact(&mut hdr)?;
    // check that request matches
    if &hdr[0..4] != &request.to_ne_bytes() {
        return Err(str_as_error("wrong reply"));
    }
    // flags should be 5 (reply flag set)
    if &hdr[4..8] != &5u32.to_ne_bytes() {
        return Err(str_as_error("wrong flags in reply"));
    }
    let size = u32::from_ne_bytes(hdr[8..12].try_into().unwrap());
    let mut data = vec![0u8; size as usize];
    socket.read_exact(&mut data)?;
    Ok(data)
}

fn send_and_ack(socket: &mut UnixStream, request: u32, data: &[u8]) -> Result<u64> {
    send_and_ack_io(socket, request, data).map_err(Error::SocketConnect)
}

fn send_and_ack_io(socket: &mut UnixStream, request: u32, data: &[u8]) -> std::io::Result<u64> {
    send_flags_io(socket, request, 9, data)?;
    let mut hdr = [0u8; 12];
    socket.read_exact(&mut hdr)?;
    // check that request matches
    if &hdr[0..4] != &request.to_ne_bytes() {
        return Err(str_as_error("wrong reply"));
    }
    // flags should be 5 (reply flag set)
    if &hdr[4..8] != &5u32.to_ne_bytes() {
        return Err(str_as_error("wrong flags in reply"));
    }
    if &hdr[8..12] != &8u32.to_ne_bytes() {
        return Err(str_as_error("wrong size of ack"));
    }
    let mut status = [0u8; 8];
    socket.read_exact(&mut status)?;
    let status = u64::from_ne_bytes(status);
    Ok(status)
}

fn get_u64(socket: &mut UnixStream, request: u32) -> Result<u64> {
    let reply = send_and_recv(socket, request, &[])?;
    if reply.len() == 8 {
        Ok(u64::from_ne_bytes(reply.try_into().unwrap()))
    } else {
        Err(Error::SocketConnect(str_as_error(
            "wrong reply, expected u64",
        )))
    }
}

fn get_queue_size(socket: &mut UnixStream, vqn: u32) -> Result<u16> {
    let mut data = [0u8; 8];
    data[0..4].copy_from_slice(&vqn.to_ne_bytes());
    let reply = send_and_recv(socket, QUEUE_SIZE, &data)?;
    if reply.len() != 8 {
        Err(Error::SocketConnect(str_as_error("wrong reply size")))
    } else if &reply[0..4] != &data[0..4] {
        Err(Error::SocketConnect(str_as_error(
            "wrong queue num in reply",
        )))
    } else {
        Ok(u32::from_ne_bytes(reply[4..8].try_into().unwrap()) as u16)
    }
}

fn str_as_error(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, message)
}

fn get_config(socket: &mut UnixStream, offset: u32, data: &mut [u8]) -> Result<()> {
    let mut payload = vec![0u8; 12 + data.len()];
    payload[0..4].copy_from_slice(&offset.to_ne_bytes());
    payload[4..8].copy_from_slice(&(data.len() as u32).to_ne_bytes());
    let reply = send_and_recv(socket, GET_CONFIG, &payload)?;
    if reply.len() != 12 + data.len() {
        Err(Error::SocketConnect(str_as_error("wrong reply size")))
    } else if &reply[..12] != &payload[..12] {
        Err(Error::SocketConnect(str_as_error(
            "wrong config offset/size/flags in reply",
        )))
    } else {
        data.copy_from_slice(&reply[12..]);
        Ok(())
    }
}

fn set_config(socket: &mut UnixStream, offset: u32, data: &[u8]) -> Result<()> {
    let mut payload = vec![0u8; 12 + data.len()];
    payload[0..4].copy_from_slice(&offset.to_ne_bytes());
    payload[4..8].copy_from_slice(&(data.len() as u32).to_ne_bytes());
    payload[12..].copy_from_slice(data);
    send_request(socket, SET_CONFIG, &payload)
}

impl GenericDevice {
    pub fn new<P: AsRef<Path>>(
        base_features: u64,
        socket_path: P,
        num_queues: Option<u64>,
    ) -> Result<Self> {
        let mut socket = UnixStream::connect(socket_path.as_ref()).map_err(Error::SocketConnect)?;

        send_request(&mut socket, SET_OWNER, &[])?;

        let features = get_u64(&mut socket, GET_FEATURES)?;
        if features & PROTOCOL_FEATURES == 0 {
            return Err(Error::SocketConnect(str_as_error(
                "protocol features are not supported",
            )));
        }

        let protocol_features = get_u64(&mut socket, GET_PROTOCOL_FEATURES)?;
        if protocol_features & DISCOVERY_FEATURE == 0 {
            return Err(Error::SocketConnect(str_as_error(
                "discovery feature is not supported",
            )));
        }
        send_request(
            &mut socket,
            SET_PROTOCOL_FEATURES,
            &DISCOVERY_FEATURE.to_ne_bytes(),
        )?;
        let device_id = get_u64(&mut socket, DEVICE_ID)? as u32;
        let num_queues = num_queues.map_or_else(|| get_u64(&mut socket, QUEUE_NUM), Ok)? as usize;
        let config_len = get_u64(&mut socket, CONFIG_SIZE)?;

        let queue_sizes = (0..num_queues)
            .map(|vqn| get_queue_size(&mut socket, vqn as u32))
            .collect::<Result<Vec<_>>>()?;

        let init_features = base_features | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let allow_features = init_features | (features & !(VRING_PACKED));
        let allow_protocol_features = VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::STATUS
            | VhostUserProtocolFeatures::REPLY_ACK;

        let mut handler = VhostUserHandler::new_from_stream(
            socket.try_clone().map_err(Error::SocketConnect)?,
            queue_sizes.len() as u64, /* queues_num */
            allow_features,
            init_features,
            allow_protocol_features,
        )?;

        Ok(Self {
            kill_evt: None,
            worker_thread: None,
            handler: RefCell::new(handler),
            queue_sizes,
            device_id,
            config_len,
            socket: RefCell::new(socket),
        })
    }
}

impl Drop for GenericDevice {
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

impl VirtioDevice for GenericDevice {
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
        self.device_id
    }

    fn queue_max_sizes(&self) -> &[u16] {
        self.queue_sizes.as_slice()
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let len = if offset < self.config_len {
            (self.config_len - offset) as usize
        } else {
            return;
        };
        if let Err(e) = get_config(
            &mut self.socket.borrow_mut(),
            offset.try_into().unwrap(),
            &mut data[..len],
        ) {
            error!("failed to read config: {}", e);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let len = if offset < self.config_len {
            (self.config_len - offset) as usize
        } else {
            return;
        };
        if let Err(e) = set_config(
            &mut self.socket.borrow_mut(),
            offset.try_into().unwrap(),
            &data[..len],
        ) {
            error!("failed to write config: {}", e);
            eprintln!("failed to write config: {}", e);
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

        send_request(
            &mut self.socket.borrow_mut(),
            SET_STATUS,
            &0xfu64.to_ne_bytes(),
        )
        .unwrap_or_default();

        let (self_kill_evt, kill_evt) = match Event::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed creating kill Event pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);
        let device_id = self.device_id;

        let worker_result = thread::Builder::new()
            .name(format!("generic {}", self.device_id))
            .spawn(move || {
                let mut worker = Worker {
                    queues,
                    mem,
                    kill_evt,
                };

                if let Err(e) = worker.run(interrupt) {
                    error!("failed to start generic device {} worker: {}", device_id, e);
                }
                worker
            });

        match worker_result {
            Err(e) => {
                error!(
                    "failed to spawn vhost-user-generic {} worker: {}",
                    self.device_id, e
                );
            }
            Ok(join_handle) => self.worker_thread = Some(join_handle),
        }
    }

    fn reset(&mut self) -> bool {
        if let Err(e) = send_and_ack(
            &mut self.socket.borrow_mut(),
            SET_STATUS,
            &0u64.to_ne_bytes(),
        ) {
            error!("Failed to reset generic device {}: {}", self.device_id, e);
            return false;
        }
        if let Err(e) = send_request(
            &mut self.socket.borrow_mut(),
            SET_STATUS,
            &3u64.to_ne_bytes(),
        ) {
            error!("Failed to reset generic device {}: {}", self.device_id, e);
            return false;
        }
        true
    }
}
