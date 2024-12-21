use crate::descriptor::{AdvancedRxDescriptor, AdvancedTxDescriptor, RX_STATUS_DD, RX_STATUS_EOP};
use crate::interrupts::Interrupts;
use crate::memory::{alloc_pkt, Dma, MemPool, Packet, PACKET_HEADROOM};
use crate::NicDevice;
use crate::{constants::*, hal::IxgbeHal};
use crate::{IxgbeError, IxgbeResult};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::{collections::VecDeque, vec::Vec};
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::time::Duration;
use core::{mem, ptr};
use smoltcp::wire::{EthernetFrame, PrettyPrinter};

const DRIVER_NAME: &str = "igb_cisco";

const MAX_QUEUES: u16 = 64;

const PKT_BUF_ENTRY_SIZE: usize = 2048;
const MIN_MEMPOOL_SIZE: usize = 4096;

// const NUM_RX_QUEUE_ENTRIES: usize = 1024;
// const NUM_TX_QUEUE_ENTRIES: usize = 1024;
const TX_CLEAN_BATCH: usize = 32;

fn wrap_ring(index: usize, ring_size: usize) -> usize {
    (index + 1) & (ring_size - 1)
}

/// Ixgbe device.
pub struct IxgbeDevice<H: IxgbeHal, const QS: usize> {
    addr: *mut u8,
    len: usize,
    num_rx_queues: u16,
    num_tx_queues: u16,
    rx_queues: Vec<IxgbeRxQueue>,
    tx_queues: Vec<IxgbeTxQueue>,
    interrupts: Interrupts,
    _marker: PhantomData<H>,
}

struct IxgbeRxQueue {
    descriptors: Box<[NonNull<AdvancedRxDescriptor>]>,
    num_descriptors: usize,
    pool: Arc<MemPool>,
    bufs_in_use: Vec<usize>,
    rx_index: usize,
}

impl IxgbeRxQueue {
    fn can_recv(&self) -> bool {
        let rx_index = self.rx_index;

        let desc = unsafe { self.descriptors[rx_index].as_ref() };
        let status = desc.get_ext_status() as u8;
        status & RX_STATUS_DD != 0
    }
}

struct IxgbeTxQueue {
    descriptors: Box<[NonNull<AdvancedTxDescriptor>]>,
    num_descriptors: usize,
    pool: Option<Arc<MemPool>>,
    bufs_in_use: VecDeque<usize>,
    clean_index: usize,
    tx_index: usize,
}

impl IxgbeTxQueue {
    fn can_send(&self) -> bool {
        let next_tx_index = wrap_ring(self.tx_index, self.num_descriptors);
        next_tx_index != self.clean_index
    }
}

/// A packet buffer for ixgbe.
pub struct IxgbeNetBuf {
    packet: Packet,
}

impl IxgbeNetBuf {
    /// Allocate a packet based on [`MemPool`].
    pub fn alloc(pool: &Arc<MemPool>, size: usize) -> IxgbeResult<Self> {
        if let Some(pkt) = alloc_pkt(pool, size) {
            Ok(Self { packet: pkt })
        } else {
            Err(IxgbeError::NoMemory)
        }
    }

    /// Returns an unmutuable packet buffer.
    pub fn packet(&self) -> &[u8] {
        self.packet.as_bytes()
    }

    /// Returns a mutuable packet buffer.
    pub fn packet_mut(&mut self) -> &mut [u8] {
        self.packet.as_mut_bytes()
    }

    /// Returns the length of the packet.
    pub fn packet_len(&self) -> usize {
        self.packet.len
    }

    /// Returns the entry of the packet.
    pub fn pool_entry(&self) -> usize {
        self.packet.pool_entry
    }

    /// Construct a [`IxgbeNetBuf`] from specified pool entry and pool.
    pub fn construct(pool_entry: usize, pool: &Arc<MemPool>, len: usize) -> IxgbeResult<Self> {
        let pkt = unsafe {
            Packet::new(
                pool.get_virt_addr(pool_entry).add(PACKET_HEADROOM),
                pool.get_phys_addr(pool_entry) + PACKET_HEADROOM,
                len,
                Arc::clone(pool),
                pool_entry,
            )
        };
        Ok(Self { packet: pkt })
    }
}

impl<H: IxgbeHal, const QS: usize> NicDevice<H> for IxgbeDevice<H, QS> {
    fn get_driver_name(&self) -> &str {
        DRIVER_NAME
    }

    /// Returns the link speed of this device.
    fn get_link_speed(&self) -> u16 {
        let speed = self.get_reg32(IXGBE_LINKS);
        if (speed & IXGBE_LINKS_UP) == 0 {
            return 0;
        }
        match speed & IXGBE_LINKS_SPEED_82599 {
            IXGBE_LINKS_SPEED_100_82599 => 100,
            IXGBE_LINKS_SPEED_1G_82599 => 1000,
            IXGBE_LINKS_SPEED_10G_82599 => 10000,
            _ => 0,
        }
    }

