// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2018-2020 Andre Richter <andre.o.richter@gmail.com>

//! PL011 UART driver.

use crate::{
    bsp, bsp::device_driver::common::MMIODerefWrapper, console, cpu, driver, exception, memory,
    memory::mmu::Physical, synchronization, synchronization::IRQSafeNullLock,
};
use core::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};
use register::{mmio::*, register_bitfields, register_structs};

//--------------------------------------------------------------------------------------------------
// Private Definitions
//--------------------------------------------------------------------------------------------------

// PL011 UART registers.
//
// Descriptions taken from
// https://github.com/raspberrypi/documentation/files/1888662/BCM2837-ARM-Peripherals.-.Revised.-.V2-1.pdf
register_bitfields! {
    u32,

    /// Flag Register
    FR [
        /// Transmit FIFO empty. The meaning of this bit depends on the state of the FEN bit in the
        /// Line Control Register, UARTLCR_ LCRH.
        ///
        /// If the FIFO is disabled, this bit is set when the transmit holding register is empty. If
        /// the FIFO is enabled, the TXFE bit is set when the transmit FIFO is empty. This bit does
        /// not indicate if there is data in the transmit shift register.
        TXFE OFFSET(7) NUMBITS(1) [],

        /// Transmit FIFO full. The meaning of this bit depends on the state of the FEN bit in the
        /// UARTLCR_ LCRH Register.
        ///
        /// If the FIFO is disabled, this bit is set when the transmit holding register is full. If
        /// the FIFO is enabled, the TXFF bit is set when the transmit FIFO is full.
        TXFF OFFSET(5) NUMBITS(1) [],

        /// Receive FIFO empty. The meaning of this bit depends on the state of the FEN bit in the
        /// UARTLCR_H Register.
        ///
        /// If the FIFO is disabled, this bit is set when the receive holding register is empty. If
        /// the FIFO is enabled, the RXFE bit is set when the receive FIFO is empty.
        RXFE OFFSET(4) NUMBITS(1) []
    ],

    /// Integer Baud rate divisor
    IBRD [
        /// Integer Baud rate divisor
        IBRD OFFSET(0) NUMBITS(16) []
    ],

    /// Fractional Baud rate divisor
    FBRD [
        /// Fractional Baud rate divisor
        FBRD OFFSET(0) NUMBITS(6) []
    ],

    /// Line Control register
    LCRH [
        /// Word length. These bits indicate the number of data bits transmitted or received in a
        /// frame.
        WLEN OFFSET(5) NUMBITS(2) [
            FiveBit = 0b00,
            SixBit = 0b01,
            SevenBit = 0b10,
            EightBit = 0b11
        ],

        /// Enable FIFOs:
        ///
        /// 0 = FIFOs are disabled (character mode) that is, the FIFOs become 1-byte-deep holding
        /// registers
        ///
        /// 1 = transmit and receive FIFO buffers are enabled (FIFO mode).
        FEN  OFFSET(4) NUMBITS(1) [
            FifosDisabled = 0,
            FifosEnabled = 1
        ]
    ],

    /// Control Register
    CR [
        /// Receive enable. If this bit is set to 1, the receive section of the UART is enabled.
        /// Data reception occurs for UART signals. When the UART is disabled in the middle of
        /// reception, it completes the current character before stopping.
        RXE    OFFSET(9) NUMBITS(1) [
            Disabled = 0,
            Enabled = 1
        ],

        /// Transmit enable. If this bit is set to 1, the transmit section of the UART is enabled.
        /// Data transmission occurs for UART signals. When the UART is disabled in the middle of
        /// transmission, it completes the current character before stopping.
        TXE    OFFSET(8) NUMBITS(1) [
            Disabled = 0,
            Enabled = 1
        ],

        /// UART enable
        UARTEN OFFSET(0) NUMBITS(1) [
            /// If the UART is disabled in the middle of transmission or reception, it completes the
            /// current character before stopping.
            Disabled = 0,
            Enabled = 1
        ]
    ],

    /// Interrupt FIFO Level Select Register
    IFLS [
        /// Receive interrupt FIFO level select. The trigger points for the receive interrupt are as
        /// follows.
        RXIFLSEL OFFSET(3) NUMBITS(5) [
            OneEigth = 0b000,
            OneQuarter = 0b001,
            OneHalf = 0b010,
            ThreeQuarters = 0b011,
            SevenEights = 0b100
        ]
    ],

    /// Interrupt Mask Set Clear Register
    IMSC [
        /// Receive timeout interrupt mask. A read returns the current mask for the UARTRTINTR
        /// interrupt. On a write of 1, the mask of the interrupt is set. A write of 0 clears the
        /// mask.
        RTIM OFFSET(6) NUMBITS(1) [
            Disabled = 0,
            Enabled = 1
        ],

        /// Receive interrupt mask. A read returns the current mask for the UARTRXINTR interrupt. On
        /// a write of 1, the mask of the interrupt is set. A write of 0 clears the mask.
        RXIM OFFSET(4) NUMBITS(1) [
            Disabled = 0,
            Enabled = 1
        ]
    ],

    /// Masked Interrupt Status Register
    MIS [
        /// Receive timeout masked interrupt status. Returns the masked interrupt state of the
        /// UARTRTINTR interrupt.
        RTMIS OFFSET(6) NUMBITS(1) [],

        /// Receive masked interrupt status. Returns the masked interrupt state of the UARTRXINTR
        /// interrupt.
        RXMIS OFFSET(4) NUMBITS(1) []
    ],

    /// Interrupt Clear Register
    ICR [
        /// Meta field for all pending interrupts
        ALL OFFSET(0) NUMBITS(11) []
    ]
}

