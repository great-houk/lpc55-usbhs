use crate::{
    hal::{
        constants::{EP_MEM_ADDR, NUM_ENDPOINTS},
        endpoint::Endpoint,
        endpoint_memory::EndpointMemoryAllocator,
        endpoint_registers,
    },
    usbhs::UsbHS,
};
use cortex_m::interrupt::{self, Mutex};
use usb_device::{
    bus::{PollResult, UsbBus},
    class_prelude::UsbBusAllocator,
    endpoint::{EndpointAddress, EndpointType},
    Result, UsbDirection, UsbError,
};

pub struct UsbHSBus {
    usb_regs: Mutex<UsbHS>,
    ep_regs: Mutex<endpoint_registers::Instance>,
    endpoints: [Endpoint; NUM_ENDPOINTS],
    ep_allocator: EndpointMemoryAllocator,
    max_endpoint: usize,
}

impl UsbHSBus {
    pub fn new(usb_device: UsbHS) -> UsbBusAllocator<UsbHSBus> {
        let bus = UsbHSBus {
            usb_regs: Mutex::new(usb_device),
            ep_regs: Mutex::new(endpoint_registers::attach().unwrap()),
            ep_allocator: EndpointMemoryAllocator::new(),
            max_endpoint: 0,
            endpoints: {
                let mut endpoints: [core::mem::MaybeUninit<Endpoint>; NUM_ENDPOINTS] =
                    unsafe { core::mem::MaybeUninit::uninit().assume_init() };

                for (i, endpoint) in endpoints.iter_mut().enumerate() {
                    *endpoint = core::mem::MaybeUninit::new(Endpoint::new(i as u8));
                }

                unsafe { core::mem::transmute::<_, _>(endpoints) }
            },
        };

        UsbBusAllocator::new(bus)
    }
}

impl UsbBus for UsbHSBus {
    // override the default (contrary to USB spec),
    // as describe in the user manual
    const QUIRK_SET_ADDRESS_BEFORE_STATUS: bool = true;

    fn alloc_ep(
        &mut self,
        ep_dir: UsbDirection,
        ep_addr: Option<EndpointAddress>,
        ep_type: EndpointType,
        max_packet_size: u16,
        _interval: u8,
    ) -> Result<EndpointAddress> {
        let addr_range = if let Some(addr) = ep_addr {
            addr.index()..addr.index() + 1
        } else {
            1..NUM_ENDPOINTS
        };

        for index in addr_range {
            let ep = &mut self.endpoints[index];

            match ep.ep_type() {
                None => {
                    ep.set_ep_type(ep_type);
                }
                Some(t) if t != ep_type => {
                    continue;
                }
                _ => {}
            };

            match ep_dir {
                UsbDirection::Out if !ep.is_out_buf_set() => {
                    let mut size = max_packet_size;
                    // ZLP NYET Fix
                    if index == 0 {
                        size += 1;
                    }
                    let buffer = self.ep_allocator.allocate_buffer(size as _)?;
                    ep.set_out_buf(buffer);
                    debug_assert!(ep.is_out_buf_set());

                    if index == 0 {
                        let setup = self.ep_allocator.allocate_buffer(8)?;
                        ep.set_setup_buf(setup);
                    }

                    return Ok(EndpointAddress::from_parts(index, ep_dir));
                }

                UsbDirection::In if !ep.is_in_buf_set() => {
                    let size = max_packet_size;
                    let buffer = self.ep_allocator.allocate_buffer(size as _)?;
                    ep.set_in_buf(buffer);

                    return Ok(EndpointAddress::from_parts(index, ep_dir));
                }

                _ => {}
            }
        }

        Err(match ep_addr {
            Some(_) => UsbError::InvalidEndpoint,
            None => UsbError::EndpointOverflow,
        })
    }

