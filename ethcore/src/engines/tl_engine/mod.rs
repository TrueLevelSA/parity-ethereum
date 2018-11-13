// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.


use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Weak, Arc};
use std::time::{UNIX_EPOCH, SystemTime, Duration};

use account_provider::AccountProvider;
use block::*;
use client::EngineClient;
use engines::{Engine, Seal, EngineError, ConstructedVerifier};
use engines::block_reward;
use engines::block_reward::{BlockRewardContract, RewardKind};
use error::{Error, ErrorKind, BlockError};
use ethjson;
use machine::{AuxiliaryData, Call, EthereumMachine};
use hash::keccak;
use header::{Header, BlockNumber, ExtendedHeader};
use super::signer::EngineSigner;
use super::validator_set::{ValidatorSet, SimpleList, new_validator_set};
use ethkey::{self, Password, Signature};
use io::{IoContext, IoHandler, TimerToken, IoService};
use itertools::{self, Itertools};
use rlp::{encode, Decodable, DecoderError, Encodable, RlpStream, Rlp};
use ethereum_types::{H256, H520, Address, U128, U256};
use ids::{BlockId, UncleId};
use parking_lot::{RwLock};
use unexpected::{Mismatch, OutOfBounds};


use casper::message::{CasperMsg, Message};
use casper::justification::{Justification, LatestMsgsHonest, SenderState, LatestMsgs};
use casper::senders_weight::SendersWeight;
use casper::traits::{Sender as CasperSender, Estimate, Data, Zero};
use casper::weight_unit::WeightUnit;

use std::collections::HashMap;
use std::convert::From;
use parity_machine::{Machine, TotalScoredHeader};
use std::sync::{Mutex};

#[derive(Clone, Ord, Hash, Debug, PartialOrd, Eq, PartialEq)]
struct ProtoBlock {
	prev_block: Option<CasperBlock>,
    address: CasperAddress,
	// height: usize,
}

#[derive(Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Debug)]
pub struct CasperBlock(Box<Arc<ProtoBlock>>);

type BlockMsg = Message<CasperBlock, CasperAddress>;

impl Data for CasperBlock {
    type Data = Self;
    fn is_valid(_data: &Self::Data) -> bool {
        true // FIXME
    }
}

impl From<ProtoBlock> for CasperBlock {
    fn from(protoblock: ProtoBlock) -> Self {
        CasperBlock(Box::new(Arc::new(protoblock)))
    }
}
impl<'z> From<&'z BlockMsg> for CasperBlock {
    fn from(msg: &BlockMsg) -> Self {
        msg.get_estimate().clone()
    }
}


impl CasperBlock {
    pub fn new(
        prev_block: Option<CasperBlock>,
        address: CasperAddress,
    ) -> Self {
        CasperBlock::from(ProtoBlock {
            prev_block,
            address,
        })
    }
    pub fn get_sender(&self) -> CasperAddress {
        self.0.address.clone()
    }
    pub fn from_prevblock_msg(
        prevblock_msg: Option<BlockMsg>,
        // a incomplete_block is a block with a None prev_block (ie, Estimate) AND is
        // not a genesis_block
        incomplete_block: CasperBlock,
    ) -> Result<Self, &'static str> {
        let prev_block = prevblock_msg.map(|m| CasperBlock::from(&m));
        let block = CasperBlock::from(ProtoBlock {
            prev_block,
            ..(**incomplete_block.0).clone()
        });

