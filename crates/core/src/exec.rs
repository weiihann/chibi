//! Local EVM over a pinned [`Store`]. Nothing here fetches.
//!
//! A run that reads keys the store lacks reports them as [`Misses`] instead of a result. The
//! caller proves them at the same anchor, inserts them into its copy of the store, and runs
//! again, so round trips scale with dependency depth rather than key count.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use alloy_eips::eip2930::AccessList;
use alloy_evm::{EthEvmFactory, Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use reth_chainspec::{ChainSpec, EthChainSpec};
use revm::context::TxEnv;
use revm::context_interface::Cfg;
use revm::context_interface::result::{
    EVMError, ExecutionResult, HaltReason, InvalidHeader, InvalidTransaction, ResultAndState,
};
use revm::state::{AccountInfo, Bytecode};

use crate::state::{Key, Miss, Store};
use crate::verify::VerifiedHeader;

/// Intrinsic gas of a plain transfer.
const MIN_TRANSACTION_GAS: u64 = 21_000;

/// Stipend a value-bearing call forwards, part of reth's optimistic estimate.
const CALL_STIPEND_GAS: u64 = 2_300;

/// Bisection stops once the remaining range is below this fraction of the upper bound.
const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

/// Chain rules for building an EVM environment from a verified header.
#[derive(Clone, Debug)]
pub struct ChainEnv {
    spec: Arc<ChainSpec>,
}

impl ChainEnv {
    /// Wraps the network's chain spec, the same value that drives the devp2p fork id.
    pub const fn new(spec: Arc<ChainSpec>) -> Self {
        Self { spec }
    }

    /// The chain spec.
    pub const fn spec(&self) -> &Arc<ChainSpec> {
        &self.spec
    }

    /// The block environment of `header` with the relaxations `eth_call` applies: no
    /// EIP-3607 sender check, no base fee check, and no fee charge.
    pub fn call_env(&self, header: &VerifiedHeader) -> EvmEnv {
        let h = header.header();
        let mut env = EvmEnv::for_eth_block(
            h,
            &*self.spec,
            self.spec.chain_id(),
            self.spec.blob_params_at_timestamp(h.timestamp),
        );
        env.cfg_env.disable_eip3607 = true;
        env.cfg_env.disable_base_fee = true;
        env.cfg_env.disable_fee_charge = true;
        env
    }
}

/// An `eth_call` or `eth_estimateGas` request, parsed at the RPC boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallRequest {
    /// Sender; the zero address when absent.
    pub from: Option<Address>,
    /// Call target or contract creation.
    pub to: TxKind,
    /// Wei sent with the call.
    pub value: U256,
    /// Calldata or init code.
    pub input: Bytes,
    /// Gas limit; the block gas limit when absent.
    pub gas: Option<u64>,
    /// Legacy gas price.
    pub gas_price: Option<u128>,
    /// EIP-1559 fee cap. Takes precedence over `gas_price`.
    pub max_fee_per_gas: Option<u128>,
    /// EIP-1559 tip cap.
    pub max_priority_fee_per_gas: Option<u128>,
    /// Sender nonce; the sender's current nonce when absent.
    pub nonce: Option<u64>,
    /// EIP-2930 access list.
    pub access_list: AccessList,
}

impl CallRequest {
    fn effective_gas_price(&self) -> u128 {
        self.max_fee_per_gas.or(self.gas_price).unwrap_or_default()
    }
}

/// At least one miss, deduplicated in discovery order. Empty cannot be represented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Misses {
    first: Miss,
    rest: Vec<Miss>,
}

impl Misses {
    fn from_vec(mut misses: Vec<Miss>) -> Option<Self> {
        if misses.is_empty() {
            return None;
        }
        let first = misses.remove(0);
        Some(Self {
            first,
            rest: misses,
        })
    }

    /// The misses in discovery order.
    pub fn iter(&self) -> impl Iterator<Item = &Miss> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// Number of distinct misses, at least one.
    pub const fn len(&self) -> usize {
        self.rest.len() + 1
    }

    /// Always `false`; present for API symmetry with collections.
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl IntoIterator for Misses {
    type Item = Miss;
    type IntoIter = std::iter::Chain<std::iter::Once<Miss>, std::vec::IntoIter<Miss>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.first).chain(self.rest)
    }
}

/// The EVM refused the transaction before or outside execution.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// Transaction validation failed (nonce, funds, intrinsic gas, gas cap).
    #[error("invalid transaction: {0}")]
    Transaction(InvalidTransaction),
    /// The block environment is inconsistent.
    #[error("invalid block environment: {0}")]
    Header(InvalidHeader),
    /// Any other EVM failure, such as a fatal precompile error.
    #[error("evm failure: {0}")]
    Custom(String),
}

