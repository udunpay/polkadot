// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Runtime component for handling disputes of parachain candidates.

use sp_std::prelude::*;
use primitives::v1::{
	SessionIndex, CandidateHash,
	DisputeState, DisputeStatementSet, MultiDisputeStatementSet, ValidatorId, ValidatorSignature,
	DisputeStatement, ValidDisputeStatementKind, InvalidDisputeStatementKind,
	ExplicitDisputeStatement, CompactStatement, SigningContext, ApprovalVote, ValidatorIndex,
	ConsensusLog,
};
use sp_runtime::{
	traits::{One, Zero, Saturating, AppVerify},
	DispatchError, SaturatedConversion, RuntimeDebug,
};
use frame_support::{ensure, traits::Get, weights::Weight};
use parity_scale_codec::{Encode, Decode};
use bitvec::{bitvec, order::Lsb0 as BitOrderLsb0};
use crate::{
	configuration::{self, HostConfiguration},
	initializer::SessionChangeNotification,
	session_info,
};

/// Whereas the dispute is local or remote.
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub enum DisputeLocation {
	Local,
	Remote,
}

/// The result of a dispute, whether the candidate is deemed valid (for) or invalid (against).
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub enum DisputeResult {
	Valid,
	Invalid,
}

/// Reward hooks for disputes.
pub trait RewardValidators {
	// Give each validator a reward, likely small, for participating in the dispute.
	fn reward_dispute_statement(session: SessionIndex, validators: impl IntoIterator<Item=ValidatorIndex>);
}

impl RewardValidators for () {
	fn reward_dispute_statement(_: SessionIndex, _: impl IntoIterator<Item=ValidatorIndex>) { }
}

/// Punishment hooks for disputes.
pub trait PunishValidators {
	/// Punish a series of validators who were for an invalid parablock. This is expected to be a major
	/// punishment.
	fn punish_for_invalid(session: SessionIndex, validators: impl IntoIterator<Item=ValidatorIndex>);

	/// Punish a series of validators who were against a valid parablock. This is expected to be a minor
	/// punishment.
	fn punish_against_valid(session: SessionIndex, validators: impl IntoIterator<Item=ValidatorIndex>);

	/// Punish a series of validators who were part of a dispute which never concluded. This is expected
	/// to be a minor punishment.
	fn punish_inconclusive(session: SessionIndex, validators: impl IntoIterator<Item=ValidatorIndex>);
}

pub use pallet::*;
#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use super::*;

	#[pallet::config]
	pub trait Config:
		frame_system::Config +
		configuration::Config +
		session_info::Config
	{
		type Event: From<Event> + IsType<<Self as frame_system::Config>::Event>;
		type RewardValidators: RewardValidators;
		type PunishValidators: PunishValidators;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// The last pruneed session, if any. All data stored by this module
	/// references sessions.
	#[pallet::storage]
	pub(super) type LastPrunedSession<T> = StorageValue<_, SessionIndex>;

	/// All ongoing or concluded disputes for the last several sessions.
	#[pallet::storage]
	pub(super) type Disputes<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat, SessionIndex,
		Blake2_128Concat, CandidateHash,
		DisputeState<T::BlockNumber>,
	>;

	/// All included blocks on the chain, as well as the block number in this chain that
	/// should be reverted back to if the candidate is disputed and determined to be invalid.
	#[pallet::storage]
	pub(super) type Included<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat, SessionIndex,
		Blake2_128Concat, CandidateHash,
		T::BlockNumber,
	>;

	/// Maps session indices to a vector indicating the number of potentially-spam disputes
	/// each validator is participating in. Potentially-spam disputes are remote disputes which have
	/// fewer than `byzantine_threshold + 1` validators.
	///
	/// The i'th entry of the vector corresponds to the i'th validator in the session.
	#[pallet::storage]
	pub(super) type SpamSlots<T> = StorageMap<_, Twox64Concat, SessionIndex, Vec<u32>>;

	/// Whether the chain is frozen or not. Starts as `false`. When this is `true`,
	/// the chain will not accept any new parachain blocks for backing or inclusion.
	/// It can only be set back to `false` by governance intervention.
	#[pallet::storage]
	#[pallet::getter(fn is_frozen)]
	pub(super) type Frozen<T> =  StorageValue<_, bool, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub fn deposit_event)]
	pub enum Event {
		/// A dispute has been initiated. \[candidate hash, dispute location\]
		DisputeInitiated(CandidateHash, DisputeLocation),
		/// A dispute has concluded for or against a candidate.
		/// \[para_id, candidate hash, dispute result\]
		DisputeConcluded(CandidateHash, DisputeResult),
		/// A dispute has timed out due to insufficient participation.
		/// \[para_id, candidate hash\]
		DisputeTimedOut(CandidateHash),
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {}

	#[pallet::error]
	pub enum Error<T> {
		/// Duplicate dispute statement sets provided.
		DuplicateDisputeStatementSets,
		/// Ancient dispute statement provided.
		AncientDisputeStatement,
		/// Validator index on statement is out of bounds for session.
		ValidatorIndexOutOfBounds,
		/// Invalid signature on statement.
		InvalidSignature,
		/// Validator vote submitted more than once to dispute.
		DuplicateStatement,
		/// Too many spam slots used by some specific validator.
		PotentialSpam,
	}
}

// The maximum number of validators `f` which may safely be faulty.
//
// The total number of validators is `n = 3f + e` where `e in { 1, 2, 3 }`.
fn byzantine_threshold(n: usize) -> usize {
	n.saturating_sub(1) / 3
}

// The supermajority threshold of validators which is required to
// conclude a dispute.
fn supermajority_threshold(n: usize) -> usize {
	n - byzantine_threshold(n)
}

bitflags::bitflags! {
	#[derive(Default)]
	struct DisputeStateFlags: u8 {
		const CONFIRMED = 0b0001;
		const FOR_SUPERMAJORITY = 0b0010;
		const AGAINST_SUPERMAJORITY = 0b0100;
	}
}

impl DisputeStateFlags {
	fn from_state<BlockNumber>(
		state: &DisputeState<BlockNumber>,
	) -> Self {
		let n = state.validators_for.len();

		let byzantine_threshold = byzantine_threshold(n);
		let supermajority_threshold = supermajority_threshold(n);

		let mut flags = DisputeStateFlags::default();
		let all_participants = {
			let mut a = state.validators_for.clone();
			*a |= state.validators_against.iter().by_val();
			a
		};
		if all_participants.count_ones() > byzantine_threshold {
			flags |= DisputeStateFlags::CONFIRMED;
		}

		if state.validators_for.count_ones() >= supermajority_threshold {
			flags |= DisputeStateFlags::FOR_SUPERMAJORITY;
		}

		if state.validators_against.count_ones() >= supermajority_threshold {
			flags |= DisputeStateFlags::AGAINST_SUPERMAJORITY;
		}

		flags
	}
}

