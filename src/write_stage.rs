//! The `write_stage` module implements the TPU's write stage. It
//! writes entries to the given writer, which is typically a file or
//! stdout, and then sends the Entry to its output channel.

use bank::Bank;
use counter::Counter;
use crdt::Crdt;
use entry::Entry;
use ledger::{Block, LedgerWriter};
use log::Level;
use packet::BlobRecycler;
use result::{Error, Result};
use service::Service;
use signature::Keypair;
use std::net::UdpSocket;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, Builder, JoinHandle};
use std::time::{Duration, Instant};
use streamer::responder;
use timing::{duration_as_ms, duration_as_s};
use vote_stage::send_leader_vote;

pub struct WriteStage {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl WriteStage {
    /// Process any Entry items that have been published by the RecordStage.
    /// continuosly send entries out
    pub fn write_and_send_entries(
        crdt: &Arc<RwLock<Crdt>>,
        ledger_writer: &mut LedgerWriter,
        entry_sender: &Sender<Vec<Entry>>,
        entry_receiver: &Receiver<Vec<Entry>>,
    ) -> Result<()> {
        let mut ventries = Vec::new();
        let entries = entry_receiver.recv_timeout(Duration::new(1, 0))?;
        let mut num_entries = entries.len();
        let mut num_txs = 0;

        ventries.push(entries);
        while let Ok(more) = entry_receiver.try_recv() {
            num_entries += more.len();
            ventries.push(more);
        }

        info!("write_stage entries: {}", num_entries);

        let to_blobs_total = 0;
        let mut blob_send_total = 0;
        let mut register_entry_total = 0;
        let mut crdt_votes_total = 0;

        let start = Instant::now();
        for _ in 0..ventries.len() {
            let entries = ventries.pop().unwrap();
            for e in entries.iter() {
                num_txs += e.transactions.len();
            }
            let crdt_votes_start = Instant::now();
            let votes = &entries.votes();
            crdt.write().unwrap().insert_votes(&votes);
            crdt_votes_total += duration_as_ms(&crdt_votes_start.elapsed());

            ledger_writer.write_entries(entries.clone())?;

            let register_entry_start = Instant::now();
            register_entry_total += duration_as_ms(&register_entry_start.elapsed());

            inc_new_counter_info!("write_stage-write_entries", entries.len());

            //TODO(anatoly): real stake based voting needs to change this
            //leader simply votes if the current set of validators have voted
            //on a valid last id

            trace!("New entries? {}", entries.len());
            let blob_send_start = Instant::now();
            if !entries.is_empty() {
                inc_new_counter_info!("write_stage-recv_vote", votes.len());
                inc_new_counter_info!("write_stage-broadcast_entries", entries.len());
                trace!("broadcasting {}", entries.len());
                entry_sender.send(entries)?;
            }

            blob_send_total += duration_as_ms(&blob_send_start.elapsed());
        }
        info!("done write_stage txs: {} time {} ms txs/s: {} to_blobs_total: {} register_entry_total: {} blob_send_total: {} crdt_votes_total: {}",
              num_txs, duration_as_ms(&start.elapsed()),
              num_txs as f32 / duration_as_s(&start.elapsed()),
              to_blobs_total,
              register_entry_total,
              blob_send_total,
              crdt_votes_total);

        Ok(())
    }

    /// Create a new WriteStage for writing and broadcasting entries.
    pub fn new(
        keypair: Keypair,
        bank: Arc<Bank>,
        crdt: Arc<RwLock<Crdt>>,
        blob_recycler: BlobRecycler,
        ledger_path: &str,
        entry_receiver: Receiver<Vec<Entry>>,
    ) -> (Self, Receiver<Vec<Entry>>) {
        let (vote_blob_sender, vote_blob_receiver) = channel();
        let send = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let t_responder = responder(
            "write_stage_vote_sender",
            Arc::new(send),
            blob_recycler.clone(),
            vote_blob_receiver,
        );
        let (entry_sender, entry_receiver_forward) = channel();
        let mut ledger_writer = LedgerWriter::recover(ledger_path).unwrap();

        let thread_hdl = Builder::new()
            .name("solana-writer".to_string())
            .spawn(move || {
                let mut last_vote = 0;
                let mut last_valid_validator_timestamp = 0;
                let id = crdt.read().unwrap().id;
                loop {
                    if let Err(e) = Self::write_and_send_entries(
                        &crdt,
                        &mut ledger_writer,
                        &entry_sender,
                        &entry_receiver,
                    ) {
                        match e {
                            Error::RecvTimeoutError(RecvTimeoutError::Disconnected) => break,
                            Error::RecvTimeoutError(RecvTimeoutError::Timeout) => (),
                            _ => {
                                inc_new_counter_info!(
                                    "write_stage-write_and_send_entries-error",
                                    1
                                );
                                error!("{:?}", e);
                            }
                        }
                    };
                    if let Err(e) = send_leader_vote(
                        &id,
                        &keypair,
                        &bank,
                        &crdt,
                        &blob_recycler,
                        &vote_blob_sender,
                        &mut last_vote,
                        &mut last_valid_validator_timestamp,
                    ) {
                        inc_new_counter_info!("write_stage-leader_vote-error", 1);
                        error!("{:?}", e);
                    }
                }
            }).unwrap();

        let thread_hdls = vec![t_responder, thread_hdl];
        (WriteStage { thread_hdls }, entry_receiver_forward)
    }
}

impl Service for WriteStage {
    type JoinReturnType = ();

    fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}
