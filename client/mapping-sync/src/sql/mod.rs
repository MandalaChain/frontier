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
use sqlx::Row;
use std::{sync::Arc, time::Duration};

pub struct SyncWorker<Block, Backend, Client>(std::marker::PhantomData<(Block, Backend, Client)>);
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
		notifications: sc_client_api::ImportNotifications<Block>,
		batch_size: usize,
		interval: Duration,
	) {
		let mut current_batch: Vec<Block::Hash> = vec![];

		// Always fire the interval future first
		let import_interval = futures_timer::Delay::new(Duration::from_nanos(1));
		let backend = substrate_backend.blockchain();
		let notifications = notifications.fuse();

		let mut known_hashes =
			sqlx::query("SELECT substrate_block_hash FROM sync_status ORDER BY id ASC")
				.fetch_all(indexer_backend.pool())
				.await
				.expect("query `sync_status` table")
				.iter()
				.map(|any_row| {
					H256::from_slice(&any_row.try_get::<Vec<u8>, _>(0).unwrap_or_default()[..])
				})
				.collect::<Vec<H256>>();

		let mut resume_at: Option<H256> = None;
		if let Some(hash) = known_hashes.last() {
			// If there is at least one know hash in the db, set a resume checkpoint
			if let Ok(Some(number)) = client.number(*hash) {
				if let Ok(Some(header)) = client.header(BlockId::Number(number)) {
					resume_at = Some(*header.parent_hash())
				}
			}
		} else {
			// If there is no data in the db, sync genesis.
			let _ = indexer_backend
				.insert_genesis_block_metadata(client.clone())
				.await
				.map_err(|e| {
					log::error!(
						target: "frontier-sql",
						"💔  Cannot sync genesis block: {}",
						e,
					)
				});
		}

		let mut try_create_indexes = true;
		futures::pin_mut!(import_interval, notifications);
		loop {
			futures::select! {
				_ = (&mut import_interval).fuse() => {
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
						Self::sync_all(
							&mut leaves,
							Arc::clone(&client),
							Arc::clone(&indexer_backend),
							backend,
							batch_size,
							&mut current_batch,
							&mut known_hashes,
							false

						).await;
					}
					// Reset the interval to user-defined Duration
					import_interval.reset(interval);
				},
				notification = notifications.next() => if let Some(notification) = notification {
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
					let mut leaves = vec![notification.hash];
					Self::sync_all(
						&mut leaves,
						Arc::clone(&client),
						Arc::clone(&indexer_backend),
						backend,
						batch_size,
						&mut current_batch,
						&mut known_hashes,
						true

					).await;
				}
			}
		}
	}
	#[allow(clippy::too_many_arguments)]
	async fn sync_all(
		leaves: &mut Vec<Block::Hash>,
		client: Arc<Client>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		blockchain_backend: &Backend::Blockchain,
		batch_size: usize,
		current_batch: &mut Vec<Block::Hash>,
		known_hashes: &mut Vec<Block::Hash>,
		notified: bool,
	) {
		while let Some(leaf) = leaves.pop() {
			if leaf == H256::default()
				|| !Self::sync_one(
					client.clone(),
					Arc::clone(&indexer_backend),
					batch_size,
					current_batch,
					known_hashes,
					leaf,
					notified,
				)
				.await
			{
				break;
			}
			if let Ok(Some(header)) = blockchain_backend.header(BlockId::Hash(leaf)) {
				let parent_hash = header.parent_hash();
				leaves.push(*parent_hash);
			}
		}
	}

	async fn sync_one(
		client: Arc<Client>,
		indexer_backend: Arc<fc_db::sql::Backend<Block>>,
		batch_size: usize,
		current_batch: &mut Vec<Block::Hash>,
		known_hashes: &mut Vec<Block::Hash>,
		hash: Block::Hash,
		notified: bool,
	) -> bool {
		if !current_batch.contains(&hash) && !known_hashes.contains(&hash) {
			current_batch.push(hash);
			log::trace!(
				target: "frontier-sql",
				"⤵️  Queued for index {}",
				hash,
			);
			if notified || current_batch.len() == batch_size {
				log::debug!(
					target: "frontier-sql",
					"🛠️  Processing batch starting at {:?}",
					current_batch.first()
				);
				let _ = indexer_backend
					.insert_block_metadata(client.clone(), current_batch)
					.await
					.map_err(|e| {
						log::error!(
							target: "frontier-sql",
							"{}",
							e,
						);
					});
				indexer_backend.spawn_logs_task(client.clone(), batch_size).await; // Spawn actual logs task
				known_hashes.append(current_batch);
				current_batch.clear();
			}
			return true;
		}
		false
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
	use sqlx::Row;
	use std::{collections::BTreeMap, path::Path, sync::Arc};
	use substrate_test_runtime_client::{
		prelude::*, DefaultTestClientBuilderExt, TestClientBuilder, TestClientBuilderExt,
	};
	use tempfile::tempdir;

	fn storage_prefix_build(module: &[u8], storage: &[u8]) -> Vec<u8> {
		[twox_128(module), twox_128(storage)].concat().to_vec()
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
		let mut logs: Vec<fc_db::sql::Log> = vec![];
		for block_number in 1..11 {
			let mut builder = client.new_block(Default::default()).unwrap();
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
			logs.push(fc_db::sql::Log {
				block_number: block_number as i32,
				address: address_1.as_bytes().to_owned(),
				topic_1: topics_1_1.as_bytes().to_owned(),
				topic_2: topics_1_2.as_bytes().to_owned(),
				topic_3: H256::default().as_bytes().to_owned(),
				topic_4: H256::default().as_bytes().to_owned(),
				log_index: 0i32,
				transaction_index: 0i32,
				substrate_block_hash: block_hash.as_bytes().to_owned(),
			});
			logs.push(fc_db::sql::Log {
				block_number: block_number as i32,
				address: address_2.as_bytes().to_owned(),
				topic_1: topics_2_1.as_bytes().to_owned(),
				topic_2: topics_2_2.as_bytes().to_owned(),
				topic_3: topics_2_3.as_bytes().to_owned(),
				topic_4: topics_2_4.as_bytes().to_owned(),
				log_index: 0i32,
				transaction_index: 1i32,
				substrate_block_hash: block_hash.as_bytes().to_owned(),
			});
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
					block_number,
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					substrate_block_hash
				FROM logs ORDER BY block_number ASC, log_index ASC, transaction_index ASC",
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
			fc_db::sql::Log {
				block_number,
				address,
				topic_1,
				topic_2,
				topic_3,
				topic_4,
				log_index,
				transaction_index,
				substrate_block_hash,
			}
		})
		.collect::<Vec<fc_db::sql::Log>>();

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
		let mut logs: Vec<fc_db::sql::Log> = vec![];
		for block_number in 1..11 {
			let mut builder = client.new_block(Default::default()).unwrap();
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
			logs.push(fc_db::sql::Log {
				block_number: block_number as i32,
				address: address_1.as_bytes().to_owned(),
				topic_1: topics_1_1.as_bytes().to_owned(),
				topic_2: topics_1_2.as_bytes().to_owned(),
				topic_3: H256::default().as_bytes().to_owned(),
				topic_4: H256::default().as_bytes().to_owned(),
				log_index: 0i32,
				transaction_index: 0i32,
				substrate_block_hash: block_hash.as_bytes().to_owned(),
			});
			logs.push(fc_db::sql::Log {
				block_number: block_number as i32,
				address: address_2.as_bytes().to_owned(),
				topic_1: topics_2_1.as_bytes().to_owned(),
				topic_2: topics_2_2.as_bytes().to_owned(),
				topic_3: topics_2_3.as_bytes().to_owned(),
				topic_4: topics_2_4.as_bytes().to_owned(),
				log_index: 0i32,
				transaction_index: 1i32,
				substrate_block_hash: block_hash.as_bytes().to_owned(),
			});
		}

		// Some time for the notification stream to be consumed
		futures_timer::Delay::new(std::time::Duration::from_millis(500)).await;

		// Query db
		let db_logs = sqlx::query(
			"SELECT
					block_number,
					address,
					topic_1,
					topic_2,
					topic_3,
					topic_4,
					log_index,
					transaction_index,
					substrate_block_hash
				FROM logs ORDER BY block_number ASC, log_index ASC, transaction_index ASC",
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
			fc_db::sql::Log {
				block_number,
				address,
				topic_1,
				topic_2,
				topic_3,
				topic_4,
				log_index,
				transaction_index,
				substrate_block_hash,
			}
		})
		.collect::<Vec<fc_db::sql::Log>>();

		// Expect the db to contain 20 rows. 10 blocks, 2 logs each.
		// Db data is sorted ASC by block_number, log_index and transaction_index.
		// This is necessary because indexing is done from tip to genesis.
		// Expect the db resultset to be equal to the locally produced Log vector.
		assert_eq!(db_logs, logs);
	}
}
