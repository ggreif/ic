use ic_base_types::{NumBytes, NumSeconds};
use ic_config::subnet_config::SchedulerConfig;
use ic_constants::SMALL_APP_SUBNET_MAX_SIZE;
use ic_interfaces::execution_environment::SystemApi;
use ic_logger::replica_logger::no_op_logger;
use ic_nns_constants::CYCLES_MINTING_CANISTER_ID;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{NetworkTopology, SystemState};
use ic_system_api::sandbox_safe_system_state::SandboxSafeSystemState;
use ic_test_utilities::{
    cycles_account_manager::CyclesAccountManagerBuilder,
    mock_time,
    state::SystemStateBuilder,
    types::{
        ids::{canister_test_id, subnet_test_id, user_test_id},
        messages::{RequestBuilder, ResponseBuilder},
    },
};
use ic_types::{
    messages::MAX_INTER_CANISTER_PAYLOAD_IN_BYTES, ComputeAllocation, Cycles, NumInstructions,
};
use prometheus::IntCounter;
use std::convert::From;

mod common;
use common::*;

const MAX_NUM_INSTRUCTIONS: NumInstructions = NumInstructions::new(1 << 30);
const INITIAL_CYCLES: Cycles = Cycles::new(5_000_000_000_000);

#[test]
fn push_output_request_fails_not_enough_cycles_for_request() {
    let request = RequestBuilder::default()
        .sender(canister_test_id(0))
        .build();

    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_max_num_instructions(MAX_NUM_INSTRUCTIONS)
        .build();

    let request_payload_cost = cycles_account_manager
        .xnet_call_bytes_transmitted_fee(request.payload_size_bytes(), SMALL_APP_SUBNET_MAX_SIZE);

    // Set cycles balance low enough that not even the cost for transferring
    // the request is covered.
    let system_state = SystemState::new_running(
        canister_test_id(0),
        user_test_id(1).get(),
        request_payload_cost - Cycles::new(10),
        NumSeconds::from(100_000),
    );

    let mut sandbox_safe_system_state = SandboxSafeSystemState::new(
        &system_state,
        cycles_account_manager,
        &NetworkTopology::default(),
        SchedulerConfig::application_subnet().dirty_page_overhead,
    );

    assert_eq!(
        sandbox_safe_system_state.push_output_request(
            NumBytes::from(0),
            ComputeAllocation::default(),
            request.clone(),
            Cycles::zero(),
            Cycles::zero(),
        ),
        Err(request)
    );
}

#[test]
fn push_output_request_fails_not_enough_cycles_for_response() {
    let request = RequestBuilder::default()
        .sender(canister_test_id(0))
        .build();

    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_max_num_instructions(MAX_NUM_INSTRUCTIONS)
        .build();

    let xnet_cost = cycles_account_manager.xnet_call_performed_fee(SMALL_APP_SUBNET_MAX_SIZE);
    let request_payload_cost = cycles_account_manager
        .xnet_call_bytes_transmitted_fee(request.payload_size_bytes(), SMALL_APP_SUBNET_MAX_SIZE);
    let prepayment_for_response_execution =
        cycles_account_manager.prepayment_for_response_execution(SMALL_APP_SUBNET_MAX_SIZE);
    let prepayment_for_response_transmission =
        cycles_account_manager.prepayment_for_response_transmission(SMALL_APP_SUBNET_MAX_SIZE);
    let total_cost = xnet_cost
        + request_payload_cost
        + prepayment_for_response_execution
        + prepayment_for_response_transmission;

    // Set cycles balance to a number that is enough to cover for the request
    // transfer but not to cover the cost of processing the expected response.
    let system_state = SystemState::new_running(
        canister_test_id(0),
        user_test_id(1).get(),
        total_cost - Cycles::new(10),
        NumSeconds::from(100_000),
    );

    let mut sandbox_safe_system_state = SandboxSafeSystemState::new(
        &system_state,
        cycles_account_manager,
        &NetworkTopology::default(),
        SchedulerConfig::application_subnet().dirty_page_overhead,
    );

    assert_eq!(
        sandbox_safe_system_state.push_output_request(
            NumBytes::from(0),
            ComputeAllocation::default(),
            request.clone(),
            prepayment_for_response_execution,
            prepayment_for_response_transmission
        ),
        Err(request)
    );
}