    /// Returns the mac address of this device.
    fn get_mac_addr(&self) -> [u8; 6] {
        let low = self.get_reg32(IXGBE_RAL(0));
        let high = self.get_reg32(IXGBE_RAH(0));

        [
            (low & 0xff) as u8,
            (low >> 8 & 0xff) as u8,
            (low >> 16 & 0xff) as u8,
            (low >> 24) as u8,
            (high & 0xff) as u8,
            (high >> 8 & 0xff) as u8,
        ]
    }

    /// Resets the stats of this device.
    fn reset_stats(&mut self) {
        self.get_reg32(IXGBE_GPRC);
        self.get_reg32(IXGBE_GPTC);
        self.get_reg32(IXGBE_GORCL);
        self.get_reg32(IXGBE_GORCH);
        self.get_reg32(IXGBE_GOTCL);
        self.get_reg32(IXGBE_GOTCH);
    }

    fn recycle_tx_buffers(&mut self, queue_id: u16) -> IxgbeResult {
        let queue = self
            .tx_queues
            .get_mut(queue_id as usize)
            .ok_or(IxgbeError::InvalidQueue)?;

        let mut clean_index = queue.clean_index;
        let cur_index = queue.tx_index;

        loop {
            let mut cleanable = cur_index as i32 - clean_index as i32;

            if cleanable < 0 {
                cleanable += queue.num_descriptors as i32;
            }

            if cleanable < TX_CLEAN_BATCH as i32 {
                break;
            }

            let mut cleanup_to = clean_index + TX_CLEAN_BATCH - 1;

            if cleanup_to >= queue.num_descriptors {
                cleanup_to -= queue.num_descriptors;
            }

            let status = unsafe {
                let descs = queue.descriptors[cleanup_to].as_mut();
                descs.paylen_popts_cc_idx_sta.read()
            };

            if (status & IXGBE_ADVTXD_STAT_DD) != 0 {
                if let Some(ref pool) = queue.pool {
                    if TX_CLEAN_BATCH >= queue.bufs_in_use.len() {
                        pool.free_stack
                            .borrow_mut()
                            .extend(queue.bufs_in_use.drain(..))
                    } else {
                        pool.free_stack
                            .borrow_mut()
                            .extend(queue.bufs_in_use.drain(..TX_CLEAN_BATCH))
                    }
                }

                clean_index = wrap_ring(cleanup_to, queue.num_descriptors);
            } else {
                break;
            }
        }

        queue.clean_index = clean_index;

        Ok(())
    }

    fn receive_packets<F>(
        &mut self,
        queue_id: u16,
        packet_nums: usize,
        mut f: F,
    ) -> IxgbeResult<usize>
    where
        F: FnMut(IxgbeNetBuf),
    {
        let mut recv_nums = 0;
        let queue = self
            .rx_queues
            .get_mut(queue_id as usize)
            .ok_or(IxgbeError::InvalidQueue)?;

        // Can't receive, return [`IxgbeError::NotReady`]
        if !queue.can_recv() {
            return Err(IxgbeError::NotReady);
        }

        let mut rx_index = queue.rx_index;
        let mut last_rx_index = queue.rx_index;

        for _ in 0..packet_nums {
            let desc = unsafe { queue.descriptors[rx_index].as_mut() };
            let status = desc.get_ext_status() as u8;

            if (status & RX_STATUS_DD) == 0 {
                break;
            }

            if (status & RX_STATUS_EOP) == 0 {
                panic!("Increase buffer size or decrease MTU")
            }

            let pool = &queue.pool;

            if let Some(buf) = pool.alloc_buf() {
                let idx = mem::replace(&mut queue.bufs_in_use[rx_index], buf);

                let packet = unsafe {
                    Packet::new(
                        pool.get_virt_addr(idx),
                        pool.get_phys_addr(idx),
                        desc.length() as usize,
                        pool.clone(),
                        idx,
                    )
                };
                // Prefetch cache line for next packet.
                #[cfg(target_arch = "x86_64")]
                packet.prefrtch(crate::memory::Prefetch::Time0);

                let rx_buf = IxgbeNetBuf { packet };

                // Call closure to avoid too many dynamic memory allocations, handle
                // by caller.
                f(rx_buf);
                recv_nums += 1;

                desc.set_packet_address(pool.get_phys_addr(queue.bufs_in_use[rx_index]) as u64);
                desc.reset_status();

                last_rx_index = rx_index;
                rx_index = wrap_ring(rx_index, queue.num_descriptors);
            } else {
                error!("Ixgbe alloc buffer failed: No Memory!");
                break;
            }
        }

        if rx_index != last_rx_index {
            self.set_reg32(IXGBE_RDT(u32::from(queue_id)), last_rx_index as u32);
            self.rx_queues[queue_id as usize].rx_index = rx_index;
        }

        Ok(recv_nums)
    }

