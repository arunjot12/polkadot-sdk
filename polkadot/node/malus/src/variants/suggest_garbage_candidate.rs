// Copyright (C) Parity Technologies (UK) Ltd.
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

//! A malicious node that stores bogus availability chunks, preventing others from
//! doing approval voting. This should lead to disputes depending if the validator
//! has fetched a malicious chunk.
//!
//! Attention: For usage with `zombienet` only!

#![allow(missing_docs)]

use futures::channel::oneshot;
use polkadot_cli::{
	service::{
		AuthorityDiscoveryApi, AuxStore, BabeApi, Block, Error, ExtendedOverseerGenArgs,
		HeaderBackend, Overseer, OverseerConnector, OverseerGen, OverseerGenArgs, OverseerHandle,
		ParachainHost, ProvideRuntimeApi,
	},
	validator_overseer_builder, Cli,
};
use polkadot_node_primitives::{AvailableData, BlockData, PoV};
use polkadot_node_subsystem_types::DefaultSubsystemClient;
use polkadot_primitives::{CandidateDescriptor, CandidateReceipt};

use polkadot_node_subsystem_util::request_validators;
use sp_core::traits::SpawnNamed;

use rand::distributions::{Bernoulli, Distribution};

// Filter wrapping related types.
use crate::{
	interceptor::*,
	shared::{MALICIOUS_POV, MALUS},
	variants::{
		create_fake_candidate_commitments, FakeCandidateValidation, FakeCandidateValidationError,
		ReplaceValidationResult,
	},
};

// Import extra types relevant to the particular
// subsystem.
use polkadot_node_subsystem::{messages::CandidateBackingMessage, SpawnGlue};

use std::sync::Arc;

/// Replace outgoing approval messages with disputes.
#[derive(Clone)]
struct NoteCandidate<Spawner> {
	spawner: Spawner,
	percentage: f64,
}

impl<Sender, Spawner> MessageInterceptor<Sender> for NoteCandidate<Spawner>
where
	Sender: overseer::CandidateBackingSenderTrait + Clone + Send + 'static,
	Spawner: overseer::gen::Spawner + Clone + 'static,
{
	type Message = CandidateBackingMessage;

	/// Intercept incoming `Second` requests from the `collator-protocol` subsystem.
	fn intercept_incoming(
		&self,
		subsystem_sender: &mut Sender,
		msg: FromOrchestra<Self::Message>,
	) -> Option<FromOrchestra<Self::Message>> {
		match msg {
			FromOrchestra::Communication {
				msg:
					CandidateBackingMessage::Second(
						relay_parent,
						ref candidate,
						ref validation_data,
						ref _pov,
					),
			} => {
				gum::debug!(
					target: MALUS,
					candidate_hash = ?candidate.hash(),
					?relay_parent,
					"Received request to second candidate",
				);

				// Need to draw value from Bernoulli distribution with given probability of success
				// defined by the clap parameter. Note that clap parameter must be f64 since this is
				// expected by the Bernoulli::new() function. It must be converted from u8, due to
				// the lack of support for the .range() call on u64 in the clap crate.
				let distribution = Bernoulli::new(self.percentage / 100.0)
					.expect("Invalid probability! Percentage must be in range [0..=100].");

				// Draw a random boolean from the Bernoulli distribution with probability of true
				// equal to `p`. We use `rand::thread_rng` as the source of randomness.
				let generate_malicious_candidate = distribution.sample(&mut rand::thread_rng());

				if generate_malicious_candidate {
					gum::debug!(target: MALUS, "😈 Suggesting malicious candidate.",);

					let pov = PoV { block_data: BlockData(MALICIOUS_POV.into()) };

					let (sender, receiver) = std::sync::mpsc::channel();
					let mut new_sender = subsystem_sender.clone();
					let _candidate = candidate.clone();
					let validation_data = validation_data.clone();
					self.spawner.spawn_blocking(
						"malus-get-validation-data",
						Some("malus"),
						Box::pin(async move {
							gum::trace!(target: MALUS, "Requesting validators");
							let n_validators = request_validators(relay_parent, &mut new_sender)
								.await
								.await
								.unwrap()
								.unwrap()
								.len();
							gum::trace!(target: MALUS, "Validators {}", n_validators);

							let validation_code = {
								let validation_code_hash =
									_candidate.descriptor().validation_code_hash;
								let (tx, rx) = oneshot::channel();
								new_sender
									.send_message(RuntimeApiMessage::Request(
										relay_parent,
										RuntimeApiRequest::ValidationCodeByHash(
											validation_code_hash,
											tx,
										),
									))
									.await;

								let code = rx.await.expect("Querying the RuntimeApi should work");
								match code {
									Err(e) => {
										gum::error!(
											target: MALUS,
											?validation_code_hash,
											error = %e,
											"Failed to fetch validation code",
										);

										sender.send(None).expect("channel is still open");
										return
									},
									Ok(None) => {
										gum::debug!(
											target: MALUS,
											?validation_code_hash,
											"Could not find validation code on chain",
										);

										sender.send(None).expect("channel is still open");
										return
									},
									Ok(Some(c)) => c,
								}
							};

							sender
								.send(Some((validation_data, validation_code, n_validators)))
								.expect("channel is still open");
						}),
					);

					let (validation_data, validation_code, n_validators) =
						receiver.recv().unwrap()?;

					let validation_data_hash = validation_data.hash();
					let validation_code_hash = validation_code.hash();
					let validation_data_relay_parent_number = validation_data.relay_parent_number;

					gum::trace!(
						target: MALUS,
						candidate_hash = ?candidate.hash(),
						?relay_parent,
						?n_validators,
						?validation_data_hash,
						?validation_code_hash,
						?validation_data_relay_parent_number,
						"Fetched validation data."
					);

					let malicious_available_data = AvailableData {
						pov: Arc::new(pov.clone()),
						validation_data: validation_data.clone(),
					};

					let pov_hash = pov.hash();
					let erasure_root = {
						let chunks = erasure::obtain_chunks_v1(
							n_validators as usize,
							&malicious_available_data,
						)
						.unwrap();

						let branches = erasure::branches(chunks.as_ref());
						branches.root()
					};

					let (collator_id, collator_signature) = {
						use polkadot_primitives::CollatorPair;
						use sp_core::crypto::Pair;

						let collator_pair = CollatorPair::generate().0;
						let signature_payload = polkadot_primitives::collator_signature_payload(
							&relay_parent,
							&candidate.descriptor().para_id,
							&validation_data_hash,
							&pov_hash,
							&validation_code_hash,
						);

						(collator_pair.public(), collator_pair.sign(&signature_payload))
					};

					let malicious_commitments = create_fake_candidate_commitments(
						&malicious_available_data.validation_data,
					);

					let malicious_candidate = CandidateReceipt {
						descriptor: CandidateDescriptor {
							para_id: candidate.descriptor().para_id,
							relay_parent,
							collator: collator_id,
							persisted_validation_data_hash: validation_data_hash,
							pov_hash,
							erasure_root,
							signature: collator_signature,
							para_head: malicious_commitments.head_data.hash(),
							validation_code_hash,
						},
						commitments_hash: malicious_commitments.hash(),
					};
					let malicious_candidate_hash = malicious_candidate.hash();

					let message = FromOrchestra::Communication {
						msg: CandidateBackingMessage::Second(
							relay_parent,
							malicious_candidate,
							validation_data,
							pov,
						),
					};

					gum::info!(
						target: MALUS,
						candidate_hash = ?candidate.hash(),
						"😈 Intercepted CandidateBackingMessage::Second and created malicious candidate with hash: {:?}",
						&malicious_candidate_hash
					);
					Some(message)
				} else {
					Some(msg)
				}
			},
			FromOrchestra::Communication { msg } => Some(FromOrchestra::Communication { msg }),
			FromOrchestra::Signal(signal) => Some(FromOrchestra::Signal(signal)),
		}
	}
}

