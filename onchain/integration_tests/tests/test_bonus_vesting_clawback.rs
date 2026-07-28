//! Integration tests: bonus_system clawback × token_vesting interaction.
//!
//! Scenario: an employer grants a bonus via bonus_system. The employee claims
//! it and deposits the proceeds into a token_vesting schedule (a common HR
//! re-vesting pattern). A subsequent admin clawback must account correctly for
//! three vesting states:
//!
//!   1. **Fully unvested** — all bonus tokens still sit inside the vesting
//!      contract. A direct clawback fails because the employee holds nothing;
//!      the correct path is to revoke the vesting schedule first, then claw back.
//!
//!   2. **Partially vested** — the employee has claimed the vested tranche from
//!      the vesting contract; only that held amount is clawback-able. The
//!      unvested portion locked in the vesting contract requires a revocation
//!      before it can be recovered.
//!
//!   3. **Fully vested** — all tokens have been claimed from the vesting
//!      schedule into the employee wallet. The full bonus amount is clawback-able.
//!      A second clawback attempt is explicitly rejected (no double-recovery).

#![cfg(test)]
#![allow(deprecated)]

use bonus_system::{BonusSystemContract, BonusSystemContractClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};
use token_vesting::{TokenVestingContract, TokenVestingContractClient, VestingStatus};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BONUS: i128 = 1_200;
const REASON: u128 = 0xdeadbeef;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn set_time(env: &Env, ts: u64) {
    env.ledger().with_mut(|li| li.timestamp = ts);
}

fn balance(env: &Env, token: &Address, who: &Address) -> i128 {
    TokenClient::new(env, token).balance(who)
}

fn deploy_bonus(env: &Env, owner: &Address) -> BonusSystemContractClient<'_> {
    let id = env.register_contract(None, BonusSystemContract);
    let client = BonusSystemContractClient::new(env, &id);
    client.initialize(owner);
    client
}

fn deploy_vesting(env: &Env, owner: &Address) -> TokenVestingContractClient<'_> {
    let id = env.register_contract(None, TokenVestingContract);
    let client = TokenVestingContractClient::new(env, &id);
    client.initialize(owner);
    client
}

fn make_token(env: &Env, funder: &Address, amount: i128) -> Address {
    let admin = Address::generate(env);
    let addr = env.register_stellar_asset_contract_v2(admin).address();
    StellarAssetClient::new(env, &addr).mint(funder, &amount);
    addr
}

/// Create a one-time bonus, approve it, advance time, and have the employee
/// claim it. Returns the incentive_id.
fn create_and_claim_bonus(
    env: &Env,
    bonus: &BonusSystemContractClient<'_>,
    employer: &Address,
    employee: &Address,
    approver: &Address,
    token: &Address,
    unlock_time: u64,
) -> u128 {
    let id = bonus.create_one_time_bonus(employer, employee, approver, token, &BONUS, &unlock_time);
    bonus.approve_incentive(approver, &id);
    set_time(env, unlock_time + 1);
    bonus.claim_incentive(employee, &id);
    id
}

// ===========================================================================
// 1. Fully unvested: all tokens locked in the vesting contract.
//
//    - Direct clawback fails (employee wallet is empty).
//    - After revoking the vesting schedule the full amount is refunded to the
//      employee, and clawback then succeeds.
// ===========================================================================