    /// Sends a [`TxBuffer`] to the network. If currently queue is full, returns an
    /// error with type [`IxgbeError::QueueFull`].
    fn send(&mut self, queue_id: u16, tx_buf: IxgbeNetBuf) -> IxgbeResult {
        let queue = self
            .tx_queues
            .get_mut(queue_id as usize)
            .ok_or(IxgbeError::InvalidQueue)?;

        if !queue.can_send() {
            warn!("Queue {} is full", queue_id);
            return Err(IxgbeError::QueueFull);
        }

        let cur_index = queue.tx_index;

        let packet = tx_buf.packet;

        trace!(
            "[ixgbe-driver] SEND PACKET: {}",
            PrettyPrinter::<EthernetFrame<&[u8]>>::new("", &packet.as_bytes())
        );

        if queue.pool.is_some() {
            if !Arc::ptr_eq(queue.pool.as_ref().unwrap(), &packet.pool) {
                queue.pool = Some(packet.pool.clone());
            }
        } else {
            queue.pool = Some(packet.pool.clone());
        }

        assert!(
            Arc::ptr_eq(queue.pool.as_ref().unwrap(), &packet.pool),
            "Distince memory pools for a single tx queue are not supported yet."
        );

        queue.tx_index = wrap_ring(queue.tx_index, queue.num_descriptors);

        trace!(
            "TX phys_addr: {:#x}, virt_addr: {:#x}",
            packet.get_phys_addr() as u64,
            packet.get_virt_addr() as u64
        );

        // update descriptor
        let desc = unsafe { queue.descriptors[cur_index].as_mut() };
        desc.send(packet.get_phys_addr() as u64, packet.len() as u16);

        trace!(
            "packet phys addr: {:#x}, len: {}",
            packet.get_phys_addr(),
            packet.len()
        );

        queue.bufs_in_use.push_back(packet.pool_entry);
        mem::forget(packet);

        self.set_reg32(
            IXGBE_TDT(u32::from(queue_id)),
            self.tx_queues[queue_id as usize].tx_index as u32,
        );

        debug!("[Ixgbe::send] SEND PACKET COMPLETE");
        Ok(())
    }

    /// Whether can receiver packet.
    fn can_receive(&self, queue_id: u16) -> IxgbeResult<bool> {
        let queue = self
            .rx_queues
            .get(queue_id as usize)
            .ok_or(IxgbeError::InvalidQueue)?;
        Ok(queue.can_recv())
    }

    /// Whether can send packet.
    fn can_send(&self, queue_id: u16) -> IxgbeResult<bool> {
        let queue = self
            .tx_queues
            .get(queue_id as usize)
            .ok_or(IxgbeError::InvalidQueue)?;
        Ok(queue.can_send())
    }
}

impl<H: IxgbeHal, const QS: usize> IxgbeDevice<H, QS> {
    /// Returns an initialized `IxgbeDevice` on success.
    ///
    /// # Panics
    /// Panics if `num_rx_queues` or `num_tx_queues` exceeds `MAX_QUEUES`.
    pub fn init(
        base: usize,
        len: usize,
        num_rx_queues: u16,
        num_tx_queues: u16,
        pool: &Arc<MemPool>,
    ) -> IxgbeResult<Self> {
        info!(
            "Initializing igb device@base: {:#x}, len: {:#x}, num_rx_queues: {}, num_tx_queues: {}",
            base, len, num_rx_queues, num_tx_queues
        );
        // initialize RX and TX queue
        let rx_queues = Vec::with_capacity(num_rx_queues as usize);
        let tx_queues = Vec::with_capacity(num_tx_queues as usize);

        let mut interrupts = Interrupts::default();
        #[cfg(feature = "irq")]
        {
            interrupts.interrupts_enabled = true;
            interrupts.itr_rate = 0x028;
        }

        let mut dev = IxgbeDevice {
            addr: base as *mut u8,
            len,
            num_rx_queues,
            num_tx_queues,
            rx_queues,
            tx_queues,
            interrupts,
            _marker: PhantomData,
        };

        #[cfg(feature = "irq")]
        {
            for queue_id in 0..num_rx_queues {
                dev.enable_msix_interrupt(queue_id);
            }
        }

        dev.reset_and_init(pool)?;
        Ok(dev)
    }

    /// Returns the number of receive queues.
    pub fn num_rx_queues(&self) -> u16 {
        self.num_rx_queues
    }