#[derive(Debug, clap::Parser)]
#[clap(rename_all = "kebab-case")]
#[allow(missing_docs)]
pub struct SuggestGarbageCandidateOptions {
	/// Determines the percentage of malicious candidates that are suggested by malus,
	/// based on the total number of intercepted CandidateBacking
	/// Must be in the range [0..=100].
	#[clap(short, long, ignore_case = true, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
	pub percentage: u8,

	#[clap(flatten)]
	pub cli: Cli,
}

/// Garbage candidate implementation wrapper which implements `OverseerGen` glue.
pub(crate) struct SuggestGarbageCandidates {
	/// The probability of behaving maliciously.
	pub percentage: u8,
}

impl OverseerGen for SuggestGarbageCandidates {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<'_, Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<
		(Overseer<SpawnGlue<Spawner>, Arc<DefaultSubsystemClient<RuntimeClient>>>, OverseerHandle),
		Error,
	>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		gum::info!(
			target: MALUS,
			"😈 Started Malus node with a {:?} percent chance of behaving maliciously for a given candidate.",
			&self.percentage,
		);

		let note_candidate = NoteCandidate {
			spawner: SpawnGlue(args.spawner.clone()),
			percentage: f64::from(self.percentage),
		};
		let fake_valid_probability = 100.0;
		let validation_filter = ReplaceValidationResult::new(
			FakeCandidateValidation::BackingAndApprovalValid,
			FakeCandidateValidationError::InvalidOutputs,
			fake_valid_probability,
			SpawnGlue(args.spawner.clone()),
		);

		validator_overseer_builder(
			args,
			ext_args.expect("Extended arguments required to build validator overseer are provided"),
		)?
		.replace_candidate_backing(move |cb| InterceptedSubsystem::new(cb, note_candidate))
		.replace_candidate_validation(move |cb| InterceptedSubsystem::new(cb, validation_filter))
		.build_with_connector(connector)
		.map_err(|e| e.into())
	}
}
