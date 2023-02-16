// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
// This file is part of Frontier.
//
// Copyright (c) 2020-2022 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use fp_rpc::EthereumRuntimeRPCApi;
use futures::prelude::*;
use sc_client_api::backend::{Backend as BackendT, StateBackend, StorageProvider};
use sp_api::{HeaderT, ProvideRuntimeApi};
use sp_blockchain::{Backend, HeaderBackend};
use sp_core::H256;
use sp_runtime::{
	generic::BlockId,
	traits::{BlakeTwo256, Block as BlockT},
};
use sqlx::{Row, SqlitePool};
use std::{collections::VecDeque, sync::Arc, time::Duration};

/// Represents known indexed block hashes.
#[derive(Debug, Default)]
pub struct KnownHashes {
	cache: VecDeque<H256>,
	cache_size: usize,
}

impl KnownHashes {
	/// Retrieves and populates the cache with upto N last indexed blocks, where N is the `cache_size`.
	pub async fn populate_cache(&mut self, pool: &SqlitePool) -> Result<(), sqlx::Error> {
		sqlx::query(&format!(
			"SELECT substrate_block_hash FROM sync_status ORDER BY id DESC LIMIT {}",
			self.cache_size
		))
		.fetch_all(pool)
		.await?
		.iter()
		.for_each(|any_row| {
			let hash = H256::from_slice(&any_row.try_get::<Vec<u8>, _>(0).unwrap_or_default()[..]);
			self.cache.push_back(hash);
		});
		Ok(())
	}

	/// Inserts a block hash.
	pub fn insert(&mut self, value: H256) -> Option<H256> {
		let maybe_popped = if self.cache.len() == self.cache_size {
			self.cache.pop_back()
		} else {
			None
		};

		self.cache.push_front(value);
		maybe_popped
	}

	/// Appends another iterator to the current one.
	pub fn append(&mut self, other: impl Iterator<Item = H256>) {
		other.into_iter().for_each(|item| {
			self.insert(item);
		});
	}

	/// Tests the cache to see if the block exists.
	pub fn contains_cached(&self, value: &H256) -> bool {
		self.cache.contains(value)
	}

	/// Tests the cache to see if the block exists. If the item does not exist in
	/// the cache, then the SQL database is queried.
	pub async fn contains(&self, value: &H256, pool: &SqlitePool) -> bool {
		if self.contains_cached(value) {
			return true;
		}

		if let Ok(result) = sqlx::query(
			"SELECT substrate_block_hash FROM sync_status WHERE substrate_block_hash = ?",
		)
		.bind(value.as_bytes().to_owned())
		.fetch_optional(pool)
		.await
		{
			result.is_some()
		} else {
			false
		}
	}

	/// Retrieves the most recent indexed block.
	pub fn latest(&self) -> Option<&H256> {
		self.cache.front()
	}
}

/// Implements an indexer that imports blocks and their transactions.
pub struct SyncWorker<Block, Backend, Client> {
	_phantom: std::marker::PhantomData<(Block, Backend, Client)>,
	imported_blocks: KnownHashes,
	current_batch: Vec<H256>,
	batch_size: usize,
}

