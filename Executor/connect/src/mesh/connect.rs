// Full-mesh connection setup.
//
// For each ordered pair (i, j) with i < j:
//   conn-1 port = BASE_PORT  + i*MAX_NODES + j  — QP metadata exchange
//                i is server; gives i.ctrl_as_sender[j] / j.ctrl_as_receiver[i]
//   conn-2 port = BASE_PORT2 + i*MAX_NODES + j  — plain TCP ctrl only
//                i is server; gives i.ctrl_as_receiver[j] / j.ctrl_as_sender[i]
//
// All server threads are spawned before any client connects (200 ms pause).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::rdma::context::RdmaContext;
use crate::rdma::exchange::{self, QpInfo};
use crate::rdma::memory_region::MemoryRegion;
use crate::rdma::queue_pair::QueuePair;

use super::{
    BASE_PORT, BASE_PORT2, CQ_SIZE, GID_INDEX, MAX_NODES, RDMA_PORT, SLOT_SIZE,
    MeshNode, PeerLink, rand_psn,
};

impl MeshNode {
    /// Establish full-mesh connections with all `total` nodes.
    pub fn connect_all(node_id: usize, total: usize, ips: &[&str]) -> Result<Self> {
        Self::connect_all_with_buf(node_id, total, ips, None, total * SLOT_SIZE)
    }

    /// Like `connect_all` but registers an externally-owned SHM buffer as the MR.
    pub unsafe fn connect_all_on_shm(
        node_id: usize,
        total:   usize,
        ips:     &[&str],
        shm_ptr: *mut u8,
        shm_len: usize,
    ) -> Result<Self> {
        Self::connect_all_with_buf(node_id, total, ips, Some((shm_ptr, shm_len)), shm_len)
    }

    fn connect_all_with_buf(
        node_id:     usize,
        total:       usize,
        ips:         &[&str],
        external_mr: Option<(*mut u8, usize)>,
        mr_size:     usize,
    ) -> Result<Self> {
        assert_eq!(ips.len(), total, "ips.len() must equal total");
        assert!(node_id < total,     "node_id out of range");
        assert!(total <= MAX_NODES as usize, "increase MAX_NODES");

        let ctx = RdmaContext::new(None, CQ_SIZE)?;
        let mr = match external_mr {
            Some((ptr, len)) => unsafe { MemoryRegion::register_external(&ctx, ptr, len)? },
            None             => MemoryRegion::alloc_and_register(&ctx, mr_size)?,
        };

        let gid       = ctx.query_gid(RDMA_PORT, GID_INDEX as i32)?;
        let port_attr = ctx.query_port(RDMA_PORT)?;

        let mut qp_pool: HashMap<usize, (QueuePair, u32)> = HashMap::new();
        for j in 0..total {
            if j == node_id { continue; }
            let qp  = QueuePair::create(&ctx)?;
            let psn = rand_psn();
            qp_pool.insert(j, (qp, psn));
        }

        let make_info = |qpn: u32, psn: u32| QpInfo {
            qpn,
            psn,
            gid:  gid.raw,
            lid:  port_attr.lid,
            rkey: mr.rkey(),
            addr: mr.addr(),
            len:  (total * SLOT_SIZE) as u32,
        };

        // ── Server threads (pairs where we are the lower-ID node) ────────────
        // For each j > node_id we spawn two sub-threads:
        //   conn-1 (port BASE_PORT + node_id*MAX_NODES + j): QP exchange
        //          → our ctrl_as_sender[j]
        //   conn-2 (port BASE_PORT2 + node_id*MAX_NODES + j): plain TCP
        //          → our ctrl_as_receiver[j]

        type ThreadMsg = Result<(usize, QueuePair, QpInfo, std::net::TcpStream, std::net::TcpStream)>;
        let (tx, rx) = mpsc::channel::<ThreadMsg>();

        for j in (node_id + 1)..total {
            let (qp, psn) = qp_pool.remove(&j).unwrap();
            let local_info = make_info(qp.qpn(), psn);
            let port1 = BASE_PORT  + node_id as u16 * MAX_NODES + j as u16;
            let port2 = BASE_PORT2 + node_id as u16 * MAX_NODES + j as u16;
            let tx    = tx.clone();

            thread::spawn(move || {
                let res = (|| -> Result<(usize, QueuePair, QpInfo, std::net::TcpStream, std::net::TcpStream)> {
                    let (tx2, rx2) = mpsc::channel::<Result<std::net::TcpStream>>();
                    thread::spawn(move || { tx2.send(exchange::server_ctrl_only(port2)).ok(); });

                    let (remote, ctrl_as_sender) = exchange::server_exchange(port1, &local_info)?;
                    let ctrl_as_receiver = rx2.recv()
                        .map_err(|_| anyhow!("ctrl-2 server thread died"))??;

                    qp.to_init(RDMA_PORT)?;
                    qp.to_rtr(&remote, RDMA_PORT, GID_INDEX)?;
                    qp.to_rts(psn)?;
                    Ok((j, qp, remote, ctrl_as_sender, ctrl_as_receiver))
                })();
                tx.send(res).ok();
            });
        }
        drop(tx);

        // Brief pause so all server threads reach accept() before clients connect.
        thread::sleep(Duration::from_millis(200));

        // ── Client connections (pairs where we are the higher-ID node) ────────
        // For each k < node_id:
        //   conn-1 (k is server): gives us ctrl_as_receiver[k]
        //   conn-2 (k is server): gives us ctrl_as_sender[k]

        let mut peers: HashMap<usize, PeerLink> = HashMap::new();

        for k in 0..node_id {
            let (qp, psn) = qp_pool.remove(&k).unwrap();
            let local_info = make_info(qp.qpn(), psn);
            let port1 = BASE_PORT  + k as u16 * MAX_NODES + node_id as u16;
            let port2 = BASE_PORT2 + k as u16 * MAX_NODES + node_id as u16;

            let (remote, ctrl_as_receiver) = exchange::client_exchange(ips[k], port1, &local_info)?;
            let ctrl_as_sender = exchange::client_ctrl_only(ips[k], port2)?;

            qp.to_init(RDMA_PORT)?;
            qp.to_rtr(&remote, RDMA_PORT, GID_INDEX)?;
            qp.to_rts(psn)?;

            peers.insert(k, PeerLink {
                qp:               Arc::new(Mutex::new(qp)),
                ctrl_as_sender:   Arc::new(Mutex::new(ctrl_as_sender)),
                ctrl_as_receiver: Arc::new(Mutex::new(ctrl_as_receiver)),
                remote_slot_addr: remote.addr + (node_id * SLOT_SIZE) as u64,
                remote_mr_base:   remote.addr,
                remote_rkey:      remote.rkey,
            });
        }

        // Collect server thread results.
        for msg in rx {
            let (j, qp, remote, ctrl_as_sender, ctrl_as_receiver) = msg?;
            peers.insert(j, PeerLink {
                qp:               Arc::new(Mutex::new(qp)),
                ctrl_as_sender:   Arc::new(Mutex::new(ctrl_as_sender)),
                ctrl_as_receiver: Arc::new(Mutex::new(ctrl_as_receiver)),
                remote_slot_addr: remote.addr + (node_id * SLOT_SIZE) as u64,
                remote_mr_base:   remote.addr,
                remote_rkey:      remote.rkey,
            });
        }

        println!("[mesh] node {} connected to {} peers", node_id, peers.len());
        Ok(MeshNode { id: node_id, total, ctx, mr, peers })
    }
}
