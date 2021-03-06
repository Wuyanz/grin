// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::net::SocketAddr;
use std::sync::Arc;

use conn::{Message, MessageHandler, Response};
use core::core;
use core::core::hash::Hash;
use core::core::CompactBlock;
use msg::{
	BanReason, GetPeerAddrs, Headers, Locator, PeerAddrs, Ping, Pong, SockAddr, TxHashSetArchive,
	TxHashSetRequest, Type,
};
use rand::{self, Rng};
use types::{Error, NetAdapter};
use util::LOGGER;

pub struct Protocol {
	adapter: Arc<NetAdapter>,
	addr: SocketAddr,
}

impl Protocol {
	pub fn new(adapter: Arc<NetAdapter>, addr: SocketAddr) -> Protocol {
		Protocol { adapter, addr }
	}
}

impl MessageHandler for Protocol {
	fn consume<'a>(&self, mut msg: Message<'a>) -> Result<Option<Response<'a>>, Error> {
		let adapter = &self.adapter;

		// If we received a msg from a banned peer then log and drop it.
		// If we are getting a lot of these then maybe we are not cleaning
		// banned peers up correctly?
		if adapter.is_banned(self.addr.clone()) {
			debug!(
				LOGGER,
				"handler: consume: peer {:?} banned, received: {:?}, dropping.",
				self.addr,
				msg.header.msg_type,
			);
			return Ok(None);
		}

		match msg.header.msg_type {
			Type::Ping => {
				let ping: Ping = msg.body()?;
				adapter.peer_difficulty(self.addr, ping.total_difficulty, ping.height);

				Ok(Some(msg.respond(
					Type::Pong,
					Pong {
						total_difficulty: adapter.total_difficulty(),
						height: adapter.total_height(),
					},
				)))
			}

			Type::Pong => {
				let pong: Pong = msg.body()?;
				adapter.peer_difficulty(self.addr, pong.total_difficulty, pong.height);
				Ok(None)
			}

			Type::BanReason => {
				let ban_reason: BanReason = msg.body()?;
				error!(LOGGER, "handle_payload: BanReason {:?}", ban_reason);
				Ok(None)
			}

			Type::Transaction => {
				debug!(
					LOGGER,
					"handle_payload: received tx: msg_len: {}", msg.header.msg_len
				);
				let tx: core::Transaction = msg.body()?;
				adapter.transaction_received(tx, false);
				Ok(None)
			}

			Type::StemTransaction => {
				debug!(
					LOGGER,
					"handle_payload: received stem tx: msg_len: {}", msg.header.msg_len
				);
				let tx: core::Transaction = msg.body()?;
				adapter.transaction_received(tx, true);
				Ok(None)
			}

			Type::GetBlock => {
				let h: Hash = msg.body()?;
				trace!(LOGGER, "handle_payload: GetBlock {}", h);

				let bo = adapter.get_block(h);
				if let Some(b) = bo {
					return Ok(Some(msg.respond(Type::Block, b)));
				}
				Ok(None)
			}

			Type::Block => {
				debug!(
					LOGGER,
					"handle_payload: received block: msg_len: {}", msg.header.msg_len
				);
				let b: core::Block = msg.body()?;

				adapter.block_received(b, self.addr);
				Ok(None)
			}

			Type::GetCompactBlock => {
				let h: Hash = msg.body()?;

				if let Some(b) = adapter.get_block(h) {
					// if we have txs in the block send a compact block
					// but if block is empty -
					// to allow us to test all code paths, randomly choose to send
					// either the block or the compact block
					let mut rng = rand::thread_rng();

					if b.kernels().len() == 1 && rng.gen() {
						debug!(
							LOGGER,
							"handle_payload: GetCompactBlock: empty block, sending full block",
						);

						Ok(Some(msg.respond(Type::Block, b)))
					} else {
						let cb: CompactBlock = b.into();
						Ok(Some(msg.respond(Type::CompactBlock, cb)))
					}
				} else {
					Ok(None)
				}
			}

			Type::CompactBlock => {
				debug!(
					LOGGER,
					"handle_payload: received compact block: msg_len: {}", msg.header.msg_len
				);
				let b: core::CompactBlock = msg.body()?;

				adapter.compact_block_received(b, self.addr);
				Ok(None)
			}

			Type::GetHeaders => {
				// load headers from the locator
				let loc: Locator = msg.body()?;
				let headers = adapter.locate_headers(loc.hashes);

				// serialize and send all the headers over
				Ok(Some(msg.respond(
					Type::Headers,
					Headers { headers: headers },
				)))
			}

			// "header first" block propagation - if we have not yet seen this block
			// we can go request it from some of our peers
			Type::Header => {
				let header: core::BlockHeader = msg.body()?;

				adapter.header_received(header, self.addr);

				// we do not return a hash here as we never request a single header
				// a header will always arrive unsolicited
				Ok(None)
			}

			Type::Headers => {
				let headers: Headers = msg.body()?;
				adapter.headers_received(headers.headers, self.addr);
				Ok(None)
			}

			Type::GetPeerAddrs => {
				let get_peers: GetPeerAddrs = msg.body()?;
				let peer_addrs = adapter.find_peer_addrs(get_peers.capabilities);
				Ok(Some(msg.respond(
					Type::PeerAddrs,
					PeerAddrs {
						peers: peer_addrs.iter().map(|sa| SockAddr(*sa)).collect(),
					},
				)))
			}

			Type::PeerAddrs => {
				let peer_addrs: PeerAddrs = msg.body()?;
				adapter.peer_addrs_received(peer_addrs.peers.iter().map(|pa| pa.0).collect());
				Ok(None)
			}

			Type::TxHashSetRequest => {
				let sm_req: TxHashSetRequest = msg.body()?;
				debug!(
					LOGGER,
					"handle_payload: txhashset req for {} at {}", sm_req.hash, sm_req.height
				);

				let txhashset = self.adapter.txhashset_read(sm_req.hash);

				if let Some(txhashset) = txhashset {
					let file_sz = txhashset.reader.metadata()?.len();
					let mut resp = msg.respond(
						Type::TxHashSetArchive,
						&TxHashSetArchive {
							height: sm_req.height as u64,
							hash: sm_req.hash,
							bytes: file_sz,
						},
					);
					resp.add_attachment(txhashset.reader);
					Ok(Some(resp))
				} else {
					Ok(None)
				}
			}

			Type::TxHashSetArchive => {
				let sm_arch: TxHashSetArchive = msg.body()?;
				debug!(
					LOGGER,
					"handle_payload: txhashset archive for {} at {}. size={}",
					sm_arch.hash,
					sm_arch.height,
					sm_arch.bytes,
				);
				if !self.adapter.txhashset_receive_ready() {
					error!(
						LOGGER,
						"handle_payload: txhashset archive received but SyncStatus not on TxHashsetDownload",
					);
					return Err(Error::BadMessage);
				}
				let mut tmp = env::temp_dir();
				tmp.push("txhashset.zip");
				let mut save_txhashset_to_file = |file| -> Result<(), Error> {
					let mut tmp_zip = BufWriter::new(File::create(file)?);
					msg.copy_attachment(sm_arch.bytes as usize, &mut tmp_zip)?;
					tmp_zip.into_inner().unwrap().sync_all()?;
					Ok(())
				};

				if let Err(e) = save_txhashset_to_file(tmp.clone()) {
					error!(
						LOGGER,
						"handle_payload: txhashset archive save to file fail. err={:?}", e
					);
					return Err(e);
				}

				trace!(
					LOGGER,
					"handle_payload: txhashset archive save to file {:?} success",
					tmp,
				);

				let tmp_zip = File::open(tmp)?;
				let res = self.adapter
					.txhashset_write(sm_arch.hash, tmp_zip, self.addr);

				debug!(
					LOGGER,
					"handle_payload: txhashset archive for {} at {}, DONE. Data Ok: {}",
					sm_arch.hash,
					sm_arch.height,
					res
				);

				Ok(None)
			}

			_ => {
				debug!(LOGGER, "unknown message type {:?}", msg.header.msg_type);
				Ok(None)
			}
		}
	}
}