        if CasperBlock::is_valid(&block) {
            Ok(block)
        } else {
            Err("CasperBlock not valid")
        }
    }
    pub fn is_member(&self, rhs: &Self) -> bool {
        self == rhs
            || rhs
                .get_prevblock()
                .as_ref()
                .map(|prev_block| self.is_member(prev_block))
                .unwrap_or(false)
    }

    //TODO: this should possibly go to Estimate trait (not sure)
    pub fn set_as_final(&mut self) -> () {
        let mut proto_block = (**self.0).clone();
        proto_block.prev_block = None;
        *self.0 = Arc::new(proto_block);
    }

    pub fn get_prevblock(&self) -> Option<Self> {
        self.0.prev_block.as_ref().cloned()
    }
    pub fn parse_blockchains(
        latest_msgs: &LatestMsgsHonest<BlockMsg>,
        finalized_msg: Option<&BlockMsg>,
    ) -> (HashMap<CasperBlock, HashSet<CasperBlock>>, HashSet<CasperBlock>) {
        let mut visited: HashMap<CasperBlock, HashSet<CasperBlock>> = latest_msgs
            .iter()
            .map(|msg| {
                let parent = CasperBlock::from(msg);
                let children = HashSet::new();
                (parent, children)
            })
            .collect();
        let mut queue: VecDeque<CasperBlock> = visited.keys().cloned().collect();
        let mut genesis: HashSet<CasperBlock> = HashSet::new();
        while let Some(child) = queue.pop_front() {
            match child.get_prevblock() {
                Some(parent) => {
                    if visited.contains_key(&parent) {
                        // println!("visited parent before, fork found");
                        visited.get_mut(&parent).map(|parents_children| {
                            parents_children.insert(child);
                        });
                    } else {
                        // println!("didnt visit parent before, set initial state, and push to queue");
                        let mut parents_children = HashSet::new();
                        parents_children.insert(child);
                        visited.insert(parent.clone(), parents_children);
                        queue.push_back(parent);
                    }
                }
                None => {
                    genesis.insert(child);
                }
            };
        }
        (visited, genesis)
    }

    // find heaviest block
    fn pick_heaviest(
        blocks: &HashSet<CasperBlock>,
        visited: &HashMap<CasperBlock, HashSet<CasperBlock>>,
        weights: &SendersWeight<CasperAddress>,
    ) -> Option<(Option<Self>, WeightUnit, HashSet<Self>)> {
        let init = Some((None, WeightUnit::ZERO, HashSet::new()));
        let heaviest_child = blocks.iter().fold(init, |best, block| {
            best.and_then(|best| {
                visited.get(&block).map(|children| (best, children))
            }).map(|((b_block, b_weight, b_children), children)| {
                let mut referred_senders: HashSet<_> =
                    children.iter().map(Self::get_sender).collect();
                // add current block sender such that its weight counts too
                referred_senders.insert(block.get_sender());
                let weight = weights.sum_weight_senders(&referred_senders);
                // TODO: break ties with blockhash
                if weight > b_weight {
                    (Some(block.clone()), weight, children.clone())
                } else {
                    (b_block, b_weight, b_children)
                }
            })
        });
        heaviest_child.and_then(|(b_block, b_weight, b_children)| {
            if b_children.is_empty() {
                // base case
                Some((b_block, b_weight, b_children))
            } else {
                // recurse
                Self::pick_heaviest(&b_children, visited, &weights)
            }
        })
    }

    pub fn ghost(
        latest_msgs: &LatestMsgsHonest<BlockMsg>,
        finalized_msg: Option<&BlockMsg>,
        senders_weights: &SendersWeight<<BlockMsg as CasperMsg>::Sender>,
    ) -> Option<Self> {
        let (visited, genesis) =
            Self::parse_blockchains(latest_msgs, finalized_msg);
        Self::pick_heaviest(&genesis, &visited, senders_weights)
            .and_then(|(opt_block, ..)| opt_block)
    }
}


impl Estimate for CasperBlock {
    type M = BlockMsg;
    fn mk_estimate(
        latest_msgs: &LatestMsgsHonest<Self::M>,
        finalized_msg: Option<&Self::M>,
        senders_weights: &SendersWeight<
            <<Self as Estimate>::M as CasperMsg>::Sender,
        >,
        // in fact i could put the whole mempool inside of this incomplete_block
        // and search for a reasonable set of txs in this function that does not
        // conflict with the past blocks
        incomplete_block: Option<<Self as Data>::Data>,
    ) -> Self {
        match (latest_msgs.len(), incomplete_block) {
            (0, _) => panic!(
                "Needs at least one latest message to be able to pick one"
            ),
            (_, None) => panic!("incomplete_block is None"),
            (1, Some(incomplete_block)) => {
                // only msg to built on top, no choice thus no ghost
                let msg = latest_msgs.iter().next().cloned();
                Self::from_prevblock_msg(msg, incomplete_block).unwrap()
            }
            (_, Some(incomplete_block)) => {
                let prev_block =
                    CasperBlock::ghost(latest_msgs, finalized_msg, senders_weights);
                let block = CasperBlock::from(ProtoBlock {
                    prev_block,
                    ..(**incomplete_block.0).clone()
                });
                block
            }
        }
    }
}