impl From<EVMError<Infallible>> for ExecError {
    fn from(error: EVMError<Infallible>) -> Self {
        match error {
            EVMError::Transaction(e) => Self::Transaction(e),
            EVMError::Header(e) => Self::Header(e),
            EVMError::Database(never) => match never {},
            EVMError::Custom(message) => Self::Custom(message),
            EVMError::CustomAny(any) => Self::Custom(any.to_string()),
        }
    }
}

/// A run that read only held keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    /// The execution result, or why the EVM refused the transaction.
    pub result: Result<ExecutionResult<HaltReason>, ExecError>,
    /// Every account and slot the run read.
    pub read: HashSet<Key>,
}

/// Why gas estimation produced no estimate.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EstimateError {
    /// The call reverts at the highest gas limit.
    #[error("execution reverted")]
    Reverted {
        /// Revert data.
        output: Bytes,
    },
    /// The call halts at the highest gas limit.
    #[error("execution halted: {reason:?}")]
    Halted {
        /// Halt reason.
        reason: HaltReason,
        /// Gas limit of the failing run.
        gas_limit: u64,
    },
    /// The call fails at the requested limit but succeeds at the block maximum.
    #[error("out of gas at {gas_limit}")]
    OutOfGas {
        /// The requested gas limit.
        gas_limit: u64,
    },
    /// The sender cannot pay for the gas the call needs.
    #[error("gas required exceeds allowance {gas_limit}")]
    ExceedsAllowance {
        /// The capped gas limit.
        gas_limit: u64,
    },
    /// The sender's balance is below the value sent.
    #[error("insufficient funds for value transfer")]
    InsufficientFunds,
    /// The EVM refused the transaction.
    #[error(transparent)]
    Invalid(#[from] ExecError),
}

/// Runs `req` against `store` at its anchor without committing anything.
///
/// Any miss withholds the result, even an EVM refusal, because the missing value read as
/// empty may have caused it.
pub fn execute(store: &Store, env: &ChainEnv, req: &CallRequest) -> Result<Executed, Misses> {
    let mut evm_env = env.call_env(store.anchor());
    evm_env.cfg_env.disable_block_gas_limit = true;
    if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    }
    let mut db = Db::new(store);
    let mut tx = tx_env(&mut db, &evm_env, req);
    if tx.gas_price == 0 {
        evm_env.block_env.basefee = 0;
    }
    if req.gas.is_none()
        && tx.gas_price > 0
        && let Ok(allowance) = caller_gas_allowance(&mut db, &tx)
    {
        tx.gas_limit = tx.gas_limit.min(allowance);
    }
    let mut evm = EthEvmFactory::default().create_evm(&mut db, evm_env);
    let result = evm
        .transact(tx)
        .map(|res| res.result)
        .map_err(ExecError::from);
    drop(evm);
    db.finish().map(|read| Executed { result, read })
}

/// Reth's `eth_estimateGas` bisection as a pure function. A miss in any probe aborts the
/// estimate with every miss found so far.
pub fn estimate_gas(
    store: &Store,
    env: &ChainEnv,
    req: &CallRequest,
) -> Result<Result<u64, EstimateError>, Misses> {
    let evm_env = env.call_env(store.anchor());
    let mut db = Db::new(store);
    let outcome = estimate_with(&mut db, evm_env, req);
    db.finish().map(|_| outcome)
}