#[derive(PartialEq, RuntimeDebug)]
enum SpamSlotChange {
	Inc,
	Dec,
}

struct ImportSummary<BlockNumber> {
	// The new state, with all votes imported.
	state: DisputeState<BlockNumber>,
	// Changes to spam slots. Validator index paired with directional change.
	spam_slot_changes: Vec<(ValidatorIndex, SpamSlotChange)>,
	// Validators to slash for being (wrongly) on the AGAINST side.
	slash_against: Vec<ValidatorIndex>,
	// Validators to slash for being (wrongly) on the FOR side.
	slash_for: Vec<ValidatorIndex>,
	// New participants in the dispute.
	new_participants: bitvec::vec::BitVec<BitOrderLsb0, u8>,
	// Difference in state flags from previous.
	new_flags: DisputeStateFlags,
}

#[derive(RuntimeDebug, PartialEq, Eq)]
enum VoteImportError {
	ValidatorIndexOutOfBounds,
	DuplicateStatement,
}

impl<T: Config> From<VoteImportError> for Error<T> {
	fn from(e: VoteImportError) -> Self {
		match e {
			VoteImportError::ValidatorIndexOutOfBounds => Error::<T>::ValidatorIndexOutOfBounds,
			VoteImportError::DuplicateStatement => Error::<T>::DuplicateStatement,
		}
	}
}

struct DisputeStateImporter<BlockNumber> {
	state: DisputeState<BlockNumber>,
	now: BlockNumber,
	new_participants: bitvec::vec::BitVec<BitOrderLsb0, u8>,
	pre_flags: DisputeStateFlags,
}

impl<BlockNumber: Clone> DisputeStateImporter<BlockNumber> {
	fn new(
		state: DisputeState<BlockNumber>,
		now: BlockNumber,
	) -> Self {
		let pre_flags = DisputeStateFlags::from_state(&state);
		let new_participants = bitvec::bitvec![BitOrderLsb0, u8; 0; state.validators_for.len()];

		DisputeStateImporter {
			state,
			now,
			new_participants,
			pre_flags,
		}
	}

	fn import(&mut self, validator: ValidatorIndex, valid: bool)
		-> Result<(), VoteImportError>
	{
		let (bits, other_bits) = if valid {
			(&mut self.state.validators_for, &mut self.state.validators_against)
		} else {
			(&mut self.state.validators_against, &mut self.state.validators_for)
		};

		// out of bounds or already participated
		match bits.get(validator.0 as usize).map(|b| *b) {
			None => return Err(VoteImportError::ValidatorIndexOutOfBounds),
			Some(true) => return Err(VoteImportError::DuplicateStatement),
			Some(false) => {}
		}

		// inefficient, and just for extra sanity.
		if validator.0 as usize >= self.new_participants.len() {
			return Err(VoteImportError::ValidatorIndexOutOfBounds);
		}

		bits.set(validator.0 as usize, true);

		// New participants tracks those which didn't appear on either
		// side of the dispute until now. So we check the other side
		// and checked the first side before.
		if other_bits.get(validator.0 as usize).map_or(false, |b| !*b) {
			self.new_participants.set(validator.0 as usize, true);
		}

		Ok(())
	}

