use crate::error::ProcessBlockError;
use channel::{self, select, Receiver, Sender};
use ckb_chain_spec::consensus::Consensus;
use ckb_core::block::Block;
use ckb_core::cell::CellProvider;
use ckb_core::extras::BlockExt;
use ckb_core::header::BlockNumber;
use ckb_core::service::{Request, DEFAULT_CHANNEL_SIZE};
use ckb_core::transaction::OutPoint;
use ckb_db::batch::Batch;
use ckb_notify::{ForkBlocks, NotifyController, NotifyService};
use ckb_shared::error::SharedError;
use ckb_shared::index::ChainIndex;
use ckb_shared::shared::{ChainProvider, ChainState, Shared};
use ckb_shared::txo_set::TxoSetDiff;
use ckb_verification::{verify_transactions, BlockVerifier, Verifier};
use faketime::unix_time_as_millis;
use fnv::{FnvHashMap, FnvHashSet};
use log::{self, debug, error, log_enabled};
use numext_fixed_hash::H256;
use numext_fixed_uint::U256;
use std::cmp;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub struct ChainService<CI> {
    shared: Shared<CI>,
    notify: NotifyController,
    block_verifier: BlockVerifier<Shared<CI>>,
}

#[derive(Clone)]
pub struct ChainController {
    process_block_sender: Sender<Request<Arc<Block>, Result<(), ProcessBlockError>>>,
}

pub struct ChainReceivers {
    process_block_receiver: Receiver<Request<Arc<Block>, Result<(), ProcessBlockError>>>,
}

impl ChainController {
    pub fn build() -> (ChainController, ChainReceivers) {
        let (process_block_sender, process_block_receiver) = channel::bounded(DEFAULT_CHANNEL_SIZE);
        (
            ChainController {
                process_block_sender,
            },
            ChainReceivers {
                process_block_receiver,
            },
        )
    }

    pub fn process_block(&self, block: Arc<Block>) -> Result<(), ProcessBlockError> {
        Request::call(&self.process_block_sender, block).expect("process_block() failed")
    }
}

#[derive(Debug, Clone)]
pub struct BlockInsertionResult {
    pub fork_blks: ForkBlocks,
    pub new_best_block: bool,
}

