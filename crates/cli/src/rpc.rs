//! The JSON-RPC server. JSON-RPC types appear only in this module, and the conversion from
//! [`RpcError`] is the only mapping from errors to JSON-RPC codes.

use std::fmt;
use std::io;
use std::net::SocketAddr;

use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, TxType};
use alloy_eips::{BlockId, BlockNumberOrTag, RpcBlockHash};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U64, U256};
use alloy_rpc_types_eth::{
    Block, BlockTransactions, FeeHistory as RpcFeeHistory, Header as RpcHeader, Log as RpcLog,
    Transaction as RpcTransaction, TransactionReceipt, TransactionRequest,
};
use chibi_core::VerifiedHeader;
use chibi_core::exec::{CallRequest, EstimateError, ExecutionResult, HaltReason};
use chibi_core::wire;
use chibi_node::{BlockSel, FeeHistory, Node, NodeError, Peers, SubmittedTx, TxReceipt};
use http::{HeaderValue, Method, header};
use jsonrpsee::server::middleware::http::{Authority, AuthorityError, HostFilterLayer, Port};
use jsonrpsee::server::{RegisterMethodError, RpcModule, Server, ServerConfig};
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use serde::{Deserialize, Serialize};
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

/// Code geth and reth use for a reverted call, with the revert data attached.
const REVERTED_CODE: i32 = 3;
/// Generic server error.
const SERVER_ERROR_CODE: i32 = -32000;
/// EIP-1474 "method not supported", used for data chibi does not keep.
const UNSUPPORTED_CODE: i32 = -32004;

const CLIENT_VERSION: &str = concat!("chibi/v", env!("CARGO_PKG_VERSION"));

/// Methods that need logs or history chibi does not keep.
const UNSUPPORTED_METHODS: [&str; 7] = [
    "eth_getLogs",
    "eth_newFilter",
    "eth_newBlockFilter",
    "eth_newPendingTransactionFilter",
    "eth_getFilterChanges",
    "eth_getFilterLogs",
    "eth_uninstallFilter",
];

/// A block as served: verified header fields and no transactions.
pub type RpcBlock = Block<RpcTransaction<wire::Transaction>>;

/// Where the server listens and whom browsers let read its responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcConfig {
    /// Listen address.
    pub addr: SocketAddr,
    /// Origins allowed to read responses cross-origin.
    pub cors_origins: Vec<OriginPattern>,
    /// `Host` headers the server answers.
    pub allowed_hosts: Vec<AllowedHost>,
}

/// An allowed browser origin. Written as a string; a trailing `*` makes it a prefix match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum OriginPattern {
    /// The origin must equal this string.
    Exact(String),
    /// The origin must start with this string.
    Prefix(String),
}

impl OriginPattern {
    /// Whether `origin` is admitted by this pattern.
    pub fn matches(&self, origin: &str) -> bool {
        match self {
            Self::Exact(exact) => origin == exact,
            Self::Prefix(prefix) => origin.starts_with(prefix.as_str()),
        }
    }
}

impl TryFrom<String> for OriginPattern {
    type Error = OriginPatternError;

    fn try_from(mut pattern: String) -> Result<Self, OriginPatternError> {
        if pattern.is_empty() {
            return Err(OriginPatternError::Empty);
        }
        match pattern.find('*') {
            None => Ok(Self::Exact(pattern)),
            Some(star) if star + 1 == pattern.len() => {
                pattern.truncate(star);
                Ok(Self::Prefix(pattern))
            }
            Some(_) => Err(OriginPatternError::InnerWildcard(pattern)),
        }
    }
}

impl From<OriginPattern> for String {
    fn from(pattern: OriginPattern) -> Self {
        match pattern {
            OriginPattern::Exact(exact) => exact,
            OriginPattern::Prefix(mut prefix) => {
                prefix.push('*');
                prefix
            }
        }
    }
}

/// A CORS origin pattern that cannot be parsed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OriginPatternError {
    /// The pattern is empty.
    #[error("empty CORS origin")]
    Empty,
    /// A `*` appears before the end.
    #[error("CORS origin {0:?} has a `*` before its end; only a trailing `*` is supported")]
    InnerWildcard(String),
}

/// Whether a request's `Origin` header is admitted by any pattern.
pub fn origin_allowed(patterns: &[OriginPattern], origin: &HeaderValue) -> bool {
    origin
        .to_str()
        .is_ok_and(|origin| patterns.iter().any(|pattern| pattern.matches(origin)))
}

/// A CORS layer that answers only admitted origins; others get no CORS headers.
pub fn cors_layer(patterns: Vec<OriginPattern>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            origin_allowed(&patterns, origin)
        }))
        .allow_methods([Method::POST])
        .allow_headers([header::CONTENT_TYPE])
}

/// A `Host` header the server answers, written `host` or `host:port`. Without a port it admits
/// the host both bare and with the listen port. Checking it defeats DNS rebinding, where a
/// website's own name resolves to loopback and its requests become same-origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AllowedHost {
    host: String,
    port: Option<u16>,
}

impl AllowedHost {
    /// The loopback names a local wallet connection uses.
    pub fn loopback() -> Vec<Self> {
        ["localhost", "127.0.0.1", "[::1]"]
            .into_iter()
            .map(|host| Self {
                host: host.to_owned(),
                port: None,
            })
            .collect()
    }