    /// Returns the number of transmit queues.
    pub fn num_tx_queues(&self) -> u16 {
        self.num_tx_queues
    }

    #[cfg(feature = "irq")]
    /// Enable MSI interrupt for queue with `queue_id`.
    pub fn enable_msi_interrupt(&self, queue_id: u16) {
        // Step 1: The software driver associates between Tx and Rx interrupt causes and the EICR
        // register by setting the IVAR[n] registers.
        self.set_ivar(0, queue_id, 0);

        // Step 2: Program SRRCTL[n].RDMTS (per receive queue) if software uses the receive
        // descriptor minimum threshold interrupt
        // We don't use the minimum threshold interrupt

        // Step 3: All interrupts should be set to 0b (no auto clear in the EIAC register). Following an
        // interrupt, software might read the EICR register to check for the interrupt causes.
        self.set_reg32(IXGBE_EIAC, 0x0000_0000);

        // Step 4: Set the auto mask in the EIAM register according to the preferred mode of operation.
        // In our case we prefer to not auto-mask the interrupts

        // Step 5: Set the interrupt throttling in EITR[n] and GPIE according to the preferred mode of operation.
        self.set_reg32(IXGBE_EITR(u32::from(queue_id)), self.interrupts.itr_rate);

        // Step 6: Software clears EICR by writing all ones to clear old interrupt causes
        self.clear_interrupts();

        // Step 7: Software enables the required interrupt causes by setting the EIMS register
        let mut mask: u32 = self.get_reg32(IXGBE_EIMS);
        mask |= 1 << queue_id;
        self.set_reg32(IXGBE_EIMS, mask);
        debug!("Using MSI interrupts");
    }

    #[cfg(feature = "irq")]
    /// Enable MSI-X interrupt for queue with `queue_id`.
    pub fn enable_msix_interrupt(&self, queue_id: u16) {
        // Step 1: The software driver associates between interrupt causes and MSI-X vectors and the
        // throttling timers EITR[n] by programming the IVAR[n] and IVAR_MISC registers.
        let mut gpie: u32 = self.get_reg32(IXGBE_GPIE);
        gpie |= IXGBE_GPIE_MSIX_MODE | IXGBE_GPIE_PBA_SUPPORT | IXGBE_GPIE_EIAME;
        self.set_reg32(IXGBE_GPIE, gpie);

        // Set IVAR reg to enable interrupst for different queues.
        self.set_ivar(0, queue_id, u32::from(queue_id));

        // Step 2: Program SRRCTL[n].RDMTS (per receive queue) if software uses the receive
        // descriptor minimum threshold interrupt
        // We don't use the minimum threshold interrupt

        // Step 3: The EIAC[n] registers should be set to auto clear for transmit and receive interrupt
        // causes (for best performance). The EIAC bits that control the other and TCP timer
        // interrupt causes should be set to 0b (no auto clear).
        self.set_reg32(IXGBE_EIAC, IXGBE_EIMS_RTX_QUEUE);

        // Step 4: Set the auto mask in the EIAM register according to the preferred mode of operation.
        // In our case we prefer to not auto-mask the interrupts

        // Step 5: Set the interrupt throttling in EITR[n] and GPIE according to the preferred mode of operation.
        // 0x000 (0us) => ... INT/s
        // 0x008 (2us) => 488200 INT/s
        // 0x010 (4us) => 244000 INT/s
        // 0x028 (10us) => 97600 INT/s
        // 0x0C8 (50us) => 20000 INT/s
        // 0x190 (100us) => 9766 INT/s
        // 0x320 (200us) => 4880 INT/s
        // 0x4B0 (300us) => 3255 INT/s
        // 0x640 (400us) => 2441 INT/s
        // 0x7D0 (500us) => 2000 INT/s
        // 0x960 (600us) => 1630 INT/s
        // 0xAF0 (700us) => 1400 INT/s
        // 0xC80 (800us) => 1220 INT/s
        // 0xE10 (900us) => 1080 INT/s
        // 0xFA7 (1000us) => 980 INT/s
        // 0xFFF (1024us) => 950 INT/s
        self.set_reg32(IXGBE_EITR(u32::from(queue_id)), self.interrupts.itr_rate);

        // Step 6: Software enables the required interrupt causes by setting the EIMS register
        let mut mask: u32 = self.get_reg32(IXGBE_EIMS);
        mask |= 1 << queue_id;
        self.set_reg32(IXGBE_EIMS, mask);
        debug!("Using MSIX interrupts");
    }
}