register_structs! {
    #[allow(non_snake_case)]
    pub RegisterBlock {
        (0x00 => DR: ReadWrite<u32>),
        (0x04 => _reserved1),
        (0x18 => FR: ReadOnly<u32, FR::Register>),
        (0x1c => _reserved2),
        (0x24 => IBRD: WriteOnly<u32, IBRD::Register>),
        (0x28 => FBRD: WriteOnly<u32, FBRD::Register>),
        (0x2c => LCRH: WriteOnly<u32, LCRH::Register>),
        (0x30 => CR: WriteOnly<u32, CR::Register>),
        (0x34 => IFLS: ReadWrite<u32, IFLS::Register>),
        (0x38 => IMSC: ReadWrite<u32, IMSC::Register>),
        (0x3C => _reserved3),
        (0x40 => MIS: ReadOnly<u32, MIS::Register>),
        (0x44 => ICR: WriteOnly<u32, ICR::Register>),
        (0x48 => @END),
    }
}

/// Abstraction for the associated MMIO registers.
type Registers = MMIODerefWrapper<RegisterBlock>;

#[derive(PartialEq)]
enum BlockingMode {
    Blocking,
    NonBlocking,
}

//--------------------------------------------------------------------------------------------------
// Public Definitions
//--------------------------------------------------------------------------------------------------

pub struct PL011UartInner {
    registers: Registers,
    chars_written: usize,
    chars_read: usize,
}

// Export the inner struct so that BSPs can use it for the panic handler.
pub use PL011UartInner as PanicUart;

/// Representation of the UART.
pub struct PL011Uart {
    phys_mmio_descriptor: memory::mmu::MMIODescriptor<Physical>,
    virt_mmio_start_addr: AtomicUsize,
    inner: IRQSafeNullLock<PL011UartInner>,
    irq_number: bsp::device_driver::IRQNumber,
}

//--------------------------------------------------------------------------------------------------
// Public Code
//--------------------------------------------------------------------------------------------------

impl PL011UartInner {
    /// Create an instance.
    ///
    /// # Safety
    ///
    /// - The user must ensure to provide a correct MMIO start address.
    pub const unsafe fn new(mmio_start_addr: usize) -> Self {
        Self {
            registers: Registers::new(mmio_start_addr),
            chars_written: 0,
            chars_read: 0,
        }
    }