    /// Admits `host`, a name or bracketed IPv6 address, bare and with the listen port.
    pub fn bare(host: String) -> Self {
        Self { host, port: None }
    }
}

impl fmt::Display for AllowedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}:{port}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

impl TryFrom<String> for AllowedHost {
    type Error = AllowedHostError;

    fn try_from(text: String) -> Result<Self, AllowedHostError> {
        let Ok(authority) = Authority::try_from(text.as_str()) else {
            return Err(AllowedHostError(text));
        };
        let port = match authority.port {
            Port::Default => None,
            Port::Fixed(port) => Some(port),
            Port::Any => return Err(AllowedHostError(text)),
        };
        let allowed = Self {
            host: authority.host,
            port,
        };
        if allowed.to_string() == text {
            Ok(allowed)
        } else {
            Err(AllowedHostError(text))
        }
    }
}

impl From<AllowedHost> for String {
    fn from(allowed: AllowedHost) -> Self {
        allowed.to_string()
    }
}

/// An allowed host that is not `host` or `host:port`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("allowed host {0:?} must be a host name or address with an optional port")]
pub struct AllowedHostError(String);

/// A layer that refuses requests whose `Host` is not allowed, with 403.
pub fn host_filter(
    hosts: &[AllowedHost],
    listen_port: u16,
) -> Result<HostFilterLayer, AuthorityError> {
    HostFilterLayer::new(hosts.iter().flat_map(|allowed| {
        let bare = allowed.port.is_none().then(|| allowed.host.clone());
        let port = allowed.port.unwrap_or(listen_port);
        bare.into_iter().chain([format!("{}:{port}", allowed.host)])
    }))
}

/// Serves JSON-RPC over HTTP until the server stops.
pub async fn serve<P: Peers>(node: Node<P>, config: RpcConfig) -> Result<(), ServeError> {
    if !config.addr.ip().is_loopback() {
        warn!(addr = %config.addr, "JSON-RPC listens beyond loopback; anyone who can reach it can read tracked balances");
    }
    let chain_id = node.chain_id();
    let module = module(node)?;
    let hosts = host_filter(&config.allowed_hosts, config.addr.port())?;
    let server = Server::builder()
        .set_config(ServerConfig::builder().http_only().build())
        .set_http_middleware(
            ServiceBuilder::new()
                .layer(hosts)
                .layer(cors_layer(config.cors_origins)),
        )
        .build(config.addr)
        .await
        .map_err(|source| ServeError::Bind {
            addr: config.addr,
            source,
        })?;
    let addr = server.local_addr().map_err(|source| ServeError::Bind {
        addr: config.addr,
        source,
    })?;
    let handle = server.start(module);
    info!("JSON-RPC on http://{addr}, chain id {chain_id}");
    handle.stopped().await;
    Ok(())
}

