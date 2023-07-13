use lpc55_hal::{
    drivers::timer::Timer,
    peripherals::ctimer,
    raw::{USB1, USBHSH, USBPHY},
    time::DurationExtensions,
    traits::wg::timer::CountDown,
    typestates::init_state,
    Anactrl, Pmc, Syscon, Usbhs,
};

pub struct UsbHS {
    pub(crate) _phy: USBPHY,
    pub(crate) dev: USB1,
    pub(crate) _host: USBHSH,
}

impl UsbHS {
    pub fn new(
        usb: Usbhs,
        syscon: &mut Syscon,
        pmc: &mut Pmc,
        _anactrl: &Anactrl,
        timer: &mut Timer<impl ctimer::Ctimer<init_state::Enabled>>,
    ) -> Self {
        // SAFTEY: We can have two references to the same peripheral, there aren't any mut references alive
        let pmc_raw = unsafe { &lpc55_hal::raw::Peripherals::steal().PMC };
        // SAFTEY: We can have two references to the same peripheral, there aren't any mut references alive
        let anactrl_raw = unsafe { &lpc55_hal::raw::Peripherals::steal().ANACTRL };

        let _ = usb;
        let (mut phy, mut dev, mut host) = {
            // SAFTEY: The required peripherals were dropped above
            let pac = unsafe { lpc55_hal::raw::Peripherals::steal() };
            (pac.USBPHY, pac.USB1, pac.USBHSH)
        };

        // Reset devices
        syscon.reset(&mut host);
        syscon.reset(&mut dev);
        syscon.reset(&mut phy);

        // Briefly turn on host controller to enable device control of USB1 port
        syscon.enable_clock(&mut host);

        host.portmode.modify(|_, w| w.dev_enable().set_bit());

        syscon.disable_clock(&mut host);

        // Power on 32M crystal for HS PHY and connect to USB PLL
        pmc_raw
            .pdruncfg0
            .modify(|_, w| w.pden_xtal32m().poweredon());
        pmc_raw
            .pdruncfg0
            .modify(|_, w| w.pden_ldoxo32m().poweredon());
        anactrl_raw
            .xo32m_ctrl
            .modify(|_, w| w.enable_pll_usb_out().set_bit());

        pmc.power_on(&mut phy);

        // Give long delay for PHY to be ready
        timer.start((5u32 * 1000).microseconds());
        nb::block!(timer.wait()).ok();

        syscon.enable_clock(&mut phy);

        // Initial config of PHY control registers
        phy.ctrl.write(|w| w.sftrst().clear_bit());

        phy.pll_sic.modify(|_, w| {
            w.pll_div_sel()
                .bits(6) /* 16MHz = xtal32m */
                .pll_reg_enable()
                .set_bit()
        });

        phy.pll_sic_clr.write(|w| unsafe {
            // must be done, according to SDK.
            w.bits(1 << 16 /* mystery bit */)
        });

        // Must wait at least 15 us for pll-reg to stabilize
        timer.start(15u32.microseconds());
        nb::block!(timer.wait()).ok();

        phy.pll_sic
            .modify(|_, w| w.pll_power().set_bit().pll_en_usb_clks().set_bit());

        phy.ctrl.modify(|_, w| {
            w.clkgate()
                .clear_bit()
                .enautoclr_clkgate()
                .set_bit()
                .enautoclr_phy_pwd()
                .clear_bit()
        });

        // Turn on everything in PHY
        phy.pwd.write(|w| unsafe { w.bits(0) });

        // turn on USB1 device controller access
        syscon.enable_clock(&mut dev);

        //
        Self {
            _phy: phy,
            dev,
            _host: host,
        }
    }
}
