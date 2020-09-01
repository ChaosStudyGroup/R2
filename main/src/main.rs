use api::ApiSvr;
use apis_interface::InterfaceSyncProcessor;
use apis_log::LogSyncProcessor;
use apis_route::RouteSyncProcessor;
use counters::Counters;
use crossbeam_queue::ArrayQueue;
use efd::Efd;
use epoll::{Epoll, EpollClient, EPOLLIN};
use graph::{GnodeCntrs, GnodeInit, Graph};
use l2_eth_encap::EncapMux;
use log::Logger;
use msg::R2Msg;
use names::rx_tx;
use packet::PktsHeap;
use std::collections::HashMap;
use std::convert::From;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
mod ifd;
use ifd::{IfdCtx, InterfaceApis};
mod ipv4;
use ipv4::{create_ipv4_nodes, IPv4Ctx, RouteApis};
mod msgs;
use msgs::{ctrl2fwd_messages, fwd2ctrl_messages};
mod logs;
use logs::LogApis;
mod pkts;
use clap::{App, Arg};
use dpdk::dpdk_launch;
use dpdk::{DpdkGlobal, PktsDpdk};
use ini::Ini;
use perf::Perf;

const THREADS: usize = 2;
const LOGSZ: usize = 32;
const LOGLINES: usize = 1000;
const MAX_FDS: i32 = 4000;
const DEF_PKTS: usize = 512;
const DEF_PARTS: usize = 2 * DEF_PKTS;
const DEF_PARTICLE_SZ: usize = 3072;
const R2_CFG_FILE: &str = "/etc/r2.cfg";
pub const MAX_HEADROOM: usize = 100;

// This holds various pieces of context for all of R2, like the interface context,
// routing context etc.. This is shared across all control threads, but NOT shared
// to forwarding threads. So if control thread wants to modify the context it will
// take a lock and modify this
pub struct R2 {
    counters: Counters,
    fwd2ctrl: Sender<R2Msg>,
    nthreads: usize,
    threads: Vec<R2PerThread>,
    ifd: IfdCtx,
    ipv4: IPv4Ctx,
    dpdk: DpdkCfg,
}

impl R2 {
    fn new(
        counter_name: &str,
        log_name: &str,
        log_data: usize,
        log_size: usize,
        fwd2ctrl: Sender<R2Msg>,
        nthreads: usize,
    ) -> Self {
        let counters = match Counters::new(counter_name) {
            Ok(c) => c,
            Err(errno) => panic!("Unable to create counters, errno {}", errno),
        };

        let mut threads = Vec::new();
        for t in 0..nthreads {
            let name = format!("{}:{}", log_name, t);
            let logger = match Logger::new(&name, log_data, log_size) {
                Ok(l) => Arc::new(l),
                Err(errno) => panic!("Unable to create logger, errno {}", errno),
            };
            let efd = Arc::new(Efd::new(0).unwrap());
            threads.push(R2PerThread {
                thread: t,
                ctrl2fwd: None,
                efd,
                poll_fds: Vec::new(),
                logger,
            });
        }

        let dpdk = DpdkCfg {
            on: false,
            mem: 0,
            ncores: 0,
            glob: Default::default(),
        };

        R2 {
            counters,
            fwd2ctrl,
            nthreads,
            threads,
            ifd: IfdCtx::new(),
            ipv4: IPv4Ctx::new(),
            dpdk,
        }
    }

    fn unicast(&mut self, msg: R2Msg, idx: usize) {
        let t = &self.threads[idx];
        if let Some(s) = &t.ctrl2fwd {
            s.send(msg).unwrap();
        }
        t.efd.write(1);
    }

    // broadcast a message to all forwarding threads. The expectation is that everyone will
    // use broadcast because everyone will just want to send the exact same message to all
    // threads. But there can be exceptions like drivers which might want to send messages
    // specific to a thread, and those rare exceptions will use unicast() above
    fn broadcast(&mut self, msg: R2Msg) {
        for t in self.threads.iter() {
            if let Some(s) = &t.ctrl2fwd {
                s.send(msg.clone(&mut self.counters, t.logger.clone()))
                    .unwrap();
            }
            t.efd.write(1);
        }
    }
}

struct DpdkCfg {
    on: bool,      // Is dpdk enabled ?
    mem: usize,    // hugepages memory in Mb
    ncores: usize, // number of cores used for dpdk
    glob: DpdkGlobal,
}

// R2 context information that is unique per forwarding thread
struct R2PerThread {
    thread: usize,
    ctrl2fwd: Option<Sender<R2Msg>>,
    efd: Arc<Efd>,
    poll_fds: Vec<i32>,
    logger: Arc<Logger>,
}