    fn enable(&mut self) {
        interrupt::free(|cs| {
            let usb = self.usb_regs.borrow(cs);
            let eps = self.ep_regs.borrow(cs);

            let mut max = 0;
            for (index, ep) in self.endpoints.iter().enumerate() {
                if ep.is_out_buf_set() || ep.is_in_buf_set() {
                    max = index;

                    // not sure this is needed
                    if ep.is_out_buf_set() {
                        ep.reset_out_buf(cs, eps);
                        if index == 0 {
                            ep.reset_setup_buf(cs, eps);
                        }
                        // ep.enable_out_interrupt(usb);
                    }
                    if ep.is_in_buf_set() {
                        ep.reset_in_buf(cs, eps);
                        // ep.enable_in_interrupt(usb);
                    }
                }
            }
            self.max_endpoint = max;

            // DATABUFSTART
            unsafe {
                // lower part is stored in endpoint registers
                let databufstart = EP_MEM_ADDR as u32;
                usb.dev
                    .databufstart
                    .modify(|_, w| w.da_buf().bits(databufstart));
            };

            // EPLISTSTART
            unsafe {
                let epliststart = eps.addr;
                debug_assert!(epliststart as u8 == 0); // needs to be 256 byte aligned
                usb.dev
                    .epliststart
                    .modify(|_, w| w.ep_list().bits(epliststart >> 8));
            }

            // Clear PHY gate
            usb.phy.ctrl_clr.write(|w| w.clkgate().set_bit());

            // ENABLE + CONNECT
            usb.dev
                .devcmdstat
                .modify(|_, w| w.dev_en().set_bit().dcon().set_bit());

            // Enable Interrupts
            usb.dev
                .inten
                .modify(|r, w| unsafe { w.bits(r.bits() | ((1 << 11) - 1)) });
            usb.dev.inten.modify(|_, w| w.dev_int_en().set_bit());
        });
    }

    fn reset(&self) {
        interrupt::free(|cs| {
            // Set device address to 0
            let usb = self.usb_regs.borrow(cs);
            let eps = self.ep_regs.borrow(cs);
            usb.dev
                .devcmdstat
                .modify(|_, w| unsafe { w.dev_addr().bits(0) });

            // Reset EPs
            for ep in self.endpoints.iter() {
                ep.configure(cs, &usb.dev, eps);
            }

            // Clear all interrupts
            usb.dev.intstat.write(|w| unsafe { w.bits(!0) });
        });
    }

    fn set_device_address(&self, addr: u8) {
        interrupt::free(|cs| {
            self.usb_regs
                .borrow(cs)
                .dev
                .devcmdstat
                .modify(|_, w| unsafe { w.dev_addr().bits(addr) });
        });
    }

    fn poll(&self) -> PollResult {
        interrupt::free(|cs| {
            let usb = self.usb_regs.borrow(cs);
            let eps = self.ep_regs.borrow(cs);

            let devcmdstat = &usb.dev.devcmdstat;
            let intstat = &usb.dev.intstat;

            // Bus reset flag?
            if devcmdstat.read().dres_c().bit_is_set() {
                devcmdstat.modify(|_, w| w.dres_c().set_bit());
                return PollResult::Reset;
            }

            // Suspend flag
            if devcmdstat.read().dsus_c().bit_is_set() || devcmdstat.read().lpm_sus().bit_is_set() {
                return PollResult::Suspend;
            }

            let mut ep_out = 0;
            let mut ep_in_complete = 0;
            let mut ep_setup = 0;

            let mut bit = 1;

            // NB: these are not "reader objects", but the actual value
            // of the registers at time of assignment :))
            let intstat_r = intstat.read();

            // First handle endpoint 0 (the only control endpoint)
            if intstat_r.ep0out().bit_is_set() {
                if devcmdstat.read().setup().bit_is_set() {
                    ep_setup |= bit;
                } else {
                    ep_out |= bit;
                }
            }

            if intstat_r.ep0in().bit_is_set() {
                intstat.write(|w| w.ep0in().set_bit());
                ep_in_complete |= bit;

                // EP0 needs manual toggling of Active bits
                // Weeelll interesting, not changing this makes no difference
                eps.eps[0].ep_in[0].modify(|_, w| w.a().not_active());
            }

            // non-CONTROL
            for ep in &self.endpoints[1..=self.max_endpoint] {
                bit <<= 1;
                let i = ep.index() as usize;

                // OUT = READ
                let out_offset = 2 * i;
                let out_int = ((intstat_r.bits() >> out_offset) & 0x1) != 0;
                let out_inactive = eps.eps[i].ep_out[0].read().a().is_not_active();

                if out_int {
                    debug_assert!(out_inactive);
                    ep_out |= bit;
                    // EXPERIMENTAL: clear interrupt
                    // usb.intstat.write(|w| unsafe { w.bits(1u32 << out_offset) } );

                    // let err_code = usb.info.read().err_code().bits();
                    // let addr_set = devcmdstat.read().dev_addr().bits() > 0;
                    // if addr_set && err_code > 0 {
                    //     hprintln!("error {}", err_code).ok();
                    // }
                }

                // IN = WRITE
                let in_offset = 2 * i + 1;
                let in_int = ((intstat_r.bits() >> in_offset) & 0x1) != 0;
                // WHYY is this sometimes still active?
                let in_inactive = eps.eps[i].ep_in[0].read().a().is_not_active();
                if in_int && !in_inactive {
                    // cortex_m_semihosting::hprintln!(
                    //     "IN is active for EP {}, but an IN interrupt fired", i,
                    // ).ok();
                    // cortex_m_semihosting::hprintln!(
                    //     "IntOnNAK_AI = {}, IntOnNAK_AO = {}",
                    //     devcmdstat.read().intonnak_ai().is_enabled(),
                    //     devcmdstat.read().intonnak_ao().is_enabled(),
                    // ).ok();

                    // debug_assert!(in_inactive);
                }
                if in_int && in_inactive {
                    ep_in_complete |= bit;
                    // clear it
                    usb.dev
                        .intstat
                        .write(|w| unsafe { w.bits(1u32 << in_offset) });
                    debug_assert!(eps.eps[i].ep_in[0].read().a().is_not_active());

                    // let err_code = usb.info.read().err_code().bits();
                    // let addr_set = devcmdstat.read().dev_addr().bits() > 0;
                    // if addr_set && err_code > 0 {
                    //     hprintln!("error {}", err_code).ok();
                    // }
                };
            }

            usb.dev.intstat.write(|w| w.dev_int().set_bit());
            if (ep_out | ep_in_complete | ep_setup) != 0 {
                PollResult::Data {
                    ep_out,
                    ep_in_complete,
                    ep_setup,
                }
            } else {
                PollResult::None
            }
        })
    }

