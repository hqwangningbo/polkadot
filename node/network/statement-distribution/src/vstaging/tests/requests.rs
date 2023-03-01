// Copyright 2023 Parity Technologies (UK) Ltd.
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

use super::*;

use parity_scale_codec::{Decode, Encode};
use polkadot_node_network_protocol::request_response::vstaging as request_vstaging;
use polkadot_primitives_test_helpers::make_candidate;
use sc_network::config::{
	IncomingRequest as RawIncomingRequest, OutgoingResponse as RawOutgoingResponse,
};

// TODO [now]: peer reported for providing statements meant to be masked out

// TODO [now]: peer reported for not providing enough statements, request retried

// Test that a peer answering an `AttestedCandidateRequest` with duplicate statements is punished.
#[test]
fn peer_reported_for_duplicate_statements() {
	let config = TestConfig {
		validator_count: 20,
		group_size: 3,
		local_validator: true,
		async_backing_params: None,
	};

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_para = ParaId::from(local_validator.group_index.0);
		let peer_id = PeerId::random();

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			vec![1, 2, 3].into(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let test_leaf = TestLeaf {
			number: 1,
			hash: relay_parent,
			parent_hash: Hash::repeat_byte(0),
			session: 1,
			availability_cores: state.make_availability_cores(|i| {
				CoreState::Scheduled(ScheduledCore {
					para_id: ParaId::from(i as u32),
					collator: None,
				})
			}),
			para_data: (0..state.session_info.validator_groups.len())
				.map(|i| {
					(
						ParaId::from(i as u32),
						PerParaData { min_relay_parent: 1, head_data: vec![1, 2, 3].into() },
					)
				})
				.collect(),
		};

		let other_group_validators = state.group_validators(local_validator.group_index, true);
		let v_a = other_group_validators[0];
		let v_b = other_group_validators[1];

		// peer A is in group, has relay parent in view.
		// peer B is in group, has no relay parent in view.
		// peer C is not in group, has relay parent in view.
		{
			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, local_para, &test_leaf, &state, true).await;

		answer_expected_hypothetical_depth_request(
			&mut overseer,
			vec![],
			Some(relay_parent),
			false,
		)
		.await;

		// Peer in cluster sends a statement, triggering a request.
		{
			let a_seconded = state
				.sign_statement(
					v_a,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();

			send_peer_message(
				&mut overseer,
				peer_a.clone(),
				protocol_vstaging::StatementDistributionMessage::Statement(
					relay_parent,
					a_seconded,
				),
			)
			.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(p, r))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT_FIRST => { }
			);
		}

		// Send a request to peer and mock its response to include two identical statements.
		{
			let b_seconded = state
				.sign_statement(
					v_b,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { parent_hash: relay_parent, session_index: 1 },
				)
				.as_unchecked()
				.clone();
			let statements = vec![b_seconded.clone(), b_seconded.clone()];
			let req = assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendRequests(mut requests, IfDisconnected::ImmediateError)) => {
					assert_eq!(requests.len(), 1);
					assert_matches!(
						requests.pop().unwrap(),
						Requests::AttestedCandidateVStaging(mut outgoing) => {
							assert_eq!(outgoing.peer, Recipient::Peer(peer_a.clone()));
							assert_eq!(outgoing.payload.candidate_hash, candidate_hash);

							let res = AttestedCandidateResponse {
								candidate_receipt: candidate.clone(),
								persisted_validation_data: pvd.clone(),
								statements,
							};
							outgoing.pending_response.send(Ok(res.encode()));
						}
					);
				}
			);

			// TODO: Is it correct that we initially send this reward for a valid statement?
			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(p, r))
					if p == peer_a && r == BENEFIT_VALID_STATEMENT => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(p, r))
					if p == peer_a && r == COST_UNREQUESTED_RESPONSE_STATEMENT => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::ReportPeer(p, r))
					if p == peer_a && r == BENEFIT_VALID_RESPONSE => { }
			);

			assert_matches!(
				overseer.recv().await,
				AllMessages:: NetworkBridgeTx(
					NetworkBridgeTxMessage::SendValidationMessage(
						peers,
						Versioned::VStaging(
							protocol_vstaging::ValidationProtocol::StatementDistribution(
								protocol_vstaging::StatementDistributionMessage::Statement(hash, statement),
							),
						),
					)
				) => {
					assert_eq!(peers, vec![peer_a]);
					assert_eq!(hash, relay_parent);
					assert_eq!(statement, b_seconded);
				}
			);
		}

		answer_expected_hypothetical_depth_request(&mut overseer, vec![], None, false).await;

		overseer
	});
}

// TODO [now]: peer reported for providing statements with invalid signatures or wrong validator IDs