#[test]
fn clawback_fully_unvested_requires_vesting_revoke_first() {
    let env = Env::default();
    env.mock_all_auths();

    let owner = Address::generate(&env);
    let employer = Address::generate(&env);
    let employee = Address::generate(&env);
    let approver = Address::generate(&env);

    let token = make_token(&env, &employer, 10_000);
    let bonus = deploy_bonus(&env, &owner);
    let vesting = deploy_vesting(&env, &owner);

    // Employee claims the bonus at t=2.
    let incentive_id =
        create_and_claim_bonus(&env, &bonus, &employer, &employee, &approver, &token, 1);
    assert_eq!(balance(&env, &token, &employee), BONUS);

    // Employee deposits entire bonus into a vesting schedule that starts in
    // the far future — nothing will vest before we intervene.
    set_time(&env, 5);
    let schedule_id = vesting.create_linear_schedule(
        &employee, // employee funds the escrow with their claimed tokens
        &employee,
        &token,
        &BONUS,
        &10_000u64, // start
        &20_000u64, // end
        &None,
        &true, // revocable
    );

    // Wallet is now empty — all tokens locked in vesting.
    assert_eq!(balance(&env, &token, &employee), 0);

    // Direct clawback must fail: employee has no tokens to transfer back.
    let direct =
        bonus.try_execute_clawback(&owner, &employee, &incentive_id, &BONUS, &REASON);
    assert!(
        direct.is_err(),
        "direct clawback must fail when tokens are locked in vesting"
    );

    // Revoke the vesting schedule — all tokens return to employee because
    // they created (and funded) the schedule themselves.
    set_time(&env, 6);
    let refunded = vesting.revoke(&employee, &schedule_id);
    assert_eq!(refunded, BONUS, "fully unvested: full amount must be refunded");
    assert_eq!(vesting.get_schedule(&schedule_id).unwrap().status, VestingStatus::Revoked);
    assert_eq!(balance(&env, &token, &employee), BONUS);

    // Now clawback succeeds.
    let clawed = bonus.execute_clawback(&owner, &employee, &incentive_id, &BONUS, &REASON);
    assert_eq!(clawed, BONUS);
    assert_eq!(bonus.get_clawback_total(&incentive_id), BONUS);
    assert_eq!(balance(&env, &token, &employee), 0);
    assert_eq!(balance(&env, &token, &employer), 10_000);
}

// ===========================================================================
// 2. Partially vested: employee claimed the vested tranche from the vesting
//    contract; unvested remainder is still locked inside it.
//
//    - Clawback of the held (vested) portion succeeds.
//    - Clawback exceeding the held amount fails.
//    - After revoking vesting, the returned unvested portion is also clawable,
//      but the combined total cannot exceed the originally claimed amount.
// ===========================================================================

#[test]
fn clawback_partially_vested_recovers_only_held_amount() {
    let env = Env::default();
    env.mock_all_auths();

    let owner = Address::generate(&env);
    let employer = Address::generate(&env);
    let employee = Address::generate(&env);
    let approver = Address::generate(&env);

    let token = make_token(&env, &employer, 10_000);
    let bonus = deploy_bonus(&env, &owner);
    let vesting = deploy_vesting(&env, &owner);

    // Linear schedule: 1200 over [10_000, 13_000].
    // At t=11_000 (elapsed=1000, duration=3000): vested = 1200 * 1000/3000 = 400.
    let vest_start: u64 = 10_000;
    let vest_end: u64 = 13_000;

    let incentive_id =
        create_and_claim_bonus(&env, &bonus, &employer, &employee, &approver, &token, 1);
    assert_eq!(balance(&env, &token, &employee), BONUS);

    set_time(&env, 5);
    let schedule_id = vesting.create_linear_schedule(
        &employee,
        &employee,
        &token,
        &BONUS,
        &vest_start,
        &vest_end,
        &None,
        &true,
    );
    assert_eq!(balance(&env, &token, &employee), 0);

    // Advance to t=11_000: 400 tokens vested.
    set_time(&env, 11_000);
    assert_eq!(vesting.get_releasable_amount(&schedule_id), 400);
    vesting.claim(&employee, &schedule_id);
    assert_eq!(balance(&env, &token, &employee), 400);

    // Attempting to claw back more than the employee holds must fail.
    let over = bonus.try_execute_clawback(&owner, &employee, &incentive_id, &BONUS, &REASON);
    assert!(over.is_err(), "clawback exceeding held balance must fail");

    // Clawback of exactly the held vested amount succeeds.
    let clawed1 = bonus.execute_clawback(&owner, &employee, &incentive_id, &400, &REASON);
    assert_eq!(clawed1, 400);
    assert_eq!(bonus.get_clawback_total(&incentive_id), 400);
    assert_eq!(balance(&env, &token, &employee), 0);

    // Revoke the vesting schedule to recover the remaining 800.
    let refunded = vesting.revoke(&employee, &schedule_id);
    assert_eq!(refunded, 800);
    assert_eq!(balance(&env, &token, &employee), 800);

    // Claw back the returned unvested portion.
    let clawed2 = bonus.execute_clawback(&owner, &employee, &incentive_id, &800, &REASON);
    assert_eq!(clawed2, 800);
    assert_eq!(bonus.get_clawback_total(&incentive_id), 1_200);

    // Employer fully restored; employee left with nothing.
    assert_eq!(balance(&env, &token, &employee), 0);
    assert_eq!(balance(&env, &token, &employer), 10_000);
}