struct R2Epoll {}

impl EpollClient for R2Epoll {
    fn event(&mut self, _fd: i32, _event: u32) {}
}

fn create_ethernet_mux(r2: &mut R2, g: &mut Graph<R2Msg>) {
    let emux = EncapMux::new();
    let init = GnodeInit {
        name: emux.name(),
        next_names: emux.next_names(),
        cntrs: GnodeCntrs::new(&emux.name(), &mut r2.counters),
        perf: Perf::new(&emux.name(), &mut r2.counters),
    };
    g.add(Box::new(emux), init);
}

// Create all the graph nodes that can be created upfront - ie those that are not
// 'dynamic' in nature. Really the only 'dynamic' nodes should be the interfaces,
// all other feature nodes should get created here.
fn create_nodes(r2: &mut R2, g: &mut Graph<R2Msg>) {
    create_ipv4_nodes(r2, g);
    create_ethernet_mux(r2, g);
    g.finalize();
}

// All the modules that expose external APIs, need to register their APIs here
// The standard format is that the module XYZ's thrift defenitions when compiled,
// will provide a 'XYZSyncProcessor' object which needs as input another object
// that has the XYZSyncHandler trait implmented -  the XYZSyncHandler trait will
// implement all the APIs that XYZ module wants to expose (defined in thrift files)
fn register_apis(r2: Arc<Mutex<R2>>) -> ApiSvr {
    let mut svr = ApiSvr::new(common::API_SVR.to_string());

    let intf_apis = InterfaceApis::new(r2.clone());
    svr.register(
        common::INTF_APIS,
        Box::new(InterfaceSyncProcessor::new(intf_apis)),
    );

    let log_apis = LogApis::new(r2.clone());
    svr.register(common::LOG_APIS, Box::new(LogSyncProcessor::new(log_apis)));

    let route_apis = RouteApis::new(r2);
    svr.register(
        common::ROUTE_APIS,
        Box::new(RouteSyncProcessor::new(route_apis)),
    );

    svr
}

struct ThreadParams {
    thread: usize,
    epoll: Epoll,
    receiver: Receiver<R2Msg>,
    g: Graph<R2Msg>,
}

fn launch_pthread(mut t: Box<ThreadParams>) {
    let name = format!("r2-{}", t.thread);
    thread::Builder::new()
        .name(name)
        .spawn(move || loop {
            let mut work = true;
            // For now we dont honor the 'time' parameter here, which mostly comes into play
            // if the scheduler has work to be done at a future time in which case we can yield
            // till that time.
            while work {
                let (w, _) = t.g.run();
                work = w;
                // interleave packet forwarding with checking for control messages, depending
                // on performance measurements, this can be done (much) less frequently
                ctrl2fwd_messages(t.thread, &mut t.epoll, &t.receiver, &mut t.g);
            }
            // No more packets or control messages to process, sleep till someone wakes us up
            t.epoll.wait();
        })
        .unwrap();
}

fn launch_dpdk_thread(t: Box<ThreadParams>) {
    dpdk_launch(
        t.thread + 1,
        Some(dpdk_eal_thread),
        Box::into_raw(t) as *mut core::ffi::c_void,
    );
}

extern "C" fn dpdk_eal_thread(arg: *mut core::ffi::c_void) -> i32 {
    unsafe {
        let t: Box<ThreadParams> = Box::from_raw(arg as *mut ThreadParams);
        dpdk_thread(t);
        0
    }
}

fn dpdk_thread(mut t: Box<ThreadParams>) {
    loop {
        let mut work = true;
        // For now we dont honor the 'time' parameter here, which mostly comes into play
        // if the scheduler has work to be done at a future time in which case we can yield
        // till that time.
        while work {
            let (w, _) = t.g.run();
            work = w;
            // interleave packet forwarding with checking for control messages, depending
            // on performance measurements, this can be done (much) less frequently
            ctrl2fwd_messages(t.thread, &mut t.epoll, &t.receiver, &mut t.g);
        }
    }
}