// Private methods implementation
impl<H: IxgbeHal, const QS: usize> IxgbeDevice<H, QS> {
    /// Resets and initializes the device.
    fn reset_and_init(&mut self, pool: &Arc<MemPool>) -> IxgbeResult {
        info!("resetting device: igb device");

        self.disable_interrupts();

        self.set_reg32(IXGBE_CTRL, IXGBE_CTRL_RST);
        self.wait_clear_reg32(IXGBE_CTRL, IXGBE_CTRL_RST);
        // TODO: sleep 10 millis.
        let _ = H::wait_until(Duration::from_millis(1000));

        self.disable_interrupts();

        let mac = self.get_mac_addr();
        info!(
            "mac address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        self.wait_set_reg32(IXGBE_EEC, IXGBE_EEC_ARD);

        self.wait_set_reg32(IXGBE_RDRXCTL, IXGBE_RDRXCTL_DMAIDONE);

        // skip last step from 4.6.3 - we don't want interrupts(ixy)

        self.init_link();

        // reset-on-read registers, just read them once
        self.reset_stats();

        // init rx
        self.init_rx(pool)?;

        // init tx
        self.init_tx()?;

        for i in 0..self.num_rx_queues {
            self.start_rx_queue(i)?;
        }

        for i in 0..self.num_tx_queues {
            self.start_tx_queue(i)?;
        }

        // enable promisc mode by default to make testing easier
        self.set_promisc(true);

        // wait some time for the link to come up
        self.wait_for_link();

        info!("igb initialization and reset complete!");

        Ok(())
    }

    // sections 4.6.7
    /// Initializes the rx queues of this device.
    #[allow(clippy::needless_range_loop)]
    fn init_rx(&mut self, pool: &Arc<MemPool>) -> IxgbeResult {
        // disable rx while re-configuring it
        // self.clear_flags32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
        self.wait_clear_flags32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
        self.set_reg32(IXGBE_RXDCTL(0), 0);

        // section 4.6.11.3.4 - allocate all queues and traffic to PB0
        // self.set_reg32(IXGBE_RXPBSIZE(0), IXGBE_RXPBSIZE_128KB);
        // for i in 1..8 {
        //     self.set_reg32(IXGBE_RXPBSIZE(i), 0);
        // }

        // // enable CRC offloading
        // self.set_flags32(IXGBE_HLREG0, IXGBE_HLREG0_RXCRCSTRP);
        // self.set_flags32(IXGBE_RDRXCTL, IXGBE_RDRXCTL_CRCSTRIP);

        // // accept broadcast packets
        // self.set_flags32(IXGBE_FCTRL, IXGBE_FCTRL_BAM);

        self.set_flags32(IXGBE_CTRL_EXT, 0x00010000);

        // configure queues, same for all queues
        for i in 0..self.num_rx_queues {
            info!("initializing rx queue {}", i);
            // enable advanced rx descriptors
            // self.set_reg32(
            //     IXGBE_SRRCTL(u32::from(i)),
            //     (self.get_reg32(IXGBE_SRRCTL(u32::from(i))) & !IXGBE_SRRCTL_DESCTYPE_MASK)
            //         | IXGBE_SRRCTL_DESCTYPE_ADV_ONEBUF,
            // );
            // let nic drop packets if no rx descriptor is available instead of buffering them
            self.wait_clear_reg32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
            self.set_reg32(IXGBE_RXDCTL(0), 0);

            assert_eq!(mem::size_of::<AdvancedTxDescriptor>(), 16);
            // section 7.1.9 - setup descriptor ring
            let ring_size_bytes = QS * mem::size_of::<AdvancedRxDescriptor>();
            let dma: Dma<AdvancedRxDescriptor, H> = Dma::allocate(ring_size_bytes, true)?;

            // initialize to 0xff to prevent rogue memory accesses on premature dma activation
            let mut descriptors: [NonNull<AdvancedRxDescriptor>; QS] = [NonNull::dangling(); QS];

            unsafe {
                for desc_id in 0..QS {
                    descriptors[desc_id] = NonNull::new(dma.virt.add(desc_id)).unwrap();
                    descriptors[desc_id].as_mut().init();
                }
            }

            self.set_reg32(
                IXGBE_RDBAL(u32::from(i)),
                (dma.phys as u64 & 0xffff_ffff) as u32,
            );
            self.set_reg32(IXGBE_RDBAH(u32::from(i)), (dma.phys as u64 >> 32) as u32);
            self.set_reg32(IXGBE_RDLEN(u32::from(i)), ring_size_bytes as u32);

            info!("rx ring {} phys addr: {:#x}", i, dma.phys);
            info!("rx ring {} virt addr: {:p}", i, dma.virt);

            // set ring to empty at start
            self.set_reg32(IXGBE_RDH(u32::from(i)), 0);
            self.set_reg32(IXGBE_RDT(u32::from(i)), 0);

            let rx_queue = IxgbeRxQueue {
                descriptors: Box::new(descriptors),
                pool: Arc::clone(pool),
                num_descriptors: QS,
                rx_index: 0,
                bufs_in_use: Vec::with_capacity(QS),
            };

            self.rx_queues.push(rx_queue);
        }

        // last sentence of section 4.6.7 - set some magic bits
        self.set_flags32(IXGBE_CTRL_EXT, IXGBE_CTRL_EXT_NS_DIS);

        // probably a broken feature, this flag is initialized with 1 but has to be set to 0
        for i in 0..self.num_rx_queues {
            // self.clear_flags32(IXGBE_DCA_RXCTRL(u32::from(i)), 1 << 12);
            self.set_flags32(IXGBE_DCA_RXCTRL(u32::fomr(i)), 1 << 25);
        }

        // start rx
        self.set_reg32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
        self.wait_set_reg32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);
        debug!("rx queue enable!");
        // self.set_flags32(IXGBE_RXCTRL, IXGBE_RXCTRL_RXEN);

        Ok(())
    }