fn estimate_with(
    db: &mut Db<'_>,
    evm_env: EvmEnv,
    req: &CallRequest,
) -> Result<u64, EstimateError> {
    let block_gas_limit = evm_env.block_env.gas_limit;
    let max_gas_limit = if evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
        block_gas_limit
    } else {
        evm_env.cfg_env.tx_gas_limit_cap().min(block_gas_limit)
    };
    let mut highest = req.gas.unwrap_or(max_gas_limit).min(max_gas_limit);
    let request_priced =
        req.gas.is_some() || req.gas_price.is_some() || req.max_fee_per_gas.is_some();

    let mut tx = tx_env(
        db,
        &evm_env,
        &CallRequest {
            nonce: None,
            ..req.clone()
        },
    );
    let is_basic_transfer = match (tx.data.is_empty(), tx.kind) {
        (true, TxKind::Call(to)) => db
            .basic_info(to)
            .is_none_or(|info| info.is_empty_code_hash()),
        (true, TxKind::Create) | (false, _) => false,
    };
    if tx.gas_price > 0 {
        highest = highest.min(caller_gas_allowance(db, &tx)?);
    }
    tx.gas_limit = tx.gas_limit.min(highest);

    let mut evm = EthEvmFactory::default().create_evm(&mut *db, evm_env);
    if is_basic_transfer {
        let probe = TxEnv {
            gas_limit: MIN_TRANSACTION_GAS.min(max_gas_limit),
            ..tx.clone()
        };
        if let Ok(res) = evm.transact(probe)
            && res.result.is_success()
        {
            return Ok(res.result.tx_gas_used());
        }
    }

    let res = match evm.transact(tx.clone()) {
        Err(EVMError::Transaction(e)) if is_gas_too_high(&e) && request_priced => {
            return out_of_gas(&mut evm, tx, max_gas_limit);
        }
        Err(EVMError::Transaction(e)) if is_gas_too_low(&e) => {
            return Err(EstimateError::ExceedsAllowance {
                gas_limit: tx.gas_limit,
            });
        }
        other => other.map_err(ExecError::from)?,
    };
    let refund = match &res.result {
        ExecutionResult::Success { gas, .. } => gas.final_refunded(),
        ExecutionResult::Halt { reason, .. } => {
            return Err(EstimateError::Halted {
                reason: reason.clone(),
                gas_limit: tx.gas_limit,
            });
        }
        ExecutionResult::Revert { output, .. } => {
            return if request_priced {
                out_of_gas(&mut evm, tx, max_gas_limit)
            } else {
                Err(EstimateError::Reverted {
                    output: output.clone(),
                })
            };
        }
    };

    highest = tx.gas_limit;
    let mut gas_used = res.result.tx_gas_used();
    let mut lowest = gas_used.saturating_sub(1);

    let optimistic = (gas_used + refund + CALL_STIPEND_GAS) * 64 / 63;
    if optimistic < highest {
        let res = evm
            .transact(TxEnv {
                gas_limit: optimistic,
                ..tx.clone()
            })
            .map_err(ExecError::from)?;
        gas_used = res.result.tx_gas_used();
        narrow(&res, optimistic, &mut highest, &mut lowest);
    }

    let mut mid = gas_used.saturating_mul(3).min(midpoint(highest, lowest));
    while lowest + 1 < highest {
        if ((highest - lowest) as f64 / highest as f64) < ESTIMATE_GAS_ERROR_RATIO {
            break;
        }
        match evm.transact(TxEnv {
            gas_limit: mid,
            ..tx.clone()
        }) {
            Err(EVMError::Transaction(e)) if is_gas_too_high(&e) => highest = mid,
            Err(EVMError::Transaction(e)) if is_gas_too_low(&e) => lowest = mid,
            other => {
                let res = other.map_err(ExecError::from)?;
                narrow(&res, mid, &mut highest, &mut lowest);
            }
        }
        mid = midpoint(highest, lowest);
    }
    Ok(highest)
}

fn midpoint(highest: u64, lowest: u64) -> u64 {
    // Both fit in u64, so their mean does too.
    ((u128::from(highest) + u128::from(lowest)) / 2) as u64
}

fn narrow(res: &ResultAndState<HaltReason>, limit: u64, highest: &mut u64, lowest: &mut u64) {
    match res.result {
        ExecutionResult::Success { .. } => *highest = limit,
        ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => *lowest = limit,
    }
}

fn out_of_gas<E>(evm: &mut E, tx: TxEnv, max_gas_limit: u64) -> Result<u64, EstimateError>
where
    E: Evm<Tx = TxEnv, Error = EVMError<Infallible>, HaltReason = HaltReason>,
{
    let requested = tx.gas_limit;
    let res = evm
        .transact(TxEnv {
            gas_limit: max_gas_limit,
            ..tx
        })
        .map_err(ExecError::from)?;
    Err(match res.result {
        ExecutionResult::Success { .. } => EstimateError::OutOfGas {
            gas_limit: requested,
        },
        ExecutionResult::Revert { output, .. } => EstimateError::Reverted { output },
        ExecutionResult::Halt { reason, .. } => EstimateError::Halted {
            reason,
            gas_limit: requested,
        },
    })
}

const fn is_gas_too_high(error: &InvalidTransaction) -> bool {
    matches!(
        error,
        InvalidTransaction::CallerGasLimitMoreThanBlock
            | InvalidTransaction::TxGasLimitGreaterThanCap { .. }
    )
}

const fn is_gas_too_low(error: &InvalidTransaction) -> bool {
    matches!(
        error,
        InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
            | InvalidTransaction::GasFloorMoreThanGasLimit { .. }
    )
}