    /// Set up baud rate and characteristics.
    ///
    /// The calculation for the BRD given a target rate of 2300400 and a clock set to 48 MHz is:
    /// `(48_000_000/16)/230400 = 13,02083`. `13` goes to the `IBRD` (integer field). The `FBRD`
    /// (fractional field) is only 6 bits so `0,0208*64 = 1,3312 rounded to 1` will give the best
    /// approximation we can get. A 5 % error margin is acceptable for UART and we're now at 0,01 %.
    ///
    /// This results in 8N1 and 230400 baud (we set the clock to 48 MHz in config.txt).
    ///
    /// # Safety
    ///
    /// - The user must ensure to provide a correct MMIO start address.
    pub unsafe fn init(&mut self, new_mmio_start_addr: Option<usize>) -> Result<(), &'static str> {
        if let Some(addr) = new_mmio_start_addr {
            self.registers = Registers::new(addr);
        }

        // Turn it off temporarily.
        self.registers.CR.set(0);

        self.registers.ICR.write(ICR::ALL::CLEAR);
        self.registers.IBRD.write(IBRD::IBRD.val(13));
        self.registers.FBRD.write(FBRD::FBRD.val(1));
        self.registers
            .LCRH
            .write(LCRH::WLEN::EightBit + LCRH::FEN::FifosEnabled); // 8N1 + Fifo on
        self.registers.IFLS.write(IFLS::RXIFLSEL::OneEigth); // RX FIFO fill level at 1/8
        self.registers
            .IMSC
            .write(IMSC::RXIM::Enabled + IMSC::RTIM::Enabled); // RX IRQ + RX timeout IRQ
        self.registers
            .CR
            .write(CR::UARTEN::Enabled + CR::TXE::Enabled + CR::RXE::Enabled);

        Ok(())
    }

    /// Send a character.
    fn write_char(&mut self, c: char) {
        // Spin while TX FIFO full is set, waiting for an empty slot.
        while self.registers.FR.matches_all(FR::TXFF::SET) {
            cpu::nop();
        }

        // Write the character to the buffer.
        self.registers.DR.set(c as u32);

        self.chars_written += 1;
    }

    /// Retrieve a character.
    fn read_char_converting(&mut self, blocking_mode: BlockingMode) -> Option<char> {
        // If RX FIFO is empty,
        if self.registers.FR.matches_all(FR::RXFE::SET) {
            // immediately return in non-blocking mode.
            if blocking_mode == BlockingMode::NonBlocking {
                return None;
            }

            // Otherwise, wait until a char was received.
            while self.registers.FR.matches_all(FR::RXFE::SET) {
                cpu::nop();
            }
        }

        // Read one character.
        let mut ret = self.registers.DR.get() as u8 as char;

        // Convert carrige return to newline.
        if ret == '\r' {
            ret = '\n'
        }

        // Update statistics.
        self.chars_read += 1;

        Some(ret)
    }
}

/// Implementing `core::fmt::Write` enables usage of the `format_args!` macros, which in turn are
/// used to implement the `kernel`'s `print!` and `println!` macros. By implementing `write_str()`,
/// we get `write_fmt()` automatically.
///
/// The function takes an `&mut self`, so it must be implemented for the inner struct.
///
/// See [`src/print.rs`].
///
/// [`src/print.rs`]: ../../print/index.html
impl fmt::Write for PL011UartInner {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }

        Ok(())
    }
}

impl PL011Uart {
    /// Create an instance.
    ///
    /// # Safety
    ///
    /// - The user must ensure to provide correct MMIO descriptors.
    /// - The user must ensure to provide correct IRQ numbers.
    pub const unsafe fn new(
        phys_mmio_descriptor: memory::mmu::MMIODescriptor<Physical>,
        irq_number: bsp::device_driver::IRQNumber,
    ) -> Self {
        Self {
            phys_mmio_descriptor,
            virt_mmio_start_addr: AtomicUsize::new(0),
            inner: IRQSafeNullLock::new(PL011UartInner::new(
                phys_mmio_descriptor.start_addr().into_usize(),
            )),
            irq_number,
        }
    }
}