    // section 4.6.8
    /// Initializes the tx queues of this device.
    #[allow(clippy::needless_range_loop)]
    fn init_tx(&mut self) -> IxgbeResult {
        // // crc offload and small packet padding
        // self.set_flags32(IXGBE_HLREG0, IXGBE_HLREG0_TXCRCEN | IXGBE_HLREG0_TXPADEN);

        // // section 4.6.11.3.4 - set default buffer size allocations
        // self.set_reg32(IXGBE_TXPBSIZE(0), IXGBE_TXPBSIZE_40KB);
        // for i in 1..8 {
        //     self.set_reg32(IXGBE_TXPBSIZE(i), 0xff);
        // }

        // required when not using DCB/VTd
        self.set_reg32(IXGBE_DTXMXSZRQ, 0xffff);
        // self.clear_flags32(IXGBE_RTTDCS, IXGBE_RTTDCS_ARBDIS);

        // configure queues
        for i in 0..self.num_tx_queues {
            info!("initializing tx queue {}", i);
            // section 7.1.9 - setup descriptor ring
            assert_eq!(mem::size_of::<AdvancedTxDescriptor>(), 16);
            let ring_size_bytes = QS * mem::size_of::<AdvancedTxDescriptor>();

            let dma: Dma<AdvancedTxDescriptor, H> = Dma::allocate(ring_size_bytes, true)?;

            let mut descriptors: [NonNull<AdvancedTxDescriptor>; QS] = [NonNull::dangling(); QS];

            unsafe {
                for desc_id in 0..QS {
                    descriptors[desc_id] = NonNull::new(dma.virt.add(desc_id)).unwrap();
                    descriptors[desc_id].as_mut().init();
                }
            }

            self.set_reg32(
                IXGBE_TDBAL(u32::from(i)),
                (dma.phys as u64 & 0xffff_ffff) as u32,
            );
            self.set_reg32(IXGBE_TDBAH(u32::from(i)), (dma.phys as u64 >> 32) as u32);
            self.set_reg32(IXGBE_TDLEN(u32::from(i)), ring_size_bytes as u32);

            trace!("tx ring {} phys addr: {:#x}", i, dma.phys);
            trace!("tx ring {} virt addr: {:p}", i, dma.virt);

            // descriptor writeback magic values, important to get good performance and low PCIe overhead
            // see 7.2.3.4.1 and 7.2.3.5 for an explanation of these values and how to find good ones
            // we just use the defaults from DPDK here, but this is a potentially interesting point for optimizations
            let mut txdctl = self.get_reg32(IXGBE_TXDCTL(u32::from(i)));
            // there are no defines for this in constants.rs for some reason
            // pthresh: 6:0, hthresh: 14:8, wthresh: 22:16
            txdctl &= !(0x7F | (0x7F << 8) | (0x7F << 16));
            txdctl |= 36 | (8 << 8) | (4 << 16);

            self.set_reg32(IXGBE_TXDCTL(u32::from(i)), txdctl);

            let tx_queue = IxgbeTxQueue {
                descriptors: Box::new(descriptors),
                bufs_in_use: VecDeque::with_capacity(QS),
                pool: None,
                num_descriptors: QS,
                clean_index: 0,
                tx_index: 0,
            };

            self.tx_queues.push(tx_queue);
        }

        // final step: enable DMA
        self.set_reg32(IXGBE_DMATXCTL, IXGBE_DMATXCTL_TE);

        Ok(())
    }