impl<CI: ChainIndex + 'static> ChainService<CI> {
    pub fn new(shared: Shared<CI>, notify: NotifyController) -> ChainService<CI> {
        let block_verifier = BlockVerifier::new(shared.clone());
        ChainService {
            shared,
            notify,
            block_verifier,
        }
    }

    pub fn start<S: ToString>(
        mut self,
        thread_name: Option<S>,
        receivers: ChainReceivers,
    ) -> JoinHandle<()> {
        let mut thread_builder = thread::Builder::new();
        // Mainly for test: give a empty thread_name
        if let Some(name) = thread_name {
            thread_builder = thread_builder.name(name.to_string());
        }
        thread_builder
            .spawn(move || loop {
                select! {
                    recv(receivers.process_block_receiver) -> msg => match msg {
                        Ok(Request { responder, arguments: block }) => {
                            let _ = responder.send(self.process_block(block));
                        },
                        _ => {
                            error!(target: "chain", "process_block_receiver closed");
                            break;
                        },
                    }
                }
            })
            .expect("Start ChainService failed")
    }

    fn process_block(&mut self, block: Arc<Block>) -> Result<(), ProcessBlockError> {
        debug!(target: "chain", "begin processing block: {}", block.header().hash());
        if self.shared.consensus().verification {
            self.block_verifier
                .verify(&block)
                .map_err(ProcessBlockError::Verification)?
        }
        let insert_result = self
            .insert_block(&block)
            .map_err(ProcessBlockError::Shared)?;
        self.post_insert_result(block, insert_result);
        debug!(target: "chain", "finish processing block");
        Ok(())
    }

    #[allow(clippy::op_ref)]
    fn insert_block(&self, block: &Block) -> Result<BlockInsertionResult, SharedError> {
        let mut new_best_block = false;
        let mut total_difficulty = U256::zero();

        let mut old_cumulative_blks = Vec::new();
        let mut new_cumulative_blks = Vec::new();

        let mut txo_set_diff = TxoSetDiff::default();

        let mut chain_state = self.shared.chain_state().write();
        let tip_number = chain_state.tip_number();
        let parent_ext = self
            .shared
            .store()
            .get_block_ext(&block.header().parent_hash())
            .expect("parent already store");

        let cannon_total_difficulty = parent_ext.total_difficulty + block.header().difficulty();
        let current_total_difficulty = chain_state.total_difficulty();

        debug!(
            target: "chain",
            "difficulty current = {}, cannon = {}",
            current_total_difficulty,
            cannon_total_difficulty,
        );

        let ext = BlockExt {
            received_at: unix_time_as_millis(),
            total_difficulty: cannon_total_difficulty.clone(),
            total_uncles_count: parent_ext.total_uncles_count + block.uncles().len() as u64,
            valid: None,
        };

        self.shared.store().save_with_batch(|batch| {
            self.shared.store().insert_block(batch, block);

            if &cannon_total_difficulty > current_total_difficulty
                || (current_total_difficulty == &cannon_total_difficulty
                    && block.header().hash() < chain_state.tip_hash())
            {
                debug!(
                    target: "chain",
                    "new best block found: {} => {}, difficulty diff = {}",
                    block.header().number(), block.header().hash(),
                    &cannon_total_difficulty - current_total_difficulty
                );

                txo_set_diff = self.reconcile_main_chain(
                    batch,
                    tip_number,
                    block,
                    &mut old_cumulative_blks,
                    &mut new_cumulative_blks,
                    ext,
                    &*chain_state,
                )?;

                self.shared
                    .store()
                    .insert_tip_header(batch, &block.header());

                new_best_block = true;

                total_difficulty = cannon_total_difficulty;
            } else {
                self.shared
                    .store()
                    .insert_block_ext(batch, &block.header().hash(), &ext);
            }
            Ok(())
        })?;

        if new_best_block {
            debug!(target: "chain", "update index");

            chain_state.update_header(block.header().clone());
            chain_state.update_difficulty(total_difficulty);
            chain_state.update_txo_set(txo_set_diff);

            debug!(target: "chain", "update index release");
        }

        Ok(BlockInsertionResult {
            new_best_block,
            fork_blks: ForkBlocks::new(old_cumulative_blks, new_cumulative_blks),
        })
    }

    fn post_insert_result(&mut self, block: Arc<Block>, result: BlockInsertionResult) {
        let BlockInsertionResult {
            new_best_block,
            mut fork_blks,
        } = result;
        if !fork_blks.old_blks().is_empty() {
            fork_blks.push_new(Block::clone(&block));
            self.notify.notify_switch_fork(Arc::new(fork_blks.clone()));
        }

        if new_best_block {
            self.notify.notify_new_tip(block);
            if log_enabled!(target: "chain", log::Level::Debug) {
                self.print_chain(10);
            }
        } else {
            self.notify.notify_new_uncle(block);
        }
    }

    fn update_index(&self, batch: &mut Batch, old_blocks: &[Block], new_blocks: &[Block]) {
        let old_number = match old_blocks.get(0) {
            Some(b) => b.header().number(),
            None => 0,
        };

        let new_number = new_blocks[0].header().number();

        for block in old_blocks {
            self.shared
                .store()
                .delete_block_number(batch, &block.header().hash());
            self.shared
                .store()
                .delete_transaction_address(batch, block.commit_transactions());
        }

        for block in new_blocks {
            let number = block.header().number();
            let hash = block.header().hash();
            self.shared.store().insert_block_hash(batch, number, &hash);
            self.shared
                .store()
                .insert_block_number(batch, &hash, number);
            self.shared.store().insert_transaction_address(
                batch,
                &hash,
                block.commit_transactions(),
            );
        }

        for n in new_number..old_number {
            self.shared.store().delete_block_hash(batch, n + 1);
        }
    }

    pub fn get_forks(
        &self,
        tip_number: BlockNumber,
        block: &Block,
        old_blocks: &mut Vec<Block>,
        new_blocks: &mut Vec<Block>,
        exts: &mut Vec<(H256, BlockExt)>,
        ext: BlockExt,
    ) -> bool {
        let mut number = block.header().number() - 1;

        // The old fork may longer than new fork
        if number < tip_number {
            for n in number..tip_number + 1 {
                let hash = self.shared.block_hash(n).unwrap();
                let old_block = self.shared.block(&hash).unwrap();

                old_blocks.push(old_block);
            }
        }

        let mut verified = ext.valid.is_some();
        let mut hash = block.header().hash();

        if !verified {
            exts.push((hash, ext));
        }

        //TODO: remove this clone
        new_blocks.push(block.clone());

        hash = block.header().parent_hash().clone();

        loop {
            if let Some(old_hash) = self.shared.block_hash(number) {
                if old_hash == hash {
                    verified = true;
                    break;
                }

                let old_block = self.shared.block(&old_hash).unwrap();

                old_blocks.push(old_block);
            }

            if !verified {
                let ext = self.shared.block_ext(&hash).unwrap();
                if let Some(x) = ext.valid {
                    if x {
                        verified = true;
                    } else {
                        break;
                    }
                } else {
                    exts.push((hash.clone(), ext));
                }
            }

            let new_block = self.shared.block(&hash).unwrap();

            hash = new_block.header().parent_hash().clone();
            number -= 1;

            new_blocks.push(new_block);
        }

        verified
    }

    // we found new best_block total_difficulty > old_chain.total_difficulty
    pub fn reconcile_main_chain(
        &self,
        batch: &mut Batch,
        tip_number: BlockNumber,
        block: &Block,
        old_blocks: &mut Vec<Block>,
        new_blocks: &mut Vec<Block>,
        ext: BlockExt,
        chain_state: &ChainState,
    ) -> Result<TxoSetDiff, SharedError> {
        let skip_verify = !self.shared.consensus().verification;
        let mut exts = Vec::new();
        let mut verified =
            self.get_forks(tip_number, block, old_blocks, new_blocks, &mut exts, ext);

        //new valid block len
        let mut new_len = 0;

        let mut old_inputs = FnvHashSet::default();
        let mut old_outputs = FnvHashSet::default();
        let mut new_inputs = FnvHashSet::default();
        let mut new_outputs = FnvHashMap::default();

        if verified {
            let new_blocks_iter = new_blocks.iter().rev();
            let old_blocks_iter = old_blocks.iter().rev();
            let new_blocks_len = new_blocks.len();

            let verified_len = new_blocks_len - exts.len();

            for b in old_blocks_iter.skip(verified_len) {
                for tx in b.commit_transactions() {
                    let input_pts = tx.input_pts();
                    let tx_hash = tx.hash();

                    for pt in input_pts {
                        old_inputs.insert(pt);
                    }

                    old_outputs.insert(tx_hash);
                }
            }

            for b in new_blocks_iter.clone().take(verified_len) {
                for tx in b.commit_transactions() {
                    let input_pts = tx.input_pts();
                    let tx_hash = tx.hash();
                    let output_len = tx.outputs().len();
                    for pt in input_pts {
                        new_inputs.insert(pt);
                    }

                    new_outputs.insert(tx_hash, output_len);
                }
            }

            for b in new_blocks_iter.clone().skip(verified_len) {
                if skip_verify
                    || verify_transactions(b, |op| {
                        self.shared.cell_at(op, |op| {
                            if new_inputs.contains(op) {
                                Some(true)
                            } else if let Some(x) = new_outputs.get(&op.hash) {
                                if op.index < (*x as u32) {
                                    Some(false)
                                } else {
                                    Some(true)
                                }
                            } else if old_outputs.contains(&op.hash) {
                                None
                            } else {
                                chain_state
                                    .is_spent(op)
                                    .map(|x| x && !old_inputs.contains(op))
                            }
                        })
                    })
                    .is_ok()
                {
                    for tx in b.commit_transactions() {
                        let input_pts = tx.input_pts();
                        let tx_hash = tx.hash();
                        let output_len = tx.outputs().len();
                        for pt in input_pts {
                            new_inputs.insert(pt);
                        }

                        new_outputs.insert(tx_hash, output_len);
                    }
                    new_len += 1;
                } else {
                    verified = false;
                    break;
                }
            }
        }

        for (hash, ext) in exts.iter_mut().rev().take(new_len) {
            ext.valid = Some(true);
            self.shared.store().insert_block_ext(batch, hash, ext);
        }

        for (hash, ext) in exts.iter_mut().rev().skip(new_len) {
            ext.valid = Some(false);
            self.shared.store().insert_block_ext(batch, hash, ext);
        }

        if !verified {
            return Err(SharedError::InvalidTransaction);
        }

        self.update_index(batch, old_blocks, new_blocks);

        new_blocks.reverse();

        let old_inputs: Vec<OutPoint> = old_inputs.into_iter().collect();
        let old_outputs: Vec<H256> = old_outputs.into_iter().collect();
        let new_inputs: Vec<OutPoint> = new_inputs.into_iter().collect();
        let new_outputs: Vec<(H256, usize)> = new_outputs.into_iter().collect();

        Ok(TxoSetDiff {
            old_inputs,
            old_outputs,
            new_inputs,
            new_outputs,
        })
    }

    fn print_chain(&self, len: u64) {
        debug!(target: "chain", "Chain {{");

        let tip = self.shared.chain_state().read().tip_number();
        let bottom = tip - cmp::min(tip, len);

        for number in (bottom..=tip).rev() {
            let hash = self.shared.block_hash(number).unwrap_or_else(|| {
                panic!(format!("invaild block number({}), tip={}", number, tip))
            });
            debug!(target: "chain", "   {} => {}", number, hash);
        }

        debug!(target: "chain", "}}");

        // TODO: remove me when block explorer is available
        debug!(target: "chain", "Tx in Head Block {{");
        for transaction in self
            .shared
            .block_hash(tip)
            .and_then(|hash| self.shared.store().get_block_body(&hash))
            .expect("invalid block number")
        {
            debug!(target: "chain", "   {} => {:?}", transaction.hash(), transaction);
        }
        debug!(target: "chain", "}}");

        debug!(target: "chain", "Uncle block {{");
        for (index, uncle) in self
            .shared
            .block_hash(tip)
            .and_then(|hash| self.shared.store().get_block_uncles(&hash))
            .expect("invalid block number")
            .iter()
            .enumerate()
        {
            debug!(target: "chain", "   {} => {:?}", index, uncle);
        }
        debug!(target: "chain", "}}");
    }
}

