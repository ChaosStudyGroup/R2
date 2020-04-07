use packet::{BoxPart, BoxPkt, PacketPool};

// We can implement custom packet pool here with packets/particles
// coming from custom memory areas like dpdk hugepages. Once thats
// done the PktsHeap usage in main.rs can be replaced with R2PktPool
struct R2PktPool {}

impl PacketPool for R2PktPool {
    fn pkt(&mut self, _headroom: usize) -> Option<BoxPkt> {
        None
    }

    fn particle(&mut self, _headroom: usize) -> Option<BoxPart> {
        None
    }

    fn free_pkt(&mut self, _pkt: BoxPkt) {}

    fn free_part(&mut self, _part: BoxPart) {}

    fn particle_sz(&self) -> usize {
        0
    }
}