impl<Block: BlockT, Backend, Client> SyncWorker<Block, Backend, Client>
where
	Block: BlockT<Hash = H256> + Send + Sync,
	Client: StorageProvider<Block, Backend> + HeaderBackend<Block> + Send + Sync + 'static,
	Client: ProvideRuntimeApi<Block>,
	Client::Api: EthereumRuntimeRPCApi<Block>,
	Backend: BackendT<Block> + 'static,
	Backend::State: StateBackend<BlakeTwo256>,
{
	pub async fn run(
		client: Arc<Client>,
		substrate_backend: Arc<Backend>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		import_notifications: sc_client_api::ImportNotifications<Block>,
		batch_size: usize,
		interval: Duration,
	) {
		let mut worker = Self::new(batch_size);
		worker
			.imported_blocks
			.populate_cache(indexer_backend.pool())
			.await
			.expect("query `sync_status` table");

		// Always fire the interval future first
		let import_interval = futures_timer::Delay::new(Duration::from_nanos(1));
		let backend = substrate_backend.blockchain();
		let notifications = import_notifications.fuse();

		let mut resume_at: Option<H256> = None;
		if let Some(hash) = worker.imported_blocks.latest() {
			// If there is at least one know hash in the db, set a resume checkpoint
			if let Ok(Some(header)) = client.header(*hash) {
				resume_at = Some(*header.parent_hash())
			}
		} else {
			// If there is no data in the db, sync genesis.
			if let Ok(Some(substrate_genesis_hash)) = indexer_backend
				.insert_genesis_block_metadata(client.clone())
				.await
				.map_err(|e| {
					log::error!(
						target: "frontier-sql",
						"💔  Cannot sync genesis block: {}",
						e,
					)
				}) {
				worker.imported_blocks.insert(substrate_genesis_hash);
			}
		}

		let mut try_create_indexes = true;
		futures::pin_mut!(import_interval, notifications);
		loop {
			futures::select! {
				_ = (&mut import_interval).fuse() => {
					log::debug!(
						target: "frontier-sql",
						"🕐  New interval"
					);
					let leaves = backend.leaves();
					if let Ok(mut leaves) = leaves {
						if let Some(hash) = resume_at {
							log::debug!(
								target: "frontier-sql",
								"🔄  Resuming index task at {}",
								hash,
							);
							leaves.push(hash);
							resume_at = None;
						}

						// If a known leaf is still present when kicking an interval
						// means the chain is slow or stall.
						// If this is the case we want to remove it to move to
						// its potential siblings.
						leaves.retain(|leaf| !worker.imported_blocks.contains_cached(leaf));

						worker.index(
							client.clone(),
							indexer_backend.clone(),
							backend,
							&mut leaves,
							false
						).await;
					}
					// Reset the interval to user-defined Duration
					import_interval.reset(interval);
				},
				notification = notifications.next() => if let Some(notification) = notification {
					log::debug!(
						target: "frontier-sql",
						"📣  New notification: #{} {:?} (parent {}), best = {}",
						notification.header.number(),
						notification.hash,
						notification.header.parent_hash(),
						notification.is_new_best,
					);
					if notification.is_new_best {
						if let Some(tree_route) = notification.tree_route {
							log::debug!(
								target: "frontier-sql",
								"🔀  Re-org happened at new best {}, proceeding to canonicalize db",
								notification.hash
							);
							Self::canonicalize(Arc::clone(&indexer_backend), tree_route).await;
						}
						// On first notification try create indexes
						if try_create_indexes {
							try_create_indexes = false;
							if let Ok(_)  = indexer_backend.create_indexes().await {
								log::debug!(
									target: "frontier-sql",
									"✅  Database indexes created"
								);
							} else {
								log::error!(
									target: "frontier-sql",
									"❌  Indexes creation failed"
								);
							}
						}
						worker.index(
							client.clone(),
							indexer_backend.clone(),
							backend,
							&mut vec![notification.hash],
							true
						).await;
					}
				}
			}
		}
	}

	pub fn new(batch_size: usize) -> Self {
		SyncWorker {
			_phantom: Default::default(),
			imported_blocks: Default::default(),
			current_batch: Default::default(),
			batch_size,
		}
	}

	pub async fn index(
		&mut self,
		client: Arc<Client>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		blockchain_backend: &Backend::Blockchain,
		hashes: &mut Vec<Block::Hash>,
		force_sync: bool,
	) {
		while let Some(hash) = hashes.pop() {
			// exit if genesis block is reached
			if hash == H256::default() {
				break;
			}

			// exit if block is already imported
			if self
				.imported_blocks
				.contains(&hash, indexer_backend.pool())
				.await
			{
				log::debug!(
					target: "frontier-sql",
					"🔴 Block {:?} already imported",
					hash,
				);
				break;
			}

			log::debug!(
				target: "frontier-sql",
				"🟡 {} sync {:?}",
				["Normal", "Force"][force_sync as usize],
				hash,
			);
			if !self
				.index_block(client.clone(), indexer_backend.clone(), hash, force_sync)
				.await
			{
				break;
			}

			if let Ok(Some(header)) = blockchain_backend.header(hash) {
				let parent_hash = header.parent_hash();
				hashes.push(*parent_hash);
			}
		}
	}

	async fn index_block(
		&mut self,
		client: Arc<Client>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		hash: Block::Hash,
		force_sync: bool,
	) -> bool {
		if !self.current_batch.contains(&hash) {
			log::debug!(
				target: "frontier-sql",
				"⤵️  Queued for index {}, (batch {}/{}) force={}",
				hash,
				self.current_batch.len()+1,
				self.batch_size,
				force_sync,
			);
			self.current_batch.push(hash);
		} else if !force_sync {
			return false;
		}

		if force_sync || self.current_batch.len() == self.batch_size {
			self.index_current_batch(client, indexer_backend).await;
		}

		true
	}

	pub async fn index_current_batch(
		&mut self,
		client: Arc<Client>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
	) {
		log::debug!(
			target: "frontier-sql",
			"🛠️  Processing batch starting at {:?}",
			self.current_batch.first()
		);
		let _ = indexer_backend
			.insert_block_metadata(client.clone(), &self.current_batch)
			.await
			.map_err(|e| {
				log::error!(
					target: "frontier-sql",
					"{}",
					e,
				);
			});
		log::debug!(
			target: "frontier-sql",
			"🛠️  Inserted block metadata"
		);
		indexer_backend
			.spawn_logs_task(client.clone(), self.batch_size)
			.await; // Spawn actual logs task
		self.imported_blocks
			.append(self.current_batch.iter().cloned());
		self.current_batch.clear();
	}

	async fn canonicalize(
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		tree_route: Arc<sp_blockchain::TreeRoute<Block>>,
	) {
		let retracted = tree_route
			.retracted()
			.iter()
			.map(|hash_and_number| hash_and_number.hash)
			.collect::<Vec<_>>();
		let enacted = tree_route
			.enacted()
			.iter()
			.map(|hash_and_number| hash_and_number.hash)
			.collect::<Vec<_>>();

		if let Err(_) = indexer_backend.canonicalize(&retracted, &enacted).await {
			log::error!(
				target: "frontier-sql",
				"❌  Canonicalization failed for common ancestor {}, potentially corrupted db. Retracted: {:?}, Enacted: {:?}",
				tree_route.common_block().hash,
				retracted,
				enacted,
			);
		}
	}
}