	fn finish(mut self) -> ImportSummary<BlockNumber> {
		let pre_flags = self.pre_flags;
		let post_flags = DisputeStateFlags::from_state(&self.state);

		let pre_post_contains = |flags| (pre_flags.contains(flags), post_flags.contains(flags));

		// 1. Act on confirmed flag state to inform spam slots changes.
		let spam_slot_changes: Vec<_> = match pre_post_contains(DisputeStateFlags::CONFIRMED) {
			(false, false) => {
				// increment spam slots for all new participants.
				self.new_participants.iter_ones()
					.map(|i| (ValidatorIndex(i as _), SpamSlotChange::Inc))
					.collect()
			}
			(false, true) => {
				let prev_participants = {
					// all participants
					let mut a = self.state.validators_for.clone();
					*a |= self.state.validators_against.iter().by_val();

					// which are not new participants
					*a &= self.new_participants.iter().by_val().map(|b| !b);

					a
				};

				prev_participants.iter_ones()
					.map(|i| (ValidatorIndex(i as _), SpamSlotChange::Dec))
					.collect()
			}
			(true, true) | (true, false) => {
				// nothing to do. (true, false) is also impossible.
				Vec::new()
			}
		};

		// 2. Check for fresh FOR supermajority. Only if not already concluded.
		let slash_against = if let (false, true) = pre_post_contains(DisputeStateFlags::FOR_SUPERMAJORITY) {
			if self.state.concluded_at.is_none() {
				self.state.concluded_at = Some(self.now.clone());
			}

			// provide AGAINST voters to slash.
			self.state.validators_against.iter_ones()
				.map(|i| ValidatorIndex(i as _))
				.collect()
		} else {
			Vec::new()
		};

		// 3. Check for fresh AGAINST supermajority.
		let slash_for = if let (false, true) = pre_post_contains(DisputeStateFlags::AGAINST_SUPERMAJORITY) {
			if self.state.concluded_at.is_none() {
				self.state.concluded_at = Some(self.now.clone());
			}

			// provide FOR voters to slash.
			self.state.validators_for.iter_ones()
				.map(|i| ValidatorIndex(i as _))
				.collect()
		} else {
			Vec::new()
		};

		ImportSummary {
			state: self.state,
			spam_slot_changes,
			slash_against,
			slash_for,
			new_participants: self.new_participants,
			new_flags: post_flags - pre_flags,
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Called by the initalizer to initialize the disputes module.
	pub(crate) fn initializer_initialize(now: T::BlockNumber) -> Weight {
		let config = <configuration::Module<T>>::config();

		let mut weight = 0;
		for (session_index, candidate_hash, mut dispute) in <Disputes<T>>::iter() {
			weight += T::DbWeight::get().reads_writes(1, 0);

			if dispute.concluded_at.is_none()
				&& dispute.start + config.dispute_conclusion_by_time_out_period < now
			{
				Self::deposit_event(Event::DisputeTimedOut(candidate_hash));

				dispute.concluded_at = Some(now);
				<Disputes<T>>::insert(session_index, candidate_hash, &dispute);

				if <Included<T>>::contains_key(&session_index, &candidate_hash) {
					// Local disputes don't count towards spam.

					weight += T::DbWeight::get().reads_writes(1, 1);
					continue;
				}

				// mildly punish all validators involved. they've failed to make
				// data available to others, so this is most likely spam.
				SpamSlots::<T>::mutate(session_index, |spam_slots| {
					let spam_slots = match spam_slots {
						Some(ref mut s) => s,
						None => return,
					};

					// also reduce spam slots for all validators involved, if the dispute was unconfirmed.
					// this does open us up to more spam, but only for validators who are willing
					// to be punished more.
					//
					// it would be unexpected for any change here to occur when the dispute has not concluded
					// in time, as a dispute guaranteed to have at least one honest participant should
					// conclude quickly.
					let participating = decrement_spam(spam_slots, &dispute);

					// Slight punishment as these validators have failed to make data available to
					// others in a timely manner.
					T::PunishValidators::punish_inconclusive(
						session_index,
						participating.iter_ones().map(|i| ValidatorIndex(i as _)),
					);
				});

				weight += T::DbWeight::get().reads_writes(2, 2);
			}
		}

		weight
	}

	/// Called by the initalizer to finalize the disputes module.
	pub(crate) fn initializer_finalize() { }

	/// Called by the initalizer to note a new session in the disputes module.
	pub(crate) fn initializer_on_new_session(notification: &SessionChangeNotification<T::BlockNumber>) {
		let config = <configuration::Pallet<T>>::config();

		if notification.session_index <= config.dispute_period + 1 {
			return
		}

		let pruning_target = notification.session_index - config.dispute_period - 1;

		LastPrunedSession::<T>::mutate(|last_pruned| {
			let to_prune = if let Some(last_pruned) = last_pruned {
				*last_pruned + 1 ..= pruning_target
			} else {
				pruning_target ..= pruning_target
			};

			for to_prune in to_prune {
				<Disputes<T>>::remove_prefix(to_prune);
				<Included<T>>::remove_prefix(to_prune);
				SpamSlots::<T>::remove(to_prune);
			}

			*last_pruned = Some(pruning_target);
		});
	}

	/// Handle sets of dispute statements corresponding to 0 or more candidates.
	/// Returns a vector of freshly created disputes.
	///
	/// # Warning
	///
	/// This functions modifies the state when failing. It is expected to be called in inherent,
	/// and to fail the extrinsic on error. As invalid inherents are not allowed, the dirty state
	/// is not commited.
	pub(crate) fn provide_multi_dispute_data(statement_sets: MultiDisputeStatementSet)
		-> Result<Vec<(SessionIndex, CandidateHash)>, DispatchError>
	{
		let config = <configuration::Pallet<T>>::config();

		// Deduplicate.
		{
			let mut targets: Vec<_> = statement_sets.iter()
				.map(|set| (set.candidate_hash.0, set.session))
				.collect();

			targets.sort();

			let submitted = targets.len();
			targets.dedup();

			ensure!(submitted == targets.len(), Error::<T>::DuplicateDisputeStatementSets);
		}

		let mut fresh = Vec::with_capacity(statement_sets.len());
		for statement_set in statement_sets {
			let dispute_target = (statement_set.session, statement_set.candidate_hash);
			if Self::provide_dispute_data(&config, statement_set)? {
				fresh.push(dispute_target);
			}
		}

		Ok(fresh)
	}

	/// Handle a set of dispute statements corresponding to a single candidate.
	///
	/// Fails if the dispute data is invalid. Returns a bool indicating whether the
	/// dispute is fresh.
	fn provide_dispute_data(config: &HostConfiguration<T::BlockNumber>, set: DisputeStatementSet)
		-> Result<bool, DispatchError>
	{
		// Dispute statement sets on any dispute which concluded
		// before this point are to be rejected.
		let now = <frame_system::Pallet<T>>::block_number();
		let oldest_accepted = now.saturating_sub(config.dispute_post_conclusion_acceptance_period);

		// Load session info to access validators
		let session_info = match <session_info::Pallet<T>>::session_info(set.session) {
			Some(s) => s,
			None => return Err(Error::<T>::AncientDisputeStatement.into()),
		};

		let n_validators = session_info.validators.len();

		// Check for ancient.
		let (fresh, dispute_state) = {
			if let Some(dispute_state) = <Disputes<T>>::get(&set.session, &set.candidate_hash) {
				ensure!(
					dispute_state.concluded_at.as_ref().map_or(true, |c| c >= &oldest_accepted),
					Error::<T>::AncientDisputeStatement,
				);

				(false, dispute_state)
			} else {
				(
					true,
					DisputeState {
						validators_for: bitvec![BitOrderLsb0, u8; 0; n_validators],
						validators_against: bitvec![BitOrderLsb0, u8; 0; n_validators],
						start: now,
						concluded_at: None,
					}
				)
			}
		};

		// Check and import all votes.
		let summary = {
			let mut importer = DisputeStateImporter::new(dispute_state, now);
			for (statement, validator_index, signature) in &set.statements {
				let validator_public = session_info.validators.get(validator_index.0 as usize)
					.ok_or(Error::<T>::ValidatorIndexOutOfBounds)?;

				// Check signature before importing.
				check_signature(
					&validator_public,
					set.candidate_hash,
					set.session,
					statement,
					signature,
				).map_err(|()| Error::<T>::InvalidSignature)?;

				let valid = match statement {
					DisputeStatement::Valid(_) => true,
					DisputeStatement::Invalid(_) => false,
				};

				importer.import(*validator_index, valid).map_err(Error::<T>::from)?;
			}

			importer.finish()
		};

		// Apply spam slot changes. Bail early if too many occupied.
		let is_local = <Included<T>>::contains_key(&set.session, &set.candidate_hash);
		if !is_local {
			let mut spam_slots: Vec<u32> = SpamSlots::<T>::get(&set.session)
				.unwrap_or_else(|| vec![0; n_validators]);

			for (validator_index, spam_slot_change) in summary.spam_slot_changes {
				let spam_slot = spam_slots.get_mut(validator_index.0 as usize)
					.expect("index is in-bounds, as checked above; qed");

				match spam_slot_change {
					SpamSlotChange::Inc => {
						ensure!(
							*spam_slot < config.dispute_max_spam_slots,
							Error::<T>::PotentialSpam,
						);

						*spam_slot += 1;
					}
					SpamSlotChange::Dec => {
						*spam_slot = spam_slot.saturating_sub(1);
					}
				}
			}

			SpamSlots::<T>::insert(&set.session, spam_slots);
		}

		if fresh {
			Self::deposit_event(Event::DisputeInitiated(
				set.candidate_hash,
				if is_local { DisputeLocation::Local } else { DisputeLocation::Remote },
			));
		}

		{
			if summary.new_flags.contains(DisputeStateFlags::FOR_SUPERMAJORITY) {
				Self::deposit_event(Event::DisputeConcluded(
					set.candidate_hash,
					DisputeResult::Valid,
				));
			}

			// It is possible, although unexpected, for a dispute to conclude twice.
			// This would require f+1 validators to vote in both directions.
			// A dispute cannot conclude more than once in each direction.

			if summary.new_flags.contains(DisputeStateFlags::AGAINST_SUPERMAJORITY) {
				Self::deposit_event(Event::DisputeConcluded(
					set.candidate_hash,
					DisputeResult::Invalid,
				));
			}
		}

		// Reward statements.
		T::RewardValidators::reward_dispute_statement(
			set.session,
			summary.new_participants.iter_ones().map(|i| ValidatorIndex(i as _)),
		);

		// Slash participants on a losing side.
		{
			// a valid candidate, according to 2/3. Punish those on the 'against' side.
			T::PunishValidators::punish_against_valid(
				set.session,
				summary.slash_against,
			);

			// an invalid candidate, according to 2/3. Punish those on the 'for' side.
			T::PunishValidators::punish_for_invalid(
				set.session,
				summary.slash_for,
			);
		}

		<Disputes<T>>::insert(&set.session, &set.candidate_hash, &summary.state);

		// Freeze if just concluded against some local candidate
		if summary.new_flags.contains(DisputeStateFlags::AGAINST_SUPERMAJORITY) {
			if let Some(revert_to) = <Included<T>>::get(&set.session, &set.candidate_hash) {
				Self::revert_and_freeze(revert_to);
			}
		}

		Ok(fresh)
	}

	#[allow(unused)]
	pub(crate) fn disputes() -> Vec<(SessionIndex, CandidateHash, DisputeState<T::BlockNumber>)> {
		<Disputes<T>>::iter().collect()
	}

	pub(crate) fn note_included(session: SessionIndex, candidate_hash: CandidateHash, included_in: T::BlockNumber) {
		if included_in.is_zero() { return }

		let revert_to = included_in - One::one();

		<Included<T>>::insert(&session, &candidate_hash, revert_to);

		// If we just included a block locally which has a live dispute, decrement spam slots
		// for any involved validators, if the dispute is not already confirmed by f + 1.
		if let Some(state) = <Disputes<T>>::get(&session, candidate_hash) {
			SpamSlots::<T>::mutate(&session, |spam_slots| {
				if let Some(ref mut spam_slots) = *spam_slots {
					decrement_spam(spam_slots, &state);
				}
			});

			if has_supermajority_against(&state) {
				Self::revert_and_freeze(revert_to);
			}
		}
	}

	pub(crate) fn could_be_invalid(session: SessionIndex, candidate_hash: CandidateHash) -> bool {
		<Disputes<T>>::get(&session, &candidate_hash).map_or(false, |dispute| {
			// A dispute that is ongoing or has concluded with supermajority-against.
			dispute.concluded_at.is_none() || has_supermajority_against(&dispute)
		})
	}

	pub(crate) fn revert_and_freeze(revert_to: T::BlockNumber) {
		if Self::is_frozen() { return }

		let revert_to = revert_to.saturated_into();
		<frame_system::Pallet<T>>::deposit_log(ConsensusLog::RevertTo(revert_to).into());

		Frozen::<T>::set(true);
	}
}

fn has_supermajority_against<BlockNumber>(dispute: &DisputeState<BlockNumber>) -> bool {
	let supermajority_threshold = supermajority_threshold(dispute.validators_against.len());
	dispute.validators_against.count_ones() >= supermajority_threshold
}

// If the dispute had not enough validators to confirm, decrement spam slots for all the participating
// validators.
//
// returns the set of participating validators as a bitvec.
fn decrement_spam<BlockNumber>(
	spam_slots: &mut [u32],
	dispute: &DisputeState<BlockNumber>,
) -> bitvec::vec::BitVec<BitOrderLsb0, u8> {
	let byzantine_threshold = byzantine_threshold(spam_slots.len());

	let participating = dispute.validators_for.clone() | dispute.validators_against.iter().by_val();
	let decrement_spam = participating.count_ones() <= byzantine_threshold;
	for validator_index in participating.iter_ones() {
		if decrement_spam {
			if let Some(occupied) = spam_slots.get_mut(validator_index as usize) {
				*occupied = occupied.saturating_sub(1);
			}
		}
	}

	participating
}

fn check_signature(
	validator_public: &ValidatorId,
	candidate_hash: CandidateHash,
	session: SessionIndex,
	statement: &DisputeStatement,
	validator_signature: &ValidatorSignature,
) -> Result<(), ()> {
	let payload = match *statement {
		DisputeStatement::Valid(ValidDisputeStatementKind::Explicit) => {
			ExplicitDisputeStatement {
				valid: true,
				candidate_hash,
				session,
			}.signing_payload()
		},
		DisputeStatement::Valid(ValidDisputeStatementKind::BackingSeconded(inclusion_parent)) => {
			CompactStatement::Seconded(candidate_hash).signing_payload(&SigningContext {
				session_index: session,
				parent_hash: inclusion_parent,
			})
		},
		DisputeStatement::Valid(ValidDisputeStatementKind::BackingValid(inclusion_parent)) => {
			CompactStatement::Valid(candidate_hash).signing_payload(&SigningContext {
				session_index: session,
				parent_hash: inclusion_parent,
			})
		},
		DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking) => {
			ApprovalVote(candidate_hash).signing_payload(session)
		},
		DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit) => {
			ExplicitDisputeStatement {
				valid: false,
				candidate_hash,
				session,
			}.signing_payload()
		},
	};