// ===========================================================================
// 3. Fully vested: employee claimed all tokens from the vesting schedule.
//
//    - The full bonus amount is clawback-able in one call.
//    - A second clawback attempt is rejected (no double-recovery).
// ===========================================================================

#[test]
fn clawback_fully_vested_succeeds_and_double_clawback_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let owner = Address::generate(&env);
    let employer = Address::generate(&env);
    let employee = Address::generate(&env);
    let approver = Address::generate(&env);

    let token = make_token(&env, &employer, 10_000);
    let bonus = deploy_bonus(&env, &owner);
    let vesting = deploy_vesting(&env, &owner);

    let incentive_id =
        create_and_claim_bonus(&env, &bonus, &employer, &employee, &approver, &token, 1);
    assert_eq!(balance(&env, &token, &employee), BONUS);

    set_time(&env, 5);
    let schedule_id = vesting.create_linear_schedule(
        &employee,
        &employee,
        &token,
        &BONUS,
        &1_000u64,
        &2_000u64,
        &None,
        &false, // non-revocable — all tokens vest normally
    );
    assert_eq!(balance(&env, &token, &employee), 0);

    // Advance past vesting end and claim everything.
    set_time(&env, 2_001);
    let released = vesting.claim(&employee, &schedule_id);
    assert_eq!(released, BONUS);
    assert_eq!(vesting.get_schedule(&schedule_id).unwrap().status, VestingStatus::Completed);
    assert_eq!(balance(&env, &token, &employee), BONUS);

    // Clawback the full amount.
    let clawed = bonus.execute_clawback(&owner, &employee, &incentive_id, &BONUS, &REASON);
    assert_eq!(clawed, BONUS);
    assert_eq!(bonus.get_clawback_total(&incentive_id), BONUS);
    assert_eq!(balance(&env, &token, &employee), 0);
    assert_eq!(balance(&env, &token, &employer), 10_000);

    // Second clawback — even for 1 token — must be rejected.
    let double = bonus.try_execute_clawback(&owner, &employee, &incentive_id, &1, &REASON);
    assert!(double.is_err(), "double clawback must be rejected");
}

// ===========================================================================
// 4. Security: clawback_total accumulates correctly across multiple partial
//    calls and can never exceed the originally claimed amount.
// ===========================================================================

#[test]
fn clawback_total_never_exceeds_claimed_amount() {
    let env = Env::default();
    env.mock_all_auths();

    let owner = Address::generate(&env);
    let employer = Address::generate(&env);
    let employee = Address::generate(&env);
    let approver = Address::generate(&env);

    let token = make_token(&env, &employer, 10_000);
    let bonus = deploy_bonus(&env, &owner);
    let vesting = deploy_vesting(&env, &owner);

    let incentive_id =
        create_and_claim_bonus(&env, &bonus, &employer, &employee, &approver, &token, 1);

    // Deposit all into vesting: linear 1200 over [5000, 10000].
    set_time(&env, 5);
    let schedule_id = vesting.create_linear_schedule(
        &employee,
        &employee,
        &token,
        &BONUS,
        &5_000u64,
        &10_000u64,
        &None,
        &true,
    );

    // Advance to midpoint: 600 vested.
    set_time(&env, 7_500);
    vesting.claim(&employee, &schedule_id);
    assert_eq!(balance(&env, &token, &employee), 600);

    // First partial clawback: 600.
    bonus.execute_clawback(&owner, &employee, &incentive_id, &600, &REASON);
    assert_eq!(bonus.get_clawback_total(&incentive_id), 600);

    // Revoke vesting: 600 unvested tokens returned to employee.
    vesting.revoke(&employee, &schedule_id);
    assert_eq!(balance(&env, &token, &employee), 600);

    // Second partial clawback: remaining 600.
    bonus.execute_clawback(&owner, &employee, &incentive_id, &600, &REASON);
    assert_eq!(bonus.get_clawback_total(&incentive_id), 1_200);

    // Any further clawback is rejected.
    let extra = bonus.try_execute_clawback(&owner, &employee, &incentive_id, &1, &REASON);
    assert!(extra.is_err(), "cannot claw back beyond the claimed amount");

    assert_eq!(balance(&env, &token, &employer), 10_000);
    assert_eq!(balance(&env, &token, &employee), 0);
}