impl From<ethjson::spec::TLEngineParams> for TLEngineParams {
	fn from(p: ethjson::spec::TLEngineParams) -> Self {
		let validators: HashMap<CasperAddress, f64> =
			p.validators.iter().fold(HashMap::new(), |mut validators, validator| {
				validators.insert(CasperAddress{ inner: validator.address }, validator.weight);
				validators
			});
		let senders_weights = SendersWeight::new(validators);
		TLEngineParams {
			thr: p.fault_tolerance_thr
				.map(Into::into)
				.unwrap_or_else(||
					senders_weights.sum_weight_senders(
				    	&senders_weights.get_senders().unwrap()
				    ) / 2.0
				),
			senders_weights,
		}
	}
}

/// `TLEngine` params.
#[derive(Clone)]
pub struct TLEngineParams {
	senders_weights: SendersWeight<CasperAddress>,
	thr: WeightUnit,
}

/// Engine using `TLEngine` casper consensus.
// #[derive(Ord, Clone, Hash, Debug, Eq, PartialEq, PartialOrd)]
pub struct TLEngine {
	client: Arc<RwLock<Option<Weak<EngineClient>>>>,
	machine: EthereumMachine,
	sender_state: RwLock<SenderState<BlockMsg>>,
	block_msgs: RwLock<HashMap<H256, BlockMsg>>,
	/// Used to sign messages and proposals.
	signer: RwLock<EngineSigner>,
}

/// Wrapper to make the Address a Sender for the casper lib
#[derive(Debug, PartialOrd, Eq, PartialEq, Ord, Hash, Clone)]
pub struct CasperAddress {
	inner: Address,
}

/// actual impl of the casper::Sender trait
impl CasperSender for CasperAddress { }


impl TLEngine {
	/// Create a new instance of TLEngine engine.
	pub fn new(params: TLEngineParams, machine: EthereumMachine) -> Result<Arc<Self>, Error> {
		let TLEngineParams{ senders_weights, thr } = params;
		let engine = Arc::new(
			TLEngine {
				client: Arc::new(RwLock::new(None)),
				sender_state: RwLock::new(
					SenderState::new(
						senders_weights,
						WeightUnit::ZERO,
						None,
						LatestMsgs::new(),
						thr,
						HashSet::new(),
					)),
				machine: machine,
				block_msgs: RwLock::new(HashMap::new()),
				signer: Default::default(),
			});

		Ok(engine)
	}

	fn mk_casper_msg(&self, header: &Header) {
		let parent_block = self.block_msgs.read().get(header.parent_hash()).map(CasperBlock::from);
		match parent_block {
			None => {
				if header.parent_hash() == &H256::from(0) {
					let genesis_address = CasperAddress { inner: 0x0000000000000000000000000000000000000000.into() };
					let genesis_block = CasperBlock::new(None, genesis_address.clone());
					let genesis_msg = BlockMsg::new(genesis_address, Justification::new(), genesis_block);

					// let genesis_msg = self.block_msgs.read().get(&parent.parent_hash()).cloned().unwrap();
					self.block_msgs.try_write().map(|mut msgs| {
						msgs.insert(H256::from(0), genesis_msg);
					});
				} else {
					self.client.read().as_ref()
						.and_then(Weak::upgrade)
						.and_then(|c| {
							c.as_full_client()
								.and_then(|client| client.block_header(BlockId::Hash(*header.parent_hash())))
								.and_then(|h| h.decode().ok())
						})
						.as_ref()
						.map(|parent| self.mk_casper_msg(parent))
						.unwrap_or_else(|| println!("Could not unwrap in casper msg!"));
				}
				self.mk_casper_msg(header);
			},
			Some(parent_block) => {
				// FIXME: author shouldnt be used for casper msg, as it can differ from the signer (block reward on
				// a different address then the validating address) or should enforce that address must be signer if
				// check done on another function (to avoid recovering signer twice)
				let author = header.author();
				let casper_address = CasperAddress{ inner: author.clone() };
				let casper_block = CasperBlock::new(Some(parent_block), casper_address.clone());
				let block_id = BlockId::Hash(header.hash());
				let uncles: Vec<_> = self.client.read().as_ref()
					.and_then(Weak::upgrade)
					.and_then(|c| {
						c.as_full_client().map(|client|
							(0..)
								.map(|i| client.uncle(UncleId { block: block_id, position: i })
									 .and_then(|u| u.decode().ok())
									 .map(|u| u.hash()))
								.take_while(|u| u.is_some())
								.filter_map(|u| u)
								.collect()
						)})
					.expect("full client must be available");
				println!("uncles: {:?}", uncles);
				let mut msgs_for_justification: Vec<_> = uncles.iter().map(|uncle| {
					self.block_msgs.read().get(&uncle).cloned()
						.expect("uncle should be in otherwise block wouldn't verify")
				}).collect();
				self.block_msgs.read()
					.get(&header.parent_hash()).cloned()
					.map(|parent| msgs_for_justification.push(parent));
				let (justification, mut new) = Justification::from_msgs(
					msgs_for_justification,
					&self.sender_state.read()
				);
				let msg = BlockMsg::new(casper_address, justification, casper_block);

				// inject the newly converted msg to the latest_msgs, as that will be used for the forkchoice rule
				new.get_latest_msgs_as_mut().update(&msg);

				*self.sender_state.write() = new;

				println!("msg: {:?}", msg);
				// println!("block: {:?}", CasperBlock::from(&msg));
				self.block_msgs.try_write().map(|mut msgs| msgs.insert(header.hash(), msg));
			}
		}
	}
}