    /// Sets the rx queues` descriptors and enables the queues.
    fn start_rx_queue(&mut self, queue_id: u16) -> IxgbeResult {
        debug!("starting rx queue {}", queue_id);

        let queue = &mut self.rx_queues[queue_id as usize];

        if queue.num_descriptors & (queue.num_descriptors - 1) != 0 {
            // return Err("number of queue entries must be a power of 2".into());
            return Err(IxgbeError::QueueNotAligned);
        }

        for i in 0..queue.num_descriptors {
            let pool = &queue.pool;

            let id = match pool.alloc_buf() {
                Some(x) => x,
                None => return Err(IxgbeError::NoMemory),
            };

            unsafe {
                let desc = queue.descriptors[i].as_mut();
                desc.set_packet_address(pool.get_phys_addr(id) as u64);
                desc.reset_status();
            }

            // we need to remember which descriptor entry belongs to which mempool entry
            queue.bufs_in_use.push(id);
        }

        let queue = &self.rx_queues[queue_id as usize];

        // enable queue and wait if necessary
        self.set_flags32(IXGBE_RXDCTL(u32::from(queue_id)), IXGBE_RXDCTL_ENABLE);
        self.wait_set_reg32(IXGBE_RXDCTL(u32::from(queue_id)), IXGBE_RXDCTL_ENABLE);

        // rx queue starts out full
        self.set_reg32(IXGBE_RDH(u32::from(queue_id)), 0);

        // was set to 0 before in the init function
        self.set_reg32(
            IXGBE_RDT(u32::from(queue_id)),
            (queue.num_descriptors - 1) as u32,
        );

        Ok(())
    }

    /// Enables the tx queues.
    fn start_tx_queue(&mut self, queue_id: u16) -> IxgbeResult {
        debug!("starting tx queue {}", queue_id);

        let queue = &mut self.tx_queues[queue_id as usize];

        if queue.num_descriptors & (queue.num_descriptors - 1) != 0 {
            return Err(IxgbeError::QueueNotAligned);
        }

        // tx queue starts out empty
        self.set_reg32(IXGBE_TDH(u32::from(queue_id)), 0);
        self.set_reg32(IXGBE_TDT(u32::from(queue_id)), 0);

        // enable queue and wait if necessary
        self.set_flags32(IXGBE_TXDCTL(u32::from(queue_id)), IXGBE_TXDCTL_ENABLE);
        self.wait_set_reg32(IXGBE_TXDCTL(u32::from(queue_id)), IXGBE_TXDCTL_ENABLE);

        Ok(())
    }

    // see section 4.6.4
    /// Initializes the link of this device.
    fn init_link(&self) {
        // // link auto-configuration register should already be set correctly, we're resetting it anyway
        // self.set_reg32(
        //     IXGBE_AUTOC,
        //     (self.get_reg32(IXGBE_AUTOC) & !IXGBE_AUTOC_LMS_MASK) | IXGBE_AUTOC_LMS_10G_SERIAL,
        // );
        // self.set_reg32(
        //     IXGBE_AUTOC,
        //     (self.get_reg32(IXGBE_AUTOC) & !IXGBE_AUTOC_10G_PMA_PMD_MASK) | IXGBE_AUTOC_10G_XAUI,
        // );
        // // negotiate link
        // self.set_flags32(IXGBE_AUTOC, IXGBE_AUTOC_AN_RESTART);
        // // datasheet wants us to wait for the link here, but we can continue and wait afterwards
        // set up phy and link
        let mut pctrl = self.read_mdic(0);
        pctrl |= 1 << 9;
        info!("start igb auto negotiation");
        self.write_mdic(0, pctrl);

        loop {
            let status = self.get_reg32(IGB_STATUS);
            debug!("igb status: {status:b}");
            if status & 0b10 == 0b10 {
                break;
            }
        }
    }

    /// Disable all interrupts for all queues.
    fn disable_interrupts(&self) {
        // Clear interrupt mask to stop from interrupts being generated
        self.set_reg32(IXGBE_EIMS, 0x0000_0000);
        self.clear_interrupts();
    }

    /// Disable interrupt for queue with `queue_id`.
    fn disable_interrupt(&self, queue_id: u16) {
        // Clear interrupt mask to stop from interrupts being generated
        let mut mask: u32 = self.get_reg32(IXGBE_EIMS);
        mask &= !(1 << queue_id);
        self.set_reg32(IXGBE_EIMS, mask);
        self.clear_interrupt(queue_id);
        debug!("Using polling");
    }

    /// Clear interrupt for queue with `queue_id`.
    fn clear_interrupt(&self, queue_id: u16) {
        // Clear interrupt mask
        self.set_reg32(IXGBE_EIMC, 1 << queue_id);
        self.get_reg32(IXGBE_EICR);
    }

    /// Clear all interrupt masks for all queues.
    fn clear_interrupts(&self) {
        // Clear interrupt mask
        self.set_reg32(IXGBE_EIMC, IXGBE_IRQ_CLEAR_MASK);
        self.get_reg32(IXGBE_EICR);
    }

