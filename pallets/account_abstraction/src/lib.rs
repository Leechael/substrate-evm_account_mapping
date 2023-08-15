#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

mod eip712;
mod encode;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod weights;
pub use weights::WeightInfo;

/// The log target of this pallet.
pub const LOG_TARGET: &str = "runtime::account_abstraction";

// Syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: $crate::LOG_TARGET,
			concat!("[{:?}] ", $patter), <frame_system::Pallet<T>>::block_number() $(, $values)*
		)
	};
}

use frame_support::{
	dispatch::{DispatchInfo, Dispatchable, GetDispatchInfo, PostDispatchInfo, RawOrigin},
	traits::{
		fungible::{Inspect as InspectFungible, Mutate as MutateFungible},
		tokens::{ExistenceRequirement, Fortitude, Preservation, WithdrawReasons},
		Contains, Currency, OriginTrait,
	},
	weights::Weight,
};
use pallet_transaction_payment::OnChargeTransaction;
use sp_runtime::traits::TrailingZeroInput;
use sp_runtime::FixedPointOperand;

type PaymentBalanceOf<T> = <<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::Balance;

type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

pub type EIP712ChainID = sp_core::U256;
pub type EIP712VerifyingContractAddress = sp_core::H160;

pub type Nonce = u64;
pub type Keccak256Signature = [u8; 32];

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_std::prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Configure the pallet by specifying the parameters and types on which it depends.
	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_transaction_payment::Config {
		/// Because this pallet emits events, it depends on the runtime's definition of an event.
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching call type.
		type RuntimeCall: Parameter
			+ Dispatchable<
				RuntimeOrigin = Self::RuntimeOrigin,
				Info = DispatchInfo,
				PostInfo = PostDispatchInfo,
			> + GetDispatchInfo
			+ codec::Decode
			+ codec::Encode
			+ scale_info::TypeInfo
			+ IsType<<Self as frame_system::Config>::RuntimeCall>;

		/// The system's currency for payment.
		type Currency: Currency<Self::AccountId>
			+ InspectFungible<Self::AccountId>
			+ MutateFungible<Self::AccountId>;

		#[pallet::constant]
		type ServiceFee: Get<BalanceOf<Self>>;

		type CallFilter: Contains<<Self as frame_system::Config>::RuntimeCall>;

		#[pallet::constant]
		type EIP712Name: Get<Vec<u8>>;

		#[pallet::constant]
		type EIP712Version: Get<Vec<u8>>;

		#[pallet::constant]
		type EIP712ChainID: Get<EIP712ChainID>;

		#[pallet::constant]
		type EIP712VerifyingContractAddress: Get<EIP712VerifyingContractAddress>;

		/// Type representing the weight of this pallet
		type WeightInfo: WeightInfo;
	}

	// Pallets use events to inform users when important changes are made.
	// https://docs.substrate.io/main-docs/build/events-errors/
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		ServiceFeePaid {
			who: T::AccountId,
			fee: BalanceOf<T>,
		},
		TransactionFeePaid {
			who: T::AccountId,
			actual_fee: PaymentBalanceOf<T>,
			tip: PaymentBalanceOf<T>,
		},
		CallDone {
			who: T::AccountId,
			call_result: DispatchResultWithPostInfo,
		},
	}

	// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		Unexpected,
		InvalidSignature,
		AccountMismatch,
		DecodeError,
		NonceError,
		PaymentError,
	}

	#[pallet::storage]
	pub(crate) type AccountNonce<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, u64, ValueQuery>;

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T>
	where
		PaymentBalanceOf<T>: Send + Sync + FixedPointOperand,
		<T as frame_system::Config>::RuntimeCall:
			Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
		<T as frame_system::Config>::AccountId: From<[u8; 32]> + Into<[u8; 32]>,
		T: frame_system::Config<AccountId = sp_runtime::AccountId32>,
	{
		type Call = Call<T>;

		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			// Only allow `remote_call_from_evm_chain`
			let Call::remote_call_from_evm_chain {
				ref who,
				ref call_data,
				ref nonce,
				ref signature,
				ref tip,
			} = call else {
				return Err(InvalidTransaction::Call.into())
			};

			// Check the signature and get the public key
			let message_hash = Self::eip712_message_hash(who.clone(), &call_data, *nonce);
			let Some(recovered_key) = Pallet::<T>::ecdsa_recover_public_key(signature, &message_hash) else {
				return Err(InvalidTransaction::BadProof.into())
			};

			// Check the caller
			let public_key = recovered_key.to_encoded_point(true).to_bytes();
			let decoded_account =
				T::AccountId::decode(&mut &sp_io::hashing::blake2_256(&public_key)[..]).unwrap();
			if who != &decoded_account {
				return Err(InvalidTransaction::BadSigner.into());
			}

			// Skip frame_system::CheckNonZeroSender
			// Skip frame_system::CheckSpecVersion<Runtime>
			// Skip frame_system::CheckTxVersion<Runtime>
			// Skip frame_system::CheckGenesis<Runtime>
			// Skip frame_system::CheckEra<Runtime>

			// frame_system::CheckNonce<Runtime>
			let account_nonce = AccountNonce::<T>::get(who);
			if nonce < &account_nonce {
				return Err(InvalidTransaction::Stale.into());
			}
			let provides = (who, nonce).encode();
			let requires = if &account_nonce < nonce && nonce > &0u64 {
				Some((who, nonce - 1).encode())
			} else {
				None
			};
			if nonce != &account_nonce {
				return Err(if nonce < &account_nonce {
					InvalidTransaction::Stale
				} else {
					InvalidTransaction::Future
				}
				.into());
			}
			AccountNonce::<T>::insert(who, account_nonce + 1);

			// Deserialize the call
			// TODO: Configurable upper bound?
			let actual_call =
				<T as Config>::RuntimeCall::decode(&mut TrailingZeroInput::new(call_data))
					.or(Err(InvalidTransaction::Call))?;

			// Skip frame_system::CheckWeight<Runtime>
			// it has implemented `validate_unsigned` and `pre_dispatch_unsigned`, we don't need to do the validate here.

			// Before we check payment, we let the account pay the service fee
			T::Currency::withdraw(
				who,
				T::ServiceFee::get(),
				WithdrawReasons::TRANSACTION_PAYMENT,
				ExistenceRequirement::KeepAlive,
			)
			.or(Err(InvalidTransaction::Payment))?;

			Self::deposit_event(Event::ServiceFeePaid {
				who: who.clone(),
				fee: T::ServiceFee::get(),
			});

			// pallet_transaction_payment::ChargeTransactionPayment<Runtime>
			let tip = tip.unwrap_or(0u32.into());
			let len = actual_call.encoded_size();
			let info = actual_call.get_dispatch_info();
			// We shall get the same `fee` later
			let est_fee =
				pallet_transaction_payment::Pallet::<T>::compute_fee(len as u32, &info, tip);
			// We don't withdraw the fee here, because we can't cache the imbalance
			// Instead, we check the account has enough fee
			// I think this is a hack, or the type can't match
			let est_fee: u128 = est_fee.try_into().or(Err(InvalidTransaction::Payment))?;
			let usable_balance_for_fees: u128 =
				T::Currency::reducible_balance(who, Preservation::Protect, Fortitude::Polite)
					.try_into()
					.or(Err(InvalidTransaction::Payment))?;
			if usable_balance_for_fees < est_fee {
				return Err(InvalidTransaction::Payment.into());
			}

			// Calculate priority
			// Cheat from `get_priority` in frame/transaction-payment/src/lib.rs
			use frame_support::traits::Defensive;
			use sp_runtime::{traits::One, SaturatedConversion, Saturating};
			// Calculate how many such extrinsics we could fit into an empty block and take the
			// limiting factor.
			let max_block_weight = <T as frame_system::Config>::BlockWeights::get().max_block;
			let max_block_length =
				*<T as frame_system::Config>::BlockLength::get().max.get(info.class) as u64;

			// bounded_weight is used as a divisor later so we keep it non-zero.
			let bounded_weight = info.weight.max(Weight::from_parts(1, 1)).min(max_block_weight);
			let bounded_length = (len as u64).clamp(1, max_block_length);

			// returns the scarce resource, i.e. the one that is limiting the number of transactions.
			let max_tx_per_block_weight = max_block_weight
				.checked_div_per_component(&bounded_weight)
				.defensive_proof("bounded_weight is non-zero; qed")
				.unwrap_or(1);
			let max_tx_per_block_length = max_block_length / bounded_length;
			// Given our current knowledge this value is going to be in a reasonable range - i.e.
			// less than 10^9 (2^30), so multiplying by the `tip` value is unlikely to overflow the
			// balance type. We still use saturating ops obviously, but the point is to end up with some
			// `priority` distribution instead of having all transactions saturate the priority.
			let max_tx_per_block = max_tx_per_block_length
				.min(max_tx_per_block_weight)
				.saturated_into::<PaymentBalanceOf<T>>();
			let max_reward = |val: PaymentBalanceOf<T>| val.saturating_mul(max_tx_per_block);

			// To distribute no-tip transactions a little bit, we increase the tip value by one.
			// This means that given two transactions without a tip, smaller one will be preferred.
			let tip = tip.saturating_add(One::one());
			let scaled_tip = max_reward(tip);

			let priority = scaled_tip.saturated_into::<TransactionPriority>();

			// Finish the validation
			let valid_transaction_builder = ValidTransaction::with_tag_prefix("AccountAbstraction")
				.priority(priority)
				.and_provides(provides)
				.longevity(5)
				.propagate(true);
			let Some(requires) = requires else {
				return valid_transaction_builder.build()
			};
			valid_transaction_builder.and_requires(requires).build()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		PaymentBalanceOf<T>: FixedPointOperand,
		<T as frame_system::Config>::RuntimeCall:
			Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
		T: frame_system::Config<AccountId = sp_runtime::AccountId32>,
	{
		/// Meta-transaction from EVM compatible chains
		#[pallet::call_index(0)]
		#[pallet::weight({
			let call = <T as Config>::RuntimeCall::decode(&mut TrailingZeroInput::new(call_data)).or(Err(Error::<T>::DecodeError));
			if let Ok(call) = call {
				let di = call.get_dispatch_info();
				// TODO: benchmarking here
				(
					Weight::zero().saturating_add(di.weight),
					di.class
				)
			} else {
				// TODO: benchmarking here
				(Weight::zero(), DispatchClass::Normal)
			}
		})]
		pub fn remote_call_from_evm_chain(
			origin: OriginFor<T>,
			who: T::AccountId,
			call_data: BoundedVec<u8, ConstU32<2048>>,
			nonce: Nonce,
			signature: [u8; 65],
			tip: Option<PaymentBalanceOf<T>>,
		) -> DispatchResultWithPostInfo {
			use sp_io::hashing::{blake2_256};

			// This is an unsigned transaction
			ensure_none(origin)?;

			// Verify the signature and get the public key
			let message_hash = Self::eip712_message_hash(who.clone(), &call_data, nonce);
			let Some(recovered_key) = Self::ecdsa_recover_public_key(&signature, &message_hash) else {
				return Err(Error::<T>::InvalidSignature.into())
			};
			let public_key = recovered_key.to_encoded_point(true).to_bytes();

			// Deserialize the caller
			let decoded_account = T::AccountId::decode(&mut &blake2_256(&public_key)[..]).unwrap();
			ensure!(decoded_account == who, Error::<T>::AccountMismatch);

			// Call
			let mut origin: T::RuntimeOrigin = RawOrigin::Signed(who.clone()).into();
			origin.add_filter(T::CallFilter::contains);
			let call = <T as Config>::RuntimeCall::decode(&mut TrailingZeroInput::new(&call_data))
				.or(Err(Error::<T>::DecodeError))?;
			let len = call.encoded_size();
			let info = call.get_dispatch_info();
			let tip = tip.unwrap_or(0u32.into());
			let est_fee =
				pallet_transaction_payment::Pallet::<T>::compute_fee(len as u32, &info, tip);
			let already_withdrawn =
				<<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::withdraw_fee(&who, &call.clone().into(), &info, est_fee, tip).map_err(|_err| Error::<T>::PaymentError)?;

			let call_result = call.dispatch(origin);
			let post_info = match call_result {
				Ok(post_info) => post_info,
				Err(error_and_info) => error_and_info.post_info,
			};
			// Deposit the call's result
			Self::deposit_event(Event::CallDone { who: who.clone(), call_result });

			let actual_fee = pallet_transaction_payment::Pallet::<T>::compute_actual_fee(
				len as u32, &info, &post_info, tip,
			);
			// frame/transaction-payment/src/payment.rs
			<<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::correct_and_deposit_fee(
				&who, &info, &post_info, actual_fee, tip, already_withdrawn
			).map_err(|_err| Error::<T>::PaymentError)?;
			Self::deposit_event(Event::TransactionFeePaid { who: who.clone(), actual_fee, tip });

			// TODO: need add the actual fee
			call_result
		}
	}

	impl<T: Config> Pallet<T>
	where
		T: frame_system::Config<AccountId = sp_runtime::AccountId32>,
	{
		pub(crate) fn ecdsa_recover_public_key(
			signature: &[u8],
			message: &[u8],
		) -> Option<k256::ecdsa::VerifyingKey> {
			use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

			let rid = RecoveryId::try_from(if signature[64] > 26 {
				signature[64] - 27
			} else {
				signature[64]
			})
			.ok()?;
			let sig = Signature::from_slice(&signature[..64]).ok()?;

			VerifyingKey::recover_from_prehash(message, &sig, rid).ok()
		}

		pub(crate) fn eip712_message_hash(
			who: T::AccountId,
			call_data: &BoundedVec<u8, ConstU32<2048>>,
			nonce: Nonce
		) -> Keccak256Signature {
			// TODO: will refactor this in Kevin's way for performance.
			let eip712_domain = crate::eip712::EIP712Domain {
				name: T::EIP712Name::get(),
				version: T::EIP712Version::get(),
				chain_id: T::EIP712ChainID::get(),
				verifying_contract: T::EIP712VerifyingContractAddress::get(),
				salt: None,
			};
			let domain_separator = eip712_domain.separator();

			let type_hash = sp_io::hashing::keccak_256(
				"SubstrateCall(string who,bytes callData,uint64 nonce)".as_bytes(),
			);
			// Token::Uint(U256::from(keccak_256(&self.name)))
			use sp_core::crypto::Ss58Codec;
			let ss58_who = who.to_ss58check_with_version(T::SS58Prefix::get().into());
			let hashed_call_data = sp_io::hashing::keccak_256(&call_data);
			let message_hash = sp_io::hashing::keccak_256(&ethabi::encode(&[
				ethabi::Token::FixedBytes(type_hash.to_vec()),
				ethabi::Token::FixedBytes(sp_io::hashing::keccak_256(ss58_who.as_bytes()).to_vec()),
				ethabi::Token::FixedBytes(hashed_call_data.to_vec()),
				ethabi::Token::Uint(nonce.into()),
			]));

			let typed_data_hash_input = &vec![
				crate::encode::SolidityDataType::String("\x19\x01"),
				crate::encode::SolidityDataType::Bytes(&domain_separator),
				crate::encode::SolidityDataType::Bytes(&message_hash),
			];
			let bytes = crate::encode::abi::encode_packed(typed_data_hash_input);
			sp_io::hashing::keccak_256(bytes.as_slice())
		}
	}
}