impl Engine<EthereumMachine> for TLEngine {

	fn name(&self) -> &str { "TLEngine" }

	fn machine(&self) -> &EthereumMachine { &self.machine }

	fn verify_local_seal(&self, _header: &Header) -> Result<(), Error> { Ok(()) }

	/// The number of additional header fields required for this engine.
	fn seal_fields(&self, _header: &Header) -> usize { 1 }

	fn fork_choice(&self, new: &ExtendedHeader, current: &ExtendedHeader) -> super::ForkChoice {
		let _ = self.mk_casper_msg(&new.header);
		let new_block = self.block_msgs.read().get(&new.header.hash()).map(CasperBlock::from);
		let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
			self.sender_state.read().get_latest_msgs(),
			self.sender_state.read().get_equivocators(),
		);
		let best_block = CasperBlock::ghost(
			&latest_honest_msgs,
			None,
			self.sender_state.read().get_senders_weights(),
		);

		let current_block = self.block_msgs.read().get(&current.header.hash()).map(CasperBlock::from);

		// FIXME: not needed to unwrap, but good for dbg
		let best_block = &best_block.expect("best block is None");
		let new_block = &new_block.expect("new_block doesnt exist as casper_msg");
		let current_block = &current_block.expect("current doesnt exist as casper_msg");

		// println!("\nsenders_weight: {:?}", self.sender_state.read().get_senders_weights());
		// println!("\nbest_block {:?}", best_block);
		// println!("\nnew_block {:?}", new_block);
		// println!("\ncurrent_block {:?}", current_block);