    /// Waits for the link to come up.
    fn wait_for_link(&self) {
        #[cfg(target_arch = "x86_64")]
        {
            info!("waiting for link");
            let _ = H::wait_until(Duration::from_secs(10));
            let mut speed = self.get_link_speed();
            while speed == 0 {
                let _ = H::wait_until(Duration::from_millis(100));
                speed = self.get_link_speed();
            }
            info!("link speed is {} Mbit/s", self.get_link_speed());
        }
    }

    /// Enables or disables promisc mode of this device.
    fn set_promisc(&self, enabled: bool) {
        if enabled {
            info!("enabling promisc mode");
            self.set_flags32(IXGBE_FCTRL, IXGBE_FCTRL_MPE | IXGBE_FCTRL_UPE);
        } else {
            info!("disabling promisc mode");
            self.clear_flags32(IXGBE_FCTRL, IXGBE_FCTRL_MPE | IXGBE_FCTRL_UPE);
        }
    }

    /// Returns the register at `self.addr` + `reg`.
    ///
    /// # Panics
    ///
    /// Panics if `self.addr` + `reg` does not belong to the mapped memory of the pci device.
    fn get_reg32(&self, reg: u32) -> u32 {
        assert!(reg as usize <= self.len - 4, "memory access out of bounds");

        unsafe { ptr::read_volatile((self.addr as usize + reg as usize) as *mut u32) }
    }

    /// Sets the register at `self.addr` + `reg` to `value`.
    ///
    /// # Panics
    ///
    /// Panics if `self.addr` + `reg` does not belong to the mapped memory of the pci device.
    fn set_reg32(&self, reg: u32, value: u32) {
        assert!(reg as usize <= self.len - 4, "memory access out of bounds");

        unsafe {
            ptr::write_volatile((self.addr as usize + reg as usize) as *mut u32, value);
        }
    }

    /// Sets the `flags` at `self.addr` + `reg`.
    fn set_flags32(&self, reg: u32, flags: u32) {
        self.set_reg32(reg, self.get_reg32(reg) | flags);
    }

    /// Clears the `flags` at `self.addr` + `reg`.
    fn clear_flags32(&self, reg: u32, flags: u32) {
        self.set_reg32(reg, self.get_reg32(reg) & !flags);
    }

    /// Waits for `self.addr` + `reg` to clear `value`.
    fn wait_clear_reg32(&self, reg: u32, value: u32) {
        loop {
            let current = self.get_reg32(reg);
            if (current & value) == 0 {
                break;
            }
            // `thread::sleep(Duration::from_millis(100));`
            // let _ = H::wait_ms(100);
            let _ = H::wait_until(Duration::from_millis(100));
        }
    }

    /// Waits for `self.addr` + `reg` to set `value`.
    fn wait_set_reg32(&self, reg: u32, value: u32) {
        loop {
            let current = self.get_reg32(reg);
            if (current & value) == value {
                break;
            }
            let _ = H::wait_until(Duration::from_millis(100));
        }
    }

    /// Maps interrupt causes to vectors by specifying the `direction` (0 for Rx, 1 for Tx),
    /// the `queue` ID and the corresponding `misx_vector`.
    fn set_ivar(&self, direction: u32, queue: u16, mut msix_vector: u32) {
        let mut ivar: u32;
        // let index: u32;
        msix_vector |= IXGBE_IVAR_ALLOC_VAL;
        let index = 16 * (u32::from(queue) & 1) + 8 * direction;
        ivar = self.get_reg32(IXGBE_IVAR(u32::from(queue) >> 1));
        ivar &= !(0xFF << index);
        ivar |= msix_vector << index;
        self.set_reg32(IXGBE_IVAR(u32::from(queue) >> 1), ivar);
    }

    /// read data from mdic
    fn read_mdic(&self, offset: u32) -> u16 {
        let mut mdic = offset << 16 | 1 << 21 | IGB_MDIC_READ;
        self.set_reg32(IGB_MDIC, mdic);

        loop {
            mdic = self.get_reg32(IGB_MDIC);
            if(mdic & IGB_MDIC_READY) == IGB_MDIC_READY {
                break;
            }
        }
        return mdic as u16;
    }

    /// wriet data to mdic, ignore errors
    fn write_mdic(&self, offset: u32, data: u16) {
        let mut mdic = offset << 16 | 1 << 21 | data as u32 | IGB_MDIC_WRITE;
        self.set_reg32(IGB_MDIC, mdic);

        loop {
            mdic = self.get_reg32(IGB_MDIC);
            if(mdic & IGB_MDIC_READY) == IGB_MDIC_READY {
                break;
            }
        }
    }
}

unsafe impl<H: IxgbeHal, const QS: usize> Sync for IxgbeDevice<H, QS> {}
unsafe impl<H: IxgbeHal, const QS: usize> Send for IxgbeDevice<H, QS> {}