// Create one forwarding thread. Each forwarding thread needs its own epoller to be woken up
// when the thread's interfaces have pending I/O, and also to be woken up for example when
// another thread wants to send packets via an interface this thread owns, and also woken up
// when control thread wants to send a message to this forwarding thread.
// NOTE: The model here is an epoll driven wakeup model - but once we have tight polling
// drivers lke DPDK integrated, this model will change - maybe epoll wait will be taken out
fn create_thread(r2: &mut R2, g: Graph<R2Msg>, thread: usize) {
    // Channel to talk to and from control plane
    let (sender, receiver) = channel();
    // This is the descriptor used to wakeup the thread in genenarl, ie unlreated to any
    // interface I/O - like when theres a control message to this thread etc..
    let efd = r2.threads[thread].efd.clone();
    let epoll = Epoll::new(efd, MAX_FDS, -1, Box::new(R2Epoll {})).unwrap();
    r2.threads[thread].ctrl2fwd = Some(sender);
    // The poll_fds are the descriptors that we know of at the moment (if any), when the
    // thread is getting launched. When interfaces are created later, they will come up
    // with their own descriptors.
    for fd in r2.threads[thread].poll_fds.iter() {
        epoll.add(*fd, EPOLLIN);
    }
    let t = Box::new(ThreadParams {
        thread,
        epoll,
        receiver,
        g,
    });

    if r2.dpdk.on {
        launch_dpdk_thread(t);
    } else {
        launch_pthread(t);
    }
}

fn launch_threads(r2: &mut R2, graph: Graph<R2Msg>) {
    for t in 1..r2.nthreads {
        let queue = Arc::new(ArrayQueue::new(DEF_PKTS));
        let pool = Box::new(PktsHeap::new(
            "PKTS_HEAP",
            queue.clone(),
            &mut r2.counters,
            DEF_PKTS,
            DEF_PARTS,
            DEF_PARTICLE_SZ,
        ));
        let g = graph.clone(
            t,
            pool,
            queue,
            &mut r2.counters,
            r2.threads[t].logger.clone(),
        );
        create_thread(r2, g, t);
    }
    create_thread(r2, graph, 0);
}

fn launch_api_svr(mut svr: ApiSvr) {
    thread::Builder::new()
        .name("r2-1".to_string())
        .spawn(move || loop {
            // Handle API calls
            svr.run().unwrap();
        })
        .unwrap();
}

fn parse_cfg(r2: &mut R2) {
    let matches = App::new("R2")
        .version("1.0")
        .author("Gopa Kumar")
        .about("Router in Rust")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("R2 config file")
                .takes_value(true),
        )
        .get_matches();

    let cfg = match matches.value_of("config") {
        Some(cfg) => cfg,
        None => R2_CFG_FILE,
    };
    if let Ok(ini) = Ini::load_from_file(cfg) {
        for (sec, prop) in ini.iter() {
            match sec.unwrap() {
                "dpdk" => {
                    for (k, v) in prop.iter() {
                        match k {
                            "on" => {
                                r2.dpdk.on = v.parse::<bool>().unwrap();
                            }
                            "mem" => {
                                r2.dpdk.mem = v.parse::<usize>().unwrap();
                            }
                            "ncores" => {
                                r2.dpdk.ncores = v.parse::<usize>().unwrap();
                            }
                            unknown => panic!("Unknown dpdk config {}", unknown),
                        }
                    }
                }
                unknown => panic!("Unknown config {}", unknown),
            }
        }
    }
}

fn main() {
    let (sender, receiver) = channel();
    let r2_rc = Arc::new(Mutex::new(R2::new(
        common::R2CNT_SHM,
        common::R2LOG_SHM,
        LOGSZ,
        LOGLINES,
        sender,
        THREADS,
    )));

    let mut r2 = r2_rc.lock().unwrap();

    parse_cfg(&mut r2);

    let queue = Arc::new(ArrayQueue::new(DEF_PKTS));
    let mut graph;
    if r2.dpdk.on {
        r2.dpdk.glob = DpdkGlobal::new(r2.dpdk.mem, r2.dpdk.ncores);
        let pool = Box::new(PktsDpdk::new(
            "GraphPool",
            queue.clone(),
            &mut r2.counters,
            DEF_PKTS,
            DEF_PARTS,
            DEF_PARTICLE_SZ,
        ));
        graph = Graph::<R2Msg>::new(0, pool, queue, &mut r2.counters);
    } else {
        let pool = Box::new(PktsHeap::new(
            "GraphPool",
            queue.clone(),
            &mut r2.counters,
            DEF_PKTS,
            DEF_PARTS,
            DEF_PARTICLE_SZ,
        ));
        graph = Graph::<R2Msg>::new(0, pool, queue, &mut r2.counters);
    }
    create_nodes(&mut r2, &mut graph);
    launch_threads(&mut r2, graph);

    let svr = register_apis(r2_rc.clone());
    // Api server attempts to take r2 locks, so release it before api svr is launched
    drop(r2);
    launch_api_svr(svr);

    // Wait (for ever) for messages from forwarding planes
    fwd2ctrl_messages(r2_rc.clone(), receiver);
}

#[cfg(test)]
mod test;
