use super::*;
use counters::Counters;
use crossbeam_queue::ArrayQueue;
use packet::{PacketPool, PktsHeap};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const NUM_PKTS: usize = 10;
const NUM_PART: usize = 20;
const MAX_PACKET: usize = 1500;
const PARTICLE_SZ: usize = 512;

fn packet_free(q: Arc<ArrayQueue<BoxPkt>>, pool: &mut dyn PacketPool) {
    while let Ok(p) = q.pop() {
        pool.free(p);
    }
}

fn packet_pool(test: &str, part_sz: usize, q: Arc<ArrayQueue<BoxPkt>>) -> Box<dyn PacketPool> {
    let mut counters = Counters::new(test).unwrap();
    Box::new(PktsHeap::new(
        "PKTS_HEAP",
        q,
        &mut counters,
        NUM_PKTS,
        NUM_PART,
        part_sz,
    ))
}

fn delete_veth() {
    let args = [
        "link", "del", "r2_eth1", "type", "veth", "peer", "name", "r2_eth2",
    ];
    Command::new("ip")
        .args(&args)
        .spawn()
        .expect("veth failed")
        .wait()
        .unwrap();
}

// We get random packets if ipv6 is enabled, we want only our own packets
fn disable_ipv6(eth: &str) -> String {
    let mut name = "net.ipv6.conf.".to_string();
    name.push_str(eth);
    name.push_str(".disable_ipv6=1");
    name
}

fn create_veth() {
    let args = [
        "link", "add", "r2_eth1", "type", "veth", "peer", "name", "r2_eth2",
    ];
    Command::new("ip")
        .args(&args)
        .spawn()
        .expect("veth failed")
        .wait()
        .unwrap();

    let args = ["r2_eth1", "up"];
    Command::new("ifconfig")
        .args(&args)
        .spawn()
        .expect("ifconfig eth1 fail")
        .wait()
        .unwrap();
    let args = ["-w", &disable_ipv6("r2_eth1")];
    Command::new("sysctl")
        .args(&args)
        .spawn()
        .expect("ipv6 disable fail")
        .wait()
        .unwrap();

    let args = ["-w", &disable_ipv6("r2_eth2")];
    Command::new("sysctl")
        .args(&args)
        .spawn()
        .expect("ipv6 disable fail")
        .wait()
        .unwrap();
    let args = ["r2_eth2", "up"];
    Command::new("ifconfig")
        .args(&args)
        .spawn()
        .expect("ifconfig eth2 fail")
        .wait()
        .unwrap();
}

#[test]
fn read_write() {
    delete_veth();
    create_veth();

    let wait = Arc::new(AtomicUsize::new(0));
    let done = wait.clone();
    let tname = "rx".to_string();
    let rx_q = Arc::new(ArrayQueue::new(NUM_PKTS));
    let mut pool = packet_pool("sock_read_write_rx", MAX_PACKET, rx_q.clone());
    let handler = thread::Builder::new().name(tname).spawn(move || {
        let mut raw = match RawSock::new("r2_eth2", false) {
            Ok(raw) => raw,
            Err(errno) => panic!("Errno {} opening socket", errno),
        };
        assert!(raw.fd > 0);

        let pkt = raw.recvmsg(&mut *pool, 0).unwrap();
        let pktlen = pkt.len();
        assert_eq!(MAX_PACKET, pktlen);
        let (buf, len) = match pkt.data(0) {
            Some((d, s)) => (d, s),
            None => panic!("Cant get offset 0"),
        };
        assert_eq!(len, pktlen);
        for i in 0..MAX_PACKET {
            assert_eq!(buf[i], i as u8);
        }
        packet_free(rx_q.clone(), &mut *pool);
        done.fetch_add(1, Ordering::Relaxed);
    });

    let mut raw = match RawSock::new("r2_eth1", false) {
        Ok(raw) => raw,
        Err(errno) => panic!("Errno {} opening socket", errno),
    };
    assert!(raw.fd > 0);
    let data: Vec<u8> = (0..MAX_PACKET).map(|x| (x % 256) as u8).collect();
    // Send data as multi particle pkt
    let tx_q = Arc::new(ArrayQueue::new(NUM_PKTS));
    let mut pool = packet_pool("sock_read_write_tx", PARTICLE_SZ, tx_q.clone());
    while wait.load(Ordering::Relaxed) == 0 {
        let mut pkt = pool.pkt(0).unwrap();
        assert!(pkt.append(&mut *pool, &data[0..]));
        assert_eq!(raw.sendmsg(&mut *pool, pkt), MAX_PACKET);
        packet_free(tx_q.clone(), &mut *pool);
    }

    handler.unwrap().join().unwrap();
    delete_veth();
}