pub struct ChainBuilder<CI> {
    shared: Shared<CI>,
    notify: Option<NotifyController>,
}

impl<CI: ChainIndex + 'static> ChainBuilder<CI> {
    pub fn new(shared: Shared<CI>) -> ChainBuilder<CI> {
        let mut consensus = Consensus::default();
        consensus.initial_block_reward = 50;
        ChainBuilder {
            shared,
            notify: None,
        }
    }

    pub fn notify(mut self, value: NotifyController) -> Self {
        self.notify = Some(value);
        self
    }

    pub fn build(mut self) -> ChainService<CI> {
        let notify = self.notify.take().unwrap_or_else(|| {
            // FIXME: notify should not be optional
            let (_handle, notify) = NotifyService::default().start::<&str>(None);
            notify
        });
        ChainService::new(self.shared, notify)
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use ckb_core::block::BlockBuilder;
    use ckb_core::cell::CellProvider;
    use ckb_core::header::{Header, HeaderBuilder};
    use ckb_core::transaction::{
        CellInput, CellOutput, OutPoint, ProposalShortId, Transaction, TransactionBuilder,
    };
    use ckb_core::uncle::UncleBlock;
    use ckb_db::memorydb::MemoryKeyValueDB;
    use ckb_shared::shared::SharedBuilder;
    use ckb_shared::store::ChainKVStore;
    use numext_fixed_uint::U256;

    fn start_chain(
        consensus: Option<Consensus>,
    ) -> (ChainController, Shared<ChainKVStore<MemoryKeyValueDB>>) {
        let builder = SharedBuilder::<ChainKVStore<MemoryKeyValueDB>>::new_memory();
        let shared = builder
            .consensus(consensus.unwrap_or_else(|| Consensus::default().set_verification(false)))
            .build();

        let (chain_controller, chain_receivers) = ChainController::build();
        let chain_service = ChainBuilder::new(shared.clone()).build();
        let _handle = chain_service.start::<&str>(None, chain_receivers);
        (chain_controller, shared)
    }

    fn create_cellbase(number: BlockNumber) -> Transaction {
        TransactionBuilder::default()
            .input(CellInput::new_cellbase_input(number))
            .output(CellOutput::new(0, vec![], H256::zero(), None))
            .build()
    }

    fn gen_block(
        parent_header: &Header,
        nonce: u64,
        difficulty: U256,
        commit_transactions: Vec<Transaction>,
        uncles: Vec<UncleBlock>,
    ) -> Block {
        let number = parent_header.number() + 1;
        let cellbase = create_cellbase(number);
        let header = HeaderBuilder::default()
            .parent_hash(parent_header.hash().clone())
            .timestamp(unix_time_as_millis())
            .number(number)
            .difficulty(difficulty)
            .nonce(nonce)
            .build();

        BlockBuilder::default()
            .header(header)
            .commit_transaction(cellbase)
            .commit_transactions(commit_transactions)
            .uncles(uncles)
            .proposal_transaction(ProposalShortId::from_slice(&[1; 10]).unwrap())
            .build()
    }

    fn create_transaction(parent: H256) -> Transaction {
        let mut output = CellOutput::default();
        output.capacity = 100_000_000 / 100 as u64;
        let outputs: Vec<CellOutput> = vec![output.clone(); 100];

        TransactionBuilder::default()
            .input(CellInput::new(OutPoint::new(parent, 0), Default::default()))
            .outputs(outputs)
            .build()
    }

    #[test]
    fn test_genesis_transaction_spend() {
        let tx = TransactionBuilder::default()
            .input(CellInput::new(OutPoint::null(), Default::default()))
            .outputs(vec![
                CellOutput::new(
                    100_000_000,
                    vec![],
                    H256::default(),
                    None
                );
                100
            ])
            .build();

        let mut root_hash = tx.hash().clone();

        let genesis_block = BlockBuilder::default()
            .commit_transaction(tx)
            .with_header_builder(HeaderBuilder::default().difficulty(U256::from(1000u64)));

        let consensus = Consensus::default()
            .set_genesis_block(genesis_block)
            .set_verification(false);
        let (chain_controller, shared) = start_chain(Some(consensus));

        let end = 21;

        let mut blocks1: Vec<Block> = vec![];
        let mut parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..end {
            let difficulty = parent.difficulty().clone();
            let tx = create_transaction(root_hash);
            root_hash = tx.hash().clone();
            let new_block = gen_block(&parent, i, difficulty + U256::from(1u64), vec![tx], vec![]);
            blocks1.push(new_block.clone());
            parent = new_block.header().clone();
        }

        for block in &blocks1[0..10] {
            assert!(chain_controller
                .process_block(Arc::new(block.clone()))
                .is_ok());
        }
    }

    #[test]
    fn test_genesis_transaction_fetch() {
        let tx = TransactionBuilder::default()
            .input(CellInput::new(OutPoint::null(), Default::default()))
            .outputs(vec![
                CellOutput::new(
                    100_000_000,
                    vec![],
                    H256::default(),
                    None
                );
                100
            ])
            .build();

        let root_hash = tx.hash().clone();

        let genesis_block = BlockBuilder::default()
            .commit_transaction(tx)
            .with_header_builder(HeaderBuilder::default().difficulty(U256::from(1000u64)));

        let consensus = Consensus::default()
            .set_genesis_block(genesis_block)
            .set_verification(false);
        let (_chain_controller, shared) = start_chain(Some(consensus));

        let out_point = OutPoint::new(root_hash, 0);
        let state = shared.cell(&out_point);
        assert!(state.is_live());
    }

    #[test]
    fn test_chain_fork_by_total_difficulty() {
        let (chain_controller, shared) = start_chain(None);
        let final_number = 20;

        let mut chain1: Vec<Block> = Vec::new();
        let mut chain2: Vec<Block> = Vec::new();

        let mut parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let new_block = gen_block(&parent, i, difficulty + U256::from(100u64), vec![], vec![]);
            chain1.push(new_block.clone());
            parent = new_block.header().clone();
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let j = if i > 10 { 110 } else { 99 };
            let new_block = gen_block(
                &parent,
                i + 1000,
                difficulty + U256::from(j as u32),
                vec![],
                vec![],
            );
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }

        for block in &chain1 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }

        for block in &chain2 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }
        assert_eq!(
            shared.block_hash(8),
            chain2.get(7).map(|b| b.header().hash())
        );
    }

    #[test]
    fn test_chain_fork_by_hash() {
        let (chain_controller, shared) = start_chain(None);
        let final_number = 20;

        let mut chain1: Vec<Block> = Vec::new();
        let mut chain2: Vec<Block> = Vec::new();

        let mut parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let new_block = gen_block(&parent, i, difficulty + U256::from(100u64), vec![], vec![]);
            chain1.push(new_block.clone());
            parent = new_block.header().clone();
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let new_block = gen_block(
                &parent,
                i + 1000,
                difficulty + U256::from(100u64),
                vec![],
                vec![],
            );
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }

        for block in &chain1 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }

        for block in &chain2 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }

        //if total_difficulty equal, we chose block which have smaller hash as best
        assert!(chain1
            .iter()
            .zip(chain2.iter())
            .all(|(a, b)| a.header().difficulty() == b.header().difficulty()));

        let best = if chain1[(final_number - 2) as usize].header().hash()
            < chain2[(final_number - 2) as usize].header().hash()
        {
            chain1
        } else {
            chain2
        };
        assert_eq!(shared.block_hash(8), best.get(7).map(|b| b.header().hash()));
        assert_eq!(
            shared.block_hash(19),
            best.get(18).map(|b| b.header().hash())
        );
    }

    #[test]
    fn test_chain_get_ancestor() {
        let (chain_controller, shared) = start_chain(None);
        let final_number = 20;

        let mut chain1: Vec<Block> = Vec::new();
        let mut chain2: Vec<Block> = Vec::new();

        let mut parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let new_block = gen_block(&parent, i, difficulty + U256::from(100u64), vec![], vec![]);
            chain1.push(new_block.clone());
            parent = new_block.header().clone();
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = parent.difficulty().clone();
            let new_block = gen_block(
                &parent,
                i + 1000,
                difficulty + U256::from(100u64),
                vec![],
                vec![],
            );
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }

        for block in &chain1 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }

        for block in &chain2 {
            chain_controller
                .process_block(Arc::new(block.clone()))
                .expect("process block ok");
        }

        assert_eq!(
            *chain1[9].header(),
            shared
                .get_ancestor(&chain1.last().unwrap().header().hash(), 10)
                .unwrap()
        );

        assert_eq!(
            *chain2[9].header(),
            shared
                .get_ancestor(&chain2.last().unwrap().header().hash(), 10)
                .unwrap()
        );
    }

    #[test]
    fn test_calculate_difficulty() {
        let genesis_block = BlockBuilder::default()
            .with_header_builder(HeaderBuilder::default().difficulty(U256::from(1000u64)));
        let mut consensus = Consensus::default()
            .set_genesis_block(genesis_block)
            .set_verification(false);
        consensus.pow_time_span = 200;
        consensus.pow_spacing = 1;

        let (chain_controller, shared) = start_chain(Some(consensus.clone()));
        let final_number = shared.consensus().difficulty_adjustment_interval();

        let mut chain1: Vec<Block> = Vec::new();
        let mut chain2: Vec<Block> = Vec::new();

        let mut parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number - 1 {
            let difficulty = shared.calculate_difficulty(&parent).unwrap();
            let new_block = gen_block(&parent, i, difficulty, vec![], vec![]);
            chain_controller
                .process_block(Arc::new(new_block.clone()))
                .expect("process block ok");
            chain1.push(new_block.clone());
            parent = new_block.header().clone();
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = shared.calculate_difficulty(&parent).unwrap();
            let mut uncles = vec![];
            if i < 26 {
                uncles.push(chain1[i as usize].clone().into());
            }
            let new_block = gen_block(&parent, i + 100, difficulty, vec![], uncles);
            chain_controller
                .process_block(Arc::new(new_block.clone()))
                .expect("process block ok");
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }
        let tip = shared.chain_state().read().tip_header().clone();
        let total_uncles_count = shared.block_ext(&tip.hash()).unwrap().total_uncles_count;
        assert_eq!(total_uncles_count, 25);
        let difficulty = shared.calculate_difficulty(&tip).unwrap();

        // 25 * 10 * 1000 / 200
        assert_eq!(difficulty, U256::from(1250u64));

        let (chain_controller, shared) = start_chain(Some(consensus.clone()));
        let mut chain2: Vec<Block> = Vec::new();
        for i in 1..final_number - 1 {
            chain_controller
                .process_block(Arc::new(chain1[(i - 1) as usize].clone()))
                .expect("process block ok");
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = shared.calculate_difficulty(&parent).unwrap();
            let mut uncles = vec![];
            if i < 11 {
                uncles.push(chain1[i as usize].clone().into());
            }
            let new_block = gen_block(&parent, i + 100, difficulty, vec![], uncles);
            chain_controller
                .process_block(Arc::new(new_block.clone()))
                .expect("process block ok");
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }
        let tip = shared.chain_state().read().tip_header().clone();
        let total_uncles_count = shared.block_ext(&tip.hash()).unwrap().total_uncles_count;
        assert_eq!(total_uncles_count, 10);
        let difficulty = shared.calculate_difficulty(&tip).unwrap();

        // min[10 * 10 * 1000 / 200, 1000]
        assert_eq!(difficulty, U256::from(1000u64));

        let (chain_controller, shared) = start_chain(Some(consensus.clone()));
        let mut chain2: Vec<Block> = Vec::new();
        for i in 1..final_number - 1 {
            chain_controller
                .process_block(Arc::new(chain1[(i - 1) as usize].clone()))
                .expect("process block ok");
        }

        parent = shared.block_header(&shared.block_hash(0).unwrap()).unwrap();
        for i in 1..final_number {
            let difficulty = shared.calculate_difficulty(&parent).unwrap();
            let mut uncles = vec![];
            if i < 151 {
                uncles.push(chain1[i as usize].clone().into());
            }
            let new_block = gen_block(&parent, i + 100, difficulty, vec![], uncles);
            chain_controller
                .process_block(Arc::new(new_block.clone()))
                .expect("process block ok");
            chain2.push(new_block.clone());
            parent = new_block.header().clone();
        }
        let tip = shared.chain_state().read().tip_header().clone();
        let total_uncles_count = shared.block_ext(&tip.hash()).unwrap().total_uncles_count;
        assert_eq!(total_uncles_count, 150);
        let difficulty = shared.calculate_difficulty(&tip).unwrap();

        // max[150 * 10 * 1000 / 200, 2 * 1000]
        assert_eq!(difficulty, U256::from(2000u64));
    }
}