#[cfg(test)]
mod test {
	use codec::Encode;
	use fc_rpc::{SchemaV3Override, StorageOverride};
	use fp_storage::{
		EthereumStorageSchema, OverrideHandle, ETHEREUM_CURRENT_RECEIPTS, PALLET_ETHEREUM,
		PALLET_ETHEREUM_SCHEMA,
	};
	use futures::executor;
	use sc_block_builder::BlockBuilderProvider;
	use sc_client_api::BlockchainEvents;
	use sp_consensus::BlockOrigin;
	use sp_core::{H160, H256, U256};
	use sp_io::hashing::twox_128;
	use sp_runtime::generic::{BlockId, Digest};
	use sqlx::Row;
	use std::{collections::BTreeMap, path::Path, sync::Arc};
	use substrate_test_runtime_client::{
		prelude::*, DefaultTestClientBuilderExt, TestClientBuilder, TestClientBuilderExt,
	};
	use tempfile::tempdir;

	fn storage_prefix_build(module: &[u8], storage: &[u8]) -> Vec<u8> {
		[twox_128(module), twox_128(storage)].concat().to_vec()
	}

	fn ethereum_digest() -> Digest {
		let partial_header = ethereum::PartialHeader {
			parent_hash: H256::random(),
			beneficiary: H160::default(),
			state_root: H256::default(),
			receipts_root: H256::default(),
			logs_bloom: ethereum_types::Bloom::default(),
			difficulty: U256::zero(),
			number: U256::zero(),
			gas_limit: U256::zero(),
			gas_used: U256::zero(),
			timestamp: 0u64,
			extra_data: Vec::new(),
			mix_hash: H256::default(),
			nonce: ethereum_types::H64::default(),
		};
		let ethereum_transactions: Vec<ethereum::TransactionV2> = vec![];
		let ethereum_block = ethereum::Block::new(partial_header, ethereum_transactions, vec![]);
		Digest {
			logs: vec![sp_runtime::generic::DigestItem::Consensus(
				fp_consensus::FRONTIER_ENGINE_ID,
				fp_consensus::PostLog::Hashes(fp_consensus::Hashes::from_block(ethereum_block))
					.encode(),
			)],
		}
	}