	if validator_signature.verify(&payload[..] , &validator_public) {
		Ok(())
	} else {
		Err(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_system::InitKind;
	use frame_support::{assert_ok, assert_err, assert_noop, traits::{OnInitialize, OnFinalize}};
	use crate::mock::{
		new_test_ext, Test, System, AllPallets, Initializer, AccountId, MockGenesisConfig,
		REWARD_VALIDATORS, PUNISH_VALIDATORS_FOR, PUNISH_VALIDATORS_AGAINST,
		PUNISH_VALIDATORS_INCONCLUSIVE,
	};
	use sp_core::{Pair, crypto::CryptoType};
	use primitives::v1::BlockNumber;

	// All arguments for `initializer::on_new_session`
	type NewSession<'a> = (bool, SessionIndex, Vec<(&'a AccountId, ValidatorId)>, Option<Vec<(&'a AccountId, ValidatorId)>>);

	// Run to specific block, while calling disputes pallet hooks manually, because disputes is not
	// integrated in initializer yet.
	fn run_to_block<'a>(
		to: BlockNumber,
		new_session: impl Fn(BlockNumber) -> Option<NewSession<'a>>,
	) {
		while System::block_number() < to {
			let b = System::block_number();
			if b != 0 {
				AllPallets::on_finalize(b);
				System::finalize();
			}

			System::initialize(&(b + 1), &Default::default(), &Default::default(), InitKind::Full);
			AllPallets::on_initialize(b + 1);

			if let Some(new_session) = new_session(b + 1) {
				Initializer::test_trigger_on_new_session(
					new_session.0,
					new_session.1,
					new_session.2.into_iter(),
					new_session.3.map(|q| q.into_iter()),
				);
			}
		}
	}

	#[test]
	fn test_byzantine_threshold() {
		assert_eq!(byzantine_threshold(0), 0);
		assert_eq!(byzantine_threshold(1), 0);
		assert_eq!(byzantine_threshold(2), 0);
		assert_eq!(byzantine_threshold(3), 0);
		assert_eq!(byzantine_threshold(4), 1);
		assert_eq!(byzantine_threshold(5), 1);
		assert_eq!(byzantine_threshold(6), 1);
		assert_eq!(byzantine_threshold(7), 2);
	}

	#[test]
	fn test_supermajority_threshold() {
		assert_eq!(supermajority_threshold(0), 0);
		assert_eq!(supermajority_threshold(1), 1);
		assert_eq!(supermajority_threshold(2), 2);
		assert_eq!(supermajority_threshold(3), 3);
		assert_eq!(supermajority_threshold(4), 3);
		assert_eq!(supermajority_threshold(5), 4);
		assert_eq!(supermajority_threshold(6), 5);
		assert_eq!(supermajority_threshold(7), 5);
	}

	#[test]
	fn test_dispute_state_flag_from_state() {
		assert_eq!(
			DisputeStateFlags::from_state(&DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			}),
			DisputeStateFlags::default(),
		);

		assert_eq!(
			DisputeStateFlags::from_state(&DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 1, 1, 1, 1, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			}),
			DisputeStateFlags::FOR_SUPERMAJORITY | DisputeStateFlags::CONFIRMED,
		);