    fn read(&self, ep_addr: EndpointAddress, buf: &mut [u8]) -> Result<usize> {
        if !ep_addr.is_out() {
            return Err(UsbError::InvalidEndpoint);
        }

        interrupt::free(|cs| {
            let usb = self.usb_regs.borrow(cs);
            let eps = self.ep_regs.borrow(cs);
            self.endpoints[ep_addr.index()].read(buf, cs, &usb.dev, eps)
        })
    }

    fn write(&self, ep_addr: EndpointAddress, buf: &[u8]) -> Result<usize> {
        if !ep_addr.is_in() {
            return Err(UsbError::InvalidEndpoint);
        }

        interrupt::free(|cs| {
            let eps = self.ep_regs.borrow(cs);
            self.endpoints[ep_addr.index()].write(buf, cs, eps)
        })
    }

    fn set_stalled(&self, ep_addr: EndpointAddress, stalled: bool) {
        interrupt::free(|cs| {
            if self.is_stalled(ep_addr) == stalled {
                return;
            }

            let i = ep_addr.index();
            let ep = &self.ep_regs.borrow(cs).eps[i];

            if i > 0 {
                match ep_addr.direction() {
                    UsbDirection::In => while ep.ep_in[0].read().a().is_active() {},
                    UsbDirection::Out => while ep.ep_out[0].read().a().is_active() {},
                }
            }

            match (stalled, ep_addr.direction()) {
                (true, UsbDirection::In) => ep.ep_in[0].modify(|_, w| w.s().stalled()),
                (true, UsbDirection::Out) => ep.ep_out[0].modify(|_, w| w.s().stalled()),

                (false, UsbDirection::In) => ep.ep_in[0].modify(|_, w| w.s().not_stalled()),
                (false, UsbDirection::Out) => ep.ep_out[0].modify(|_, w| w.s().not_stalled()),
            };
        });
    }

    fn is_stalled(&self, ep_addr: EndpointAddress) -> bool {
        interrupt::free(|cs| {
            let ep = &self.ep_regs.borrow(cs).eps[ep_addr.index()];
            match ep_addr.direction() {
                UsbDirection::In => ep.ep_in[0].read().s().is_stalled(),
                UsbDirection::Out => ep.ep_out[0].read().s().is_stalled(),
            }
        })
    }

    fn suspend(&self) {}

    fn resume(&self) {
        interrupt::free(|cs| {
            let usb = self.usb_regs.borrow(cs);
            let devcmdstat = &usb.dev.devcmdstat;

            if devcmdstat.read().lpm_rewp().bit_is_set() {
                devcmdstat.modify(|_, w| w.lpm_sus().clear_bit());
            }
            devcmdstat.modify(|_, w| w.dsus().clear_bit());
        });
    }
}