/// Why the server could not run.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// A method was registered twice.
    #[error(transparent)]
    Register(#[from] RegisterMethodError),
    /// An allowed host could not be turned into a filter rule.
    #[error("invalid allowed host: {0}")]
    Hosts(#[from] AuthorityError),
    /// The listen address could not be bound.
    #[error("failed to listen on {addr}: {source}")]
    Bind {
        /// The configured address.
        addr: SocketAddr,
        /// The I/O failure.
        source: io::Error,
    },
}

/// Every served method, sharing one node handle.
pub fn module<P: Peers>(node: Node<P>) -> Result<RpcModule<Node<P>>, RegisterMethodError> {
    let mut module = RpcModule::new(node);

    module.register_method("web3_clientVersion", |_, _, _| {
        Ok::<_, ErrorObjectOwned>(CLIENT_VERSION)
    })?;
    module.register_method("net_version", |_, node, _| {
        Ok::<_, ErrorObjectOwned>(node.chain_id().to_string())
    })?;
    module.register_method("eth_chainId", |_, node, _| {
        Ok::<_, ErrorObjectOwned>(U64::from(node.chain_id()))
    })?;
    module.register_method("eth_blockNumber", |_, node, _| {
        Ok::<_, ErrorObjectOwned>(U64::from(node.head().number()))
    })?;
    module.register_method("eth_syncing", |_, _, _| Ok::<_, ErrorObjectOwned>(false))?;
    module.register_method("eth_accounts", |_, _, _| {
        Ok::<_, ErrorObjectOwned>(Vec::<Address>::new())
    })?;

    module.register_method("eth_getBlockByNumber", |params, node, _| {
        let tag: BlockNumberOrTag = params.sequence().next()?;
        let block = block_sel(tag).map_err(|unsupported| unsupported_error(node, unsupported));
        block
            .and_then(|block| block_by(node, block))
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_method("eth_getBlockByHash", |params, node, _| {
        let hash: B256 = params.sequence().next()?;
        block_by(node, BlockSel::Hash(hash)).map_err(ErrorObjectOwned::from)
    })?;

    module.register_async_method("eth_getBalance", |params, node, _| async move {
        let (address, block) = address_at(&params)?;
        balance(&node, address, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_getTransactionCount", |params, node, _| async move {
        let (address, block) = address_at(&params)?;
        transaction_count(&node, address, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_getCode", |params, node, _| async move {
        let (address, block) = address_at(&params)?;
        code(&node, address, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_getStorageAt", |params, node, _| async move {
        let (address, slot, block) = {
            let mut params = params.sequence();
            (
                params.next::<Address>()?,
                params.next::<U256>()?,
                params.optional_next::<BlockId>()?,
            )
        };
        storage(&node, address, slot, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_call", |params, node, _| async move {
        let (request, block) = request_at(&params)?;
        call(&node, request, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_estimateGas", |params, node, _| async move {
        let (request, block) = request_at(&params)?;
        estimate_gas(&node, request, block)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_gasPrice", |_, node, _| async move {
        node.gas_price()
            .await
            .map(U256::from)
            .map_err(|error| ErrorObjectOwned::from(RpcError::from(error)))
    })?;
    module.register_async_method("eth_maxPriorityFeePerGas", |_, node, _| async move {
        node.max_priority_fee()
            .await
            .map(U256::from)
            .map_err(|error| ErrorObjectOwned::from(RpcError::from(error)))
    })?;
    module.register_async_method("eth_feeHistory", |params, node, _| async move {
        let (blocks, newest, percentiles) = {
            let mut params = params.sequence();
            (
                params.next::<U64>()?,
                params.next::<BlockNumberOrTag>()?,
                params.optional_next::<Vec<f64>>()?,
            )
        };
        fee_history(&node, blocks.to(), newest, percentiles)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_async_method("eth_sendRawTransaction", |params, node, _| async move {
        let raw: Bytes = params.sequence().next()?;
        send_raw_transaction(&node, &raw)
            .await
            .map_err(ErrorObjectOwned::from)
    })?;
    module.register_method("eth_getTransactionReceipt", |params, node, _| {
        let hash: TxHash = params.sequence().next()?;
        transaction_receipt(node, hash).map_err(ErrorObjectOwned::from)
    })?;
    module.register_method("eth_getTransactionByHash", |params, node, _| {
        let hash: TxHash = params.sequence().next()?;
        transaction_by_hash(node, hash).map_err(ErrorObjectOwned::from)
    })?;

    for method in UNSUPPORTED_METHODS {
        module.register_method(method, move |_, node, _| {
            Err::<(), _>(ErrorObjectOwned::from(unsupported_error(
                node,
                Unsupported(method),
            )))
        })?;
    }

    Ok(module)
}

fn address_at(
    params: &jsonrpsee::types::Params<'_>,
) -> Result<(Address, Option<BlockId>), ErrorObjectOwned> {
    let mut params = params.sequence();
    Ok((params.next()?, params.optional_next()?))
}

fn request_at(
    params: &jsonrpsee::types::Params<'_>,
) -> Result<(TransactionRequest, Option<BlockId>), ErrorObjectOwned> {
    let mut params = params.sequence();
    Ok((params.next()?, params.optional_next()?))
}

async fn balance<P: Peers>(
    node: &Node<P>,
    address: Address,
    block: Option<BlockId>,
) -> Result<U256, RpcError> {
    let block = state_block(node, block)?;
    Ok(node
        .account(address, block)
        .await?
        .map_or(U256::ZERO, |account| account.balance))
}

async fn transaction_count<P: Peers>(
    node: &Node<P>,
    address: Address,
    block: Option<BlockId>,
) -> Result<U64, RpcError> {
    let block = state_block(node, block)?;
    Ok(U64::from(node.transaction_count(address, block).await?))
}

async fn code<P: Peers>(
    node: &Node<P>,
    address: Address,
    block: Option<BlockId>,
) -> Result<Bytes, RpcError> {
    let block = state_block(node, block)?;
    Ok(node.code(address, block).await?)
}

async fn storage<P: Peers>(
    node: &Node<P>,
    address: Address,
    slot: U256,
    block: Option<BlockId>,
) -> Result<B256, RpcError> {
    let block = state_block(node, block)?;
    Ok(B256::from(node.storage(address, slot, block).await?))
}

async fn call<P: Peers>(
    node: &Node<P>,
    request: TransactionRequest,
    block: Option<BlockId>,
) -> Result<Bytes, RpcError> {
    let request = call_request(request)?;
    let block = state_block(node, block)?;
    call_output(node.call(request, block).await?)
}

async fn estimate_gas<P: Peers>(
    node: &Node<P>,
    request: TransactionRequest,
    block: Option<BlockId>,
) -> Result<U64, RpcError> {
    let request = call_request(request)?;
    let block = state_block(node, block)?;
    Ok(U64::from(node.estimate_gas(request, block).await?))
}

async fn fee_history<P: Peers>(
    node: &Node<P>,
    blocks: u64,
    newest: BlockNumberOrTag,
    percentiles: Option<Vec<f64>>,
) -> Result<RpcFeeHistory, RpcError> {
    let percentiles = percentiles.unwrap_or_default();
    check_percentiles(&percentiles)?;
    let newest = block_sel(newest).map_err(|unsupported| unsupported_error(node, unsupported))?;
    let history = node.fee_history(blocks, newest, &percentiles).await?;
    Ok(render_fee_history(history))
}

async fn send_raw_transaction<P: Peers>(node: &Node<P>, raw: &[u8]) -> Result<TxHash, RpcError> {
    let raw = check_raw_transaction(raw)?;
    Ok(node.submit(raw).await?)
}

fn transaction_receipt<P: Peers>(
    node: &Node<P>,
    hash: TxHash,
) -> Result<Option<TransactionReceipt>, RpcError> {
    let Some(receipt) = node.receipt(hash)? else {
        return Ok(None);
    };
    let submitted = node
        .transaction(hash)?
        .ok_or(RpcError::MissingTransaction(hash))?;
    Ok(Some(render_receipt(
        &receipt,
        submitted.transaction.tx_type(),
    )))
}

fn transaction_by_hash<P: Peers>(
    node: &Node<P>,
    hash: TxHash,
) -> Result<Option<RpcTransaction<wire::Transaction>>, RpcError> {
    let Some(submitted) = node.transaction(hash)? else {
        return Ok(None);
    };
    let effective_gas_price = match submitted.included {
        Some(_) => node
            .receipt(hash)?
            .map(|receipt| receipt.receipt.effective_gas_price()),
        None => None,
    };
    Ok(Some(render_transaction(submitted, effective_gas_price)))
}

fn state_block<P: Peers>(node: &Node<P>, block: Option<BlockId>) -> Result<BlockSel, RpcError> {
    block
        .map_or(Ok(BlockSel::Latest), block_id_sel)
        .map_err(|unsupported| unsupported_error(node, unsupported))
}

fn block_by<P: Peers>(node: &Node<P>, block: BlockSel) -> Result<RpcBlock, RpcError> {
    Ok(render_block(&node.header(block)?))
}

fn unsupported_error<P: Peers>(node: &Node<P>, Unsupported(what): Unsupported) -> RpcError {
    RpcError::Node(NodeError::Unsupported {
        what,
        head: node.head().number(),
    })
}

/// A request that names something chibi never holds, before any node lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsupported(pub &'static str);

/// Maps a block tag or number to the node's block selector.
pub fn block_sel(tag: BlockNumberOrTag) -> Result<BlockSel, Unsupported> {
    match tag {
        BlockNumberOrTag::Latest => Ok(BlockSel::Latest),
        BlockNumberOrTag::Pending => Ok(BlockSel::Pending),
        BlockNumberOrTag::Safe => Ok(BlockSel::Safe),
        BlockNumberOrTag::Finalized => Ok(BlockSel::Finalized),
        BlockNumberOrTag::Number(number) => Ok(BlockSel::Number(number)),
        BlockNumberOrTag::Earliest => Err(Unsupported("the earliest block")),
    }
}

/// Maps an EIP-1898 block id to the node's block selector. Every header chibi holds is
/// canonical, so `requireCanonical` needs no check.
pub fn block_id_sel(id: BlockId) -> Result<BlockSel, Unsupported> {
    match id {
        BlockId::Hash(RpcBlockHash { block_hash, .. }) => Ok(BlockSel::Hash(block_hash)),
        BlockId::Number(tag) => block_sel(tag),
    }
}

/// Parses an `eth_call`/`eth_estimateGas` request. Fields the local EVM cannot honour are
/// refused instead of ignored, since ignoring them would change the result.
pub fn call_request(request: TransactionRequest) -> Result<CallRequest, ParamError> {
    let TransactionRequest {
        from,
        to,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        max_fee_per_blob_gas,
        gas,
        value,
        input,
        nonce: _,
        chain_id: _,
        access_list,
        transaction_type: _,
        blob_versioned_hashes,
        sidecar,
        authorization_list,
    } = request;
    if max_fee_per_blob_gas.is_some()
        || blob_versioned_hashes.is_some_and(|hashes| !hashes.is_empty())
        || sidecar.is_some()
    {
        return Err(ParamError::Blob);
    }
    if authorization_list.is_some_and(|list| !list.is_empty()) {
        return Err(ParamError::Authorization);
    }
    let input = input
        .try_into_unique_input()
        .map_err(|_| ParamError::ConflictingInput)?
        .unwrap_or_default();
    Ok(CallRequest {
        from,
        to: to.unwrap_or(TxKind::Create),
        value: value.unwrap_or_default(),
        input,
        gas,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        access_list: access_list.unwrap_or_default(),
    })
}

/// The `eth_call` result: output data, or the revert or halt as an error.
pub fn call_output(result: ExecutionResult<HaltReason>) -> Result<Bytes, RpcError> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data()),
        ExecutionResult::Revert { output, .. } => Err(RpcError::Reverted(output)),
        ExecutionResult::Halt { reason, .. } => Err(RpcError::Halted(reason)),
    }
}

/// Reward percentiles must lie in `0..=100` and never decrease.
pub fn check_percentiles(percentiles: &[f64]) -> Result<(), ParamError> {
    let in_range = percentiles.iter().all(|p| (0.0..=100.0).contains(p));
    let ascending = percentiles.windows(2).all(|pair| pair[0] <= pair[1]);
    if in_range && ascending {
        Ok(())
    } else {
        Err(ParamError::Percentiles)
    }
}

/// Refuses empty input and EIP-4844 transactions before the node decodes the rest.
pub fn check_raw_transaction(raw: &[u8]) -> Result<&[u8], ParamError> {
    match raw.first() {
        None => Err(ParamError::EmptyTransaction),
        Some(&ty) if ty == u8::from(TxType::Eip4844) => Err(ParamError::Blob),
        Some(_) => Ok(raw),
    }
}

/// A block built from a verified header alone: chibi serves no transaction lists.
pub fn render_block(header: &VerifiedHeader) -> RpcBlock {
    Block {
        header: RpcHeader {
            hash: header.hash(),
            inner: header.header().clone(),
            total_difficulty: None,
            size: None,
        },
        uncles: Vec::new(),
        transactions: BlockTransactions::Hashes(Vec::new()),
        withdrawals: None,
    }
}

/// A verified receipt in RPC form. `tx_type` comes from the transaction chibi submitted.
pub fn render_receipt(receipt: &TxReceipt, tx_type: TxType) -> TransactionReceipt {
    let verified = &receipt.receipt;
    let block = verified.block();
    let tx_hash = verified.tx_hash();
    let logs = (verified.first_log_index()..)
        .zip(verified.logs())
        .map(|(log_index, log)| RpcLog {
            inner: log.clone(),
            block_hash: Some(block.hash),
            block_number: Some(block.number),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(verified.index()),
            log_index: Some(log_index),
            removed: false,
        })
        .collect();
    let inner = ReceiptEnvelope::from_typed(
        tx_type,
        ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(verified.success()),
                cumulative_gas_used: verified.cumulative_gas_used(),
                logs,
            },
            logs_bloom: verified.logs_bloom(),
        },
    );
    TransactionReceipt {
        inner,
        transaction_hash: tx_hash,
        transaction_index: Some(verified.index()),
        block_hash: Some(block.hash),
        block_number: Some(block.number),
        gas_used: verified.gas_used(),
        effective_gas_price: verified.effective_gas_price(),
        blob_gas_used: None,
        blob_gas_price: None,
        from: receipt.from,
        to: receipt.to,
        contract_address: receipt.contract_address,
    }
}

/// A chibi-submitted transaction in RPC form, with its inclusion once located.
pub fn render_transaction(
    submitted: SubmittedTx,
    effective_gas_price: Option<u128>,
) -> RpcTransaction<wire::Transaction> {
    let (block_hash, block_number, transaction_index) = match submitted.included {
        Some((block, index)) => (Some(block.hash), Some(block.number), Some(index)),
        None => (None, None, None),
    };
    RpcTransaction {
        inner: Recovered::new_unchecked(submitted.transaction, submitted.from),
        block_hash,
        block_number,
        transaction_index,
        effective_gas_price,
        block_timestamp: None,
    }
}

fn render_fee_history(history: FeeHistory) -> RpcFeeHistory {
    let FeeHistory {
        oldest_block,
        base_fee_per_gas,
        gas_used_ratio,
        base_fee_per_blob_gas,
        blob_gas_used_ratio,
        reward,
    } = history;
    RpcFeeHistory {
        base_fee_per_gas,
        gas_used_ratio,
        base_fee_per_blob_gas,
        blob_gas_used_ratio,
        oldest_block,
        reward,
    }
}

/// Request parameters chibi refuses.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    /// `input` and `data` are both set and differ.
    #[error("input and data are both set and differ")]
    ConflictingInput,
    /// Blob transactions cannot be simulated or submitted.
    #[error("blob transactions are not supported")]
    Blob,
    /// The local EVM does not apply EIP-7702 authorizations in calls.
    #[error("EIP-7702 authorization lists are not supported in calls")]
    Authorization,
    /// Reward percentiles out of range or out of order.
    #[error("reward percentiles must be ascending and between 0 and 100")]
    Percentiles,
    /// `eth_sendRawTransaction` with no bytes.
    #[error("empty transaction")]
    EmptyTransaction,
}

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The node failed or refused.
    #[error(transparent)]
    Node(#[from] NodeError),
    /// `eth_call` reverted; carries the revert data.
    #[error("execution reverted")]
    Reverted(Bytes),
    /// `eth_call` halted.
    #[error("execution halted: {0:?}")]
    Halted(HaltReason),
    /// The parameters were refused.
    #[error(transparent)]
    Params(#[from] ParamError),
    /// The node has a receipt but not the transaction it belongs to.
    #[error("receipt for {0} has no recorded transaction")]
    MissingTransaction(TxHash),
}

impl From<RpcError> for ErrorObjectOwned {
    fn from(error: RpcError) -> Self {
        let code = match &error {
            RpcError::Reverted(output)
            | RpcError::Node(NodeError::Estimate(EstimateError::Reverted { output })) => {
                return Self::owned(REVERTED_CODE, "execution reverted", Some(output));
            }
            RpcError::Node(NodeError::Unsupported { .. }) => UNSUPPORTED_CODE,
            RpcError::Params(_) => INVALID_PARAMS_CODE,
            RpcError::Node(
                NodeError::Beacon(_)
                | NodeError::Devp2p(_)
                | NodeError::Disk(_)
                | NodeError::Net(_)
                | NodeError::Unprovable(_)
                | NodeError::State(_)
                | NodeError::Exec(_)
                | NodeError::Estimate(
                    EstimateError::Halted { .. }
                    | EstimateError::OutOfGas { .. }
                    | EstimateError::ExceedsAllowance { .. }
                    | EstimateError::Invalid(_),
                )
                | NodeError::TooManyMissRounds { .. }
                | NodeError::InvalidTransaction(_)
                | NodeError::Stopped
                | NodeError::NotReady,
            )
            | RpcError::Halted(_)
            | RpcError::MissingTransaction(_) => SERVER_ERROR_CODE,
        };
        Self::owned(code, error.to_string(), None::<()>)
    }
}

#[cfg(test)]
mod node_tests;

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use alloy_eips::BlockNumHash;
    use alloy_eips::eip2930::{AccessList, AccessListItem};
    use alloy_primitives::{address, bytes};
    use alloy_rpc_types_eth::TransactionInput;
    use chibi_core::exec::Output;
    use chibi_core::testing::{World, WorldAccount, eth, legacy_transfer};
    use jsonrpsee::server::HttpBody;
    use revm::context_interface::result::{OutOfGasError, SuccessReason};
    use tower::ServiceExt;

    use super::*;

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");

    #[test]
    fn block_tags_map_to_selectors() {
        assert_eq!(block_sel(BlockNumberOrTag::Latest), Ok(BlockSel::Latest));
        assert_eq!(block_sel(BlockNumberOrTag::Pending), Ok(BlockSel::Pending));
        assert_eq!(block_sel(BlockNumberOrTag::Safe), Ok(BlockSel::Safe));
        assert_eq!(
            block_sel(BlockNumberOrTag::Finalized),
            Ok(BlockSel::Finalized)
        );
        assert_eq!(
            block_sel(BlockNumberOrTag::Number(7)),
            Ok(BlockSel::Number(7))
        );
        assert_eq!(
            block_sel(BlockNumberOrTag::Earliest),
            Err(Unsupported("the earliest block"))
        );
    }

    #[test]
    fn block_ids_map_to_selectors() {
        let hash = B256::repeat_byte(9);
        assert_eq!(
            block_id_sel(BlockId::Hash(RpcBlockHash {
                block_hash: hash,
                require_canonical: Some(true),
            })),
            Ok(BlockSel::Hash(hash))
        );
        assert_eq!(
            block_id_sel(BlockId::Number(BlockNumberOrTag::Number(42))),
            Ok(BlockSel::Number(42))
        );
        assert_eq!(
            block_id_sel(BlockId::Number(BlockNumberOrTag::Earliest)),
            Err(Unsupported("the earliest block"))
        );
    }

    #[test]
    fn block_ids_parse_from_wallet_json() {
        let parse = |json: &str| block_id_sel(serde_json::from_str::<BlockId>(json).unwrap());
        assert_eq!(parse(r#""latest""#), Ok(BlockSel::Latest));
        assert_eq!(parse(r#""0x10""#), Ok(BlockSel::Number(16)));
        assert_eq!(
            parse(
                r#"{"blockHash":"0x0909090909090909090909090909090909090909090909090909090909090909"}"#
            ),
            Ok(BlockSel::Hash(B256::repeat_byte(9)))
        );
        assert_eq!(
            parse(r#""earliest""#),
            Err(Unsupported("the earliest block"))
        );
    }

    #[test]
    fn transaction_request_maps_every_field() {
        let access_list = AccessList(vec![AccessListItem {
            address: BOB,
            storage_keys: vec![B256::repeat_byte(1)],
        }]);
        let request = TransactionRequest {
            from: Some(ALICE),
            to: Some(TxKind::Call(BOB)),
            gas_price: Some(3),
            max_fee_per_gas: Some(5),
            max_priority_fee_per_gas: Some(1),
            gas: Some(50_000),
            value: Some(U256::from(10)),
            input: TransactionInput::new(bytes!("a9059cbb")),
            nonce: Some(4),
            chain_id: Some(11_155_111),
            access_list: Some(access_list.clone()),
            transaction_type: Some(2),
            ..TransactionRequest::default()
        };
        assert_eq!(
            call_request(request),
            Ok(CallRequest {
                from: Some(ALICE),
                to: TxKind::Call(BOB),
                value: U256::from(10),
                input: bytes!("a9059cbb"),
                gas: Some(50_000),
                gas_price: Some(3),
                max_fee_per_gas: Some(5),
                max_priority_fee_per_gas: Some(1),
                access_list,
            })
        );
    }

    #[test]
    fn wallet_json_request_takes_data_and_defaults() {
        let request: TransactionRequest = serde_json::from_str(
            r#"{"from":"0x00000000000000000000000000000000000a11ce","to":"0x0000000000000000000000000000000000000b0b","data":"0x70a08231"}"#,
        )
        .unwrap();
        assert_eq!(
            call_request(request),
            Ok(CallRequest {
                from: Some(ALICE),
                to: TxKind::Call(BOB),
                input: bytes!("70a08231"),
                ..CallRequest::default()
            })
        );
    }

    #[test]
    fn request_without_to_is_a_create() {
        let request = TransactionRequest {
            input: TransactionInput::new(bytes!("6000")),
            ..TransactionRequest::default()
        };
        assert_eq!(call_request(request).map(|r| r.to), Ok(TxKind::Create));
    }

    #[test]
    fn refuses_requests_it_cannot_simulate() {
        let conflicting = TransactionRequest {
            input: TransactionInput::both(bytes!("01")),
            ..TransactionRequest::default()
        };
        let mut differing = conflicting.clone();
        differing.input.data = Some(bytes!("02"));
        assert!(call_request(conflicting).is_ok());
        assert_eq!(call_request(differing), Err(ParamError::ConflictingInput));

        let blob = TransactionRequest {
            blob_versioned_hashes: Some(vec![B256::repeat_byte(1)]),
            ..TransactionRequest::default()
        };
        assert_eq!(call_request(blob), Err(ParamError::Blob));
        let blob_fee = TransactionRequest {
            max_fee_per_blob_gas: Some(1),
            ..TransactionRequest::default()
        };
        assert_eq!(call_request(blob_fee), Err(ParamError::Blob));
    }

    fn rpc_error(error: RpcError) -> (i32, String, Option<String>) {
        let object = ErrorObjectOwned::from(error);
        (
            object.code(),
            object.message().to_owned(),
            object.data().map(|data| data.get().to_owned()),
        )
    }

    #[test]
    fn reverts_are_code_3_with_revert_data() {
        let data = bytes!("08c379a0deadbeef");
        let expected = (
            3,
            "execution reverted".to_owned(),
            Some(r#""0x08c379a0deadbeef""#.to_owned()),
        );
        assert_eq!(rpc_error(RpcError::Reverted(data.clone())), expected);
        assert_eq!(
            rpc_error(RpcError::Node(NodeError::Estimate(
                EstimateError::Reverted { output: data }
            ))),
            expected
        );
    }

    #[test]
    fn call_results_keep_output_or_revert_data() {
        let gas = Default::default();
        assert_eq!(
            call_output(ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas,
                logs: Vec::new(),
                output: Output::Call(bytes!("2a")),
            })
            .unwrap(),
            bytes!("2a")
        );
        let reverted = call_output(ExecutionResult::Revert {
            gas,
            logs: Vec::new(),
            output: bytes!("beef"),
        })
        .unwrap_err();
        assert_eq!(
            rpc_error(reverted),
            (
                3,
                "execution reverted".to_owned(),
                Some(r#""0xbeef""#.to_owned())
            )
        );
        let halted = call_output(ExecutionResult::Halt {
            reason: HaltReason::OutOfGas(OutOfGasError::Basic),
            gas,
            logs: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(rpc_error(halted).0, -32000);
    }

    #[test]
    fn unsupported_is_32004_naming_the_head() {
        assert_eq!(
            rpc_error(RpcError::Node(NodeError::Unsupported {
                what: "eth_getLogs",
                head: 9_318_211,
            })),
            (
                -32004,
                "eth_getLogs is not supported by chibi (verified head is 9318211)".to_owned(),
                None
            )
        );
    }

    #[test]
    fn param_errors_are_invalid_params() {
        assert_eq!(
            rpc_error(RpcError::Params(ParamError::Blob)),
            (
                -32602,
                "blob transactions are not supported".to_owned(),
                None
            )
        );
    }

    #[test]
    fn other_node_errors_are_server_errors() {
        for error in [
            NodeError::NotReady,
            NodeError::Stopped,
            NodeError::TooManyMissRounds { rounds: 8 },
            NodeError::InvalidTransaction("wrong chain id"),
            NodeError::Estimate(EstimateError::OutOfGas { gas_limit: 21_000 }),
        ] {
            let message = error.to_string();
            assert_eq!(rpc_error(RpcError::Node(error)), (-32000, message, None));
        }
    }

    #[test]
    fn percentiles_must_ascend_within_range() {
        assert_eq!(check_percentiles(&[]), Ok(()));
        assert_eq!(check_percentiles(&[10.0, 50.0, 50.0, 100.0]), Ok(()));
        assert_eq!(
            check_percentiles(&[50.0, 10.0]),
            Err(ParamError::Percentiles)
        );
        assert_eq!(check_percentiles(&[101.0]), Err(ParamError::Percentiles));
        assert_eq!(check_percentiles(&[f64::NAN]), Err(ParamError::Percentiles));
    }

    #[test]
    fn raw_transactions_refuse_empty_and_blob() {
        assert_eq!(
            check_raw_transaction(&[]),
            Err(ParamError::EmptyTransaction)
        );
        assert_eq!(check_raw_transaction(&[0x03, 0xf8]), Err(ParamError::Blob));
        assert_eq!(check_raw_transaction(&[0x02, 0xf8]), Ok(&[0x02, 0xf8][..]));
        assert_eq!(check_raw_transaction(&[0xf8, 0x6c]), Ok(&[0xf8, 0x6c][..]));
    }

    fn patterns(text: &[&str]) -> Vec<OriginPattern> {
        text.iter()
            .map(|p| OriginPattern::try_from((*p).to_owned()).unwrap())
            .collect()
    }

    fn default_patterns() -> Vec<OriginPattern> {
        patterns(&["chrome-extension://*", "moz-extension://*"])
    }

    #[test]
    fn origin_predicate_admits_only_configured_origins() {
        let allowed = |patterns: &[OriginPattern], origin: &str| {
            origin_allowed(patterns, &HeaderValue::from_str(origin).unwrap())
        };
        let defaults = default_patterns();
        assert!(allowed(
            &defaults,
            "chrome-extension://nkbihfbeogaeaoehlefnkodbefgpgknn"
        ));
        assert!(allowed(
            &defaults,
            "moz-extension://5b7a8c1e-0000-4000-8000-000000000000"
        ));
        assert!(!allowed(&defaults, "https://evil.example"));
        assert!(!allowed(&defaults, "null"));
        assert!(!allowed(&defaults, "https://chrome-extension.example"));

        let exact = patterns(&["https://app.example"]);
        assert!(allowed(&exact, "https://app.example"));
        assert!(!allowed(&exact, "https://app.example.evil.example"));
        assert!(!allowed(&exact, "http://app.example"));
    }

    #[test]
    fn origin_patterns_round_trip_and_refuse_inner_wildcards() {
        for text in ["chrome-extension://*", "https://app.example", "*"] {
            let pattern = OriginPattern::try_from(text.to_owned()).unwrap();
            assert_eq!(String::from(pattern), text);
        }
        assert_eq!(
            OriginPattern::try_from("https://*.example".to_owned()),
            Err(OriginPatternError::InnerWildcard(
                "https://*.example".to_owned()
            ))
        );
        assert_eq!(
            OriginPattern::try_from(String::new()),
            Err(OriginPatternError::Empty)
        );
    }

    async fn cors_response(method: Method, origin: &str) -> http::HeaderMap {
        let service = ServiceBuilder::new()
            .layer(cors_layer(default_patterns()))
            .service_fn(|_: http::Request<String>| async {
                Ok::<_, Infallible>(http::Response::new(String::new()))
            });
        let request = http::Request::builder()
            .method(method)
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(String::new())
            .unwrap();
        service.oneshot(request).await.unwrap().headers().clone()
    }

    #[tokio::test]
    async fn cors_layer_answers_extensions_and_ignores_websites() {
        let extension = "chrome-extension://nkbihfbeogaeaoehlefnkodbefgpgknn";
        for method in [Method::POST, Method::OPTIONS] {
            let headers = cors_response(method.clone(), extension).await;
            assert_eq!(
                headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
                Some(&HeaderValue::from_static(extension)),
                "{method}"
            );

            let headers = cors_response(method.clone(), "https://evil.example").await;
            assert_eq!(
                headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
                None,
                "{method}"
            );
        }

        let headers = cors_response(Method::POST, "https://evil.example").await;
        assert!(
            headers
                .keys()
                .all(|name| !name.as_str().starts_with("access-control-")),
            "{headers:?}"
        );
    }

    async fn host_status(hosts: &[AllowedHost], host: &str) -> http::StatusCode {
        let service = ServiceBuilder::new()
            .layer(host_filter(hosts, 8545).unwrap())
            .service_fn(|_: http::Request<HttpBody>| async {
                Ok::<_, Infallible>(http::Response::new(HttpBody::empty()))
            });
        let request = http::Request::builder()
            .method(Method::POST)
            .header(header::HOST, host)
            .body(HttpBody::empty())
            .unwrap();
        service.oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn host_filter_admits_loopback_names_and_refuses_rebound_hosts() {
        let loopback = AllowedHost::loopback();
        for host in [
            "127.0.0.1:8545",
            "127.0.0.1",
            "localhost:8545",
            "localhost",
            "[::1]:8545",
            "[::1]",
        ] {
            assert_eq!(
                host_status(&loopback, host).await,
                http::StatusCode::OK,
                "{host}"
            );
        }
        for host in ["evil.example", "evil.example:8545", "127.0.0.1:9999"] {
            assert_eq!(
                host_status(&loopback, host).await,
                http::StatusCode::FORBIDDEN,
                "{host}"
            );
        }

        let pinned = [AllowedHost::try_from("chibi.local:9000".to_owned()).unwrap()];
        assert_eq!(
            host_status(&pinned, "chibi.local:9000").await,
            http::StatusCode::OK
        );
        assert_eq!(
            host_status(&pinned, "chibi.local:8545").await,
            http::StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn allowed_hosts_round_trip_and_refuse_urls() {
        for text in ["localhost", "127.0.0.1:8545", "[::1]", "chibi.local:9000"] {
            let allowed = AllowedHost::try_from(text.to_owned()).unwrap();
            assert_eq!(String::from(allowed), text);
        }
        for text in ["http://localhost", "localhost/rpc", "localhost:*", ""] {
            assert_eq!(
                AllowedHost::try_from(text.to_owned()),
                Err(AllowedHostError(text.to_owned())),
                "{text}"
            );
        }
    }

    #[test]
    fn rendered_block_carries_the_verified_header() {
        let world = World::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
        let header = world.verified(world.head());
        let json = serde_json::to_value(render_block(&header)).unwrap();

        assert_eq!(json["hash"], serde_json::json!(header.hash()));
        assert_eq!(
            json["number"],
            serde_json::json!(U64::from(header.number()))
        );
        assert_eq!(
            json["baseFeePerGas"],
            serde_json::json!(U64::from(header.header().base_fee_per_gas.unwrap()))
        );
        assert_eq!(json["transactions"], serde_json::json!([]));
        assert_eq!(json["uncles"], serde_json::json!([]));

        let parsed: RpcBlock = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.header.inner.hash_slow(), header.hash());
    }

    #[test]
    fn rendered_transaction_shows_inclusion_once_located() {
        let transaction = legacy_transfer(0, BOB, eth(1));
        let hash = *transaction.tx_hash();
        let pending = render_transaction(
            SubmittedTx {
                transaction: transaction.clone(),
                from: ALICE,
                included: None,
            },
            None,
        );
        let json = serde_json::to_value(&pending).unwrap();
        assert_eq!(json["hash"], serde_json::json!(hash));
        assert_eq!(json["from"], serde_json::json!(ALICE));
        assert_eq!(json["blockHash"], serde_json::Value::Null);

        let block = BlockNumHash::new(24_000_001, B256::repeat_byte(4));
        let included = render_transaction(
            SubmittedTx {
                transaction,
                from: ALICE,
                included: Some((block, 3)),
            },
            Some(7),
        );
        assert_eq!(
            (
                included.block_hash,
                included.block_number,
                included.transaction_index,
                included.effective_gas_price,
            ),
            (Some(block.hash), Some(block.number), Some(3), Some(7))
        );
    }
}
