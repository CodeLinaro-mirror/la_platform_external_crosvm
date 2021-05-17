// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{INTERRUPT_STATUS_CONFIG_CHANGED, INTERRUPT_STATUS_USED_RING, VIRTIO_MSI_NO_VECTOR};
use crate::pci::MsixConfig;
use base::Event;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::rc::Rc;
use sync::Mutex;
use std::cell::RefCell;

pub struct InterruptBase {
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: Event,
    interrupt_resample_evt: Event,
    msix_config: Option<Arc<Mutex<MsixConfig>>>,
    config_msix_vector: u16,
}

impl InterruptBase {
    pub fn new(
        interrupt_status: Arc<AtomicUsize>,
        interrupt_evt: Event,
        interrupt_resample_evt: Event,
        msix_config: Option<Arc<Mutex<MsixConfig>>>,
        config_msix_vector: u16,
    ) -> InterruptBase {
        InterruptBase {
            interrupt_status,
            interrupt_evt,
            interrupt_resample_evt,
            msix_config,
            config_msix_vector,
        }
    }

    /// Make a shallow copy
    fn get_copy(&self) -> Self {
        Self {
               interrupt_status: self.interrupt_status.clone(),
               interrupt_evt: self.interrupt_evt.try_clone().unwrap(),
               interrupt_resample_evt: self.interrupt_resample_evt.try_clone().unwrap(),
               msix_config: self.msix_config.clone(),
               config_msix_vector: self.config_msix_vector.clone(),
        }
    }

    /// Virtqueue Interrupts From The Device
    ///
    /// If MSI-X is enabled in this device, MSI-X interrupt is preferred.
    /// Write to the irqfd to VMM to deliver virtual interrupt to the guest
    fn signal(&self, vector: u16, interrupt_status_mask: u32) {
        // Don't need to set ISR for MSI-X interrupts
        if let Some(msix_config) = &self.msix_config {
            let mut msix_config = msix_config.lock();
            if msix_config.enabled() {
                if vector != VIRTIO_MSI_NO_VECTOR {
                    msix_config.trigger(vector);
                }
                return;
            }
        }

        // Set bit in ISR and inject the interrupt if it was not already pending.
        // Don't need to inject the interrupt if the guest hasn't processed it.
        if self
            .interrupt_status
            .fetch_or(interrupt_status_mask as usize, Ordering::SeqCst)
            == 0
        {
            // Write to irqfd to inject INTx interrupt
            self.interrupt_evt.write(1).unwrap();
        }
    }
}

pub trait Interrupt : Send + Sync {
    fn signal_used_queue(&self, vector: u16);
    fn signal_config_changed(&self);
    fn interrupt_resample(&self);
    fn do_interrupt_resample(&self);
    fn get_resample_evt(&self) -> &Event;
    fn get_msix_config(&self) -> &Option<Arc<Mutex<MsixConfig>>>;
    fn try_clone_rc(&self) -> Rc<RefCell<dyn Interrupt>>;
    fn try_clone_arc(&self) -> Arc<dyn Interrupt>;
}

impl Interrupt for InterruptBase {

    /// Notify the driver that buffers have been placed in the used queue.
    fn signal_used_queue(&self, vector: u16) {
        self.signal(vector, INTERRUPT_STATUS_USED_RING)
    }

    /// Notify the driver that the device configuration has changed.
    fn signal_config_changed(&self) {
        self.signal(self.config_msix_vector, INTERRUPT_STATUS_CONFIG_CHANGED)
    }

    /// Handle interrupt resampling event, reading the value from the event and doing the resample.
    fn interrupt_resample(&self) {
        let _ = self.interrupt_resample_evt.read();
        self.do_interrupt_resample();
    }

    /// Read the status and write to the interrupt event. Don't read the resample event, assume the
    /// resample has been requested.
    fn do_interrupt_resample(&self) {
        if self.interrupt_status.load(Ordering::SeqCst) != 0 {
            self.interrupt_evt.write(1).unwrap();
        }
    }

    /// Return the reference of interrupt_resample_evt
    /// To keep the interface clean, this member is private.
    fn get_resample_evt(&self) -> &Event {
        &self.interrupt_resample_evt
    }

    // Return the reference of MsixConfig.
    fn get_msix_config(&self) -> &Option<Arc<Mutex<MsixConfig>>> {
        &self.msix_config
    }

    /// Get a clone in a reference counter.
    fn try_clone_rc(&self) -> Rc<RefCell<dyn Interrupt>>
    {
        Rc::new(RefCell::new(self.get_copy()))
    }

    /// Get a clone in a atomic reference counter.
    fn try_clone_arc(&self) -> Arc<dyn Interrupt>
    {
        Arc::new(self.get_copy())
    }
}
