/// Available Sealing methods.
#[cfg(feature = "manual-seal")]
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum Sealing {
	// Seal using rpc method.
	Manual,
	// Seal when transaction is executed.
	Instant,
}

#[cfg(feature = "manual-seal")]
impl Default for Sealing {
	fn default() -> Sealing {
		Sealing::Manual
	}
}

/// Avalailable Backend types.
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum BackendType {
	/// Either RocksDb or ParityDb as per inherited from the global backend settings.
	KeyValue,
	/// Sql database with custom log indexing.
	Sql,
}

impl Default for BackendType {
	fn default() -> BackendType {
		BackendType::KeyValue
	}
}

#[allow(missing_docs)]
#[derive(Debug, clap::Parser)]
pub struct RunCmd {
	#[allow(missing_docs)]
	#[command(flatten)]
	pub base: sc_cli::RunCmd,

	/// Choose sealing method.
	#[cfg(feature = "manual-seal")]
	#[arg(long, value_enum, ignore_case = true)]
	pub sealing: Sealing,

	#[arg(long)]
	pub enable_dev_signer: bool,

	/// Maximum number of logs in a query.
	#[arg(long, default_value = "10000")]
	pub max_past_logs: u32,

	/// Maximum fee history cache size.
	#[arg(long, default_value = "2048")]
	pub fee_history_limit: u64,

	/// The dynamic-fee pallet target gas price set by block author
	#[arg(long, default_value = "1")]
	pub target_gas_price: u64,

	/// Sets the backend type (KeyValue or Sql)
	#[clap(long, value_enum, ignore_case = true, default_value_t = BackendType::default())]
	pub frontier_backend_type: BackendType,

	/// Sets the SQL backend's query timeout in number of VM ops.
	#[clap(long, default_value = "10000000")]
	pub frontier_sql_backend_num_ops_timeout: u32,

	/// Sets the SQL backend's auxiliary thread limit.
	#[clap(long, default_value = "4")]
	pub frontier_sql_backend_thread_count: u32,

	/// Sets the SQL backend's query timeout in number of VM ops.
	/// Default value is 200MB.
	#[clap(long, default_value = "209715200")]
	pub frontier_sql_backend_cache_size: u64,
}

#[derive(Debug, clap::Parser)]
pub struct Cli {
	#[command(subcommand)]
	pub subcommand: Option<Subcommand>,

	#[command(flatten)]
	pub run: RunCmd,
}

#[derive(Debug, clap::Subcommand)]
pub enum Subcommand {
	/// Key management cli utilities
	#[command(subcommand)]
	Key(sc_cli::KeySubcommand),

	/// Build a chain specification.
	BuildSpec(sc_cli::BuildSpecCmd),

	/// Validate blocks.
	CheckBlock(sc_cli::CheckBlockCmd),

	/// Export blocks.
	ExportBlocks(sc_cli::ExportBlocksCmd),

	/// Export the state of a given block into a chain spec.
	ExportState(sc_cli::ExportStateCmd),

	/// Import blocks.
	ImportBlocks(sc_cli::ImportBlocksCmd),

	/// Remove the whole chain.
	PurgeChain(sc_cli::PurgeChainCmd),

	/// Revert the chain to a previous state.
	Revert(sc_cli::RevertCmd),

	/// Sub-commands concerned with benchmarking.
	#[cfg(feature = "runtime-benchmarks")]
	#[command(subcommand)]
	Benchmark(frame_benchmarking_cli::BenchmarkCmd),

	/// Sub-commands concerned with benchmarking.
	#[cfg(not(feature = "runtime-benchmarks"))]
	Benchmark,

	/// Db meta columns information.
	FrontierDb(fc_cli::FrontierDbCmd),
}