//------------------------------------------------------------------------------
// OS Interface Code
//------------------------------------------------------------------------------
use synchronization::interface::Mutex;

impl driver::interface::DeviceDriver for PL011Uart {
    fn compatible(&self) -> &'static str {
        "BCM PL011 UART"
    }

    unsafe fn init(&self) -> Result<(), &'static str> {
        let virt_addr =
            memory::mmu::kernel_map_mmio(self.compatible(), &self.phys_mmio_descriptor)?;

        self.inner
            .lock(|inner| inner.init(Some(virt_addr.into_usize())))?;

        self.virt_mmio_start_addr
            .store(virt_addr.into_usize(), Ordering::Relaxed);

        Ok(())
    }

    fn register_and_enable_irq_handler(&'static self) -> Result<(), &'static str> {
        use bsp::exception::asynchronous::irq_manager;
        use exception::asynchronous::{interface::IRQManager, IRQDescriptor};

        let descriptor = IRQDescriptor {
            name: "BCM PL011 UART",
            handler: self,
        };

        irq_manager().register_handler(self.irq_number, descriptor)?;
        irq_manager().enable(self.irq_number);

        Ok(())
    }

    fn virt_mmio_start_addr(&self) -> Option<usize> {
        let addr = self.virt_mmio_start_addr.load(Ordering::Relaxed);

        if addr == 0 {
            return None;
        }

        Some(addr)
    }
}

impl console::interface::Write for PL011Uart {
    /// Passthrough of `args` to the `core::fmt::Write` implementation, but guarded by a Mutex to
    /// serialize access.
    fn write_char(&self, c: char) {
        self.inner.lock(|inner| inner.write_char(c));
    }

    fn write_fmt(&self, args: core::fmt::Arguments) -> fmt::Result {
        // Fully qualified syntax for the call to `core::fmt::Write::write:fmt()` to increase
        // readability.
        self.inner.lock(|inner| fmt::Write::write_fmt(inner, args))
    }

    fn flush(&self) {
        // Spin until TX FIFO empty is set.
        self.inner.lock(|inner| {
            while !inner.registers.FR.matches_all(FR::TXFE::SET) {
                cpu::nop();
            }
        });
    }
}

impl console::interface::Read for PL011Uart {
    fn read_char(&self) -> char {
        self.inner
            .lock(|inner| inner.read_char_converting(BlockingMode::Blocking).unwrap())
    }

    fn clear(&self) {
        self.inner.lock(|inner| {
            // Read from the RX FIFO until it is indicating empty.
            while !inner.registers.FR.matches_all(FR::RXFE::SET) {
                inner.registers.DR.get();
            }
        })
    }
}

impl console::interface::Statistics for PL011Uart {
    fn chars_written(&self) -> usize {
        self.inner.lock(|inner| inner.chars_written)
    }

    fn chars_read(&self) -> usize {
        self.inner.lock(|inner| inner.chars_read)
    }
}

impl exception::asynchronous::interface::IRQHandler for PL011Uart {
    fn handle(&self) -> Result<(), &'static str> {
        self.inner.lock(|inner| {
            let pending = inner.registers.MIS.extract();

            // Clear all pending IRQs.
            inner.registers.ICR.write(ICR::ALL::CLEAR);

            // Check for any kind of RX interrupt.
            if pending.matches_any(MIS::RXMIS::SET + MIS::RTMIS::SET) {
                // Echo any received characters.
                while let Some(c) = inner.read_char_converting(BlockingMode::NonBlocking) {
                    inner.write_char(c)
                }
            }
        });

        Ok(())
    }
}
