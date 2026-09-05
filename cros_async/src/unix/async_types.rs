// Copyright 2022 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::{Executor, IoSourceExt};
use base::{Tube, TubeError, TubeResult};
use serde::{de::DeserializeOwned, Serialize};
use std::io;
use std::ops::Deref;

pub struct AsyncTube {
    inner: Box<dyn IoSourceExt<Tube>>,
}

impl AsyncTube {
    pub fn new(ex: &Executor, tube: Tube) -> io::Result<AsyncTube> {
        return Ok(AsyncTube {
            inner: ex.async_from(tube)?,
        });
    }
    pub async fn next<T: DeserializeOwned>(&self) -> TubeResult<T> {
        // Propagate wait_readable() failures as a normal TubeResult error instead of
        // panicking. This can legitimately fail with an error derived from EINTR-style
        // interruptions of the underlying executor (e.g. io_uring_enter() interrupted by
        // the kernel process freezer during host-level suspend/resume), which should not
        // crash the worker thread driving this future.
        self.inner
            .wait_readable()
            .await
            .map_err(|e| TubeError::Recv(e.into()))?;
        self.inner.as_source().recv()
    }

    pub async fn send<T: 'static + Serialize + Send + Sync>(&self, msg: T) -> TubeResult<()> {
        self.inner.as_source().send(&msg)
    }
}

impl Deref for AsyncTube {
    type Target = Tube;

    fn deref(&self) -> &Self::Target {
        self.inner.as_source()
    }
}

impl From<AsyncTube> for Tube {
    fn from(at: AsyncTube) -> Tube {
        at.inner.into_source()
    }
}