/// `(balance - value) / gas_price`, after alloy-evm's `caller_gas_allowance`.
fn caller_gas_allowance(db: &mut Db<'_>, tx: &TxEnv) -> Result<u64, EstimateError> {
    let balance = db
        .basic_info(tx.caller)
        .map(|info| info.balance)
        .unwrap_or_default();
    let spendable = balance
        .checked_sub(tx.value)
        .ok_or(EstimateError::InsufficientFunds)?;
    Ok(spendable
        .checked_div(U256::from(tx.gas_price))
        .unwrap_or_default()
        .saturating_to())
}

fn tx_env(db: &mut Db<'_>, evm_env: &EvmEnv, req: &CallRequest) -> TxEnv {
    let caller = req.from.unwrap_or_default();
    let nonce = req.nonce.unwrap_or_else(|| {
        db.basic_info(caller)
            .map(|info| info.nonce)
            .unwrap_or_default()
    });
    let (tx_type, gas_priority_fee) = match req.max_fee_per_gas {
        Some(_) => (2, Some(req.max_priority_fee_per_gas.unwrap_or_default())),
        None if req.access_list.is_empty() => (0, None),
        None => (1, None),
    };
    TxEnv {
        tx_type,
        caller,
        gas_limit: req.gas.unwrap_or(evm_env.block_env.gas_limit),
        gas_price: req.effective_gas_price(),
        kind: req.to,
        value: req.value,
        data: req.input.clone(),
        nonce,
        chain_id: Some(evm_env.cfg_env.chain_id),
        access_list: req.access_list.clone(),
        gas_priority_fee,
        ..TxEnv::default()
    }
}

/// revm database over a store snapshot.
///
/// A miss is recorded and reads as empty for the rest of this run only. The run's result is
/// withheld whenever a miss was recorded, so an invented value never leaves this module.
#[derive(Debug)]
struct Db<'a> {
    store: &'a Store,
    misses: Vec<Miss>,
    read: HashSet<Key>,
}

impl<'a> Db<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            misses: Vec::new(),
            read: HashSet::new(),
        }
    }

    fn miss(&mut self, miss: Miss) {
        if !self.misses.contains(&miss) {
            self.misses.push(miss);
        }
    }

    fn finish(self) -> Result<HashSet<Key>, Misses> {
        match Misses::from_vec(self.misses) {
            Some(misses) => Err(misses),
            None => Ok(self.read),
        }
    }

    fn basic_info(&mut self, address: Address) -> Option<AccountInfo> {
        match self.store.account(address) {
            Ok(account) => {
                self.read.insert(Key::Account(address));
                account.map(|account| AccountInfo {
                    balance: account.balance,
                    nonce: account.nonce,
                    code_hash: account.code_hash,
                    code: self.store.code(account.code_hash).ok(),
                    ..AccountInfo::default()
                })
            }
            Err(miss) => {
                self.miss(miss);
                None
            }
        }
    }
}