#[test]
fn push_output_request_succeeds_with_enough_cycles() {
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_max_num_instructions(MAX_NUM_INSTRUCTIONS)
        .build();

    let system_state = SystemState::new_running(
        canister_test_id(0),
        user_test_id(1).get(),
        INITIAL_CYCLES,
        NumSeconds::from(100_000),
    );

    let mut sandbox_safe_system_state = SandboxSafeSystemState::new(
        &system_state,
        cycles_account_manager,
        &NetworkTopology::default(),
        SchedulerConfig::application_subnet().dirty_page_overhead,
    );

    let prepayment_for_response_execution =
        cycles_account_manager.prepayment_for_response_execution(SMALL_APP_SUBNET_MAX_SIZE);
    let prepayment_for_response_transmission =
        cycles_account_manager.prepayment_for_response_transmission(SMALL_APP_SUBNET_MAX_SIZE);

    assert_eq!(
        sandbox_safe_system_state.push_output_request(
            NumBytes::from(0),
            ComputeAllocation::default(),
            RequestBuilder::default()
                .sender(canister_test_id(0))
                .build(),
            prepayment_for_response_execution,
            prepayment_for_response_transmission,
        ),
        Ok(())
    );
}

#[test]
fn correct_charging_source_canister_for_a_request() {
    let subnet_type = SubnetType::Application;
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_max_num_instructions(MAX_NUM_INSTRUCTIONS)
        .with_subnet_type(subnet_type)
        .build();
    let mut system_state = SystemState::new_running(
        canister_test_id(0),
        user_test_id(1).get(),
        INITIAL_CYCLES,
        NumSeconds::from(100_000),
    );

    let initial_cycles_balance = system_state.balance();

    let mut sandbox_safe_system_state = SandboxSafeSystemState::new(
        &system_state,
        cycles_account_manager,
        &NetworkTopology::default(),
        SchedulerConfig::application_subnet().dirty_page_overhead,
    );

    let request = RequestBuilder::default()
        .sender(canister_test_id(0))
        .receiver(canister_test_id(1))
        .build();

    let xnet_cost = cycles_account_manager.xnet_call_performed_fee(SMALL_APP_SUBNET_MAX_SIZE);
    let request_payload_cost = cycles_account_manager
        .xnet_call_bytes_transmitted_fee(request.payload_size_bytes(), SMALL_APP_SUBNET_MAX_SIZE);
    let prepayment_for_response_execution =
        cycles_account_manager.prepayment_for_response_execution(SMALL_APP_SUBNET_MAX_SIZE);
    let prepayment_for_response_transmission =
        cycles_account_manager.prepayment_for_response_transmission(SMALL_APP_SUBNET_MAX_SIZE);
    let total_cost = xnet_cost
        + request_payload_cost
        + prepayment_for_response_execution
        + prepayment_for_response_transmission;

    // Enqueue the Request.
    sandbox_safe_system_state
        .push_output_request(
            NumBytes::from(0),
            ComputeAllocation::default(),
            request,
            prepayment_for_response_execution,
            prepayment_for_response_transmission,
        )
        .unwrap();

    // Assume the destination canister got the message and prepared a response
    let response = ResponseBuilder::default()
        .respondent(canister_test_id(1))
        .originator(canister_test_id(0))
        .build();

    // The response will find its way into the
    // ExecutionEnvironmentImpl::execute_canister_response()
    // => Mock the response_cycles_refund() invocation from the
    // execute_canister_response()
    sandbox_safe_system_state
        .system_state_changes
        .apply_changes(
            mock_time(),
            &mut system_state,
            &default_network_topology(),
            subnet_test_id(1),
            &no_op_logger(),
        )
        .unwrap();
    let no_op_counter: IntCounter = IntCounter::new("no_op", "no_op").unwrap();
    let refund_cycles = cycles_account_manager.refund_for_response_transmission(
        &no_op_logger(),
        &no_op_counter,
        &response,
        prepayment_for_response_transmission,
        SMALL_APP_SUBNET_MAX_SIZE,
    );

    cycles_account_manager.refund_cycles(&mut system_state, refund_cycles);

    // MAX_NUM_INSTRUCTIONS also gets partially refunded in the real
    // ExecutionEnvironmentImpl::execute_canister_response()
    assert_eq!(
        initial_cycles_balance - total_cost
            + cycles_account_manager.xnet_call_bytes_transmitted_fee(
                MAX_INTER_CANISTER_PAYLOAD_IN_BYTES - response.payload_size_bytes(),
                SMALL_APP_SUBNET_MAX_SIZE
            ),
        system_state.balance()
    );
}