		if best_block == new_block { super::ForkChoice::New }
		else if best_block == current_block { super::ForkChoice::Old }
		else {
			panic!(
				"Block picked on forkchoice rule not available in fork_choice fun \n best: {:?} \n new: {:?} \n current: {:?} \n ",
				best_block,
				new,
				current
			);
		}
	}

	fn maximum_uncle_count(&self, _block: BlockNumber) -> usize { 40 }

	fn maximum_uncle_age(&self) -> usize { 10 }

	fn seals_internally(&self) -> Option<bool> {
		Some(self.signer.read().is_some())
	}


	/// Phase 3 verification. Check block information against parent. Returns either a null `Ok` or a general error detailing the problem with import.
	fn verify_block_family(&self, header: &Header, parent: &Header) -> Result<(), <EthereumMachine as ::parity_machine::Machine>::Error> {
		// println!("block_family");
		Ok(())
	}

	fn verify_block_basic(&self, header: &<EthereumMachine as ::parity_machine::Machine>::Header) -> Result<(), <EthereumMachine as ::parity_machine::Machine>::Error> {
		// println!("verify_block_basic");
		let found_seal_len = header.seal().len();
		let expected_seal_len = self.seal_fields(header);
		if found_seal_len != expected_seal_len {
			return Err(BlockError::InvalidSealArity(
				Mismatch { expected: expected_seal_len, found: found_seal_len}
			).into())
		}

		// Check if the signature belongs to a validator, can depend on parent state.
		let author = header.author();
		let seal = &header.seal();
		let header_signature = seal.get(0).ok_or(BlockError::InvalidSeal)?;
		let sig = Rlp::new(header_signature).as_val::<H520>()?;
		let signer = ethkey::public_to_address(&ethkey::recover(&sig.into(), &header.bare_hash())?);
		// println!("author: {:?}", author);
		// println!("signer: {:?}", signer);

		if *author != signer {
			return Err(EngineError::NotAuthorized(*author).into())
		}

		// if !senders.iter().any(|sender| sender.inner == signer) {
		// 	return Err(EngineError::NotAuthorized(*author).into());
		// }

		Ok(())
	}

	fn on_new_block(
		&self,
		block: &mut <EthereumMachine as ::parity_machine::Machine>::LiveBlock,
		_epoch_begin: bool,
		_ancestry: &mut Iterator<Item=<EthereumMachine as ::parity_machine::Machine>::ExtendedHeader>,
	) -> Result<(), <EthereumMachine as ::parity_machine::Machine>::Error> {
		// println!("on_new_block");
		Ok(())
	}

	/// Block transformation functions, after the transactions.
	fn on_close_block(&self, block: &mut <EthereumMachine as ::parity_machine::Machine>::LiveBlock) -> Result<(), <EthereumMachine as ::parity_machine::Machine>::Error> {
		// println!("on_close_block");
		// TODO: block reward, slashing, adding / removing validators from the set should be done here
		Ok(())
	}

	fn generate_seal(&self, block: &<EthereumMachine as ::parity_machine::Machine>::LiveBlock, _parent: &Header) -> Seal {
		self.sign(block.header().bare_hash())
			.map(|raw_sig| ::rlp::encode(&(&H520::from(raw_sig) as &[u8])).into_vec())
			.map(|sig| Seal::Regular(vec![sig]))
			.unwrap_or_else(|e| {
				println!("Failed to sign in generate_seal!, {:?}", e);
				Seal::None
			})

	}

	fn open_block_header_timestamp(&self, parent_timestamp: u64) -> u64 {
		use std::{time, cmp};

		let now = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap_or_default();
		cmp::max(now.as_secs(), parent_timestamp)
	}

	fn is_timestamp_valid(&self, header_timestamp: u64, parent_timestamp: u64) -> bool {
		header_timestamp >= parent_timestamp
	}

	/// necessary in order to receive messages
	fn register_client(&self, client: Weak<EngineClient>) {
		*self.client.write() = Some(client.clone());
	}

	fn set_signer(&self, ap: Arc<AccountProvider>, address: Address, password: Password) {
		self.signer.write().set(ap, address, password);
	}

	fn sign(&self, hash: H256) -> Result<Signature, Error> {
		Ok(self.signer.read().sign(hash)?)
	}
}

/// tests from the instant seal consensus, applied to the current TLEngine consensus
#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use ethereum_types::{H520, Address};
	use test_helpers::get_temp_state_db;
	use spec::Spec;
	use header::Header;
	use block::*;
	use engines::Seal;

	#[test]
	fn tlengine_can_seal() {
		let spec = Spec::new_tlengine();
		let engine = &*spec.engine;
		let db = spec.ensure_db_good(get_temp_state_db(), &Default::default()).unwrap();
		let genesis_header = spec.genesis_header();
		let last_hashes = Arc::new(vec![genesis_header.hash()]);
		let b = OpenBlock::new(engine, Default::default(), false, db, &genesis_header, last_hashes, Address::default(), (3141562.into(), 31415620.into()), vec![], false, &mut Vec::new().into_iter()).unwrap();
		let b = b.close_and_lock().unwrap();
		if let Seal::Regular(seal) = engine.generate_seal(b.block(), &genesis_header) {
			assert!(b.try_seal(engine, seal).is_ok());
		}
	}

	#[test]
	fn tlengine_cant_verify() {
		let engine = Spec::new_tlengine().engine;
		let mut header: Header = Header::default();

		assert!(engine.verify_block_basic(&header).is_ok());

		header.set_seal(vec![::rlp::encode(&H520::default()).into_vec()]);

		assert!(engine.verify_block_unordered(&header).is_ok());
	}
}