impl revm::Database for Db<'_> {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
        Ok(self.basic_info(address))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Infallible> {
        Ok(self.store.code(code_hash).unwrap_or_else(|miss| {
            self.miss(miss);
            Bytecode::new()
        }))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Infallible> {
        match self.store.slot(address, index) {
            Ok(value) => {
                self.read.insert(Key::Slot(address, index));
                Ok(value)
            }
            Err(Miss::Account(_)) => {
                // One storage proof carries its account proof, so ask for the slot directly.
                self.miss(Miss::Storage(address, index));
                Ok(U256::ZERO)
            }
            Err(miss) => {
                self.miss(miss);
                Ok(U256::ZERO)
            }
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Infallible> {
        Ok(self.store.block_hash(number).unwrap_or_else(|miss| {
            self.miss(miss);
            B256::ZERO
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        EIP1967_IMPLEMENTATION_SLOT, World, WorldAccount, balance_of_calldata,
        delegating_proxy_code, eth, mapping_reader_code, mapping_slot,
    };
    use alloy_primitives::{address, keccak256};

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
    const PROXY: Address = address!("0x0000000000000000000000000000000000c0ffee");
    const IMPL: Address = address!("0x00000000000000000000000000000000000001f1");

    fn world() -> World {
        let proxy = WorldAccount::contract(delegating_proxy_code())
            .with_slot(
                EIP1967_IMPLEMENTATION_SLOT.into(),
                U256::from_be_slice(IMPL.as_slice()),
            )
            .with_slot(mapping_slot(ALICE), U256::from(1234));
        World::genesis([
            (ALICE, WorldAccount::eoa(eth(5))),
            (BOB, WorldAccount::eoa(eth(1))),
            (PROXY, proxy),
            (IMPL, WorldAccount::contract(mapping_reader_code())),
        ])
    }

    fn balance_of() -> CallRequest {
        CallRequest {
            from: Some(ALICE),
            to: TxKind::Call(PROXY),
            input: balance_of_calldata(ALICE),
            ..CallRequest::default()
        }
    }

    /// Runs `step` over `store`, proving each round's misses from `world`, until it answers.
    fn discover<T>(
        world: &World,
        store: &mut Store,
        step: impl Fn(&Store) -> Result<T, Misses>,
    ) -> (T, usize) {
        let at = store.anchor().clone();
        for round in 1..=16 {
            match step(store) {
                Ok(answer) => return (answer, round),
                Err(misses) => {
                    for miss in misses {
                        store.insert(&world.prove_miss(&at, miss).unwrap()).unwrap();
                    }
                }
            }
        }
        panic!("discovery did not converge");
    }

    #[test]
    fn discovery_converges_to_full_state_result() {
        let world = world();
        let env = ChainEnv::new(world.spec());
        let head = world.verified(world.head());

        let mut discovered = Store::new(head.clone());
        let (found, rounds) = discover(&world, &mut discovered, |store| {
            execute(store, &env, &balance_of())
        });
        assert!(
            rounds > 2,
            "the proxy chain needs several rounds, took {rounds}"
        );

        let mut full = Store::new(head.clone());
        for key in world.keys() {
            full.insert(&world.prove(&head, key)).unwrap();
        }
        for code in [delegating_proxy_code(), mapping_reader_code()] {
            full.insert(&world.prove_code(keccak256(&code))).unwrap();
        }
        let beneficiary = head.header().beneficiary;
        full.insert(&world.prove(&head, Key::Account(beneficiary)))
            .unwrap();
        let complete = execute(&full, &env, &balance_of()).unwrap();

        assert_eq!(found, complete);
        let Ok(ExecutionResult::Success { output, .. }) = &found.result else {
            panic!("balanceOf failed: {:?}", found.result)
        };
        assert_eq!(
            output.data().as_ref(),
            B256::from(U256::from(1234)).as_slice()
        );
        for key in [
            Key::Account(ALICE),
            Key::Account(PROXY),
            Key::Account(IMPL),
            Key::Slot(PROXY, EIP1967_IMPLEMENTATION_SLOT.into()),
            Key::Slot(PROXY, mapping_slot(ALICE)),
        ] {
            assert!(found.read.contains(&key), "{key:?} not in {:?}", found.read);
        }
    }

    #[test]
    fn first_run_over_empty_store_reports_misses() {
        let world = world();
        let env = ChainEnv::new(world.spec());
        let store = Store::new(world.verified(world.head()));
        let misses = execute(&store, &env, &balance_of()).unwrap_err();
        assert!(misses.iter().any(|miss| *miss == Miss::Account(ALICE)));
        assert!(misses.iter().any(|miss| *miss == Miss::Account(PROXY)));
    }

    #[test]
    fn plain_transfer_estimates_21000() {
        let world = world();
        let env = ChainEnv::new(world.spec());
        let mut store = Store::new(world.verified(world.head()));
        let transfer = CallRequest {
            from: Some(ALICE),
            to: TxKind::Call(BOB),
            value: eth(1),
            ..CallRequest::default()
        };
        let (estimate, _) = discover(&world, &mut store, |store| {
            estimate_gas(store, &env, &transfer)
        });
        assert_eq!(estimate, Ok(MIN_TRANSACTION_GAS));
    }

    #[test]
    fn contract_call_estimate_is_sufficient_and_within_reth_bounds() {
        let world = world();
        let env = ChainEnv::new(world.spec());
        let mut store = Store::new(world.verified(world.head()));
        let (estimate, _) = discover(&world, &mut store, |store| {
            estimate_gas(store, &env, &balance_of())
        });
        let estimate = estimate.unwrap();
        let run = |gas| {
            execute(
                &store,
                &env,
                &CallRequest {
                    gas,
                    ..balance_of()
                },
            )
            .unwrap()
            .result
            .unwrap()
        };
        let unlimited = run(None);
        let used = unlimited.tx_gas_used();
        assert_eq!(run(Some(estimate)).output(), unlimited.output());
        assert!(
            (used..=(used + CALL_STIPEND_GAS) * 64 / 63).contains(&estimate),
            "estimate {estimate} outside reth's bounds for {used} gas used"
        );
    }
}