#[test]
fn mint_all_cycles() {
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_subnet_type(SubnetType::System)
        .build();

    let api_type = ApiTypeBuilder::build_update_api();
    let mut api = get_system_api(api_type, &get_cmc_system_state(), cycles_account_manager);
    let balance_before = api.ic0_canister_cycle_balance().unwrap();

    let amount = 50;
    assert_eq!(api.ic0_mint_cycles(amount), Ok(amount));
    assert_eq!(
        api.ic0_canister_cycle_balance().unwrap() - balance_before,
        amount
    );
}

#[test]
fn mint_cycles_large_value() {
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_subnet_type(SubnetType::System)
        .build();
    let mut system_state = SystemStateBuilder::new()
        .canister_id(CYCLES_MINTING_CANISTER_ID)
        .build();

    cycles_account_manager.add_cycles(
        system_state.balance_mut(),
        Cycles::from(1_000_000_000_000_000_u128),
    );

    let api_type = ApiTypeBuilder::build_update_api();
    let mut api = get_system_api(api_type, &system_state, cycles_account_manager);
    let balance_before = api.ic0_canister_cycle_balance().unwrap();

    let amount = 50;
    // Canisters on the System subnet can hold any amount of cycles
    assert_eq!(api.ic0_mint_cycles(amount), Ok(amount));
    assert_eq!(
        api.ic0_canister_cycle_balance().unwrap() - balance_before,
        amount
    );
}

#[test]
fn mint_cycles_fails_caller_not_on_nns() {
    let system_state = SystemStateBuilder::default().build();
    let cycles_account_manager = CyclesAccountManagerBuilder::new().build();
    let mut api = get_system_api(
        ApiTypeBuilder::build_update_api(),
        &system_state,
        cycles_account_manager,
    );

    let balance_before = api.ic0_canister_cycle_balance().unwrap();

    assert!(api.ic0_mint_cycles(50).is_err());
    assert_eq!(
        api.ic0_canister_cycle_balance().unwrap() - balance_before,
        0
    );
}

#[test]
fn call_increases_cycles_consumed_metric() {
    let mut system_state = SystemStateBuilder::default().build();
    let cycles_account_manager = CyclesAccountManagerBuilder::new().build();
    let mut api = get_system_api(
        ApiTypeBuilder::build_update_api(),
        &system_state,
        cycles_account_manager,
    );

    api.ic0_call_simple(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, &[])
        .unwrap();

    let system_state_changes = api.into_system_state_changes();
    system_state_changes
        .apply_changes(
            mock_time(),
            &mut system_state,
            &default_network_topology(),
            subnet_test_id(1),
            &no_op_logger(),
        )
        .unwrap();
    assert!(
        system_state
            .canister_metrics
            .consumed_cycles_since_replica_started
            .get()
            > 0
    );
}