#[test]
fn local_node_sanity_checks_incoming_requests() {
	let config = TestConfig {
		validator_count: 20,
		group_size: 3,
		local_validator: true,
		async_backing_params: None,
	};

	let relay_parent = Hash::repeat_byte(1);
	let peer_a = PeerId::random();
	let peer_b = PeerId::random();
	let peer_c = PeerId::random();
	let peer_d = PeerId::random();

	test_harness(config, |mut state, mut overseer| async move {
		let local_validator = state.local.clone().unwrap();
		let local_para = ParaId::from(local_validator.group_index.0);

		let (candidate, pvd) = make_candidate(
			relay_parent,
			1,
			local_para,
			vec![1, 2, 3].into(),
			vec![4, 5, 6].into(),
			Hash::repeat_byte(42).into(),
		);
		let candidate_hash = candidate.hash();

		let test_leaf = TestLeaf {
			number: 1,
			hash: relay_parent,
			parent_hash: Hash::repeat_byte(0),
			session: 1,
			availability_cores: state.make_availability_cores(|i| {
				CoreState::Scheduled(ScheduledCore {
					para_id: ParaId::from(i as u32),
					collator: None,
				})
			}),
			para_data: (0..state.session_info.validator_groups.len())
				.map(|i| {
					(
						ParaId::from(i as u32),
						PerParaData { min_relay_parent: 1, head_data: vec![1, 2, 3].into() },
					)
				})
				.collect(),
		};

		// peer A is in group, has relay parent in view.
		// peer B is in group, has no relay parent in view.
		// peer C is not in group, has relay parent in view.
		{
			let other_group_validators = state.group_validators(local_validator.group_index, true);

			connect_peer(
				&mut overseer,
				peer_a.clone(),
				Some(vec![state.discovery_id(other_group_validators[0])].into_iter().collect()),
			)
			.await;

			connect_peer(
				&mut overseer,
				peer_b.clone(),
				Some(vec![state.discovery_id(other_group_validators[1])].into_iter().collect()),
			)
			.await;

			connect_peer(&mut overseer, peer_c.clone(), None).await;

			send_peer_view_change(&mut overseer, peer_a.clone(), view![relay_parent]).await;
			send_peer_view_change(&mut overseer, peer_c.clone(), view![relay_parent]).await;
		}

		activate_leaf(&mut overseer, local_para, &test_leaf, &state, true).await;

		answer_expected_hypothetical_depth_request(
			&mut overseer,
			vec![],
			Some(relay_parent),
			false,
		)
		.await;

		let mask = StatementFilter::blank(state.config.group_size);

		// Should drop requests for unknown candidates.
		{
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_c,
					payload: request_vstaging::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(rx.await, Err(oneshot::Canceled));
		}

		// Confirm candidate.
		{
			let full_signed = state
				.sign_statement(
					local_validator.validator_index,
					CompactStatement::Seconded(candidate_hash),
					&SigningContext { session_index: 1, parent_hash: relay_parent },
				)
				.convert_to_superpayload(StatementWithPVD::Seconded(candidate.clone(), pvd.clone()))
				.unwrap();

			overseer
				.send(FromOrchestra::Communication {
					msg: StatementDistributionMessage::Share(relay_parent, full_signed),
				})
				.await;

			assert_matches!(
				overseer.recv().await,
				AllMessages::NetworkBridgeTx(NetworkBridgeTxMessage::SendValidationMessage(
					peers,
					Versioned::VStaging(protocol_vstaging::ValidationProtocol::StatementDistribution(
						protocol_vstaging::StatementDistributionMessage::Statement(
							r,
							s,
						)
					))
				)) => {
					assert_eq!(peers, vec![peer_a.clone()]);
					assert_eq!(r, relay_parent);
					assert_eq!(s.unchecked_payload(), &CompactStatement::Seconded(candidate_hash));
					assert_eq!(s.unchecked_validator_index(), local_validator.validator_index);
				}
			);

			answer_expected_hypothetical_depth_request(&mut overseer, vec![], None, false).await;
		}

		// Should drop requests from unknown peers.
		{
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_d,
					payload: request_vstaging::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(rx.await, Err(oneshot::Canceled));
		}

		// Should drop requests with bitfields of the wrong size.
		{
			let mask = StatementFilter::blank(state.config.group_size + 1);
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_c,
					payload: request_vstaging::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask,
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			assert_matches!(
				rx.await,
				Ok(RawOutgoingResponse {
					result,
					reputation_changes,
					sent_feedback
				}) => {
					assert_matches!(result, Err(()));
					assert_eq!(reputation_changes, vec![COST_INVALID_REQUEST_BITFIELD_SIZE.into_base_rep()]);
					assert_matches!(sent_feedback, None);
				}
			);
		}

		// Local node should reject requests if we did not send a manifest to that peer.
		{
			let (pending_response, rx) = oneshot::channel();
			state
				.req_sender
				.send(RawIncomingRequest {
					// Request from peer that received manifest.
					peer: peer_c,
					payload: request_vstaging::AttestedCandidateRequest {
						candidate_hash: candidate.hash(),
						mask: mask.clone(),
					}
					.encode(),
					pending_response,
				})
				.await
				.unwrap();

			// Should get `COST_UNEXPECTED_REQUEST` response.
			assert_matches!(
				rx.await,
				Ok(RawOutgoingResponse {
					result,
					reputation_changes,
					sent_feedback
				}) => {
					assert_matches!(result, Err(()));
					assert_eq!(reputation_changes, vec![COST_UNEXPECTED_REQUEST.into_base_rep()]);
					assert_matches!(sent_feedback, None);
				}
			);
		}

		overseer
	});
}

// TODO [now]: local node respects statement mask