	#[tokio::test]
	async fn interval_indexing_works() {
		let tmp = tempdir().expect("create a temporary directory");
		// Initialize storage with schema V3
		let builder = TestClientBuilder::new().add_extra_storage(
			PALLET_ETHEREUM_SCHEMA.to_vec(),
			Encode::encode(&EthereumStorageSchema::V3),
		);
		// Backend
		let backend = builder.backend();
		// Client
		let (client, _) =
			builder.build_with_native_executor::<frontier_template_runtime::RuntimeApi, _>(None);
		let mut client = Arc::new(client);
		// Overrides
		let mut overrides_map = BTreeMap::new();
		overrides_map.insert(
			EthereumStorageSchema::V3,
			Box::new(SchemaV3Override::new(client.clone()))
				as Box<dyn StorageOverride<_> + Send + Sync>,
		);
		let overrides = Arc::new(OverrideHandle {
			schemas: overrides_map,
			fallback: Box::new(SchemaV3Override::new(client.clone())),
		});
		// Indexer backend
		let indexer_backend = fc_db::sql::Backend::new(
			fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
				path: Path::new("sqlite:///")
					.join(tmp.path().strip_prefix("/").unwrap().to_str().unwrap())
					.join("test.db3")
					.to_str()
					.unwrap(),
				create_if_missing: true,
			}),
			100,
			overrides.clone(),
		)
		.await
		.expect("indexer pool to be created");
		// Pool
		let pool = indexer_backend.pool().clone();

		// Create 10 blocks, 2 receipts each, 1 log per receipt
		let mut logs: Vec<(i32, fc_db::sql::Log)> = vec![];
		for block_number in 1..11 {
			// New block including pallet ethereum block digest
			let mut builder = client.new_block(ethereum_digest()).unwrap();
			// Addresses
			let address_1 = H160::random();
			let address_2 = H160::random();
			// Topics
			let topics_1_1 = H256::random();
			let topics_1_2 = H256::random();
			let topics_2_1 = H256::random();
			let topics_2_2 = H256::random();
			let topics_2_3 = H256::random();
			let topics_2_4 = H256::random();

			let receipts = Encode::encode(&vec![
				ethereum::ReceiptV3::EIP1559(ethereum::EIP1559ReceiptData {
					status_code: 0u8,
					used_gas: U256::zero(),
					logs_bloom: ethereum_types::Bloom::zero(),
					logs: vec![ethereum::Log {
						address: address_1,
						topics: vec![topics_1_1, topics_1_2],
						data: vec![],
					}],
				}),
				ethereum::ReceiptV3::EIP1559(ethereum::EIP1559ReceiptData {
					status_code: 0u8,
					used_gas: U256::zero(),
					logs_bloom: ethereum_types::Bloom::zero(),
					logs: vec![ethereum::Log {
						address: address_2,
						topics: vec![topics_2_1, topics_2_2, topics_2_3, topics_2_4],
						data: vec![],
					}],
				}),
			]);
			builder
				.push_storage_change(
					storage_prefix_build(PALLET_ETHEREUM, ETHEREUM_CURRENT_RECEIPTS),
					Some(receipts),
				)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			logs.push((
				block_number as i32,
				fc_db::sql::Log {
					address: address_1.as_bytes().to_owned(),
					topic_1: topics_1_1.as_bytes().to_owned(),
					topic_2: topics_1_2.as_bytes().to_owned(),
					topic_3: H256::default().as_bytes().to_owned(),
					topic_4: H256::default().as_bytes().to_owned(),
					log_index: 0i32,
					transaction_index: 0i32,
					substrate_block_hash: block_hash.as_bytes().to_owned(),
				},
			));
			logs.push((
				block_number as i32,
				fc_db::sql::Log {
					address: address_2.as_bytes().to_owned(),
					topic_1: topics_2_1.as_bytes().to_owned(),
					topic_2: topics_2_2.as_bytes().to_owned(),
					topic_3: topics_2_3.as_bytes().to_owned(),
					topic_4: topics_2_4.as_bytes().to_owned(),
					log_index: 0i32,
					transaction_index: 1i32,
					substrate_block_hash: block_hash.as_bytes().to_owned(),
				},
			));
		}

		// Spawn worker after creating the blocks will resolve the interval future.
		// Because the SyncWorker is spawned at service level, in the real world this will only
		// happen when we are in major syncing (where there is lack of import notificatons).
		tokio::task::spawn(async move {
			crate::sql::SyncWorker::run(
				client.clone(),
				backend.clone(),
				Arc::new(indexer_backend),
				client.clone().import_notification_stream(),
				10,                                // batch size
				std::time::Duration::from_secs(1), // interval duration
			)
			.await
		});

		// Enough time for interval to run
		futures_timer::Delay::new(std::time::Duration::from_millis(1100)).await;

		// Query db
		let db_logs = sqlx::query(
			"SELECT
					b.block_number,
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					a.substrate_block_hash
				FROM logs AS a INNER JOIN blocks AS b ON a.substrate_block_hash = b.substrate_block_hash
				ORDER BY b.block_number ASC, log_index ASC, transaction_index ASC",
		)
		.fetch_all(&pool)
		.await
		.expect("test query result")
		.iter()
		.map(|row| {
			let block_number = row.get::<i32, _>(0);
			let address = row.get::<Vec<u8>, _>(1);
			let topic_1 = row.get::<Vec<u8>, _>(2);
			let topic_2 = row.get::<Vec<u8>, _>(3);
			let topic_3 = row.get::<Vec<u8>, _>(4);
			let topic_4 = row.get::<Vec<u8>, _>(5);
			let log_index = row.get::<i32, _>(6);
			let transaction_index = row.get::<i32, _>(7);
			let substrate_block_hash = row.get::<Vec<u8>, _>(8);
			(
				block_number,
				fc_db::sql::Log {
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					substrate_block_hash,
				},
			)
		})
		.collect::<Vec<(i32, fc_db::sql::Log)>>();

		// Expect the db to contain 20 rows. 10 blocks, 2 logs each.
		// Db data is sorted ASC by block_number, log_index and transaction_index.
		// This is necessary because indexing is done from tip to genesis.
		// Expect the db resultset to be equal to the locally produced Log vector.
		assert_eq!(db_logs, logs);
	}

	#[tokio::test]
	async fn notification_indexing_works() {
		let tmp = tempdir().expect("create a temporary directory");
		// Initialize storage with schema V3
		let builder = TestClientBuilder::new().add_extra_storage(
			PALLET_ETHEREUM_SCHEMA.to_vec(),
			Encode::encode(&EthereumStorageSchema::V3),
		);
		// Backend
		let backend = builder.backend();
		// Client
		let (client, _) =
			builder.build_with_native_executor::<frontier_template_runtime::RuntimeApi, _>(None);
		let mut client = Arc::new(client);
		// Overrides
		let mut overrides_map = BTreeMap::new();
		overrides_map.insert(
			EthereumStorageSchema::V3,
			Box::new(SchemaV3Override::new(client.clone()))
				as Box<dyn StorageOverride<_> + Send + Sync>,
		);
		let overrides = Arc::new(OverrideHandle {
			schemas: overrides_map,
			fallback: Box::new(SchemaV3Override::new(client.clone())),
		});
		// Indexer backend
		let indexer_backend = fc_db::sql::Backend::new(
			fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
				path: Path::new("sqlite:///")
					.join(tmp.path().strip_prefix("/").unwrap().to_str().unwrap())
					.join("test.db3")
					.to_str()
					.unwrap(),
				create_if_missing: true,
			}),
			100,
			overrides.clone(),
		)
		.await
		.expect("indexer pool to be created");
		// Pool
		let pool = indexer_backend.pool().clone();

		// Spawn worker after creating the blocks will resolve the interval future.
		// Because the SyncWorker is spawned at service level, in the real world this will only
		// happen when we are in major syncing (where there is lack of import notificatons).
		let notification_stream = client.clone().import_notification_stream();
		let client_inner = client.clone();
		tokio::task::spawn(async move {
			crate::sql::SyncWorker::run(
				client_inner,
				backend.clone(),
				Arc::new(indexer_backend),
				notification_stream,
				10,                                // batch size
				std::time::Duration::from_secs(1), // interval duration
			)
			.await
		});
		// Create 10 blocks, 2 receipts each, 1 log per receipt
		let mut logs: Vec<(i32, fc_db::sql::Log)> = vec![];
		for block_number in 1..11 {
			// New block including pallet ethereum block digest
			let mut builder = client.new_block(ethereum_digest()).unwrap();
			// Addresses
			let address_1 = H160::random();
			let address_2 = H160::random();
			// Topics
			let topics_1_1 = H256::random();
			let topics_1_2 = H256::random();
			let topics_2_1 = H256::random();
			let topics_2_2 = H256::random();
			let topics_2_3 = H256::random();
			let topics_2_4 = H256::random();

			let receipts = Encode::encode(&vec![
				ethereum::ReceiptV3::EIP1559(ethereum::EIP1559ReceiptData {
					status_code: 0u8,
					used_gas: U256::zero(),
					logs_bloom: ethereum_types::Bloom::zero(),
					logs: vec![ethereum::Log {
						address: address_1,
						topics: vec![topics_1_1, topics_1_2],
						data: vec![],
					}],
				}),
				ethereum::ReceiptV3::EIP1559(ethereum::EIP1559ReceiptData {
					status_code: 0u8,
					used_gas: U256::zero(),
					logs_bloom: ethereum_types::Bloom::zero(),
					logs: vec![ethereum::Log {
						address: address_2,
						topics: vec![topics_2_1, topics_2_2, topics_2_3, topics_2_4],
						data: vec![],
					}],
				}),
			]);
			builder
				.push_storage_change(
					storage_prefix_build(PALLET_ETHEREUM, ETHEREUM_CURRENT_RECEIPTS),
					Some(receipts),
				)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			logs.push((
				block_number as i32,
				fc_db::sql::Log {
					address: address_1.as_bytes().to_owned(),
					topic_1: topics_1_1.as_bytes().to_owned(),
					topic_2: topics_1_2.as_bytes().to_owned(),
					topic_3: H256::default().as_bytes().to_owned(),
					topic_4: H256::default().as_bytes().to_owned(),
					log_index: 0i32,
					transaction_index: 0i32,
					substrate_block_hash: block_hash.as_bytes().to_owned(),
				},
			));
			logs.push((
				block_number as i32,
				fc_db::sql::Log {
					address: address_2.as_bytes().to_owned(),
					topic_1: topics_2_1.as_bytes().to_owned(),
					topic_2: topics_2_2.as_bytes().to_owned(),
					topic_3: topics_2_3.as_bytes().to_owned(),
					topic_4: topics_2_4.as_bytes().to_owned(),
					log_index: 0i32,
					transaction_index: 1i32,
					substrate_block_hash: block_hash.as_bytes().to_owned(),
				},
			));
			// Let's not notify too quickly
			futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
		}

		// Query db
		let db_logs = sqlx::query(
			"SELECT
					b.block_number,
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					a.substrate_block_hash
				FROM logs AS a INNER JOIN blocks AS b ON a.substrate_block_hash = b.substrate_block_hash
				ORDER BY b.block_number ASC, log_index ASC, transaction_index ASC",
		)
		.fetch_all(&pool)
		.await
		.expect("test query result")
		.iter()
		.map(|row| {
			let block_number = row.get::<i32, _>(0);
			let address = row.get::<Vec<u8>, _>(1);
			let topic_1 = row.get::<Vec<u8>, _>(2);
			let topic_2 = row.get::<Vec<u8>, _>(3);
			let topic_3 = row.get::<Vec<u8>, _>(4);
			let topic_4 = row.get::<Vec<u8>, _>(5);
			let log_index = row.get::<i32, _>(6);
			let transaction_index = row.get::<i32, _>(7);
			let substrate_block_hash = row.get::<Vec<u8>, _>(8);
			(
				block_number,
				fc_db::sql::Log {
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					substrate_block_hash,
				},
			)
		})
		.collect::<Vec<(i32, fc_db::sql::Log)>>();

		// Expect the db to contain 20 rows. 10 blocks, 2 logs each.
		// Db data is sorted ASC by block_number, log_index and transaction_index.
		// This is necessary because indexing is done from tip to genesis.
		// Expect the db resultset to be equal to the locally produced Log vector.
		assert_eq!(db_logs, logs);
	}

	#[tokio::test]
	async fn canonicalize_with_interval_notification_mix_works() {
		let tmp = tempdir().expect("create a temporary directory");
		// Initialize storage with schema V3
		let builder = TestClientBuilder::new().add_extra_storage(
			PALLET_ETHEREUM_SCHEMA.to_vec(),
			Encode::encode(&EthereumStorageSchema::V3),
		);
		// Backend
		let backend = builder.backend();
		// Client
		let (client, _) =
			builder.build_with_native_executor::<frontier_template_runtime::RuntimeApi, _>(None);
		let mut client = Arc::new(client);
		// Overrides
		let mut overrides_map = BTreeMap::new();
		overrides_map.insert(
			EthereumStorageSchema::V3,
			Box::new(SchemaV3Override::new(client.clone()))
				as Box<dyn StorageOverride<_> + Send + Sync>,
		);
		let overrides = Arc::new(OverrideHandle {
			schemas: overrides_map,
			fallback: Box::new(SchemaV3Override::new(client.clone())),
		});
		// Indexer backend
		let indexer_backend = fc_db::sql::Backend::new(
			fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
				path: Path::new("sqlite:///")
					.join(tmp.path().strip_prefix("/").unwrap().to_str().unwrap())
					.join("test.db3")
					.to_str()
					.unwrap(),
				create_if_missing: true,
			}),
			100,
			overrides.clone(),
		)
		.await
		.expect("indexer pool to be created");

		// Pool
		let pool = indexer_backend.pool().clone();

		// Create 10 blocks saving the common ancestor for branching.
		let mut parent_hash = client
			.header(&BlockId::Number(sp_runtime::traits::Zero::zero()))
			.unwrap()
			.expect("genesis header")
			.hash();
		let mut common_ancestor = parent_hash;
		let mut hashes_to_be_orphaned: Vec<H256> = vec![];
		for block_number in 1..11 {
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			if block_number == 8 {
				common_ancestor = block_hash;
			}
			if block_number == 9 || block_number == 10 {
				hashes_to_be_orphaned.push(block_hash);
			}
			parent_hash = block_hash;
		}

		// Spawn indexer task
		let notification_stream = client.clone().import_notification_stream();
		let client_inner = client.clone();
		tokio::task::spawn(async move {
			crate::sql::SyncWorker::run(
				client_inner,
				backend.clone(),
				Arc::new(indexer_backend),
				notification_stream,
				10,                                // batch size
				std::time::Duration::from_secs(1), // interval duration
			)
			.await
		});

		// Enough time for interval to run
		futures_timer::Delay::new(std::time::Duration::from_millis(1100)).await;

		// Create the new longest chain, 10 more blocks on top of the common ancestor.
		parent_hash = common_ancestor;
		for _ in 1..11 {
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			parent_hash = block_hash;
			futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
		}

		// Test the reorged chain is correctly indexed.
		let res = sqlx::query("SELECT substrate_block_hash, is_canon, block_number FROM blocks")
			.fetch_all(&pool)
			.await
			.expect("test query result")
			.iter()
			.map(|row| {
				let substrate_block_hash = H256::from_slice(&row.get::<Vec<u8>, _>(0)[..]);
				let is_canon = row.get::<i32, _>(1);
				let block_number = row.get::<i32, _>(2);
				(substrate_block_hash, is_canon, block_number)
			})
			.collect::<Vec<(H256, i32, i32)>>();

		// 20 blocks in total
		assert_eq!(res.len(), 20);

		// 18 of which are canon
		let canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 1 { Some(it) } else { None })
			.collect::<Vec<(H256, i32, i32)>>();
		assert_eq!(canon.len(), 18);

		// and 2 of which are the originally tracked as orphaned
		let not_canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 0 { Some(it.0) } else { None })
			.collect::<Vec<H256>>();
		assert_eq!(not_canon.len(), hashes_to_be_orphaned.len());
		assert!(not_canon.iter().all(|h| hashes_to_be_orphaned.contains(h)));
	}

	#[tokio::test]
	async fn canonicalize_with_interval_works() {
		let tmp = tempdir().expect("create a temporary directory");
		// Initialize storage with schema V3
		let builder = TestClientBuilder::new().add_extra_storage(
			PALLET_ETHEREUM_SCHEMA.to_vec(),
			Encode::encode(&EthereumStorageSchema::V3),
		);
		// Backend
		let backend = builder.backend();
		// Client
		let (client, _) =
			builder.build_with_native_executor::<frontier_template_runtime::RuntimeApi, _>(None);
		let mut client = Arc::new(client);
		// Overrides
		let mut overrides_map = BTreeMap::new();
		overrides_map.insert(
			EthereumStorageSchema::V3,
			Box::new(SchemaV3Override::new(client.clone()))
				as Box<dyn StorageOverride<_> + Send + Sync>,
		);
		let overrides = Arc::new(OverrideHandle {
			schemas: overrides_map,
			fallback: Box::new(SchemaV3Override::new(client.clone())),
		});
		// Indexer backend
		let indexer_backend = fc_db::sql::Backend::new(
			fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
				path: Path::new("sqlite:///")
					.join(tmp.path().strip_prefix("/").unwrap().to_str().unwrap())
					.join("test.db3")
					.to_str()
					.unwrap(),
				create_if_missing: true,
			}),
			100,
			overrides.clone(),
		)
		.await
		.expect("indexer pool to be created");

		// Pool
		let pool = indexer_backend.pool().clone();

		// Create 10 blocks saving the common ancestor for branching.
		let mut parent_hash = client
			.header(&BlockId::Number(sp_runtime::traits::Zero::zero()))
			.unwrap()
			.expect("genesis header")
			.hash();
		let mut common_ancestor = parent_hash;
		let mut hashes_to_be_orphaned: Vec<H256> = vec![];
		for block_number in 1..11 {
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			if block_number == 8 {
				common_ancestor = block_hash;
			}
			if block_number == 9 || block_number == 10 {
				hashes_to_be_orphaned.push(block_hash);
			}
			parent_hash = block_hash;
		}

		// Create the new longest chain, 10 more blocks on top of the common ancestor.
		parent_hash = common_ancestor;
		for _ in 1..11 {
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			parent_hash = block_hash;
		}

		// Spawn indexer task
		tokio::task::spawn(async move {
			crate::sql::SyncWorker::run(
				client.clone(),
				backend.clone(),
				Arc::new(indexer_backend),
				client.clone().import_notification_stream(),
				10,                                // batch size
				std::time::Duration::from_secs(1), // interval duration
			)
			.await
		});
		// Enough time for interval to run
		futures_timer::Delay::new(std::time::Duration::from_millis(2500)).await;

		// Test the reorged chain is correctly indexed.
		let res = sqlx::query("SELECT substrate_block_hash, is_canon, block_number FROM blocks")
			.fetch_all(&pool)
			.await
			.expect("test query result")
			.iter()
			.map(|row| {
				let substrate_block_hash = H256::from_slice(&row.get::<Vec<u8>, _>(0)[..]);
				let is_canon = row.get::<i32, _>(1);
				let block_number = row.get::<i32, _>(2);
				(substrate_block_hash, is_canon, block_number)
			})
			.collect::<Vec<(H256, i32, i32)>>();

		// 20 blocks in total
		assert_eq!(res.len(), 20);

		// 18 of which are canon
		let canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 1 { Some(it) } else { None })
			.collect::<Vec<(H256, i32, i32)>>();
		assert_eq!(canon.len(), 18);

		// and 2 of which are the originally tracked as orphaned
		let not_canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 0 { Some(it.0) } else { None })
			.collect::<Vec<H256>>();
		assert_eq!(not_canon.len(), hashes_to_be_orphaned.len());
		assert!(not_canon.iter().all(|h| hashes_to_be_orphaned.contains(h)));
	}

	#[tokio::test]
	async fn canonicalize_with_notification_works() {
		let tmp = tempdir().expect("create a temporary directory");
		// Initialize storage with schema V3
		let builder = TestClientBuilder::new().add_extra_storage(
			PALLET_ETHEREUM_SCHEMA.to_vec(),
			Encode::encode(&EthereumStorageSchema::V3),
		);
		// Backend
		let backend = builder.backend();
		// Client
		let (client, _) =
			builder.build_with_native_executor::<frontier_template_runtime::RuntimeApi, _>(None);
		let mut client = Arc::new(client);
		// Overrides
		let mut overrides_map = BTreeMap::new();
		overrides_map.insert(
			EthereumStorageSchema::V3,
			Box::new(SchemaV3Override::new(client.clone()))
				as Box<dyn StorageOverride<_> + Send + Sync>,
		);
		let overrides = Arc::new(OverrideHandle {
			schemas: overrides_map,
			fallback: Box::new(SchemaV3Override::new(client.clone())),
		});
		// Indexer backend
		let indexer_backend = fc_db::sql::Backend::new(
			fc_db::sql::BackendConfig::Sqlite(fc_db::sql::SqliteBackendConfig {
				path: Path::new("sqlite:///")
					.join(tmp.path().strip_prefix("/").unwrap().to_str().unwrap())
					.join("test.db3")
					.to_str()
					.unwrap(),
				create_if_missing: true,
			}),
			100,
			overrides.clone(),
		)
		.await
		.expect("indexer pool to be created");

		// Pool
		let pool = indexer_backend.pool().clone();

		// Spawn indexer task
		let notification_stream = client.clone().import_notification_stream();
		let client_inner = client.clone();
		tokio::task::spawn(async move {
			crate::sql::SyncWorker::run(
				client_inner,
				backend.clone(),
				Arc::new(indexer_backend),
				notification_stream,
				10,                                // batch size
				std::time::Duration::from_secs(1), // interval duration
			)
			.await
		});

		// Create 10 blocks saving the common ancestor for branching.
		let mut parent_hash = client
			.header(&BlockId::Number(sp_runtime::traits::Zero::zero()))
			.unwrap()
			.expect("genesis header")
			.hash();
		let mut common_ancestor = parent_hash;
		let mut hashes_to_be_orphaned: Vec<H256> = vec![];
		for block_number in 1..11 {
			// New block including pallet ethereum block digest
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			if block_number == 8 {
				common_ancestor = block_hash;
			}
			if block_number == 9 || block_number == 10 {
				hashes_to_be_orphaned.push(block_hash);
			}
			parent_hash = block_hash;
			// Let's not notify too quickly
			futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
		}

		// Test all blocks are initially canon.
		let mut res = sqlx::query("SELECT is_canon FROM blocks")
			.fetch_all(&pool)
			.await
			.expect("test query result")
			.iter()
			.map(|row| row.get::<i32, _>(0))
			.collect::<Vec<i32>>();

		assert_eq!(res.len(), 10);
		res.dedup();
		assert_eq!(res.len(), 1);

		// Create the new longest chain, 10 more blocks on top of the common ancestor.
		parent_hash = common_ancestor;
		for _ in 1..11 {
			// New block including pallet ethereum block digest
			let builder = client
				.new_block_at(&BlockId::Hash(parent_hash), ethereum_digest(), false)
				.unwrap();
			let block = builder.build().unwrap().block;
			let block_hash = block.header.hash();
			executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();
			parent_hash = block_hash;
			// Let's not notify too quickly
			futures_timer::Delay::new(std::time::Duration::from_millis(100)).await;
		}

		// Test the reorged chain is correctly indexed.
		let res = sqlx::query("SELECT substrate_block_hash, is_canon, block_number FROM blocks")
			.fetch_all(&pool)
			.await
			.expect("test query result")
			.iter()
			.map(|row| {
				let substrate_block_hash = H256::from_slice(&row.get::<Vec<u8>, _>(0)[..]);
				let is_canon = row.get::<i32, _>(1);
				let block_number = row.get::<i32, _>(2);
				(substrate_block_hash, is_canon, block_number)
			})
			.collect::<Vec<(H256, i32, i32)>>();

		// 20 blocks in total
		assert_eq!(res.len(), 20);

		// 18 of which are canon
		let canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 1 { Some(it) } else { None })
			.collect::<Vec<(H256, i32, i32)>>();
		assert_eq!(canon.len(), 18);

		// and 2 of which are the originally tracked as orphaned
		let not_canon = res
			.clone()
			.into_iter()
			.filter_map(|it| if it.1 == 0 { Some(it.0) } else { None })
			.collect::<Vec<H256>>();
		assert_eq!(not_canon.len(), hashes_to_be_orphaned.len());
		assert!(not_canon.iter().all(|h| hashes_to_be_orphaned.contains(h)));
	}
}