		assert_eq!(
			DisputeStateFlags::from_state(&DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 1, 1, 1, 1, 1, 0, 0],
				start: 0,
				concluded_at: None,
			}),
			DisputeStateFlags::AGAINST_SUPERMAJORITY | DisputeStateFlags::CONFIRMED,
		);
	}

	#[test]
	fn test_import_new_participant_spam_inc() {
		let mut importer = DisputeStateImporter::new(
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
			0,
		);

		assert_err!(
			importer.import(ValidatorIndex(9), true),
			VoteImportError::ValidatorIndexOutOfBounds,
		);

		assert_err!(
			importer.import(ValidatorIndex(0), true),
			VoteImportError::DuplicateStatement,
		);
		assert_ok!(importer.import(ValidatorIndex(0), false));

		assert_ok!(importer.import(ValidatorIndex(2), true));
		assert_err!(
			importer.import(ValidatorIndex(2), true),
			VoteImportError::DuplicateStatement,
		);

		assert_ok!(importer.import(ValidatorIndex(2), false));
		assert_err!(
			importer.import(ValidatorIndex(2), false),
			VoteImportError::DuplicateStatement,
		);

		let summary = importer.finish();
		assert_eq!(summary.new_flags, DisputeStateFlags::default());
		assert_eq!(
			summary.state,
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
		);
		assert_eq!(
			summary.spam_slot_changes,
			vec![(ValidatorIndex(2), SpamSlotChange::Inc)],
		);
		assert!(summary.slash_for.is_empty());
		assert!(summary.slash_against.is_empty());
		assert_eq!(summary.new_participants, bitvec![BitOrderLsb0, u8; 0, 0, 1, 0, 0, 0, 0, 0]);
	}

	#[test]
	fn test_import_prev_participant_spam_dec_confirmed() {
		let mut importer = DisputeStateImporter::new(
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
			0,
		);

		assert_ok!(importer.import(ValidatorIndex(2), true));

		let summary = importer.finish();
		assert_eq!(
			summary.state,
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
		);
		assert_eq!(
			summary.spam_slot_changes,
			vec![
				(ValidatorIndex(0), SpamSlotChange::Dec),
				(ValidatorIndex(1), SpamSlotChange::Dec),
			],
		);
		assert!(summary.slash_for.is_empty());
		assert!(summary.slash_against.is_empty());
		assert_eq!(summary.new_participants, bitvec![BitOrderLsb0, u8; 0, 0, 1, 0, 0, 0, 0, 0]);
		assert_eq!(summary.new_flags, DisputeStateFlags::CONFIRMED);
	}

	#[test]
	fn test_import_prev_participant_spam_dec_confirmed_slash_for() {
		let mut importer = DisputeStateImporter::new(
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
			0,
		);

		assert_ok!(importer.import(ValidatorIndex(2), true));
		assert_ok!(importer.import(ValidatorIndex(2), false));
		assert_ok!(importer.import(ValidatorIndex(3), false));
		assert_ok!(importer.import(ValidatorIndex(4), false));
		assert_ok!(importer.import(ValidatorIndex(5), false));
		assert_ok!(importer.import(ValidatorIndex(6), false));

		let summary = importer.finish();
		assert_eq!(
			summary.state,
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 1, 1, 1, 1, 1, 0],
				start: 0,
				concluded_at: Some(0),
			},
		);
		assert_eq!(
			summary.spam_slot_changes,
			vec![
				(ValidatorIndex(0), SpamSlotChange::Dec),
				(ValidatorIndex(1), SpamSlotChange::Dec),
			],
		);
		assert_eq!(summary.slash_for, vec![ValidatorIndex(0), ValidatorIndex(2)]);
		assert!(summary.slash_against.is_empty());
		assert_eq!(summary.new_participants, bitvec![BitOrderLsb0, u8; 0, 0, 1, 1, 1, 1, 1, 0]);
		assert_eq!(
			summary.new_flags,
			DisputeStateFlags::CONFIRMED | DisputeStateFlags::AGAINST_SUPERMAJORITY,
		);
	}

	#[test]
	fn test_import_slash_against() {
		let mut importer = DisputeStateImporter::new(
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0, 0, 0, 0, 0],
				start: 0,
				concluded_at: None,
			},
			0,
		);

		assert_ok!(importer.import(ValidatorIndex(3), true));
		assert_ok!(importer.import(ValidatorIndex(4), true));
		assert_ok!(importer.import(ValidatorIndex(5), false));
		assert_ok!(importer.import(ValidatorIndex(6), true));
		assert_ok!(importer.import(ValidatorIndex(7), true));

		let summary = importer.finish();
		assert_eq!(
			summary.state,
			DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 1, 1, 0, 1, 1],
				validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0, 0, 1, 0, 0],
				start: 0,
				concluded_at: Some(0),
			},
		);
		assert!(summary.spam_slot_changes.is_empty());
		assert!(summary.slash_for.is_empty());
		assert_eq!(summary.slash_against, vec![ValidatorIndex(1), ValidatorIndex(5)]);
		assert_eq!(summary.new_participants, bitvec![BitOrderLsb0, u8; 0, 0, 0, 1, 1, 1, 1, 1]);
		assert_eq!(summary.new_flags, DisputeStateFlags::FOR_SUPERMAJORITY);
	}

	// Test that punish_inconclusive is correctly called.
	#[test]
	fn test_initializer_initialize() {
		let dispute_conclusion_by_time_out_period = 3;
		let start = 10;

		let mock_genesis_config = MockGenesisConfig {
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					dispute_conclusion_by_time_out_period,
					.. Default::default()
				},
				.. Default::default()
			},
			.. Default::default()
		};

		new_test_ext(mock_genesis_config).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v1 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v2 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v3 = <ValidatorId as CryptoType>::Pair::generate().0;

			// NOTE: v0 index will be 0
			// NOTE: v1 index will be 3
			// NOTE: v2 index will be 2
			// NOTE: v3 index will be 1

			run_to_block(
				start,
				|b| {
					// a new session at each block
					Some((
						true,
						b,
						vec![(&0, v0.public()), (&1, v1.public()), (&2, v2.public()), (&3, v3.public())],
						Some(vec![(&0, v0.public()), (&1, v1.public()), (&2, v2.public()), (&3, v3.public())]),
					))
				}
			);

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));

			// v0 votes for 3
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: start - 1,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: start - 1,
								}.signing_payload()
							)
						),
					],
				},
			];

			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![(9, candidate_hash.clone())],
			);
			assert_eq!(SpamSlots::<Test>::get(start - 1), Some(vec![1, 0, 0, 0]));

			// Run to timeout period
			run_to_block(start + dispute_conclusion_by_time_out_period, |_| None);
			assert_eq!(SpamSlots::<Test>::get(start - 1), Some(vec![1, 0, 0, 0]));

			// Run to timeout + 1 in order to executive on_finalize(timeout)
			run_to_block(start + dispute_conclusion_by_time_out_period + 1, |_| None);
			assert_eq!(SpamSlots::<Test>::get(start - 1), Some(vec![0, 0, 0, 0]));
			assert_eq!(
				PUNISH_VALIDATORS_INCONCLUSIVE.with(|r| r.borrow()[0].clone()),
				(9, vec![ValidatorIndex(0)]),
			);
		});
	}

	// Test prunning works
	#[test]
	fn test_initializer_on_new_session() {
		let dispute_period = 3;

		let mock_genesis_config = MockGenesisConfig {
			configuration: crate::configuration::GenesisConfig {
				config: HostConfiguration {
					dispute_period,
					.. Default::default()
				},
				.. Default::default()
			},
			.. Default::default()
		};

		new_test_ext(mock_genesis_config).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));
			Pallet::<Test>::note_included(0, candidate_hash.clone(), 0);
			Pallet::<Test>::note_included(1, candidate_hash.clone(), 1);
			Pallet::<Test>::note_included(2, candidate_hash.clone(), 2);
			Pallet::<Test>::note_included(3, candidate_hash.clone(), 3);
			Pallet::<Test>::note_included(4, candidate_hash.clone(), 4);
			Pallet::<Test>::note_included(5, candidate_hash.clone(), 5);
			Pallet::<Test>::note_included(6, candidate_hash.clone(), 5);

			run_to_block(
				7,
				|b| {
					// a new session at each block
					Some((
						true,
						b,
						vec![(&0, v0.public())],
						Some(vec![(&0, v0.public())]),
					))
				}
			);

			// current session is 7,
			// we keep for dispute_period + 1 session and we remove in on_finalize
			// thus we keep info for session 3, 4, 5, 6, 7.
			assert_eq!(Included::<Test>::iter_prefix(0).count(), 0);
			assert_eq!(Included::<Test>::iter_prefix(1).count(), 0);
			assert_eq!(Included::<Test>::iter_prefix(2).count(), 0);
			assert_eq!(Included::<Test>::iter_prefix(3).count(), 1);
			assert_eq!(Included::<Test>::iter_prefix(4).count(), 1);
			assert_eq!(Included::<Test>::iter_prefix(5).count(), 1);
			assert_eq!(Included::<Test>::iter_prefix(6).count(), 1);
		});
	}

	#[test]
	fn test_provide_multi_dispute_data_duplicate_error() {
		new_test_ext(Default::default()).execute_with(|| {
			let candidate_hash_1 = CandidateHash(sp_core::H256::repeat_byte(1));
			let candidate_hash_2 = CandidateHash(sp_core::H256::repeat_byte(2));

			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash_2,
					session: 2,
					statements: vec![],
				},
				DisputeStatementSet {
					candidate_hash: candidate_hash_1,
					session: 1,
					statements: vec![],
				},
				DisputeStatementSet {
					candidate_hash: candidate_hash_2,
					session: 2,
					statements: vec![],
				},
			];

			assert_err!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				DispatchError::from(Error::<Test>::DuplicateDisputeStatementSets),
			);
		})
	}

	// Test:
	// * wrong signature fails
	// * signature is checked for correct validator
	#[test]
	fn test_provide_multi_dispute_is_checking_signature_correctly() {
		new_test_ext(Default::default()).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v1 = <ValidatorId as CryptoType>::Pair::generate().0;

			run_to_block(
				3,
				|b| {
					// a new session at each block
					if b == 1 {
						Some((
							true,
							b,
							vec![(&0, v0.public())],
							Some(vec![(&0, v0.public())]),
						))
					} else {
						Some((
							true,
							b,
							vec![(&1, v1.public())],
							Some(vec![(&1, v1.public())]),
						))
					}
				}
			);


			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 1,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 1,
								}.signing_payload()
							),
						),
					],
				},
			];

			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![(1, candidate_hash.clone())],
			);

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 2,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 2,
								}.signing_payload()
							),
						),
					],
				},
			];

			assert_noop!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				DispatchError::from(Error::<Test>::InvalidSignature),
			);
		})
	}

	#[test]
	fn test_freeze_on_note_included() {
		new_test_ext(Default::default()).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;

			run_to_block(
				6,
				|b| {
					// a new session at each block
					Some((
						true,
						b,
						vec![(&0, v0.public())],
						Some(vec![(&0, v0.public())]),
					))
				}
			);

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));

			// v0 votes for 3
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							)
						),
					],
				},
			];
			assert!(Pallet::<Test>::provide_multi_dispute_data(stmts).is_ok());

			Pallet::<Test>::note_included(3, candidate_hash.clone(), 3);
			assert_eq!(Frozen::<Test>::get(), true);
		});
	}

	#[test]
	fn test_freeze_provided_against_supermajority_for_included() {
		new_test_ext(Default::default()).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;

			run_to_block(
				6,
				|b| {
					// a new session at each block
					Some((
						true,
						b,
						vec![(&0, v0.public())],
						Some(vec![(&0, v0.public())]),
					))
				}
			);

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));

			Pallet::<Test>::note_included(3, candidate_hash.clone(), 3);

			// v0 votes for 3
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							)
						),
					],
				},
			];
			assert!(Pallet::<Test>::provide_multi_dispute_data(stmts).is_ok());

			assert_eq!(Frozen::<Test>::get(), true);
		});
	}

	// tests for:
	// * provide_multi_dispute: with success scenario
	// * disputes: correctness of datas
	// * could_be_invalid: correctness of datas
	// * note_included: decrement spam correctly
	// * spam slots: correctly incremented and decremented
	// * ensure rewards and punishment are correctly called.
	#[test]
	fn test_provide_multi_dispute_success_and_other() {
		new_test_ext(Default::default()).execute_with(|| {
			let v0 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v1 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v2 = <ValidatorId as CryptoType>::Pair::generate().0;
			let v3 = <ValidatorId as CryptoType>::Pair::generate().0;

			// NOTE: v0 index will be 0
			// NOTE: v1 index will be 3
			// NOTE: v2 index will be 2
			// NOTE: v3 index will be 1

			run_to_block(
				6,
				|b| {
					// a new session at each block
					Some((
						true,
						b,
						vec![(&0, v0.public()), (&1, v1.public()), (&2, v2.public()), (&3, v3.public())],
						Some(vec![(&0, v0.public()), (&1, v1.public()), (&2, v2.public()), (&3, v3.public())]),
					))
				}
			);

			let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));

			// v0 votes for 3
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							)
						),
					],
				},
			];

			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![(3, candidate_hash.clone())],
			);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![1, 0, 0, 0]));

			// v1 votes for 4 and for 3
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 4,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(3),
							v1.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 4,
								}.signing_payload()
							)
						),
					],
				},
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(3),
							v1.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							),
						),
					],
				},
			];

			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![(4, candidate_hash.clone())],
			);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![0, 0, 0, 0])); // Confirmed as no longer spam
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));

			// v3 votes against 3 and for 5
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(1),
							v3.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							),
						),
					],
				},
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 5,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(1),
							v3.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 5,
								}.signing_payload()
							),
						),
					],
				},
			];
			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![(5, candidate_hash.clone())],
			);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![0, 0, 0, 0]));
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));
			assert_eq!(SpamSlots::<Test>::get(5), Some(vec![0, 1, 0, 0]));

			// v2 votes for 3 and againt 5
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 3,
					statements: vec![
						(
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit),
							ValidatorIndex(2),
							v2.sign(
								&ExplicitDisputeStatement {
									valid: true,
									candidate_hash: candidate_hash.clone(),
									session: 3,
								}.signing_payload()
							)
						),
					],
				},
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 5,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(2),
							v2.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 5,
								}.signing_payload()
							),
						),
					],
				},
			];
			assert_ok!(Pallet::<Test>::provide_multi_dispute_data(stmts), vec![]);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![0, 0, 0, 0]));
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));
			assert_eq!(SpamSlots::<Test>::get(5), Some(vec![0, 0, 0, 0]));

			// v0 votes for 5
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 5,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(0),
							v0.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 5,
								}.signing_payload()
							),
						),
					],
				},
			];

			assert_ok!(Pallet::<Test>::provide_multi_dispute_data(stmts), vec![]);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![0, 0, 0, 0]));
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));
			assert_eq!(SpamSlots::<Test>::get(5), Some(vec![0, 0, 0, 0]));

			// v1 votes for 5
			let stmts = vec![
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 5,
					statements: vec![
						(
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit),
							ValidatorIndex(3),
							v1.sign(
								&ExplicitDisputeStatement {
									valid: false,
									candidate_hash: candidate_hash.clone(),
									session: 5,
								}.signing_payload()
							)
						),
					],
				},
			];

			assert_ok!(
				Pallet::<Test>::provide_multi_dispute_data(stmts),
				vec![],
			);
			assert_eq!(SpamSlots::<Test>::get(3), Some(vec![0, 0, 0, 0]));
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));
			assert_eq!(SpamSlots::<Test>::get(5), Some(vec![0, 0, 0, 0]));

			assert_eq!(
				Pallet::<Test>::disputes(),
				vec![
					(
						5,
						candidate_hash.clone(),
						DisputeState {
							validators_for: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0],
							validators_against: bitvec![BitOrderLsb0, u8; 1, 0, 1, 1],
							start: 6,
							concluded_at: Some(6), // 3 vote against
						}
					),
					(
						3,
						candidate_hash.clone(),
						DisputeState {
							validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 1, 1],
							validators_against: bitvec![BitOrderLsb0, u8; 0, 1, 0, 0],
							start: 6,
							concluded_at: Some(6), // 3 vote for
						}
					),
					(
						4,
						candidate_hash.clone(),
						DisputeState {
							validators_for: bitvec![BitOrderLsb0, u8; 0, 0, 0, 1],
							validators_against: bitvec![BitOrderLsb0, u8; 0, 0, 0, 0],
							start: 6,
							concluded_at: None,
						}
					),
				]
			);

			assert_eq!(Pallet::<Test>::could_be_invalid(3, candidate_hash.clone()), false); // It has 3 votes for
			assert_eq!(Pallet::<Test>::could_be_invalid(4, candidate_hash.clone()), true);
			assert_eq!(Pallet::<Test>::could_be_invalid(5, candidate_hash.clone()), true);

			// Ensure inclusion removes spam slots
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 1]));
			Pallet::<Test>::note_included(4, candidate_hash.clone(), 4);
			assert_eq!(SpamSlots::<Test>::get(4), Some(vec![0, 0, 0, 0]));

			// Ensure the reward_validator function was correctly called
			assert_eq!(
				REWARD_VALIDATORS.with(|r| r.borrow().clone()),
				vec![
					(3, vec![ValidatorIndex(0)]),
					(4, vec![ValidatorIndex(3)]),
					(3, vec![ValidatorIndex(3)]),
					(3, vec![ValidatorIndex(1)]),
					(5, vec![ValidatorIndex(1)]),
					(3, vec![ValidatorIndex(2)]),
					(5, vec![ValidatorIndex(2)]),
					(5, vec![ValidatorIndex(0)]),
					(5, vec![ValidatorIndex(3)]),
				],
			);

			// Ensure punishment against is called
			assert_eq!(
				PUNISH_VALIDATORS_AGAINST.with(|r| r.borrow().clone()),
				vec![
					(3, vec![]),
					(4, vec![]),
					(3, vec![]),
					(3, vec![]),
					(5, vec![]),
					(3, vec![ValidatorIndex(1)]),
					(5, vec![]),
					(5, vec![]),
					(5, vec![]),
				],
			);

			// Ensure punishment for is called
			assert_eq!(
				PUNISH_VALIDATORS_FOR.with(|r| r.borrow().clone()),
				vec![
					(3, vec![]),
					(4, vec![]),
					(3, vec![]),
					(3, vec![]),
					(5, vec![]),
					(3, vec![]),
					(5, vec![]),
					(5, vec![]),
					(5, vec![ValidatorIndex(1)]),
				],
			);
		})
	}

	#[test]
	fn test_revert_and_freeze() {
		new_test_ext(Default::default()).execute_with(|| {
			Frozen::<Test>::put(true);
			assert_noop!(
				{
					Pallet::<Test>::revert_and_freeze(0);
					Result::<(), ()>::Err(()) // Just a small trick in order to use assert_noop.
				},
				(),
			);

			Frozen::<Test>::kill();
			Pallet::<Test>::revert_and_freeze(0);
			assert_eq!(Frozen::<Test>::get(), true);
		})
	}

	#[test]
	fn test_has_supermajority_against() {
		assert_eq!(
			has_supermajority_against(&DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 1, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 1, 1, 1, 1, 1, 0, 0, 0],
				start: 0,
				concluded_at: None,
			}),
			false,
		);

		assert_eq!(
			has_supermajority_against(&DisputeState {
				validators_for: bitvec![BitOrderLsb0, u8; 1, 1, 0, 0, 0, 0, 0, 0],
				validators_against: bitvec![BitOrderLsb0, u8; 1, 1, 1, 1, 1, 1, 0, 0],
				start: 0,
				concluded_at: None,
			}),
			true,
		);
	}

	#[test]
	fn test_decrement_spam() {
		let original_spam_slots = vec![0, 1, 2, 3, 4, 5, 6, 7];

		// Test confirm is no-op
		let mut spam_slots = original_spam_slots.clone();
		let dispute_state_confirm = DisputeState {
			validators_for: bitvec![BitOrderLsb0, u8; 1, 1, 0, 0, 0, 0, 0, 0],
			validators_against: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
			start: 0,
			concluded_at: None,
		};
		assert_eq!(
			DisputeStateFlags::from_state(&dispute_state_confirm),
			DisputeStateFlags::CONFIRMED
		);
		assert_eq!(
			decrement_spam(spam_slots.as_mut(), &dispute_state_confirm),
			bitvec![BitOrderLsb0, u8; 1, 1, 1, 0, 0, 0, 0, 0],
		);
		assert_eq!(spam_slots, original_spam_slots);

		// Test not confirm is decreasing spam
		let mut spam_slots = original_spam_slots.clone();
		let dispute_state_no_confirm = DisputeState {
			validators_for: bitvec![BitOrderLsb0, u8; 1, 0, 0, 0, 0, 0, 0, 0],
			validators_against: bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
			start: 0,
			concluded_at: None,
		};
		assert_eq!(
			DisputeStateFlags::from_state(&dispute_state_no_confirm),
			DisputeStateFlags::default()
		);
		assert_eq!(
			decrement_spam(spam_slots.as_mut(), &dispute_state_no_confirm),
			bitvec![BitOrderLsb0, u8; 1, 0, 1, 0, 0, 0, 0, 0],
		);
		assert_eq!(spam_slots, vec![0, 1, 1, 3, 4, 5, 6, 7]);
	}

	#[test]
	fn test_check_signature() {
		let validator_id = <ValidatorId as CryptoType>::Pair::generate().0;
		let wrong_validator_id = <ValidatorId as CryptoType>::Pair::generate().0;

		let session = 0;
		let wrong_session = 1;
		let candidate_hash = CandidateHash(sp_core::H256::repeat_byte(1));
		let wrong_candidate_hash = CandidateHash(sp_core::H256::repeat_byte(2));
		let inclusion_parent = sp_core::H256::repeat_byte(3);
		let wrong_inclusion_parent = sp_core::H256::repeat_byte(4);

		let statement_1 = DisputeStatement::Valid(ValidDisputeStatementKind::Explicit);
		let statement_2 = DisputeStatement::Valid(
			ValidDisputeStatementKind::BackingSeconded(inclusion_parent.clone())
		);
		let wrong_statement_2 = DisputeStatement::Valid(
			ValidDisputeStatementKind::BackingSeconded(wrong_inclusion_parent.clone())
		);
		let statement_3 = DisputeStatement::Valid(
			ValidDisputeStatementKind::BackingValid(inclusion_parent.clone())
		);
		let wrong_statement_3 = DisputeStatement::Valid(
			ValidDisputeStatementKind::BackingValid(wrong_inclusion_parent.clone())
		);
		let statement_4 = DisputeStatement::Valid(ValidDisputeStatementKind::ApprovalChecking);
		let statement_5 = DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit);

		let signed_1 = validator_id.sign(
			&ExplicitDisputeStatement {
				valid: true,
				candidate_hash: candidate_hash.clone(),
				session,
			}.signing_payload()
		);
		let signed_2 = validator_id.sign(
			&CompactStatement::Seconded(candidate_hash.clone())
				.signing_payload(&SigningContext {
					session_index: session,
					parent_hash: inclusion_parent.clone()
				})
		);
		let signed_3 = validator_id.sign(
			&CompactStatement::Valid(candidate_hash.clone())
				.signing_payload(&SigningContext {
					session_index: session,
					parent_hash: inclusion_parent.clone()
				})
		);
		let signed_4 = validator_id.sign(
			&ApprovalVote(candidate_hash.clone()).signing_payload(session)
		);
		let signed_5 = validator_id.sign(
			&ExplicitDisputeStatement {
				valid: false,
				candidate_hash: candidate_hash.clone(),
				session,
			}.signing_payload()
		);

		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_1, &signed_1).is_ok());
		assert!(check_signature(&wrong_validator_id.public(), candidate_hash, session, &statement_1, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), wrong_candidate_hash, session, &statement_1, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, wrong_session, &statement_1, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_2, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_3, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_4, &signed_1).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_5, &signed_1).is_err());

		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_2, &signed_2).is_ok());
		assert!(check_signature(&wrong_validator_id.public(), candidate_hash, session, &statement_2, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), wrong_candidate_hash, session, &statement_2, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, wrong_session, &statement_2, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &wrong_statement_2, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_1, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_3, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_4, &signed_2).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_5, &signed_2).is_err());

		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_3, &signed_3).is_ok());
		assert!(check_signature(&wrong_validator_id.public(), candidate_hash, session, &statement_3, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), wrong_candidate_hash, session, &statement_3, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, wrong_session, &statement_3, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &wrong_statement_3, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_1, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_2, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_4, &signed_3).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_5, &signed_3).is_err());

		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_4, &signed_4).is_ok());
		assert!(check_signature(&wrong_validator_id.public(), candidate_hash, session, &statement_4, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), wrong_candidate_hash, session, &statement_4, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, wrong_session, &statement_4, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_1, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_2, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_3, &signed_4).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_5, &signed_4).is_err());

		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_5, &signed_5).is_ok());
		assert!(check_signature(&wrong_validator_id.public(), candidate_hash, session, &statement_5, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), wrong_candidate_hash, session, &statement_5, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, wrong_session, &statement_5, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_1, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_2, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_3, &signed_5).is_err());
		assert!(check_signature(&validator_id.public(), candidate_hash, session, &statement_4, &signed_5).is_err());
	}
}